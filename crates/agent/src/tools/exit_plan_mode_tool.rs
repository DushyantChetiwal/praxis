use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, SessionMode, Thread, ToolCallEventStream, ToolCapability, ToolInput};

const START_BUILDING: &str = "start_building";
const KEEP_PLANNING: &str = "keep_planning";

/// Present the plan you have worked out and ask the user to approve it.
///
/// You are in Plan mode: you can read and search the project and the web, but
/// you cannot change anything or run commands. Once you understand the task
/// well enough to say exactly what you would do, call this tool with the plan.
/// The user either approves it, which switches the conversation to Build mode
/// so you can carry it out, or asks you to keep planning.
///
/// ### What to write
/// - The concrete changes you intend to make, file by file where that helps.
/// - How you will check that the work is correct, such as tests to run.
/// - Anything you are unsure about, stated as an open question.
///
/// Do not call this to ask a clarifying question; ask it in your reply instead.
/// Do not start implementing before the plan is approved.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ExitPlanModeToolInput {
    /// The plan, in Markdown.
    pub plan: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExitPlanModeToolOutput {
    Decided { approved: bool, message: String },
    Error { error: String },
}

impl From<ExitPlanModeToolOutput> for LanguageModelToolResultContent {
    fn from(output: ExitPlanModeToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize exit_plan_mode output: {error}"))
            .into()
    }
}

/// Ends Plan mode once the user approves the plan the agent proposes.
///
/// The decision is the user's: the tool shows the plan and waits for them to
/// choose, and only an approval switches the conversation to Build mode.
pub struct ExitPlanModeTool {
    thread: WeakEntity<Thread>,
}

impl ExitPlanModeTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

impl AgentTool for ExitPlanModeTool {
    type Input = ExitPlanModeToolInput;
    type Output = ExitPlanModeToolOutput;

    const NAME: &'static str = "exit_plan_mode";

    fn capability() -> ToolCapability {
        ToolCapability::PlanHandoff
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::SwitchMode
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Ready to build?".into()
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
                .map_err(|error| ExitPlanModeToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;
            let plan = input.plan.trim().to_string();
            if plan.is_empty() {
                return Err(ExitPlanModeToolOutput::Error {
                    error: "The plan cannot be empty; write out what you intend to do.".into(),
                });
            }

            let decision = cx.update(|cx| {
                event_stream.prompt_for_decision(
                    Some("Ready to build?".into()),
                    Some(plan),
                    vec![
                        acp::PermissionOption::new(
                            acp::PermissionOptionId::new(START_BUILDING),
                            "Start Building",
                            acp::PermissionOptionKind::AllowOnce,
                        ),
                        acp::PermissionOption::new(
                            acp::PermissionOptionId::new(KEEP_PLANNING),
                            "Keep Planning",
                            acp::PermissionOptionKind::RejectOnce,
                        ),
                    ],
                    cx,
                )
            });
            let decision = decision
                .await
                .map_err(|error| ExitPlanModeToolOutput::Error {
                    error: format!("No decision was made: {error}"),
                })?;

            if decision.0.as_ref() != START_BUILDING {
                return Ok(ExitPlanModeToolOutput::Decided {
                    approved: false,
                    message: "The user wants to keep planning. You are still in Plan mode. Ask \
                              what they would change, or refine the plan and present it again."
                        .into(),
                });
            }

            self.thread
                .update(cx, |thread, cx| {
                    thread.set_session_mode(SessionMode::Build, cx);
                })
                .map_err(|error| ExitPlanModeToolOutput::Error {
                    error: format!("The conversation is gone: {error}"),
                })?;
            Ok(ExitPlanModeToolOutput::Decided {
                approved: true,
                message: "The user approved the plan and the conversation is now in Build mode. \
                          Carry out the plan now."
                    .into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plan_is_required() {
        assert!(serde_json::from_value::<ExitPlanModeToolInput>(json!({})).is_err());
        let input: ExitPlanModeToolInput =
            serde_json::from_value(json!({ "plan": "1. Change add() in src/calc.rs." }))
                .expect("a plan should deserialize");
        assert!(input.plan.contains("src/calc.rs"));
    }
}
