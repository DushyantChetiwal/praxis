use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::{ProposedEdge, ProposedGraph, ProposedNode};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};

/// Draw the plan for this task as a flowchart on the Architect canvas.
///
/// Each step becomes a node the user can read, rewrite, discuss in its own
/// chat, and lock once they are satisfied with it. Each connection says when
/// one step leads to another. Nothing is carried out by this tool: the plan is
/// a proposal the user reshapes, and they run it when they are ready.
///
/// ### What makes a good plan
/// - One step per meaningful unit of work. A step that says "do the task" is
///   useless, and twenty steps for a two-line change is noise.
/// - Give every step an `intent` saying what "done" means for it. That is what
///   the user will argue with, and what you will be held to later.
/// - Put constraints in `rules`, not in the intent. Rules are what must remain
///   true regardless of how the step is carried out.
///
/// ### Connections
/// - Leave out `condition` when a step simply follows another.
/// - Use `deterministic` when the answer can be checked without judgement,
///   such as a command's exit status or whether a file exists.
/// - Use `llm_evaluated` only when the decision genuinely needs judgement, and
///   phrase it as a yes-or-no question. The user will see which parts of their
///   control flow depend on a model's opinion, so do not reach for this to
///   avoid stating a real condition.
/// - Pointing a connection back at an earlier step is how you express a loop,
///   such as returning to the edit step when tests fail. Loops are expected;
///   just make sure something can leave the loop.
///
/// ### Replacing an existing plan
/// This call replaces the whole plan. Send the complete set of steps every
/// time, including the ones that are unchanged. Positions the user has
/// arranged are not preserved across a replacement, so do not redraw a plan
/// the user has already arranged unless they ask for changes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct DraftPlanToolInput {
    /// The steps of the plan, in any order; the canvas lays them out from the
    /// connections.
    pub nodes: Vec<ProposedNode>,
    /// How the steps connect. A plan with more than one step needs these,
    /// otherwise nothing says what order the work happens in.
    #[serde(default)]
    pub edges: Vec<ProposedEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DraftPlanToolOutput {
    Success {
        steps: usize,
        connections: usize,
        /// Anything wrong with the plan as drawn, such as a connection to a
        /// step that does not exist. Worth fixing before the user sees it.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        problems: Vec<String>,
    },
    Error {
        error: String,
    },
}

impl From<DraftPlanToolOutput> for LanguageModelToolResultContent {
    fn from(output: DraftPlanToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize draft_plan output: {error}"))
            .into()
    }
}

pub struct DraftPlanTool {
    thread: WeakEntity<Thread>,
}

impl DraftPlanTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

impl AgentTool for DraftPlanTool {
    type Input = DraftPlanToolInput;
    type Output = DraftPlanToolOutput;

    const NAME: &'static str = "draft_plan";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match input.nodes.len() {
                1 => "Draft a plan of 1 step".into(),
                count => format!("Draft a plan of {count} steps").into(),
            },
            Err(_) => "Draft a plan".into(),
        }
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
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;

            if input.nodes.is_empty() {
                return Err(DraftPlanToolOutput::Error {
                    error: "A plan needs at least one step.".into(),
                });
            }

            let graph = ProposedGraph {
                nodes: input.nodes,
                edges: input.edges,
            }
            .into_graph();

            let steps = graph.nodes.len();
            let connections = graph.edges.len();
            let problems = graph
                .problems()
                .iter()
                .map(|problem| problem.to_string())
                .collect();

            self.thread
                .update(cx, |thread, cx| {
                    thread.set_architect_graph(Some(graph), cx);
                })
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: format!("The thread this plan belongs to is gone: {error}"),
                })?;

            Ok(DraftPlanToolOutput::Success {
                steps,
                connections,
                problems,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plan_the_model_might_send_deserializes() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "reproduce",
                    "title": "Reproduce the failure",
                    "intent": "Get a failing test that shows the bug",
                    "rules": ["Do not change behaviour yet"],
                },
                { "id": "fix", "title": "Fix it", "intent": "Make the test pass" },
            ],
            "edges": [
                { "from": "reproduce", "to": "fix" },
                {
                    "from": "fix",
                    "to": "reproduce",
                    "condition": { "kind": "llm_evaluated", "question": "Is the test still failing?" },
                },
            ],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 2);
        assert!(
            graph.problems().is_empty(),
            "a well-formed plan should have nothing to report: {:?}",
            graph.problems()
        );
        assert!(
            graph.nodes.iter().all(|node| node.position.is_some()),
            "the plan should arrive laid out"
        );
    }

    #[test]
    fn a_connection_may_leave_out_its_condition() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [{ "id": "only", "title": "Only step" }],
        }))
        .unwrap();

        assert!(input.edges.is_empty());
        assert!(input.nodes[0].rules.is_empty());
    }
}
