use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::{ArchitectGraph, EdgeCondition, GraphMutationError, NodeId, NodePath, StepModel};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolCapability, ToolInput};

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
/// Locking is their signal that deliberation is over, not yours. A locked step
/// cannot be refined here, and this conversation cannot unlock it: the user can
/// unlock it on the canvas, or ask for it in the main conversation.
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
    /// Execution model using exact available provider/model ids. Omit to keep
    /// the current choice; send null to inherit the plan model. Applied before
    /// locking. Live run changes belong in the main plan conversation.
    #[serde(
        default,
        deserialize_with = "deserialize_model_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<Option<StepModel>>,
    /// When this step leads to each step that follows it. Leave out to keep the
    /// current routing.
    #[serde(default)]
    pub routing: Option<Vec<StepRoute>>,
    /// Whether the user has declared this step settled.
    #[serde(default)]
    pub lock: bool,
}

// Serde normally collapses both a missing field and null to None. Refinement
// needs null to clear an override without making omission erase saved choices.
fn deserialize_model_update<'de, D>(deserializer: D) -> Result<Option<Option<StepModel>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<StepModel>::deserialize(deserializer).map(Some)
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
    /// An objective statement, such as whether a command succeeded. The current
    /// runner asks the model to evaluate it from the completed step summary.
    #[serde(alias = "deterministic")]
    Objective {
        #[serde(alias = "expression")]
        statement: String,
    },
    /// A yes-or-no question that genuinely needs judgement.
    LlmEvaluated { question: String },
}

impl From<RouteCondition> for EdgeCondition {
    fn from(condition: RouteCondition) -> Self {
        match condition {
            RouteCondition::Objective { statement } => EdgeCondition::Objective { statement },
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    node_path: NodePath,
}

impl RefineStepTool {
    pub fn new(plan_thread: WeakEntity<Thread>, node_path: NodePath) -> Self {
        Self {
            plan_thread,
            node_path,
        }
    }
}

fn apply_refinement(
    graph: &mut ArchitectGraph,
    node_path: &NodePath,
    input: RefineStepToolInput,
) -> Result<(String, bool, Vec<String>), GraphMutationError> {
    let RefineStepToolInput {
        goal,
        rules,
        capture,
        model,
        routing,
        lock,
    } = input;

    // Apply the whole refinement to a clone first. A lock invariant or bad path
    // must not leave half the request written to the live plan.
    let mut revised = graph.clone();
    let unknown_steps = if let Some(routing) = routing {
        let routes = routing
            .into_iter()
            .map(|route| {
                (
                    NodeId(route.to),
                    route
                        .condition
                        .map(EdgeCondition::from)
                        .unwrap_or(EdgeCondition::Always),
                )
            })
            .collect();
        revised
            .replace_outgoing_at(node_path, routes)?
            .into_iter()
            .map(|id| id.0)
            .collect()
    } else {
        Vec::new()
    };

    let title = revised.mutate_node_at(node_path, |node| {
        if let Some(goal) = goal {
            node.intent = goal;
        }
        if let Some(rules) = rules {
            node.rules = rules;
        }
        if let Some(capture) = capture {
            node.capture = capture;
        }
        if let Some(model) = model {
            node.model = model;
        }
        node.title.clone()
    })?;
    if lock {
        revised.set_locked_at(node_path, true)?;
    }
    let locked = revised.node_at(node_path).is_some_and(|node| node.locked);
    *graph = revised;
    Ok((title, locked, unknown_steps))
}

impl AgentTool for RefineStepTool {
    type Input = RefineStepToolInput;
    type Output = RefineStepToolOutput;

    const NAME: &'static str = "refine_step";

    fn capability() -> ToolCapability {
        ToolCapability::ConversationMutation
    }

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

            let node_path = self.node_path.clone();
            let outcome = self
                .plan_thread
                .update(cx, |thread, cx| {
                    if thread
                        .architect_run()
                        .is_some_and(|run| run.is_running() || run.can_resume())
                    {
                        return Err(RefineStepToolOutput::Error {
                            error: "This plan has an active or resumable run. Ask for live step model changes in the main plan conversation.".into(),
                        });
                    }
                    if let Some(Some(model)) = &input.model {
                        super::draft_plan_tool::validate_step_model(model, cx).map_err(|error| {
                            RefineStepToolOutput::Error {
                                error: error.to_string(),
                            }
                        })?;
                    }
                    Ok(thread.update_architect_graph(
                        |graph| apply_refinement(graph, &node_path, input),
                        cx,
                    ))
                })
                .map_err(|error| RefineStepToolOutput::Error {
                    error: format!("The plan this step belongs to is gone: {error}"),
                })??;

            let Some(outcome) = outcome else {
                return Err(RefineStepToolOutput::Error {
                    error: "This step's plan is no longer available.".into(),
                });
            };
            let (step, locked, unknown_steps) =
                outcome.map_err(|error| RefineStepToolOutput::Error {
                    error: format!("Could not refine this step: {error}"),
                })?;

            Ok(RefineStepToolOutput::Success {
                step,
                locked,
                unknown_steps,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::ArchitectNode;
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
                {
                    "to": "ship",
                    "condition": { "kind": "objective", "statement": "all checks passed" },
                },
            ],
            "lock": true,
        }))
        .unwrap();

        assert_eq!(input.rules.as_ref().unwrap().len(), 2);
        assert!(input.lock);

        let routing = input.routing.unwrap();
        assert!(
            routing[0].condition.is_none(),
            "a plain connection needs no condition"
        );
        assert!(matches!(
            routing[1].condition,
            Some(RouteCondition::LlmEvaluated { .. })
        ));
        assert!(matches!(
            routing[2].condition,
            Some(RouteCondition::Objective { .. })
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
    fn model_refinements_distinguish_omission_null_and_an_override() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("step", "Step"));
        let path = NodePath::root("step".into());
        let model = StepModel {
            provider: "test-provider".into(),
            model: "test-model".into(),
        };
        for (input, expected) in [
            (json!({"model": model}), Some(model.clone())),
            (json!({"goal": "Refined"}), Some(model.clone())),
            (json!({"model": null}), None),
            (json!({"model": model, "lock": true}), Some(model.clone())),
        ] {
            let input: RefineStepToolInput =
                serde_json::from_value(input).expect("input should load");
            let encoded = serde_json::to_value(&input).expect("input should serialize");
            let restored: RefineStepToolInput =
                serde_json::from_value(encoded).expect("input should replay");
            assert_eq!(input.model, restored.model);
            apply_refinement(&mut graph, &path, restored).expect("refinement should apply");
            assert_eq!(
                graph.node_at(&path).expect("step should exist").model,
                expected
            );
        }
        assert!(graph.node_at(&path).expect("step should exist").locked);
        let before = graph.clone();
        let clear = serde_json::from_value(json!({"model": null})).expect("input should load");
        assert!(apply_refinement(&mut graph, &path, clear).is_err());
        assert_eq!(graph, before, "locked models must not change");
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

    #[test]
    fn refinement_uses_the_complete_nested_path() {
        let nested = || {
            let mut graph = ArchitectGraph::default();
            graph.add_node(ArchitectNode::new("same", "Nested"));
            graph
        };
        let mut first = ArchitectNode::new("first", "First");
        first.subplan = Some(Box::new(nested()));
        let mut second = ArchitectNode::new("second", "Second");
        second.subplan = Some(Box::new(nested()));
        let mut graph = ArchitectGraph::default();
        graph.add_node(first);
        graph.add_node(second);

        let path = NodePath::from(vec!["second".into(), "same".into()]);
        apply_refinement(
            &mut graph,
            &path,
            RefineStepToolInput {
                goal: Some("Only this nested step".into()),
                model: None,
                rules: None,
                capture: None,
                routing: None,
                lock: false,
            },
        )
        .unwrap();

        assert_eq!(
            graph
                .node_at(&NodePath::from(vec!["first".into(), "same".into()]))
                .unwrap()
                .intent,
            ""
        );
        assert_eq!(
            graph.node_at(&path).unwrap().intent,
            "Only this nested step"
        );
    }

    #[test]
    fn failed_locking_is_atomic() {
        let mut nested = ArchitectGraph::default();
        nested.add_node(ArchitectNode::new("child", "Child"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.intent = "Original".into();
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        let path = NodePath::root("parent".into());

        let error = apply_refinement(
            &mut graph,
            &path,
            RefineStepToolInput {
                goal: Some("Changed".into()),
                model: Some(Some(StepModel {
                    provider: "test-provider".into(),
                    model: "test-model".into(),
                })),
                rules: None,
                capture: None,
                routing: None,
                lock: true,
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            GraphMutationError::NestedPlanUnlocked { path: path.clone() }
        );
        let parent = graph.node_at(&path).unwrap();
        assert_eq!(parent.intent, "Original");
        assert!(parent.model.is_none());
        assert!(!parent.locked);
    }
}
