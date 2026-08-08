use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::{EdgeCondition, NodeId};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};

/// Record what has been settled about the one step this conversation is about.
///
/// This conversation exists to pin down a single step of the plan. Use this tool
/// to write down what you and the user have agreed, so it appears on the canvas
/// and survives this conversation.
///
/// ### What belongs where
/// - `goal` is what "done" means for this step, in one or two sentences. If the
///   user has sharpened it, send the sharpened version.
/// - `rules` are the constraints that must hold however the step is carried out.
///   Send the complete list every time; it replaces the previous one.
/// - `capture` is what this step's summary must contain. The steps that follow
///   it are shown that summary and nothing else about this step, so name the
///   specifics they will need rather than saying "what happened".
/// - `routing` states when this step leads to each of the steps that follow it.
///   Only send it once the user has settled the routing.
///
/// ### Nothing else is yours to change
/// You cannot touch another step, add steps, or remove them. If the discussion
/// reveals that the shape of the plan is wrong, say so and let the user take it
/// back to the main conversation, which owns the plan as a whole. You also do
/// not carry out the step: the main thread builds, this thread decides.
///
/// ### Locking
/// Set `lock` only when the user has explicitly said this step is settled.
/// Locking is their signal that deliberation is over, not yours.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct RefineStepToolInput {
    /// What this step must accomplish. Leave out to keep what is already there.
    #[serde(default)]
    pub goal: Option<String>,
    /// The constraints on this step, replacing the current list. Leave out to
    /// keep what is already there; send an empty list to clear them.
    #[serde(default)]
    pub rules: Option<Vec<String>>,
    /// What this step's summary must contain, for the steps that follow it.
    /// Leave out to keep what is already there.
    #[serde(default)]
    pub capture: Option<String>,
    /// When this step leads to each step that follows it. Leave out to keep the
    /// current routing.
    #[serde(default)]
    pub routing: Option<Vec<StepRoute>>,
    /// Whether the user has declared this step settled.
    #[serde(default)]
    pub lock: bool,
}

/// One outgoing connection from this step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct StepRoute {
    /// The id of the step this connection leads to.
    pub to: String,
    /// When this connection is taken. Leave out when the step simply follows.
    #[serde(default)]
    pub condition: Option<RouteCondition>,
}

/// When a connection is taken.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteCondition {
    /// Something checkable without judgement, such as a command's exit status.
    Deterministic { expression: String },
    /// A yes-or-no question that genuinely needs judgement.
    LlmEvaluated { question: String },
}

impl From<RouteCondition> for EdgeCondition {
    fn from(condition: RouteCondition) -> Self {
        match condition {
            RouteCondition::Deterministic { expression } => {
                EdgeCondition::Deterministic { expression }
            }
            RouteCondition::LlmEvaluated { question } => EdgeCondition::LlmEvaluated { question },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RefineStepToolOutput {
    Success {
        step: String,
        locked: bool,
        /// Routing that was asked for but could not be applied, because it
        /// pointed at a step that does not exist.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unknown_steps: Vec<String>,
    },
    Error {
        error: String,
    },
}

impl From<RefineStepToolOutput> for LanguageModelToolResultContent {
    fn from(output: RefineStepToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize refine_step output: {error}"))
            .into()
    }
}

/// Bound at construction to one step of one plan, so a step's conversation
/// cannot reach past its own step however it is prompted.
pub struct RefineStepTool {
    /// The thread that owns the plan, which is the parent of this one.
    plan_thread: WeakEntity<Thread>,
    node_id: NodeId,
}

impl RefineStepTool {
    pub fn new(plan_thread: WeakEntity<Thread>, node_id: NodeId) -> Self {
        Self {
            plan_thread,
            node_id,
        }
    }
}

impl AgentTool for RefineStepTool {
    type Input = RefineStepToolInput;
    type Output = RefineStepToolOutput;

    const NAME: &'static str = "refine_step";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) if input.lock => "Settle and lock this step".into(),
            _ => "Record what this step must do".into(),
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
                .map_err(|error| RefineStepToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;

            let node_id = self.node_id.clone();
            let outcome = self
                .plan_thread
                .update(cx, |thread, cx| {
                    let mut title = None;
                    let mut unknown_steps = Vec::new();

                    thread.update_architect_graph(
                        |graph| {
                            if graph.node(&node_id).is_none() {
                                return;
                            }

                            let known: Vec<NodeId> =
                                graph.nodes.iter().map(|node| node.id.clone()).collect();

                            if let Some(routing) = input.routing {
                                let mut wanted = Vec::new();
                                for route in routing {
                                    let to = NodeId(route.to.clone());
                                    if known.contains(&to) {
                                        wanted.push((
                                            to,
                                            route
                                                .condition
                                                .map(EdgeCondition::from)
                                                .unwrap_or(EdgeCondition::Always),
                                        ));
                                    } else {
                                        unknown_steps.push(route.to);
                                    }
                                }

                                // Only this step's own outgoing connections are
                                // replaced; every other connection in the plan
                                // is left exactly as it was.
                                graph.edges.retain(|edge| edge.from != node_id);
                                for (to, condition) in wanted {
                                    graph.connect_with(node_id.clone(), to, condition);
                                }
                            }

                            let Some(node) = graph.node_mut(&node_id) else {
                                return;
                            };
                            if let Some(goal) = input.goal {
                                node.intent = goal;
                            }
                            if let Some(rules) = input.rules {
                                node.rules = rules;
                            }
                            if let Some(capture) = input.capture {
                                node.capture = capture;
                            }
                            if input.lock {
                                node.locked = true;
                            }
                            title = Some(node.title.clone());
                        },
                        cx,
                    );

                    (title, unknown_steps)
                })
                .map_err(|error| RefineStepToolOutput::Error {
                    error: format!("The plan this step belongs to is gone: {error}"),
                })?;

            let (title, unknown_steps) = outcome;
            let Some(step) = title else {
                return Err(RefineStepToolOutput::Error {
                    error: "This step is no longer part of the plan; it was probably deleted."
                        .into(),
                });
            };

            Ok(RefineStepToolOutput::Success {
                step,
                locked: input.lock,
                unknown_steps,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_input_a_model_would_send_deserializes() {
        let input: RefineStepToolInput = serde_json::from_value(json!({
            "goal": "Every public endpoint rejects an expired token",
            "rules": ["Do not change the token format", "Keep the existing error codes"],
            "routing": [
                { "to": "tests" },
                {
                    "to": "fix",
                    "condition": { "kind": "llm_evaluated", "question": "Did any endpoint still accept it?" },
                },
            ],
            "lock": true,
        }))
        .unwrap();

        assert_eq!(input.rules.as_ref().unwrap().len(), 2);
        assert!(input.lock);

        let routing = input.routing.unwrap();
        assert!(routing[0].condition.is_none(), "a plain connection needs no condition");
        assert!(matches!(
            routing[1].condition,
            Some(RouteCondition::LlmEvaluated { .. })
        ));
    }

    #[test]
    fn every_field_is_optional_so_a_partial_refinement_is_allowed() {
        let input: RefineStepToolInput = serde_json::from_value(json!({
            "goal": "Just the goal this time",
        }))
        .unwrap();

        assert!(input.rules.is_none(), "omitted rules must not clear them");
        assert!(input.routing.is_none());
        assert!(!input.lock);
    }

    #[test]
    fn rules_can_be_cleared_explicitly() {
        let input: RefineStepToolInput = serde_json::from_value(json!({ "rules": [] })).unwrap();

        assert_eq!(
            input.rules,
            Some(Vec::new()),
            "an empty list is a deliberate instruction to drop the rules"
        );
    }
}
