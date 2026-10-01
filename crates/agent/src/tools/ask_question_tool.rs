use acp_thread::AcpThread;
use agent_client_protocol::schema::v1 as acp;
use futures::FutureExt as _;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};

use crate::{AgentTool, ToolCallEventStream, ToolCapability, ToolInput};

const ANSWER_FIELD: &str = "answer";
const FREEFORM_ANSWER_FIELD: &str = "freeform_answer";

/// Ask the user one question whose answer is needed before continuing.
///
/// Use this when a genuine user preference or missing requirement would change
/// the result. Do not ask for facts you can discover from the project. When a
/// choice affects a plan or an Architect step, call this tool by itself and wait
/// for the answer before calling `draft_plan` or locking with `refine_step`.
///
/// Leave `options` empty for a free-text answer. Provide options for a
/// single-select question, or set `allow_multiple` to let the user select more
/// than one. A freeform answer is always available, including with options.
/// Keep the options concise and use their descriptions to explain meaningful
/// tradeoffs. Always supply a recommendation when it is safe to proceed
/// automatically after 10 seconds with all Praxis windows inactive. Otherwise
/// omit it and wait for a manual answer. Do not use this tool to request secrets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct AskQuestionToolInput {
    /// The question shown to the user.
    pub question: String,
    /// Choices shown as selectable rows. Leave empty to request free text.
    #[serde(default)]
    pub options: Vec<AskQuestionOption>,
    /// Whether more than one option may be selected. This requires options.
    #[serde(default)]
    pub allow_multiple: bool,
    /// Always supply a recommendation when it is safe to proceed without user input.
    /// Use a string for free text/single select, or a nonempty array of unique option
    /// values for multi select. After 10 seconds with all Praxis windows inactive
    /// and no edits or selections, this answer is used automatically. Active time
    /// never counts toward the timeout. Omit only when no safe recommendation
    /// exists; then wait for a manual answer. Never use this to authorize tools
    /// or exit plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<AskQuestionAnswer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct AskQuestionOption {
    /// Stable, non-empty value returned to you when this option is selected.
    pub value: String,
    /// Human-readable label shown to the user.
    pub label: String,
    /// Optional explanation of the option or its tradeoffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AskQuestionAnswer {
    Text(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AskQuestionToolOutput {
    Answered { answer: AskQuestionAnswer },
    TimedOut { answer: AskQuestionAnswer },
    Declined,
    Canceled,
    Error { error: String },
}

impl From<AskQuestionToolOutput> for LanguageModelToolResultContent {
    fn from(output: AskQuestionToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize ask_question output: {error}"))
            .into()
    }
}

pub struct AskQuestionTool {
    acp_thread: WeakEntity<AcpThread>,
}

impl AskQuestionTool {
    pub fn new(acp_thread: WeakEntity<AcpThread>) -> Self {
        Self { acp_thread }
    }

    fn validate_input(mut input: AskQuestionToolInput) -> Result<AskQuestionToolInput, String> {
        input.question = input.question.trim().to_string();
        if input.question.is_empty() {
            return Err("The question cannot be empty.".into());
        }
        if input.options.is_empty() && input.allow_multiple {
            return Err("allow_multiple requires at least one option.".into());
        }

        let mut values = HashSet::new();
        for option in &mut input.options {
            option.value = option.value.trim().to_string();
            option.label = option.label.trim().to_string();
            option.description = option
                .description
                .take()
                .map(|description| description.trim().to_string())
                .filter(|description| !description.is_empty());

            if option.value.is_empty() {
                return Err("Option values cannot be empty.".into());
            }
            if option.label.is_empty() {
                return Err(format!("The option `{}` needs a label.", option.value));
            }
            if !values.insert(option.value.clone()) {
                return Err(format!(
                    "Option values must be unique; `{}` was provided more than once.",
                    option.value
                ));
            }
        }

        if let Some(recommendation) = &input.recommendation
            && let AskQuestionToolOutput::Error { error } =
                Self::response_output(&input, Self::answer_response(recommendation))
        {
            return Err(format!("Invalid recommendation: {error}"));
        }
        Ok(input)
    }

    fn answer_response(answer: &AskQuestionAnswer) -> acp::CreateElicitationResponse {
        let value = match answer {
            AskQuestionAnswer::Text(value) => acp::ElicitationContentValue::String(value.clone()),
            AskQuestionAnswer::Multiple(values) => {
                acp::ElicitationContentValue::StringArray(values.clone())
            }
        };
        acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
            acp::ElicitationAcceptAction::new().content(std::collections::BTreeMap::from([(
                ANSWER_FIELD.to_string(),
                value,
            )])),
        ))
    }

    fn recommendation_label(input: &AskQuestionToolInput, answer: &AskQuestionAnswer) -> String {
        match answer {
            AskQuestionAnswer::Text(value) => Self::display_value(input, value).to_string(),
            AskQuestionAnswer::Multiple(values) => values
                .iter()
                .map(|value| Self::display_value(input, value))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    fn resolved_output(
        input: &AskQuestionToolInput,
        response: acp::CreateElicitationResponse,
        timed_out: bool,
    ) -> AskQuestionToolOutput {
        match Self::response_output(input, response) {
            AskQuestionToolOutput::Answered { answer } if timed_out => {
                AskQuestionToolOutput::TimedOut { answer }
            }
            output => output,
        }
    }

    fn elicitation_options(input: &AskQuestionToolInput) -> Vec<acp::EnumOption> {
        input
            .options
            .iter()
            .map(|option| {
                acp::EnumOption::new(option.value.clone(), option.label.clone())
                    .description(option.description.clone())
            })
            .collect()
    }

    pub fn elicitation_schema(input: &AskQuestionToolInput) -> acp::ElicitationSchema {
        if input.options.is_empty() {
            acp::ElicitationSchema::new().property(
                ANSWER_FIELD,
                acp::StringPropertySchema::new().title("Your answer"),
                true,
            )
        } else {
            // MCP/ACP forms only support flat properties, not an object-level
            // oneOf. Both fields are optional on the wire; the service requires
            // an answer and gives explicit freeform text precedence over choices.
            let schema = if input.allow_multiple {
                acp::ElicitationSchema::new().property(
                    ANSWER_FIELD,
                    acp::MultiSelectPropertySchema::titled(Self::elicitation_options(input))
                        .title("Choose one or more"),
                    false,
                )
            } else {
                acp::ElicitationSchema::new().property(
                    ANSWER_FIELD,
                    acp::StringPropertySchema::new()
                        .title("Choose one")
                        .one_of(Self::elicitation_options(input)),
                    false,
                )
            };
            schema.property(
                FREEFORM_ANSWER_FIELD,
                acp::StringPropertySchema::new()
                    .title("Or write your own answer")
                    .description(
                        "If provided, this answer is used instead of the selected options.",
                    ),
                false,
            )
        }
    }

    fn response_output(
        input: &AskQuestionToolInput,
        response: acp::CreateElicitationResponse,
    ) -> AskQuestionToolOutput {
        match response.action {
            acp::ElicitationAction::Accept(action) => {
                let Some(mut content) = action.content else {
                    return AskQuestionToolOutput::Error {
                        error: "The submitted response did not contain an answer.".into(),
                    };
                };
                if !input.options.is_empty()
                    && let Some(freeform) = content.remove(FREEFORM_ANSWER_FIELD)
                {
                    match freeform {
                        acp::ElicitationContentValue::String(answer) => {
                            if !answer.trim().is_empty() {
                                return AskQuestionToolOutput::Answered {
                                    answer: AskQuestionAnswer::Text(answer),
                                };
                            }
                        }
                        _ => {
                            return AskQuestionToolOutput::Error {
                                error: "The freeform answer must be text.".into(),
                            };
                        }
                    }
                }
                let Some(answer) = content.remove(ANSWER_FIELD) else {
                    return AskQuestionToolOutput::Error {
                        error: "The submitted response did not contain an answer.".into(),
                    };
                };

                match (input.allow_multiple, answer) {
                    (false, acp::ElicitationContentValue::String(answer)) => {
                        if answer.trim().is_empty() {
                            AskQuestionToolOutput::Error {
                                error: "The submitted answer was empty.".into(),
                            }
                        } else if !input.options.is_empty()
                            && !input.options.iter().any(|option| option.value == answer)
                        {
                            AskQuestionToolOutput::Error {
                                error: "The submitted answer was not one of the provided options."
                                    .into(),
                            }
                        } else {
                            AskQuestionToolOutput::Answered {
                                answer: AskQuestionAnswer::Text(answer),
                            }
                        }
                    }
                    (true, acp::ElicitationContentValue::StringArray(answers)) => {
                        let mut seen = HashSet::new();
                        if answers.is_empty() {
                            return AskQuestionToolOutput::Error {
                                error: "The submitted answer did not select any options.".into(),
                            };
                        }
                        if answers.iter().any(|answer| {
                            !seen.insert(answer.as_str())
                                || !input.options.iter().any(|option| option.value == *answer)
                        }) {
                            AskQuestionToolOutput::Error {
                                error: "The submitted answer contained an invalid option.".into(),
                            }
                        } else {
                            AskQuestionToolOutput::Answered {
                                answer: AskQuestionAnswer::Multiple(answers),
                            }
                        }
                    }
                    _ => AskQuestionToolOutput::Error {
                        error: "The submitted answer had an unexpected type.".into(),
                    },
                }
            }
            acp::ElicitationAction::Decline => AskQuestionToolOutput::Declined,
            acp::ElicitationAction::Cancel => AskQuestionToolOutput::Canceled,
            _ => AskQuestionToolOutput::Error {
                error: "The user returned an unsupported response.".into(),
            },
        }
    }

    fn display_value<'a>(input: &'a AskQuestionToolInput, value: &'a str) -> &'a str {
        input
            .options
            .iter()
            .find(|option| option.value == value)
            .map_or(value, |option| option.label.as_str())
    }

    fn update_card(
        input: &AskQuestionToolInput,
        output: &AskQuestionToolOutput,
        event_stream: &ToolCallEventStream,
    ) {
        let (title, result) = match output {
            AskQuestionToolOutput::Answered {
                answer: AskQuestionAnswer::Text(answer),
            } => (
                "User answered the question",
                format!(
                    "**Question:** {}\n\n**Answer:** {}",
                    input.question,
                    Self::display_value(input, answer)
                ),
            ),
            AskQuestionToolOutput::Answered {
                answer: AskQuestionAnswer::Multiple(answers),
            } => {
                let answers = answers
                    .iter()
                    .map(|answer| format!("- {}", Self::display_value(input, answer)))
                    .collect::<Vec<_>>()
                    .join("\n");
                (
                    "User answered the question",
                    format!("**Question:** {}\n\n**Answer:**\n{answers}", input.question),
                )
            }
            AskQuestionToolOutput::TimedOut { answer } => (
                "Question timed out; used model recommendation",
                format!(
                    "**Question:** {}\n\nPraxis was inactive for 10 seconds with no edits or selections. Used the model recommendation (not a user answer): {}",
                    input.question,
                    Self::recommendation_label(input, answer)
                ),
            ),
            AskQuestionToolOutput::Declined => (
                "User declined the question",
                format!("The user declined to answer: {}", input.question),
            ),
            AskQuestionToolOutput::Canceled => (
                "Question canceled",
                format!("The question was canceled: {}", input.question),
            ),
            AskQuestionToolOutput::Error { error } => (
                "Could not ask the question",
                format!("Could not ask `{}`: {error}", input.question),
            ),
        };

        event_stream.update_fields(
            acp::ToolCallUpdateFields::new()
                .title(title)
                .content(vec![result.into()]),
        );
    }
}

impl AgentTool for AskQuestionTool {
    type Input = AskQuestionToolInput;
    type Output = AskQuestionToolOutput;

    const NAME: &'static str = "ask_question";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Ask the user a question".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| AskQuestionToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;
            let input = Self::validate_input(input)
                .map_err(|error| AskQuestionToolOutput::Error { error })?;
            let schema = Self::elicitation_schema(&input);
            let tool_call_id = event_stream.tool_call_id().clone();
            let acp_thread = self.acp_thread.clone();

            let request = acp_thread.update(cx, |thread, cx| {
                let scope = acp::ElicitationSessionScope::new(thread.session_id().clone())
                    .tool_call_id(tool_call_id);
                let request = acp::CreateElicitationRequest::new(
                    acp::ElicitationFormMode::new(scope, schema),
                    input.question.clone(),
                );
                if let Some(recommendation) = &input.recommendation {
                    let cancellation_stream = event_stream.clone();
                    thread.request_question_with_timeout(
                        request,
                        Self::answer_response(recommendation),
                        Self::recommendation_label(&input, recommendation),
                        move || cancellation_stream.was_cancelled_by_user(),
                        cx,
                    )
                } else {
                    thread
                        .request_elicitation_with_id(request, cx)
                        .map(|(id, response)| {
                            (id, cx.spawn(async move |_, _| (response.await, false)))
                        })
                }
            });
            let (elicitation_id, response_task) = match request {
                Ok(Ok(request)) => request,
                Ok(Err(error)) => {
                    let output = AskQuestionToolOutput::Error {
                        error: format!("Could not request user input: {error}"),
                    };
                    Self::update_card(&input, &output, &event_stream);
                    return Err(output);
                }
                Err(error) => {
                    let output = AskQuestionToolOutput::Error {
                        error: format!("The conversation is no longer available: {error}"),
                    };
                    Self::update_card(&input, &output, &event_stream);
                    return Err(output);
                }
            };

            let cancellation_stream = event_stream.clone();
            let (response, timed_out) = futures::select_biased! {
                _ = cancellation_stream.cancelled_by_user().fuse() => {
                    if let Err(error) = acp_thread.update(cx, |thread, cx| {
                        thread.mark_question_interaction(&elicitation_id, cx);
                        thread.cancel_elicitation(&elicitation_id, cx);
                    }) {
                        log::debug!("Question thread disappeared during cancellation: {error}");
                    }
                    let output = AskQuestionToolOutput::Canceled;
                    Self::update_card(&input, &output, &event_stream);
                    return Err(output);
                },
                response = response_task.fuse() => response,
            };

            let output = Self::resolved_output(&input, response, timed_out);
            Self::update_card(&input, &output, &event_stream);
            if matches!(&output, AskQuestionToolOutput::Error { .. }) {
                Err(output)
            } else {
                Ok(output)
            }
        })
    }

    fn replay(
        &self,
        input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> anyhow::Result<()> {
        Self::update_card(&input, &output, &event_stream);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn option(value: &str, label: &str) -> AskQuestionOption {
        AskQuestionOption {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    fn input(options: Vec<AskQuestionOption>, allow_multiple: bool) -> AskQuestionToolInput {
        AskQuestionToolInput {
            question: "Which database should the service use?".into(),
            options,
            allow_multiple,
            recommendation: None,
        }
    }

    fn assert_answer_is_required(schema: &acp::ElicitationSchema) {
        assert!(
            schema
                .required
                .as_deref()
                .is_some_and(|required| required == [ANSWER_FIELD]),
            "the answer field must be required: {:?}",
            schema.required
        );
    }

    fn assert_options_allow_freeform(schema: &acp::ElicitationSchema) {
        assert!(schema.required.as_deref().unwrap_or_default().is_empty());
        let Some(acp::ElicitationPropertySchema::String(freeform)) =
            schema.properties.get(FREEFORM_ANSWER_FIELD)
        else {
            panic!("questions with options must also expose freeform text")
        };
        assert!(freeform.enum_values.is_none());
        assert!(freeform.one_of.is_none());
    }

    #[gpui::test]
    fn question_always_offers_freeform(_cx: &mut gpui::TestAppContext) {
        for input in [
            input(Vec::new(), false),
            input(vec![option("postgres", "PostgreSQL")], false),
            input(vec![option("postgres", "PostgreSQL")], true),
        ] {
            let schema = AskQuestionTool::elicitation_schema(&input);
            let schema = serde_json::from_value::<acp::ElicitationSchema>(
                serde_json::to_value(schema).unwrap(),
            )
            .unwrap();
            if input.options.is_empty() {
                assert_answer_is_required(&schema);
            } else {
                assert_options_allow_freeform(&schema);
            }
            let field = if input.options.is_empty() {
                ANSWER_FIELD
            } else {
                FREEFORM_ANSWER_FIELD
            };
            let response = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                acp::ElicitationAcceptAction::new().content(BTreeMap::from([(
                    field.into(),
                    acp::ElicitationContentValue::from("Use an embedded database instead"),
                )])),
            ));
            assert_eq!(
                AskQuestionTool::response_output(&input, response),
                AskQuestionToolOutput::Answered {
                    answer: AskQuestionAnswer::Text("Use an embedded database instead".into()),
                },
            );
        }
    }

    #[test]
    fn freeform_answers_override_selections_without_weakening_option_validation() {
        for allow_multiple in [false, true] {
            let input = input(vec![option("postgres", "PostgreSQL")], allow_multiple);
            let selected = if allow_multiple {
                acp::ElicitationContentValue::StringArray(vec!["postgres".into()])
            } else {
                acp::ElicitationContentValue::from("postgres")
            };
            for freeform in ["An alternative", "  "] {
                let response = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                    acp::ElicitationAcceptAction::new().content(BTreeMap::from([
                        (ANSWER_FIELD.into(), selected.clone()),
                        (FREEFORM_ANSWER_FIELD.into(), freeform.into()),
                    ])),
                ));
                let expected = if freeform.trim().is_empty() {
                    if allow_multiple {
                        AskQuestionAnswer::Multiple(vec!["postgres".into()])
                    } else {
                        AskQuestionAnswer::Text("postgres".into())
                    }
                } else {
                    AskQuestionAnswer::Text(freeform.into())
                };
                assert_eq!(
                    AskQuestionTool::response_output(&input, response),
                    AskQuestionToolOutput::Answered { answer: expected }
                );
            }
            for content in [
                BTreeMap::new(),
                BTreeMap::from([(FREEFORM_ANSWER_FIELD.into(), "  ".into())]),
                BTreeMap::from([(
                    FREEFORM_ANSWER_FIELD.into(),
                    acp::ElicitationContentValue::StringArray(vec!["text".into()]),
                )]),
                BTreeMap::from([(ANSWER_FIELD.into(), "unknown".into())]),
            ] {
                let response = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                    acp::ElicitationAcceptAction::new().content(content),
                ));
                assert!(matches!(
                    AskQuestionTool::response_output(&input, response),
                    AskQuestionToolOutput::Error { .. }
                ));
            }
        }
    }

    #[test]
    fn recommendations_use_response_validation() {
        let text = input(Vec::new(), false);
        let single = input(
            vec![option("postgres", "PostgreSQL"), option("sqlite", "SQLite")],
            false,
        );
        let multiple = input(single.options.clone(), true);
        for (base, answers) in [
            (
                text,
                vec![
                    (AskQuestionAnswer::Text("Use PostgreSQL".into()), true),
                    (AskQuestionAnswer::Text("".into()), false),
                    (AskQuestionAnswer::Text("  \n".into()), false),
                    (AskQuestionAnswer::Multiple(vec!["postgres".into()]), false),
                ],
            ),
            (
                single,
                vec![
                    (AskQuestionAnswer::Text("postgres".into()), true),
                    (AskQuestionAnswer::Text("unknown".into()), false),
                    (AskQuestionAnswer::Text("PostgreSQL".into()), false),
                    (AskQuestionAnswer::Multiple(vec!["postgres".into()]), false),
                ],
            ),
            (
                multiple,
                vec![
                    (
                        AskQuestionAnswer::Multiple(vec!["postgres".into(), "sqlite".into()]),
                        true,
                    ),
                    (AskQuestionAnswer::Multiple(vec![]), false),
                    (
                        AskQuestionAnswer::Multiple(vec!["postgres".into(), "postgres".into()]),
                        false,
                    ),
                    (AskQuestionAnswer::Multiple(vec!["unknown".into()]), false),
                    (AskQuestionAnswer::Multiple(vec!["".into()]), false),
                    (AskQuestionAnswer::Text("postgres".into()), false),
                ],
            ),
        ] {
            for (answer, valid) in answers {
                let mut input = base.clone();
                input.recommendation = Some(answer.clone());
                assert_eq!(
                    AskQuestionTool::validate_input(input.clone()).is_ok(),
                    valid,
                    "{answer:?}"
                );
                assert_eq!(
                    matches!(
                        AskQuestionTool::response_output(
                            &input,
                            AskQuestionTool::answer_response(&answer)
                        ),
                        AskQuestionToolOutput::Answered { .. }
                    ),
                    valid
                );
            }
        }
    }

    #[test]
    fn legacy_inputs_have_no_recommendation() {
        for json in [
            serde_json::json!({"question": "Which database?"}),
            serde_json::json!({"question": "Which database?", "recommendation": null}),
        ] {
            let input =
                AskQuestionTool::validate_input(serde_json::from_value(json).unwrap()).unwrap();
            assert!(input.recommendation.is_none());
        }
    }

    #[test]
    fn timeout_output_is_not_a_user_answer() {
        for (input, answer) in [
            (
                input(Vec::new(), false),
                AskQuestionAnswer::Text("PostgreSQL".into()),
            ),
            (
                input(vec![option("postgres", "PostgreSQL")], false),
                AskQuestionAnswer::Text("postgres".into()),
            ),
            (
                input(vec![option("postgres", "PostgreSQL")], true),
                AskQuestionAnswer::Multiple(vec!["postgres".into()]),
            ),
        ] {
            for timed_out in [false, true] {
                let output = AskQuestionTool::resolved_output(
                    &input,
                    AskQuestionTool::answer_response(&answer),
                    timed_out,
                );
                let expected = if timed_out {
                    AskQuestionToolOutput::TimedOut {
                        answer: answer.clone(),
                    }
                } else {
                    AskQuestionToolOutput::Answered {
                        answer: answer.clone(),
                    }
                };
                assert_eq!(output, expected);
                let json = serde_json::to_value(&output).unwrap();
                assert_eq!(
                    json["status"],
                    if timed_out { "timed_out" } else { "answered" }
                );
                assert_eq!(
                    serde_json::from_value::<AskQuestionToolOutput>(json).unwrap(),
                    expected
                );
            }
            assert_eq!(
                AskQuestionTool::resolved_output(
                    &input,
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
                    true
                ),
                AskQuestionToolOutput::Canceled
            );
            assert_eq!(
                AskQuestionTool::resolved_output(
                    &input,
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
                    true
                ),
                AskQuestionToolOutput::Declined
            );
        }
    }

    #[gpui::test]
    async fn timeout_card_identifies_model_recommendation(cx: &mut gpui::TestAppContext) {
        let (stream, mut events) = ToolCallEventStream::test();
        cx.update(|cx| {
            AskQuestionTool::new(WeakEntity::new_invalid())
                .replay(
                    input(vec![option("postgres", "PostgreSQL")], false),
                    AskQuestionToolOutput::TimedOut {
                        answer: AskQuestionAnswer::Text("postgres".into()),
                    },
                    stream,
                    cx,
                )
                .unwrap();
        });
        let fields = events.expect_update_fields().await;
        assert_eq!(
            fields.title.as_deref(),
            Some("Question timed out; used model recommendation")
        );
        assert!(fields.content.unwrap().iter().any(|block| matches!(
            block,
            acp::ToolCallContent::Content(content) if matches!(&content.content,
                acp::ContentBlock::Text(text) if text.text.contains("not a user answer") && text.text.contains("PostgreSQL")
            )
        )));
    }

    #[test]
    fn empty_options_create_a_free_text_field() {
        let input = AskQuestionTool::validate_input(input(Vec::new(), false)).unwrap();
        let schema = AskQuestionTool::elicitation_schema(&input);
        assert_answer_is_required(&schema);

        let Some(acp::ElicitationPropertySchema::String(answer)) =
            schema.properties.get(ANSWER_FIELD)
        else {
            panic!("free text must use a string property")
        };
        assert!(answer.enum_values.is_none());
        assert!(answer.one_of.is_none());
    }

    #[test]
    fn options_create_a_titled_single_select() {
        let mut production = option("production", "Production");
        production.description = Some("Use live data".into());
        let input = AskQuestionTool::validate_input(input(
            vec![production, option("staging", "Staging")],
            false,
        ))
        .unwrap();
        let schema = AskQuestionTool::elicitation_schema(&input);
        assert_options_allow_freeform(&schema);

        let Some(acp::ElicitationPropertySchema::String(answer)) =
            schema.properties.get(ANSWER_FIELD)
        else {
            panic!("single select must use a string property")
        };
        let options = answer.one_of.as_deref().expect("single select needs oneOf");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].value, "production");
        assert_eq!(options[0].title, "Production");
        assert_eq!(options[0].description.as_deref(), Some("Use live data"));
    }

    #[test]
    fn multiple_options_create_a_multi_select_with_freeform() {
        let input = AskQuestionTool::validate_input(input(
            vec![option("api", "API"), option("worker", "Worker")],
            true,
        ))
        .unwrap();
        let schema = AskQuestionTool::elicitation_schema(&input);
        assert_options_allow_freeform(&schema);

        let Some(acp::ElicitationPropertySchema::Array(answer)) =
            schema.properties.get(ANSWER_FIELD)
        else {
            panic!("multi-select must use an array property")
        };
        assert_eq!(answer.min_items, None);
        let acp::MultiSelectItems::Titled(items) = &answer.items else {
            panic!("multi-select options must preserve their labels")
        };
        assert_eq!(items.options.len(), 2);
        assert_eq!(items.options[1].value, "worker");
        assert_eq!(items.options[1].title, "Worker");
    }

    #[test]
    fn invalid_option_values_are_rejected() {
        let empty = AskQuestionTool::validate_input(input(vec![option("  ", "Empty")], false))
            .expect_err("empty values must be rejected");
        assert!(empty.contains("cannot be empty"));

        let duplicate = AskQuestionTool::validate_input(input(
            vec![option("same", "First"), option(" same ", "Second")],
            false,
        ))
        .expect_err("duplicate values must be rejected after normalization");
        assert!(duplicate.contains("must be unique"));
    }

    #[test]
    fn multiple_free_text_answers_are_rejected() {
        let error = AskQuestionTool::validate_input(input(Vec::new(), true))
            .expect_err("multi-select requires selectable options");
        assert!(error.contains("requires at least one option"));
    }

    #[test]
    fn responses_distinguish_answer_decline_and_cancel() {
        let text_input = AskQuestionTool::validate_input(input(Vec::new(), false)).unwrap();
        let accepted = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
            acp::ElicitationAcceptAction::new().content(BTreeMap::from([(
                ANSWER_FIELD.to_string(),
                acp::ElicitationContentValue::from("PostgreSQL"),
            )])),
        ));
        assert_eq!(
            AskQuestionTool::response_output(&text_input, accepted),
            AskQuestionToolOutput::Answered {
                answer: AskQuestionAnswer::Text("PostgreSQL".into())
            }
        );

        assert_eq!(
            AskQuestionTool::response_output(
                &text_input,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
            ),
            AskQuestionToolOutput::Declined
        );
        assert_eq!(
            AskQuestionTool::response_output(
                &text_input,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
            ),
            AskQuestionToolOutput::Canceled
        );
    }
}
