//! Cursor's ACP extension methods. Cursor blocks its turn until the client
//! answers them, so they are shown as Praxis prompts and answered from there.

use std::collections::BTreeMap;

use acp_thread::{AcpThread, AgentThreadEntry};
use agent_client_protocol::Responder;
use agent_client_protocol::schema::{
    v1::{self as acp, ErrorCode},
    v2 as acp_v2,
};
use gpui::{AsyncApp, Entity};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use super::{ClientContext, respond_err};

#[derive(Debug, Clone, Deserialize, Serialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "cursor/ask_question", response = CursorAskQuestionResponse)]
pub(super) struct CursorAskQuestionRequest {
    #[serde(rename = "toolCallId", default)]
    tool_call_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    questions: Vec<CursorQuestion>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CursorQuestion {
    #[serde(default)]
    id: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    options: Vec<CursorOption>,
    #[serde(rename = "allowMultiple", default)]
    allow_multiple: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CursorOption {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, agent_client_protocol::JsonRpcResponse)]
pub(super) struct CursorAskQuestionResponse {
    outcome: CursorQuestionOutcome,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CursorQuestionOutcome {
    Answered { answers: Vec<CursorAnswer> },
    Skipped { reason: Option<String> },
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorAnswer {
    question_id: String,
    selected_option_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "cursor/create_plan", response = CursorCreatePlanResponse)]
pub(super) struct CursorCreatePlanRequest {
    #[serde(rename = "toolCallId", default)]
    tool_call_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    plan: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, agent_client_protocol::JsonRpcResponse)]
pub(super) struct CursorCreatePlanResponse {
    outcome: CursorPlanOutcome,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CursorPlanOutcome {
    Accepted {
        #[serde(rename = "planUri", skip_serializing_if = "Option::is_none")]
        plan_uri: Option<String>,
    },
    Rejected {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Cancelled,
}

/// How one Cursor question was offered, so the chosen labels can be mapped
/// back to the option ids Cursor expects.
struct QuestionMapping {
    id: String,
    /// Label shown in the form, paired with the option id it stands for.
    options: Vec<(String, String)>,
    multiple: bool,
}

const PLAN_DECISION_FIELD: &str = "decision";
const ACCEPT_PLAN: &str = "Accept";
const REJECT_PLAN: &str = "Reject";
const MAX_PLAN_MESSAGE_CHARS: usize = 4_000;

pub(super) fn handle_cursor_ask_question(
    request: CursorAskQuestionRequest,
    responder: Responder<CursorAskQuestionResponse>,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let Some(thread) = session_thread_for_tool_call(ctx, &request.tool_call_id, cx) else {
        log::info!(
            "Cursor asked {count} question(s) for tool call {tool_call}, but no conversation could be found to show them in",
            count = request.questions.len(),
            tool_call = request.tool_call_id
        );
        responder
            .respond(CursorAskQuestionResponse {
                outcome: CursorQuestionOutcome::Skipped {
                    reason: Some("Praxis could not find the conversation for this question.".into()),
                },
            })
            .log_err();
        return;
    };

    // Cursor only accepts option ids back, so a free-text answer cannot be
    // returned to it.
    if request
        .questions
        .iter()
        .all(|question| question.options.is_empty())
    {
        log::info!(
            "Cursor asked {count} question(s) for tool call {tool_call} without options, which Praxis cannot answer",
            count = request.questions.len(),
            tool_call = request.tool_call_id
        );
        responder
            .respond(CursorAskQuestionResponse {
                outcome: CursorQuestionOutcome::Skipped {
                    reason: Some(
                        "Praxis can only answer Cursor's questions that offer options.".into(),
                    ),
                },
            })
            .log_err();
        return;
    }

    let mappings = question_mappings(&request);
    let message = request
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Cursor needs an answer");
    let elicitation = acp_v2::CreateElicitationRequest::new(
        acp_v2::ElicitationFormMode::new(
            acp_v2::ElicitationSessionScope::new(thread.read_with(cx, |thread, _| {
                thread.session_id().clone()
            })),
            question_schema(&request),
        ),
        message,
    );
    ask(&thread, elicitation, responder, cx, move |response| {
        cursor_question_response(response, &mappings)
    });
}

pub(super) fn handle_cursor_create_plan(
    request: CursorCreatePlanRequest,
    responder: Responder<CursorCreatePlanResponse>,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let Some(thread) = session_thread_for_tool_call(ctx, &request.tool_call_id, cx) else {
        log::info!(
            "Cursor proposed a plan for tool call {tool_call}, but no conversation could be found to show it in",
            tool_call = request.tool_call_id
        );
        responder
            .respond(CursorCreatePlanResponse {
                outcome: CursorPlanOutcome::Accepted { plan_uri: None },
            })
            .log_err();
        return;
    };

    let mut message = String::new();
    if let Some(overview) = request.overview.as_deref().filter(|text| !text.trim().is_empty()) {
        message.push_str(overview);
        message.push_str("\n\n");
    }
    message.push_str(&request.plan);
    if message.chars().count() > MAX_PLAN_MESSAGE_CHARS {
        message = message.chars().take(MAX_PLAN_MESSAGE_CHARS).collect();
        message.push('…');
    }
    if message.trim().is_empty() {
        message.push_str("Cursor proposed a plan");
    }

    let schema = acp_v2::ElicitationSchema::new().property(
        PLAN_DECISION_FIELD,
        acp_v2::StringPropertySchema::new()
            .title(request.name.clone().unwrap_or_else(|| "Plan".to_string()))
            .enum_values(vec![ACCEPT_PLAN.to_string(), REJECT_PLAN.to_string()]),
        true,
    );
    let elicitation = acp_v2::CreateElicitationRequest::new(
        acp_v2::ElicitationFormMode::new(
            acp_v2::ElicitationSessionScope::new(thread.read_with(cx, |thread, _| {
                thread.session_id().clone()
            })),
            schema,
        ),
        message,
    );
    ask(&thread, elicitation, responder, cx, cursor_plan_response);
}

/// Shows `elicitation` in `thread` and answers the agent with whatever the
/// user chose. A cancelled request also removes the prompt.
fn ask<Response, Convert>(
    thread: &Entity<AcpThread>,
    elicitation: acp_v2::CreateElicitationRequest,
    responder: Responder<Response>,
    cx: &mut AsyncApp,
    convert: Convert,
) where
    Response: agent_client_protocol::JsonRpcResponse + Send + 'static,
    Convert: Fn(acp_v2::CreateElicitationResponse) -> Response + Send + 'static,
{
    let cancellation = responder.cancellation();
    let thread = thread.clone();
    cx.spawn(async move |cx| {
        let (elicitation_id, task) = match thread.update(cx, |thread, cx| {
            thread.request_elicitation_with_id(elicitation, cx)
        }) {
            Ok(pending) => pending,
            Err(error) => return respond_err(responder, acp_thread::elicitation::error_to_v1(error)),
        };
        let result: Result<_, acp::Error> = cancellation
            .run_until_cancelled(async { Ok(task.await) })
            .await;
        match result {
            Ok(response) => {
                responder.respond(convert(response)).log_err();
            }
            Err(error) => {
                if error.code == ErrorCode::RequestCancelled {
                    thread
                        .update(cx, |thread, cx| {
                            thread.cancel_elicitation(&elicitation_id, cx)
                        })
                        .log_err();
                }
                respond_err(responder, error);
            }
        }
    })
    .detach();
}

/// Cursor's requests carry no session id, so the conversation is found by the
/// tool call they belong to. A single open conversation is used as a fallback.
fn session_thread_for_tool_call(
    ctx: &ClientContext,
    tool_call_id: &str,
    cx: &mut AsyncApp,
) -> Option<Entity<AcpThread>> {
    cx.update(|cx| {
        let sessions = ctx.sessions.borrow();
        let mut only = None;
        for session in sessions.values() {
            let Some(thread) = session.thread.upgrade() else {
                continue;
            };
            let owns_call = thread.read(cx).entries().iter().any(|entry| match entry {
                AgentThreadEntry::ToolCall(call) => call.id.0.as_ref() == tool_call_id,
                _ => false,
            });
            if owns_call {
                return Some(thread);
            }
            if only.is_none() {
                only = Some(thread);
            }
        }
        if sessions.len() == 1 { only } else { None }
    })
}

fn question_mappings(request: &CursorAskQuestionRequest) -> Vec<QuestionMapping> {
    request
        .questions
        .iter()
        .map(|question| QuestionMapping {
            id: question.id.clone(),
            options: question
                .options
                .iter()
                .map(|option| (option.label.clone(), option.id.clone()))
                .collect(),
            multiple: question.allow_multiple,
        })
        .collect()
}

fn question_schema(request: &CursorAskQuestionRequest) -> acp_v2::ElicitationSchema {
    let mut schema = acp_v2::ElicitationSchema::new();
    for question in &request.questions {
        if question.id.is_empty() {
            continue;
        }
        if question.options.is_empty() {
            schema = schema.property(
                question.id.clone(),
                acp_v2::StringPropertySchema::new().title(question.prompt.clone()),
                true,
            );
        } else if question.allow_multiple {
            schema = schema.property(
                question.id.clone(),
                acp_v2::MultiSelectPropertySchema::titled(
                    question
                        .options
                        .iter()
                        .map(|option| acp_v2::EnumOption::new(option.id.clone(), option.label.clone()))
                        .collect(),
                ),
                true,
            );
        } else {
            schema = schema.property(
                question.id.clone(),
                acp_v2::StringPropertySchema::new()
                    .title(question.prompt.clone())
                    .enum_values(
                        question
                            .options
                            .iter()
                            .map(|option| option.label.clone())
                            .collect(),
                    ),
                true,
            );
        }
    }
    schema
}

fn cursor_question_response(
    response: acp_v2::CreateElicitationResponse,
    mappings: &[QuestionMapping],
) -> CursorAskQuestionResponse {
    let outcome = match response.action {
        acp_v2::ElicitationAction::Accept(action) => {
            let content = action.content.unwrap_or_default();
            let answers = mappings
                .iter()
                .filter(|mapping| !mapping.options.is_empty())
                .filter_map(|mapping| {
                    let selected = selected_option_ids(content.get(&mapping.id)?, mapping)?;
                    Some(CursorAnswer {
                        question_id: mapping.id.clone(),
                        selected_option_ids: selected,
                    })
                })
                .collect();
            CursorQuestionOutcome::Answered { answers }
        }
        acp_v2::ElicitationAction::Decline => CursorQuestionOutcome::Skipped {
            reason: Some("The user dismissed the question.".to_string()),
        },
        _ => CursorQuestionOutcome::Cancelled,
    };
    CursorAskQuestionResponse { outcome }
}

fn selected_option_ids(
    value: &acp_v2::ElicitationContentValue,
    mapping: &QuestionMapping,
) -> Option<Vec<String>> {
    let labels: Vec<&str> = match value {
        acp_v2::ElicitationContentValue::String(label) => vec![label.as_str()],
        acp_v2::ElicitationContentValue::StringArray(labels) => {
            labels.iter().map(String::as_str).collect()
        }
        _ => return None,
    };
    let ids: Vec<String> = labels
        .into_iter()
        .filter_map(|label| {
            mapping
                .options
                .iter()
                .find(|(option_label, _)| option_label == label)
                .map(|(_, id)| id.clone())
        })
        .collect();
    if mapping.multiple || ids.len() == 1 {
        Some(ids)
    } else {
        None
    }
}

fn cursor_plan_response(response: acp_v2::CreateElicitationResponse) -> CursorCreatePlanResponse {
    let outcome = match response.action {
        acp_v2::ElicitationAction::Accept(action) => {
            let accepted = action
                .content
                .as_ref()
                .and_then(|content| content.get(PLAN_DECISION_FIELD))
                .and_then(|value| match value {
                    acp_v2::ElicitationContentValue::String(decision) => Some(decision.as_str()),
                    _ => None,
                })
                .map(|decision| decision == ACCEPT_PLAN)
                .unwrap_or(true);
            if accepted {
                CursorPlanOutcome::Accepted { plan_uri: None }
            } else {
                CursorPlanOutcome::Rejected { reason: None }
            }
        }
        acp_v2::ElicitationAction::Decline => CursorPlanOutcome::Rejected {
            reason: Some("The user dismissed the plan.".to_string()),
        },
        _ => CursorPlanOutcome::Cancelled,
    };
    CursorCreatePlanResponse { outcome }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question_request() -> CursorAskQuestionRequest {
        serde_json::from_value(serde_json::json!({
            "toolCallId": "call_1",
            "title": "Need input",
            "questions": [
                {
                    "id": "q1",
                    "prompt": "Which mode?",
                    "options": [
                        { "id": "agent", "label": "Agent" },
                        { "id": "plan", "label": "Plan" }
                    ],
                    "allowMultiple": false
                },
                {
                    "id": "q2",
                    "prompt": "Which files?",
                    "options": [
                        { "id": "src", "label": "Sources" },
                        { "id": "tests", "label": "Tests" }
                    ],
                    "allowMultiple": true
                }
            ]
        }))
        .expect("a Cursor question request")
    }

    fn accept(content: &[(&str, acp_v2::ElicitationContentValue)]) -> acp_v2::CreateElicitationResponse {
        acp_v2::CreateElicitationResponse::new(acp_v2::ElicitationAction::Accept(
            acp_v2::ElicitationAcceptAction::new().content(
                content
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect::<BTreeMap<_, _>>(),
            ),
        ))
    }

    #[test]
    fn cursor_questions_become_a_form_and_return_option_ids() {
        let request = question_request();
        let mappings = question_mappings(&request);
        let schema = question_schema(&request);
        assert_eq!(schema.properties.len(), 2);

        let response = cursor_question_response(
            accept(&[
                (
                    "q1",
                    acp_v2::ElicitationContentValue::String("Plan".into()),
                ),
                (
                    "q2",
                    acp_v2::ElicitationContentValue::StringArray(vec![
                        "Sources".into(),
                        "Tests".into(),
                    ]),
                ),
            ]),
            &mappings,
        );
        assert_eq!(
            serde_json::to_value(response).expect("a response"),
            serde_json::json!({
                "outcome": {
                    "outcome": "answered",
                    "answers": [
                        { "questionId": "q1", "selectedOptionIds": ["plan"] },
                        { "questionId": "q2", "selectedOptionIds": ["src", "tests"] }
                    ]
                }
            })
        );
    }

    #[test]
    fn dismissed_questions_and_plans_are_reported() {
        let mappings = question_mappings(&question_request());
        let dismissed = acp_v2::CreateElicitationResponse::new(acp_v2::ElicitationAction::Decline);
        assert_eq!(
            serde_json::to_value(cursor_question_response(dismissed.clone(), &mappings))
                .expect("a response"),
            serde_json::json!({
                "outcome": { "outcome": "skipped", "reason": "The user dismissed the question." }
            })
        );
        assert_eq!(
            serde_json::to_value(cursor_plan_response(dismissed)).expect("a response"),
            serde_json::json!({
                "outcome": { "outcome": "rejected", "reason": "The user dismissed the plan." }
            })
        );
    }

    #[test]
    fn plan_decisions_map_to_accept_and_reject() {
        let accepted = cursor_plan_response(accept(&[(
            PLAN_DECISION_FIELD,
            acp_v2::ElicitationContentValue::String(ACCEPT_PLAN.into()),
        )]));
        assert_eq!(
            serde_json::to_value(accepted).expect("a response"),
            serde_json::json!({ "outcome": { "outcome": "accepted" } })
        );
        let rejected = cursor_plan_response(accept(&[(
            PLAN_DECISION_FIELD,
            acp_v2::ElicitationContentValue::String(REJECT_PLAN.into()),
        )]));
        assert_eq!(
            serde_json::to_value(rejected).expect("a response"),
            serde_json::json!({ "outcome": { "outcome": "rejected" } })
        );
    }
}
