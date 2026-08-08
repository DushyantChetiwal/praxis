use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::StepResult;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};

/// Report what you did in the step of the plan you have just finished.
///
/// The steps that come after this one are not shown your transcript. They are
/// shown this summary and nothing else, so anything you leave out is lost to
/// them. If this step is reached again by a loop, this is also what you will be
/// reminded of, so that you do not try the same thing twice.
///
/// ### What to write
/// - Cover everything the step's "Capture in your summary" line asked for. That
///   line is the contract between this step and the ones that follow it.
/// - Be concrete: name the files you changed, the commands you ran, and the
///   exact output when something failed. "Fixed the bug" tells the next step
///   nothing it can act on.
/// - Say what you did, not what you intend to do next. What happens next is not
///   yours to decide, and a plan is not advanced by predicting it.
/// - If you could not finish, say so plainly and say what blocked you. A step
///   that reports honest failure is more useful than one that reports success
///   it cannot back up.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct CompleteStepToolInput {
    /// What you did, in enough detail that a later step which cannot see your
    /// transcript could carry on from it.
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompleteStepToolOutput {
    Success {
        step: String,
        attempt: usize,
    },
    Error {
        error: String,
    },
}

impl From<CompleteStepToolOutput> for LanguageModelToolResultContent {
    fn from(output: CompleteStepToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize complete_step output: {error}"))
            .into()
    }
}

/// Writes a step's summary onto the plan.
///
/// Which step that is comes from the thread rather than from the model: the run
/// sets it before each step and clears it afterwards. Letting the model name the
/// step would let a confused one overwrite the summary of work it never did.
pub struct CompleteStepTool {
    thread: WeakEntity<Thread>,
}

impl CompleteStepTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

impl AgentTool for CompleteStepTool {
    type Input = CompleteStepToolInput;
    type Output = CompleteStepToolOutput;

    const NAME: &'static str = "complete_step";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Report what this step did".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| CompleteStepToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;

            let summary = input.summary.trim().to_string();
            if summary.is_empty() {
                return Err(CompleteStepToolOutput::Error {
                    error: "A summary cannot be empty; the steps that follow have nothing else \
                            to go on."
                        .into(),
                });
            }

            let recorded = self
                .thread
                .update(cx, |thread, cx| {
                    let Some(path) = thread.architect_running_step().cloned() else {
                        return None;
                    };
                    thread.update_architect_graph(
                        |graph| {
                            let node = graph.node_at_mut(&path)?;
                            let attempt =
                                node.result.as_ref().map_or(1, |result| result.attempt + 1);
                            node.result = Some(StepResult {
                                summary: summary.clone(),
                                attempt,
                            });
                            Some((node.title.clone(), attempt))
                        },
                        cx,
                    )
                    .flatten()
                })
                .map_err(|error| CompleteStepToolOutput::Error {
                    error: format!("The plan this step belongs to is gone: {error}"),
                })?;

            let Some((step, attempt)) = recorded else {
                return Err(CompleteStepToolOutput::Error {
                    error: "There is no step running, so there is nothing to report on. This tool \
                            is only for steps of a plan being run from the Architect canvas."
                        .into(),
                });
            };

            Ok(CompleteStepToolOutput::Success { step, attempt })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_summary_a_model_would_send_deserializes() {
        let input: CompleteStepToolInput = serde_json::from_value(json!({
            "summary": "Changed add() in src/calc.py to return a + b. Ran run_tests.py; all 6 pass.",
        }))
        .unwrap();

        assert!(input.summary.contains("src/calc.py"));
    }

    #[test]
    fn a_summary_is_required() {
        assert!(serde_json::from_value::<CompleteStepToolInput>(json!({})).is_err());
    }
}
