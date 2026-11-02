//! Cursor's ACP extension methods. Praxis answers them so the agent keeps
//! going instead of waiting for a prompt this client cannot show yet.

use agent_client_protocol::Responder;
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use super::ClientContext;

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
}

#[derive(Debug, Clone, Deserialize, Serialize, agent_client_protocol::JsonRpcResponse)]
pub(super) struct CursorAskQuestionResponse {
    outcome: CursorQuestionOutcome,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CursorQuestionOutcome {
    Skipped {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
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
}

/// Cursor asks the client to collect answers. Praxis has no prompt for these
/// yet, so the agent is told they were skipped and continues rather than
/// blocking the turn.
pub(super) fn handle_cursor_ask_question(
    request: CursorAskQuestionRequest,
    responder: Responder<CursorAskQuestionResponse>,
    _cx: &mut AsyncApp,
    _ctx: &ClientContext,
) {
    log::info!(
        "Cursor asked {count} question(s) for tool call {tool_call} ({title}): {first}; Praxis cannot display them yet, so the agent continues without an answer",
        count = request.questions.len(),
        tool_call = request.tool_call_id,
        title = request.title.as_deref().unwrap_or("untitled"),
        first = request
            .questions
            .first()
            .map(|question| question.prompt.as_str())
            .unwrap_or("no question text"),
    );
    responder
        .respond(CursorAskQuestionResponse {
            outcome: CursorQuestionOutcome::Skipped {
                reason: Some("Praxis cannot display Cursor's questions yet.".to_string()),
            },
        })
        .log_err();
}

/// Cursor asks the client to approve a plan. The plan is already part of the
/// conversation, so it is accepted to keep the turn moving.
pub(super) fn handle_cursor_create_plan(
    request: CursorCreatePlanRequest,
    responder: Responder<CursorCreatePlanResponse>,
    _cx: &mut AsyncApp,
    _ctx: &ClientContext,
) {
    log::info!(
        "Cursor proposed a plan for tool call {tool_call} ({name}, {steps} bytes): {overview}",
        tool_call = request.tool_call_id,
        name = request.name.as_deref().unwrap_or("untitled"),
        steps = request.plan.len(),
        overview = request.overview.as_deref().unwrap_or("no overview"),
    );
    responder
        .respond(CursorCreatePlanResponse {
            outcome: CursorPlanOutcome::Accepted { plan_uri: None },
        })
        .log_err();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_questions_deserialize_and_skip() {
        let request: CursorAskQuestionRequest = serde_json::from_value(serde_json::json!({
            "toolCallId": "call_1",
            "title": "Need input",
            "questions": [
                { "id": "q1", "prompt": "Which mode?", "options": [{ "id": "a", "label": "Agent" }], "allowMultiple": false }
            ]
        }))
        .expect("a Cursor question request");
        assert_eq!(request.tool_call_id, "call_1");
        assert_eq!(request.questions.len(), 1);
        assert_eq!(request.questions[0].id, "q1");
        assert_eq!(request.questions[0].prompt, "Which mode?");

        let response = CursorAskQuestionResponse {
            outcome: CursorQuestionOutcome::Skipped {
                reason: Some("not shown".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(response).expect("a response"),
            serde_json::json!({ "outcome": { "outcome": "skipped", "reason": "not shown" } })
        );
    }

    #[test]
    fn cursor_plans_are_accepted_without_a_uri() {
        let request: CursorCreatePlanRequest = serde_json::from_value(serde_json::json!({
            "toolCallId": "call_2",
            "name": "Refactor tabs",
            "plan": "1. Inspect\n2. Change",
            "todos": []
        }))
        .expect("a Cursor plan request");
        assert_eq!(request.name.as_deref(), Some("Refactor tabs"));
        assert_eq!(
            serde_json::to_value(CursorCreatePlanResponse {
                outcome: CursorPlanOutcome::Accepted { plan_uri: None },
            })
            .expect("a response"),
            serde_json::json!({ "outcome": { "outcome": "accepted" } })
        );
    }
}
