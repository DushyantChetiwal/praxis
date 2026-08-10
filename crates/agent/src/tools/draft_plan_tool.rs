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
/// ### Drawing over a plan that already exists
/// This replaces the whole plan, so when one is already on the canvas you are
/// editing rather than starting over. The current plan, with each step's id, is
/// given to you above; reuse those ids for steps that are staying, so their
/// history stays attached to them.
///
/// A step the user has **locked** is settled: its goal, rules, capture and
/// routing were argued out, often in a chat of its own. Restate a locked step
/// exactly as it is, connections included, or this tool will refuse the whole
/// draft. If a locked step genuinely has to change, say so and let the user
/// unlock it.
///
/// For an unlocked step you are keeping, anything you leave blank keeps what is
/// already there, so you need only state what you are actually changing.
///
/// ### What makes a good plan
/// - One step per meaningful unit of work. A step that says "do the task" is
///   useless, and twenty steps for a two-line change is noise.
/// - Give every step an `intent` saying what "done" means for it. That is what
///   the user will argue with, and what you will be held to later.
/// - Put constraints in `rules`, not in the intent. Rules are what must remain
///   true regardless of how the step is carried out.
///
/// ### What each step hands on: `capture`
/// `capture` says what a step's summary must contain. It is the contract
/// between a step and the steps that follow it: whatever you name there is what
/// they will be told, and nothing else about the step reaches them — not its
/// reasoning, not the files it read, not the commands it ran.
/// - Name the specifics the later steps actually need, such as "the path of the
///   file that failed and the exact assertion message", not "what happened".
/// - A step that leads nowhere usually needs no `capture`, because there is
///   nobody left to tell.
/// - A condition on a connection out of a step can only be judged from that
///   step's summary, so make sure the `capture` covers whatever the condition
///   asks about.
///
/// ### Steps that contain plans: `steps`
/// A step may carry a nested plan in `steps`, for work that is one step at this
/// level but several once you look closely. The nested plan runs in place of
/// the step, and the step is done when its plan is.
/// - Reach for this when a step would otherwise need more than a handful of
///   rules. That is the sign it is really several steps wearing one hat.
/// - Nest only as far as the work genuinely divides. A plan nested more than 5
///   deep is refused rather than run.
/// - A nested plan is a plan like any other: its steps need intents, captures
///   and connections of their own.
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
        /// Steps that kept detail this draft did not carry, because it was
        /// settled after the plan was first drawn. Said out loud so the model
        /// does not assume the plan now reads exactly as it wrote it.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        kept_existing_detail: Vec<String>,
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

            let draft = ProposedGraph {
                nodes: input.nodes,
                edges: input.edges,
            }
            .into_graph();

            // Drawing over a plan that already exists is an edit, not a fresh
            // start: everything settled since it was first drawn has to survive.
            let merged = self
                .thread
                .read_with(cx, |thread, _cx| {
                    thread
                        .architect_graph()
                        .map(|existing| existing.merge_draft(draft.clone()))
                })
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: format!("The thread this plan belongs to is gone: {error}"),
                })?;

            let merged = match merged {
                Some(Ok(merged)) => merged,
                Some(Err(refusal)) => {
                    return Err(DraftPlanToolOutput::Error {
                        error: format!(
                            "This draft would have discarded steps the user has locked: {}. A \
                             locked step is settled — its goal, rules, capture and where it leads \
                             were argued out, often in its own chat. Draw the plan again, \
                             restating those steps and their connections exactly as they are, and \
                             change only what is not locked. If one of them really does have to \
                             change, say so and let the user unlock it first.",
                            refusal.steps.join(", "),
                        ),
                    });
                }
                None => architect::MergedDraft {
                    graph: draft,
                    preserved: Vec::new(),
                },
            };

            let kept_existing_detail: Vec<String> =
                merged.preserved.iter().map(|id| id.0.clone()).collect();
            let graph = merged.graph;

            let steps = graph.nodes.len();
            let connections = graph.edges.len();
            let mut problems: Vec<String> = graph
                .problems()
                .iter()
                .map(|problem| problem.to_string())
                .collect();
            // Not a structural fault, so the plan is still drawn. But a step
            // that leads somewhere while saying nothing about what it hands on
            // leaves the steps after it with only their own goal to work from,
            // which is almost always an oversight rather than a decision.
            problems.extend(
                graph.steps_without_capture().iter().map(|id| {
                    format!("{id} leads to another step but does not say what it hands on")
                }),
            );

            self.thread
                .update(cx, |thread, cx| {
                    thread.set_architect_graph(Some(graph), cx);
                    // Drawing a plan is the act that says this task is being
                    // planned rather than carried out, so it is what puts the
                    // thread into Plan. Nothing else has to be asked or set.
                    thread.set_session_mode(crate::SessionMode::Plan, cx);
                })
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: format!("The thread this plan belongs to is gone: {error}"),
                })?;

            Ok(DraftPlanToolOutput::Success {
                steps,
                connections,
                problems,
                kept_existing_detail,
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
                    "capture": "The test file and the assertion that failed",
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
    fn a_step_can_carry_a_plan_of_its_own() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "handlers",
                    "title": "Write handlers",
                    "intent": "Endpoints behave per the schema",
                    "capture": "Which endpoints you added and their status codes",
                    "steps": {
                        "nodes": [
                            { "id": "parse", "title": "Parse the body" },
                            { "id": "respond", "title": "Respond" },
                        ],
                        "edges": [{ "from": "parse", "to": "respond" }],
                    },
                },
            ],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        let handlers = graph.node(&"handlers".into()).unwrap();
        assert_eq!(
            handlers.capture,
            "Which endpoints you added and their status codes"
        );

        let subplan = handlers.subplan().expect("the nested plan should survive");
        assert_eq!(subplan.nodes.len(), 2);
        assert_eq!(subplan.edges.len(), 1);
        assert!(
            subplan.nodes.iter().all(|node| node.position.is_some()),
            "a nested plan should arrive laid out, like any other"
        );
    }

    #[test]
    fn a_step_that_leads_somewhere_without_a_capture_is_reported() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                { "id": "first", "title": "First" },
                { "id": "second", "title": "Second" },
            ],
            "edges": [{ "from": "first", "to": "second" }],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        let missing = graph.steps_without_capture();
        assert!(missing.contains(&"first".into()));
        assert!(
            !missing.contains(&"second".into()),
            "a step nothing leads out of has nobody to hand anything to"
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

    #[test]
    fn a_step_that_contains_a_plan_arrives_with_that_plan_inside_it() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "survey",
                    "title": "Survey the callers",
                    "capture": "Every call site of the old API",
                },
                {
                    "id": "migrate",
                    "title": "Migrate the callers",
                    "steps": {
                        "nodes": [
                            { "id": "rewrite", "title": "Rewrite them", "capture": "Files touched" },
                            { "id": "compile", "title": "Compile" },
                        ],
                        "edges": [{ "from": "rewrite", "to": "compile" }],
                    },
                },
            ],
            "edges": [{ "from": "survey", "to": "migrate" }],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        let nested = graph
            .node(&"migrate".into())
            .and_then(|node| node.subplan())
            .expect("the nested plan should have become a subplan");
        assert_eq!(nested.nodes.len(), 2);
        assert_eq!(nested.edges.len(), 1);
        assert!(
            graph
                .node(&"survey".into())
                .is_some_and(|node| !node.has_subplan()),
            "a step without nested steps should not gain an empty plan"
        );
    }

    #[test]
    fn what_a_step_hands_on_survives_into_the_plan() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "measure",
                    "title": "Measure the regression",
                    "capture": "The before and after timings, in milliseconds",
                },
                { "id": "report", "title": "Report" },
            ],
            "edges": [{ "from": "measure", "to": "report" }],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        assert_eq!(
            graph
                .node(&"measure".into())
                .map(|node| node.capture.as_str()),
            Some("The before and after timings, in milliseconds")
        );
        assert!(
            graph.steps_without_capture().is_empty(),
            "only the last step lacks a capture, and it hands nothing on"
        );
    }

    #[test]
    fn a_step_that_leads_somewhere_without_saying_what_it_hands_on_is_reported() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                { "id": "build", "title": "Build" },
                { "id": "ship", "title": "Ship" },
            ],
            "edges": [{ "from": "build", "to": "ship" }],
        }))
        .unwrap();

        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();

        assert_eq!(
            graph.steps_without_capture(),
            vec!["build".into()],
            "the step feeding another one should be flagged, not the last one"
        );
    }
}
