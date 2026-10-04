use acp_thread::{AcpThread, AgentThreadEntry, Elicitation, ElicitationEntryId, ElicitationStatus};
use agent::{AskQuestionOption, AskQuestionTool, AskQuestionToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Entity};
use serde_json::{Value, json};

fn question_input(request: &acp::CreateElicitationRequest) -> Result<AskQuestionToolInput> {
    let acp::ElicitationMode::Form(form) = &request.mode else {
        bail!("This request needs the desktop");
    };
    let schema = serde_json::to_value(&form.requested_schema)?;
    let properties = schema["properties"]
        .as_object()
        .context("Missing question fields")?;
    if properties
        .keys()
        .any(|key| key != "answer" && key != "freeform_answer")
    {
        bail!("This form needs the desktop");
    }
    let answer = properties
        .get("answer")
        .context("This form is not a question")?;
    let multiple = answer["type"] == "array";
    if !multiple && answer["type"] != "string" {
        bail!("This question type needs the desktop");
    }
    let choices = if multiple { &answer["items"] } else { answer };
    let choices = choices
        .get("oneOf")
        .or_else(|| choices.get("anyOf"))
        .or_else(|| choices.get("enum"));
    let options = choices
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|option| {
            let value = option
                .as_str()
                .or_else(|| option["const"].as_str())
                .context("Unsupported question choice")?;
            Ok(AskQuestionOption {
                value: value.to_string(),
                label: option
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    AskQuestionTool::validate_input(AskQuestionToolInput {
        question: request.message.clone(),
        options,
        allow_multiple: multiple,
        recommendation: None,
    })
    .map_err(|error| anyhow!(error))
}

fn pending<'a>(thread: &'a AcpThread, id: &ElicitationEntryId) -> Result<&'a Elicitation> {
    let (_, question) = thread
        .elicitation(id)
        .context("That question is no longer available. Refresh.")?;
    let ElicitationStatus::Pending { respond_tx } = &question.status else {
        bail!("That question was already answered or cancelled. Refresh.");
    };
    if respond_tx.is_canceled() {
        bail!("The agent is no longer waiting for that question. Refresh.");
    }
    if !matches!(question.request.scope(), acp::ElicitationScope::Session(scope) if &scope.session_id == thread.session_id())
    {
        bail!("That question belongs to another session");
    }
    Ok(question)
}

pub(super) fn headers(threads: &[Entity<AcpThread>], cx: &App) -> Vec<Value> {
    let mut headers = Vec::new();
    for thread in threads {
        let thread = thread.read(cx);
        for entry in thread.entries() {
            let AgentThreadEntry::Elicitation(id) = entry else {
                continue;
            };
            let Ok(question) = pending(thread, id) else {
                continue;
            };
            if question_input(&question.request).is_err() {
                continue;
            }
            headers.push(json!({
                "id": id.0.as_ref(),
                "session_id": thread.session_id().0.as_ref(),
                "title": super::truncate(&question.request.message, 160),
                "session_title": thread.title().map(|title| super::truncate(&title, 128)),
            }));
        }
    }
    headers
}

fn check_session(thread: &AcpThread, args: &Value) -> Result<()> {
    if super::required(args, "session_id")? != thread.session_id().0.as_ref() {
        bail!("That question belongs to another session");
    }
    Ok(())
}

pub(super) fn content(thread: &Entity<AcpThread>, args: &Value, cx: &mut App) -> Result<String> {
    let offset = super::transcript::index(args, "offset")?.unwrap_or(0);
    if offset > 0
        && (args.get("version").and_then(Value::as_str).is_none()
            || super::transcript::index(args, "total_bytes")?.is_none())
    {
        bail!("Continue with the question version and total_bytes");
    }
    let id = ElicitationEntryId(super::required(args, "question_id")?.to_string().into());
    thread.update(cx, |thread, cx| {
        check_session(thread, args)?;
        question_input(&pending(thread, &id)?.request)?;
        // Explicitly opening the answer form is manual engagement. Claim it
        // before network paging so an inactivity default cannot race typing.
        thread.mark_question_interaction(&id, cx);
        let input = question_input(&pending(thread, &id)?.request)?;
        let mut value = serde_json::to_value(input)?;
        value["auto_answer_paused"] = json!(thread.question_interaction_flag(&id).is_some());
        Ok(serde_json::to_string(&value)?)
    })
}

pub(super) fn answer(thread: &Entity<AcpThread>, args: &Value, cx: &mut App) -> Result<Value> {
    let id = ElicitationEntryId(super::required(args, "question_id")?.to_string().into());
    thread.update(cx, |thread, cx| {
        check_session(thread, args)?;
        let input = question_input(&pending(thread, &id)?.request)?;
        let response = if args.get("decline").and_then(Value::as_bool) == Some(true) {
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline)
        } else {
            let content = args.get("content").context("Provide a question answer")?;
            let fields = content
                .as_object()
                .context("The answer must be an object")?;
            if fields
                .keys()
                .any(|key| key != "answer" && key != "freeform_answer")
            {
                bail!("The response contains unsupported answer fields");
            }
            let content: std::collections::BTreeMap<String, acp::ElicitationContentValue> =
                serde_json::from_value(content.clone()).context("Invalid question answer")?;
            acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                acp::ElicitationAcceptAction::new().content(content),
            ))
        };
        AskQuestionTool::validate_response(&input, &response).map_err(|error| anyhow!(error))?;
        thread.mark_question_interaction(&id, cx);
        thread.respond_to_elicitation(&id, response, cx);
        Ok(json!({ "answered": true }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::AgentConnection as _;
    use std::{path::PathBuf, rc::Rc};

    #[test]
    fn native_choice_schemas_keep_values_descriptions_and_multiple_selection() {
        for multiple in [false, true] {
            let input = AskQuestionToolInput {
                question: "Which database?".into(),
                options: vec![AskQuestionOption {
                    value: "postgres".into(),
                    label: "PostgreSQL".into(),
                    description: Some("Shared service".into()),
                }],
                allow_multiple: multiple,
                recommendation: None,
            };
            let request = acp::CreateElicitationRequest::new(
                acp::ElicitationFormMode::new(
                    acp::ElicitationSessionScope::new(acp::SessionId::new("session")),
                    AskQuestionTool::elicitation_schema(&input),
                ),
                input.question.clone(),
            );
            let parsed = question_input(&request).expect("native question schema");
            assert_eq!(parsed.allow_multiple, multiple);
            assert_eq!(parsed.options[0].value, "postgres");
            assert_eq!(
                parsed.options[0].description.as_deref(),
                Some("Shared service")
            );
        }
    }

    #[gpui::test]
    async fn phone_answer_unblocks_the_original_question_and_rejects_replays(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::conversation_view::tests::init_test(cx);
        let filesystem = project::FakeFs::new(cx.executor());
        let root = PathBuf::from(util::path!("/question-project"));
        filesystem.insert_tree(&root, json!({})).await;
        let project = project::Project::test(filesystem, [root.as_path()], cx).await;
        let connection = Rc::new(acp_thread::StubAgentConnection::new());
        let thread = cx
            .update(|cx| {
                connection.new_session(
                    project,
                    util::path_list::PathList::new(&[root.as_path()]),
                    cx,
                )
            })
            .await
            .expect("session");
        let (id, response) = thread.update(cx, |thread, cx| {
            thread
                .request_elicitation_with_id(
                    acp::CreateElicitationRequest::new(
                        acp::ElicitationFormMode::new(
                            acp::ElicitationSessionScope::new(thread.session_id().clone()),
                            AskQuestionTool::elicitation_schema(&AskQuestionToolInput {
                                question: "Name the branch".into(),
                                options: vec![],
                                allow_multiple: false,
                                recommendation: None,
                            }),
                        ),
                        "Name the branch",
                    ),
                    cx,
                )
                .expect("question")
        });
        let session = thread.read_with(cx, |thread, _| thread.session_id().0.to_string());
        let mut args = json!({ "session_id": session, "question_id": id.0.as_ref(), "content": { "answer": "" } });
        assert!(cx.update(|cx| answer(&thread, &args, cx)).is_err());
        args["content"] = json!({ "answer": "feature/mobile" });
        let mut wrong = args.clone();
        wrong["session_id"] = json!("another-session");
        assert!(cx.update(|cx| answer(&thread, &wrong, cx)).is_err());
        assert_eq!(
            cx.read(|cx| headers(std::slice::from_ref(&thread), cx))
                .len(),
            1
        );
        assert_eq!(
            cx.update(|cx| answer(&thread, &args, cx)).expect("answer")["answered"],
            true
        );
        let acp::ElicitationAction::Accept(answered) = response.await.action else {
            panic!("accepted response");
        };
        assert!(
            matches!(answered.content.as_ref().and_then(|content| content.get("answer")), Some(acp::ElicitationContentValue::String(value)) if value == "feature/mobile")
        );
        assert!(cx.update(|cx| answer(&thread, &args, cx)).is_err());
        assert!(
            cx.read(|cx| headers(std::slice::from_ref(&thread), cx))
                .is_empty()
        );

        let request = thread.read_with(cx, |thread, _| {
            acp::CreateElicitationRequest::new(
                acp::ElicitationFormMode::new(
                    acp::ElicitationSessionScope::new(thread.session_id().clone()),
                    acp::ElicitationSchema::new().string("answer", true),
                ),
                "Continue?",
            )
        });
        let (cancelled_id, cancelled_response) = thread.update(cx, |thread, cx| {
            thread
                .request_elicitation_with_id(request.clone(), cx)
                .expect("cancelled question")
        });
        thread.update(cx, |thread, cx| {
            thread.cancel_elicitation(&cancelled_id, cx)
        });
        args["question_id"] = json!(cancelled_id.0.as_ref());
        assert!(cx.update(|cx| answer(&thread, &args, cx)).is_err());
        assert!(matches!(
            cancelled_response.await.action,
            acp::ElicitationAction::Cancel
        ));

        let recommendation = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
            acp::ElicitationAcceptAction::new().content(std::collections::BTreeMap::from([(
                "answer".into(),
                acp::ElicitationContentValue::String("default".into()),
            )])),
        ));
        let (timed_id, timed_response) = thread.update(cx, |thread, cx| {
            thread
                .request_question_with_timeout(
                    request,
                    recommendation,
                    "default".into(),
                    || false,
                    cx,
                )
                .expect("timed question")
        });
        args["question_id"] = json!(timed_id.0.as_ref());
        let content = cx
            .update(|cx| content(&thread, &args, cx))
            .expect("phone form");
        let form: Value = serde_json::from_str(&content).expect("question form JSON");
        assert_eq!(form["auto_answer_paused"], true);
        assert!(thread.read_with(cx, |thread, _| {
            thread
                .question_interaction_flag(&timed_id)
                .expect("interaction flag")
                .get()
        }));
        cx.update(|cx| answer(&thread, &args, cx))
            .expect("manual answer wins");
        let (_, timed_out) = timed_response.await;
        assert!(!timed_out);
    }
}
