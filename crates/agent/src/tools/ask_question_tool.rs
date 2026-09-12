use acp_thread::AcpThread;
use agent_client_protocol::schema::v1 as acp;
use futures::FutureExt as _;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};

use crate::{AgentTool, ToolCallEventStream, ToolInput};

const ANSWER_FIELD: &str = "answer";

/// Ask the user one question whose answer is needed before continuing.
///
/// Use this when a genuine user preference or missing requirement would change
/// the result. Do not ask for facts you can discover from the project. When a
/// choice affects a plan or an Architect step, call this tool by itself and wait
/// for the answer before calling `draft_plan` or locking with `refine_step`.
///
/// Leave `options` empty for a free-text answer. Provide options for a
/// single-select question, or set `allow_multiple` to let the user select more
/// than one. Keep the options concise and use their descriptions to explain
/// meaningful tradeoffs. Do not use this tool to request secrets.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AskQuestionAnswer {
    Text(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AskQuestionToolOutput {
    Answered { answer: AskQuestionAnswer },
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

        Ok(input)
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

    fn elicitation_schema(input: &AskQuestionToolInput) -> acp::ElicitationSchema {
        if input.options.is_empty() {
            acp::ElicitationSchema::new().property(
                ANSWER_FIELD,
                acp::StringPropertySchema::new().title("Your answer"),
                true,
            )
        } else if input.allow_multiple {
            acp::ElicitationSchema::new().property(
                ANSWER_FIELD,
                acp::MultiSelectPropertySchema::titled(Self::elicitation_options(input))
                    .title("Choose one or more")
                    .min_items(1),
                true,
            )
        } else {
            acp::ElicitationSchema::new().property(
                ANSWER_FIELD,
                acp::StringPropertySchema::new()
                    .title("Choose one")
                    .one_of(Self::elicitation_options(input)),
                true,
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
                let Some(answer) = content.remove(ANSWER_FIELD) else {
                    return AskQuestionToolOutput::Error {
                        error: "The submitted response did not contain an answer.".into(),
                    };
                };

                match (input.allow_multiple, answer) {
                    (false, acp::ElicitationContentValue::String(answer)) => {
                        if answer.is_empty() {
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
                thread.request_elicitation_with_id(
                    acp::CreateElicitationRequest::new(
                        acp::ElicitationFormMode::new(scope, schema),
                        input.question.clone(),
                    ),
                    cx,
                )
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
            let response = futures::select! {
                response = response_task.fuse() => response,
                _ = cancellation_stream.cancelled_by_user().fuse() => {
                    acp_thread
                        .update(cx, |thread, cx| {
                            thread.cancel_elicitation(&elicitation_id, cx);
                        })
                        .ok();
                    let output = AskQuestionToolOutput::Canceled;
                    Self::update_card(&input, &output, &event_stream);
                    return Err(output);
                }
            };

            let output = Self::response_output(&input, response);
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
        assert_answer_is_required(&schema);

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
    fn multiple_options_create_a_required_multi_select() {
        let input = AskQuestionTool::validate_input(input(
            vec![option("api", "API"), option("worker", "Worker")],
            true,
        ))
        .unwrap();
        let schema = AskQuestionTool::elicitation_schema(&input);
        assert_answer_is_required(&schema);

        let Some(acp::ElicitationPropertySchema::Array(answer)) =
            schema.properties.get(ANSWER_FIELD)
        else {
            panic!("multi-select must use an array property")
        };
        assert_eq!(answer.min_items, Some(1));
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
