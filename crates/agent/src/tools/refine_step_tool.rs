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
/// and survives this conversation. This is a partial update bound to this
/// conversation's full node path, not a graph replacement. Omitted fields are
/// preserved. Use draft_plan only for deliberate whole-graph replacement;
/// use edit_architect_plan in the main conversation for targeted topology,
/// layout, repeat-limit, or approved surface edits. Active or resumable runs
/// refuse refine_step; use control_architect_run for pending brief/model changes.
///
/// ### What belongs where
/// - `goal` is what "done" means for this step, in one or two sentences. If the
///   user has sharpened it, send the sharpened version.
/// - `rules` are the constraints that must hold however the step is carried out.
///   If changing rules, send the complete list; [] clears it. Unlike draft_plan
///   merging, empty values here deliberately clear the supplied brief field.
/// - `capture` is what this step's summary must contain. The steps that follow
///   it are shown that summary and nothing else about this step, so name the
///   specifics they will need rather than saying "what happened".
/// - `file_surface` replaces the existing-file declaration: project-relative
///   paths such as ["src/main.rs","README.md"], or [] when no existing files
///   are anticipated. For multiple roots, ambiguous paths need a root prefix,
///   such as "backend/src/main.rs". Saved declarations use root/path identities.
///   Local and connected remote projects use the same path rules. Omit it
///   to preserve the declaration; null is not a declaration. This is planning
///   information for concurrency checks, not a write allowlist. Parallel steps,
///   including nested children, must have disjoint surfaces.
/// - `routing` replaces ALL outgoing routes, not just the changed connection.
///   Destinations are local IDs in this step's containing graph. [] removes all
///   outgoing routes. Unknown destinations are omitted and reported; inspect
///   unknown_steps and problems rather than assuming every route was accepted.
///   Existing repeat limits to retained destinations survive. This tool cannot
///   set max_repeats; use edit_architect_plan remove/insert edge operations.
///   A route to this step's own ID is a self-loop: use a condition or a retained
///   repeat limit. Plain fan-out is not if/else; all plain routes are the fallback
///   when no conditional route is selected. The current runner takes the first
///   conditional YES. Conditions do not waive file-surface overlap validation.
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
/// unlock it on the canvas, or ask for it in the main conversation. Locked
/// ancestors also prevent refinement. Edits and the optional lock are atomic
/// on mutation failure, but validation problems can remain in the saved draft;
/// fix them before running. A parent can lock only after its children are locked.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct RefineStepToolInput {
    /// Replace this step's goal. Omit or null to preserve; "" explicitly clears
    /// it. This updates intent, not the node title or another step.
    #[serde(default)]
    pub goal: Option<String>,
    /// The constraints on this step, replacing the current list. Leave out to
    /// keep what is already there (null also preserves); [] explicitly clears.
    #[serde(default)]
    pub rules: Option<Vec<String>>,
    /// What this step's summary must contain, for the steps that follow it.
    /// Omit or null to preserve; "" explicitly clears the summary requirements.
    #[serde(default)]
    pub capture: Option<String>,
    /// Complete existing-file surface: ["src/main.rs","README.md"] relative to
    /// the project root, or root-prefixed paths for ambiguous multi-root files.
    /// No directories, globs, absolute paths, or '..'. Omit to preserve;
    /// [] explicitly anticipates no existing files;
    /// null is rejected. This replaces, not appends, and imposes no write allowlist.
    /// Invalid or overlapping declarations are saved with actionable problems.
    #[serde(
        default,
        deserialize_with = "deserialize_file_surface_update",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<String>")]
    pub file_surface: Option<Vec<String>>,
    /// Execution model using exact available provider/model ids. Omit to keep
    /// the current choice; send null to inherit the plan model. Applied before
    /// locking. Copy exact models[].configuration from list_agents_and_models;
    /// never infer IDs from display labels. Live updates use control_architect_run.
    #[serde(
        default,
        deserialize_with = "deserialize_model_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<Option<StepModel>>,
    /// Replace all outgoing routes. Omit or null to preserve; [] removes all.
    /// Example: [{"to":"test"},{"to":"audit"}]. IDs are local to the containing
    /// graph, not paths; both plain routes may run. Unknown targets are reported.
    /// Retained destinations keep existing repeat limits; no max_repeats input.
    #[serde(default)]
    pub routing: Option<Vec<StepRoute>>,
    /// Lock after applying edits only with explicit user agreement. Default
    /// false preserves the current lock state; it does not unlock. Children must
    /// already be locked before locking a parent; mutation failure is atomic.
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

fn deserialize_file_surface_update<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}

/// One outgoing connection from this step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct StepRoute {
    /// Destination ID in this step's containing graph, not a full node path.
    /// The current step's own ID makes a self-loop; unknown IDs are reported.
    pub to: String,
    /// Omit or null for an unconditional route. For a conditional self-loop,
    /// use {"kind":"objective","statement":"tests failed"} or
    /// {"kind":"llm_evaluated","question":"Does the result need another pass?"}.
    /// Objective statements are model-evaluated, not executable commands.
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
        /// Remaining plan blockers; the refinement is saved, but these need edits
        /// before execution. Includes invalid or conflicting file surfaces.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        problems: Vec<String>,
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
        file_surface,
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
        if let Some(file_surface) = file_surface {
            node.file_surface = Some(file_surface);
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
            let mut input = input
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
                            error: "This plan has an active or resumable run. Ask the main plan conversation to use control_architect_run for brief/model changes, or stop execution and preview edit_architect_plan for file-surface or routing changes.".into(),
                        });
                    }
                    if let Some(Some(model)) = &input.model {
                        super::draft_plan_tool::validate_step_model(model, cx).map_err(|error| {
                            RefineStepToolOutput::Error {
                                error: error.to_string(),
                            }
                        })?;
                    }
                    if let Some(surface) = &mut input.file_surface {
                        *surface = super::draft_plan_tool::resolve_file_surface(
                            surface, thread.project(), cx,
                        ).map_err(|error| RefineStepToolOutput::Error { error: error.to_string() })?;
                    }
                    Ok(thread.update_architect_graph(
                        |graph| {
                            apply_refinement(graph, &node_path, input).map(|outcome| {
                                let problems = super::draft_plan_tool::plan_validation_problems(graph);
                                (outcome, problems)
                            })
                        },
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
            let ((step, locked, unknown_steps), problems) =
                outcome.map_err(|error| RefineStepToolOutput::Error {
                    error: format!("Could not refine this step: {error}"),
                })?;

            Ok(RefineStepToolOutput::Success {
                step,
                locked,
                unknown_steps,
                problems,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::ArchitectNode;
    use serde_json::json;

    #[gpui::test]
    async fn refinement_resolves_project_relative_surface(cx: &mut gpui::TestAppContext) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        thread.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(ArchitectGraph {
                nodes: vec![ArchitectNode::new("step", "Step")],
                edges: vec![],
            }), cx);
        });
        let (events, _receiver) = ToolCallEventStream::test();
        let input = ToolInput::ready(json!({"file_surface": ["README.md", "src/main.rs"]}));
        cx.update(|cx| {
            Arc::new(RefineStepTool::new(thread.downgrade(), NodePath::root("step".into())))
                .run(input, events, cx)
        }).await.expect("refine relative surface");
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph().expect("graph").node(&"step".into()).expect("step").file_surface,
                Some(vec!["a/README.md".into(), "a/src/main.rs".into()]));
        });
    }

    #[test]
    fn model_description_and_schema_keep_partial_update_contracts() {
        let description = <RefineStepTool as AgentTool>::description()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "partial update",
            "draft_plan",
            "edit_architect_plan",
            "control_architect_run",
            "Active or resumable runs",
            "replaces ALL outgoing routes",
            "Unknown destinations",
            "cannot set max_repeats",
            "self-loop",
            "first conditional YES",
            "Locked ancestors",
            "validation problems",
            "not a write allowlist",
        ] {
            assert!(
                description.contains(required),
                "missing refinement guidance: {required}"
            );
        }
        let mut schema = RefineStepTool::input_schema().to_value();
        language_model::tool_schema::normalize_tool_schema(&mut schema);
        for (field, required) in [
            ("goal", "explicitly clears"),
            ("rules", "[] explicitly clears"),
            ("capture", "Omit or null to preserve"),
            ("file_surface", "null is rejected"),
            ("file_surface", "replaces, not appends"),
            ("model", "send null"),
            ("routing", "Replace all outgoing routes"),
            ("routing", "no max_repeats"),
            ("lock", "does not unlock"),
        ] {
            let description = schema["properties"][field]["description"]
                .as_str()
                .expect(field)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(description.contains(required), "{field}: {required}");
        }
    }

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
        assert!(input.file_surface.is_none());
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
            (json!({"model": model, "lock": true}), Some(model)),
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

    #[gpui::test]
    async fn surface_refinements_report_blockers_and_respect_active_runs(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let path = NodePath::root("step".into());
        thread.update(cx, |thread, cx| {
            thread.set_architect_graph(
                Some(ArchitectGraph {
                    nodes: vec![ArchitectNode::new("step", "Step")],
                    edges: vec![],
                }),
                cx,
            );
        });
        for (surface, blocked) in [(json!(["../outside.rs"]), true), (json!([]), false)] {
            let (events, _receiver) = ToolCallEventStream::test();
            let input = ToolInput::ready(json!({"file_surface": surface}));
            let output = cx
                .update(|cx| {
                    Arc::new(RefineStepTool::new(thread.downgrade(), path.clone()))
                        .run(input, events, cx)
                })
                .await
                .expect("save refinement");
            let RefineStepToolOutput::Success { problems, .. } = output else {
                panic!("expected saved refinement");
            };
            assert_eq!(!problems.is_empty(), blocked);
            thread.read_with(cx, |thread, _| {
                let node = thread
                    .architect_graph()
                    .expect("graph")
                    .node_at(&path)
                    .expect("step");
                assert_eq!(json!(node.file_surface), surface);
            });
        }
        let before = thread.read_with(cx, |thread, _| thread.architect_graph().cloned());
        thread.update(cx, |thread, cx| {
            thread.start_architect_run(path.clone(), "Step".into(), Task::ready(()), cx);
        });
        let (events, _receiver) = ToolCallEventStream::test();
        let input = ToolInput::ready(json!({"file_surface": ["project/new.rs"]}));
        let result = cx
            .update(|cx| {
                Arc::new(RefineStepTool::new(thread.downgrade(), path)).run(input, events, cx)
            })
            .await;
        assert!(result.is_err());
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph(), before.as_ref())
        });
        thread.update(cx, |thread, cx| {
            thread.finish_architect_run(architect::RunOutcome::Completed, cx);
        });
    }

    #[test]
    fn file_surface_updates_preserve_omission_and_reject_null() {
        let schema =
            serde_json::to_value(schemars::schema_for!(RefineStepToolInput)).expect("schema");
        assert_eq!(schema["properties"]["file_surface"]["type"], "array");
        let mut node = ArchitectNode::new("step", "Step");
        node.file_surface = None;
        let mut graph = ArchitectGraph {
            nodes: vec![node],
            edges: vec![],
        };
        let path = NodePath::root("step".into());
        for (value, expected) in [
            (json!({"goal": "Reviewed"}), None),
            (
                json!({"file_surface": ["project/src/main.rs"]}),
                Some(vec!["project/src/main.rs".into()]),
            ),
            (
                json!({"capture": "Summary"}),
                Some(vec!["project/src/main.rs".into()]),
            ),
            (json!({"file_surface": []}), Some(vec![])),
        ] {
            let input: RefineStepToolInput = serde_json::from_value(value).expect("input");
            let restored = serde_json::from_value(serde_json::to_value(input).expect("serialize"))
                .expect("replay");
            apply_refinement(&mut graph, &path, restored).expect("refine");
            assert_eq!(graph.node_at(&path).expect("step").file_surface, expected);
        }
        assert!(
            serde_json::from_value::<RefineStepToolInput>(json!({"file_surface": null})).is_err()
        );
        graph.set_locked_at(&path, true).expect("lock");
        let before = graph.clone();
        let input =
            serde_json::from_value(json!({"file_surface": ["project/other.rs"]})).expect("input");
        assert!(apply_refinement(&mut graph, &path, input).is_err());
        assert_eq!(graph, before);
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
                file_surface: Some(vec!["project/nested.rs".into()]),
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
        assert_eq!(
            graph.node_at(&path).expect("target").file_surface,
            Some(vec!["project/nested.rs".into()])
        );
        assert_eq!(
            graph
                .node_at(&NodePath::from(vec!["first".into(), "same".into()]))
                .expect("other child")
                .file_surface,
            Some(vec![])
        );
        graph
            .lock_deeply_at(&NodePath::root("second".into()))
            .expect("lock parent");
        graph.node_at_mut(&path).expect("target").locked = false;
        let before = graph.clone();
        let input = serde_json::from_value(json!({"file_surface": []})).expect("input");
        assert!(apply_refinement(&mut graph, &path, input).is_err());
        assert_eq!(
            graph, before,
            "a locked ancestor protects the child's surface"
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
                file_surface: Some(vec!["project/changed.rs".into()]),
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
        assert_eq!(parent.file_surface, Some(Vec::new()));
        assert!(!parent.locked);
    }
}
