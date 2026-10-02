use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use architect::{ArchitectGraph, ProposedEdge, ProposedGraph, ProposedNode, StepModel};
use gpui::{App, Entity, SharedString, Task, WeakEntity};
use language_model::{LanguageModelRegistry, LanguageModelToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolCapability, ToolInput};

/// Draw the plan for this task as a flowchart on the Architect canvas.
///
/// Each step becomes a node the user can read, rewrite, discuss in its own
/// chat, and lock once they are satisfied with it. Each connection says when
/// one step leads to another. Nothing is carried out by this tool: the plan is
/// a proposal the user reshapes, and they run it when they are ready.
///
/// ### Choose the right tool
/// Use draft_plan to create a plan or deliberately replace the whole graph, not
/// to patch one step. Inspect the authoritative graph with inspect_architect_plan
/// first. Use edit_architect_plan for targeted node/edge/surface/layout edits;
/// it can preserve unaffected results and a supported checkpoint after approval.
/// Use refine_step in an unlocked step's own draft chat, or control_architect_run
/// for permission-approved pending brief/model updates in the main conversation.
///
/// A changed replacement archives the previous run and clears its current
/// checkpoint; archived history is inspectable, not a resumable update to the
/// replacement. Active or resumable runs refuse draft_plan. Do not use redrafting
/// as recovery from a stopped run; inspect it and use targeted edits or controls.
/// Reuse the same full node paths for retained steps and send the complete graph.
/// Step IDs must be non-blank and unique within each graph. Nested graphs have
/// separate namespaces. Duplicate IDs are rejected before saving, never silently
/// renamed or used to redirect connections.
///
/// A step the user has **locked** is settled: its goal, rules, capture, file surface and
/// routing were argued out, often in a chat of its own. Restate a locked step
/// exactly as it is, connections included, or this tool will refuse the whole
/// draft. If a locked step genuinely has to change, say so and ask the user.
/// Once they agree, unlock it with `set_step_locks` before redrawing, or let
/// them unlock it on the canvas. Never unlock a step they have not asked you to.
///
/// For a retained unlocked step, blank responsibility/intent/capture, empty
/// rules, omitted or null model, and omitted steps preserve their existing
/// values. A supplied subplan is recursively merged. This is NOT a general
/// patch rule: title is replaced, outgoing connections come from the new edges,
/// and file_surface always replaces the declaration, including explicit [].
/// Use refine_step (or approved control_architect_run brief/model updates) to
/// explicitly clear rules, goal, capture, or a model override. Matching paths
/// retain locks, pins, positions, chat links, and results; these retained results
/// do not make the archived run's checkpoint resumable on a replacement.
///
/// ### What makes a good plan
/// - One step per meaningful unit of work. A step that says "do the task" is
///   useless, and twenty steps for a two-line change is noise.
/// - Give every step an `intent` saying what "done" means for it. That is what
///   the user will argue with, and what you will be held to later.
/// - Put constraints in `rules`, not in the intent. Rules are what must remain
///   true regardless of how the step is carried out.
///
/// ### Existing files: `file_surface`
/// Every step, including parents and nested children, MUST declare existing files
/// it anticipates modifying, renaming, or deleting. Use project-relative paths:
/// {"file_surface":["src/main.rs","README.md"]}. With one open root, paths are
/// relative to that root. With multiple roots, an unprefixed path must identify
/// an existing file in exactly one root; otherwise use "backend/src/main.rs",
/// where backend is an actual open root name, not an absolute host path.
/// Root-prefixed paths are accepted in either case. The saved plan and inspection
/// use root/path identities so relative and prefixed aliases cannot hide overlap.
/// This works the same for local and connected remote projects.
/// Reading a file alone does not require declaring it. Use explicit [] when no
/// existing files will be affected; omission and null are not accepted.
/// Prefer '/' separators; './' and backslashes are normalized. Do not send
/// directories, globs, absolute paths, '..', or duplicates. Keep actual case;
/// comparisons are case-insensitive. A parent's effective surface includes descendants.
/// Steps that may run concurrently must have disjoint effective surfaces. If
/// they need the same existing file, serialize them instead of hiding the overlap.
/// This is a planning declaration, not a write allowlist; it does not prohibit
/// creating new files or otherwise impose tool write restrictions.
/// The native runtime automatically adds observed new files to reachable
/// successors and containing scopes, including nested successors, then validates
/// again before dispatch. Ignored files are excluded from automatic discovery
/// unless always included; explicitly declared ignored files are still checked.
/// Newly exposed conflicts stop further dispatch while retaining completed
/// results. Inspect, correct surfaces or serialize work, review/relock, then
/// resume. This scheduling guard is not filesystem write enforcement.
/// Ambiguous root selection and known directory entries are rejected before saving.
/// Other invalid or overlapping declarations are retained on the canvas with
/// actionable problems, but the plan cannot run until corrected. Use targeted
/// `edit_architect_plan` set_file_surface or connection edits to repair it.
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
/// ### Execution model: `model`
/// A step may override the plan model with `{"provider": "...", "model": "..."}`.
/// Use exact ids from the available models, never display labels or invented ids.
/// Omit `model` to inherit the plan model on new steps and preserve the existing
/// choice on redrafted steps. Change a model before locking the step. During a
/// run, use the main conversation's live model-change tool instead of redrafting.
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
/// - Unconditional fan-out is not an if/else: all available plain branches are
///   taken when no conditional route is selected. Structured independent branches
///   run concurrently in the native runtime and stop before their shared join;
///   the join runs once after all branches finish. Overlapping dependency regions
///   run ready steps serially, waiting for every incoming prerequisite to finish
///   or be explicitly skipped. A node with no selected incoming route is skipped.
///   Unsupported cyclic overlaps are refused; isolate retry loops in a subplan.
/// - The current runner evaluates conditional edges in stored order and takes
///   the first YES; plain edges are the fallback when all answers are NO.
///   Conditions are not proof of mutual exclusion for file-surface validation:
///   never hide conflicting parallel work behind condition labels.
/// - Use `objective` for an externally observable statement, such as whether a
///   command succeeded or a file exists. The current runner asks the model to
///   evaluate that statement from the step summary; it is not executable code.
/// - Use `llm_evaluated` only when the decision genuinely needs judgement, and
///   phrase it as a yes-or-no question. The user will see which parts of their
///   control flow depend on a model's opinion, so do not reach for this to
///   avoid stating a real condition.
/// - Self-loops are supported: from == to repeats the same step. For example,
///   {"from":"test","to":"test","condition":{"kind":"objective","statement":"tests failed"},"max_repeats":3}.
///   Back-edges to earlier steps work too. Use a condition or a positive
///   max_repeats and provide a way out. max_repeats counts edge traversals, not
///   total step visits: 3 permits three retries after the first visit. Drafting
///   clamps zero to one; omit for unlimited. Counts belong to the current local
///   plan invocation, not a lifetime budget across re-entered subplans.
///   A bounded unconditional loop takes priority over plain exits until spent
///   (conditional routes are still checked first). An unconditional unlimited
///   loop with no way out is a validation problem. Run safety limits still apply:
///   200 total steps, 25 visits to one step, and at most 5 levels of nesting.
///
/// ### Replacing an existing plan
/// This call replaces the whole plan. Send the complete set of steps every
/// time, including the ones that are unchanged. Reuse the same full node paths
/// to preserve the user's canvas positions, including in nested plans. Only new
/// steps use the proposed layout; replacing a plan does not rearrange kept steps.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct DraftPlanToolInput {
    /// Complete nonempty replacement node list, not a patch. IDs must be non-blank
    /// and unique within each graph; duplicates are rejected, never renamed.
    /// Nested graphs have separate ID namespaces. Preserve full paths for retained nodes. Every node, including
    /// nested steps.nodes, requires file_surface (explicit [] if none). Array
    /// order does not declare dependencies; use edges for required ordering.
    pub nodes: Vec<ProposedNode>,
    /// Complete replacement connections at this graph level; omission means [].
    /// from/to are local node IDs, not full paths; nested edges belong in steps.
    /// Example: {"from":"build","to":"test"}. Self-loop example:
    /// {"from":"test","to":"test","max_repeats":3}. Omit condition for a
    /// plain route. max_repeats limits edge traversals, not total node visits.
    /// Multiple roots are supported; add edges to express required dependencies.
    #[serde(default)]
    pub edges: Vec<ProposedEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DraftPlanToolOutput {
    Success {
        steps: usize,
        connections: usize,
        /// The draft is saved even with invalid/conflicting surfaces. These
        /// problems block execution until corrected through plan edits (capture
        /// reminders are advisory). Never describe a saved draft as runnable.
        // Defaulted as well as skipped: output saved without the field has
        // to read back when a conversation is reopened.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        problems: Vec<String>,
        /// Steps that kept detail this draft did not carry, because it was
        /// settled after the plan was first drawn. Said out loud so the model
        /// does not assume the plan now reads exactly as it wrote it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

pub(crate) fn resolve_file_surface(
    files: &[String],
    project: &Entity<project::Project>,
    cx: &App,
) -> Result<Vec<String>> {
    let roots: Vec<_> = project
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| !worktree.read(cx).is_single_file())
        .map(|worktree| {
            let snapshot = worktree.read(cx).snapshot();
            (snapshot.root_name_str().to_string(), snapshot)
        })
        .collect();
    files
        .iter()
        .map(|file| {
            let portable = file.replace('\\', "/");
            // Keep malformed declarations visible as graph problems, not silently
            // repair traversal or turn an absolute path into a relative one.
            if portable.starts_with('/')
                || ArchitectGraph::normalize_file_surface_path(&format!("project/{portable}")).is_err()
            {
                return Ok(file.clone());
            }
            let relative = portable
                .split('/')
                .filter(|component| !component.is_empty() && *component != ".")
                .collect::<Vec<_>>()
                .join("/");
            let qualified: Vec<_> = roots
                .iter()
                .filter_map(|(name, snapshot)| {
                    let (prefix, suffix) = relative.split_once('/')?;
                    if name.to_lowercase() == prefix.to_lowercase() {
                        Some((name, snapshot, suffix))
                    } else {
                        None
                    }
                })
                .collect();
            anyhow::ensure!(qualified.len() <= 1, "Project root names are ambiguous. Give the roots distinct names before declaring file_surface.");
            if let Some((name, snapshot, relative)) = qualified.into_iter().next() {
                let path = util::rel_path::RelPath::from_unix_str(relative)?;
                anyhow::ensure!(
                    snapshot.entry_for_path(path).is_none_or(|entry| !entry.is_dir()),
                    "file_surface entry {file:?} is a directory. List the existing files individually, not the containing folder."
                );
                return Ok(format!("{name}/{relative}"));
            }
            if let [(name, snapshot)] = roots.as_slice() {
                let path = util::rel_path::RelPath::from_unix_str(&relative)?;
                anyhow::ensure!(
                    snapshot.entry_for_path(path).is_none_or(|entry| !entry.is_dir()),
                    "file_surface entry {file:?} is a directory. List the existing files individually, not the containing folder."
                );
                return Ok(format!("{name}/{relative}"));
            }
            let path = util::rel_path::RelPath::from_unix_str(&relative)?;
            let matching: Vec<_> = roots
                .iter()
                .filter(|(_, snapshot)| snapshot.entry_for_path(path).is_some_and(|entry| !entry.is_dir()))
                .map(|(name, _)| format!("{name}/{relative}"))
                .collect();
            anyhow::ensure!(matching.len() == 1,
                "Cannot resolve file_surface entry {file:?} to one project root. Use root/path, for example {}/{}. Available roots: {}. Paths are relative to the project, never the host filesystem.",
                roots.first().map(|(name, _)| name.as_str()).unwrap_or("project"), relative,
                roots.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", "));
            matching.into_iter().next().ok_or_else(|| anyhow::anyhow!("No project root contains {file:?}"))
        })
        .collect()
}

pub(crate) fn resolve_graph_file_surfaces(
    graph: &mut ArchitectGraph,
    project: &Entity<project::Project>,
    cx: &App,
) -> Result<()> {
    for node in &mut graph.nodes {
        resolve_node_file_surfaces(node, project, cx)?;
    }
    Ok(())
}

pub(crate) fn resolve_node_file_surfaces(
    node: &mut architect::ArchitectNode,
    project: &Entity<project::Project>,
    cx: &App,
) -> Result<()> {
    if let Some(surface) = &mut node.file_surface {
        *surface = resolve_file_surface(surface, project, cx)?;
    }
    if let Some(subplan) = node.subplan.as_deref_mut() {
        resolve_graph_file_surfaces(subplan, project, cx)?;
    }
    Ok(())
}

impl DraftPlanTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

pub(super) fn validate_step_model(model: &StepModel, cx: &App) -> Result<()> {
    anyhow::ensure!(
        LanguageModelRegistry::read_global(cx)
            .available_models(cx)
            .any(|available| {
                available.provider_id().0.as_ref() == model.provider.as_str()
                    && available.id().0.as_ref() == model.model.as_str()
            }),
        "The step model {}/{} is unavailable. Choose an available model in the step inspector or inherit the plan model.",
        model.provider,
        model.model,
    );
    Ok(())
}

pub(super) fn plan_validation_problems(graph: &architect::ArchitectGraph) -> Vec<String> {
    let mut problems: Vec<_> = graph.problems().iter().map(ToString::to_string).collect();
    for node in &graph.nodes {
        if let Some(subplan) = node.subplan() {
            problems.extend(
                plan_validation_problems(subplan)
                    .into_iter()
                    .map(|problem| format!("inside {}: {problem}", node.id)),
            );
        }
    }
    problems
}

fn validate_proposed_ids(nodes: &[ProposedNode]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for node in nodes {
        anyhow::ensure!(
            !node.id.0.trim().is_empty(),
            "Every step needs a non-blank stable id."
        );
        anyhow::ensure!(
            ids.insert(&node.id),
            "Step id {:?} is duplicated. Use a unique id for each step in the same graph; IDs are never silently renamed.",
            node.id.0
        );
        if let Some(subplan) = &node.steps {
            validate_proposed_ids(&subplan.nodes)
                .with_context(|| format!("Inside step {:?}", node.id.0))?;
        }
    }
    Ok(())
}

fn validate_proposed_models(nodes: &[ProposedNode], cx: &App) -> Result<()> {
    for node in nodes {
        if let Some(model) = &node.model {
            validate_step_model(model, cx)?;
        }
        if let Some(subplan) = &node.steps {
            validate_proposed_models(&subplan.nodes, cx)?;
        }
    }
    Ok(())
}

impl AgentTool for DraftPlanTool {
    type Input = DraftPlanToolInput;
    type Output = DraftPlanToolOutput;

    const NAME: &'static str = "draft_plan";

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
            validate_proposed_ids(&input.nodes).map_err(|error| DraftPlanToolOutput::Error {
                error: format!("{error:#}"),
            })?;

            self.thread
                .read_with(cx, |thread, cx| {
                    anyhow::ensure!(
                        !thread
                            .architect_run()
                            .is_some_and(|run| run.is_running() || run.can_resume()),
                        "This plan has an active or resumable run. Use control_architect_run for model changes. For file-surface or routing changes, stop execution and preview edit_architect_plan in the main conversation instead of redrafting."
                    );
                    validate_proposed_models(&input.nodes, cx)
                })
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: format!("The thread this plan belongs to is gone: {error}"),
                })?
                .map_err(|error| DraftPlanToolOutput::Error {
                    error: error.to_string(),
                })?;

            let mut draft = ProposedGraph {
                nodes: input.nodes,
                edges: input.edges,
            }
            .into_graph();
            self.thread
                .read_with(cx, |thread, cx| {
                    resolve_graph_file_surfaces(&mut draft, thread.project(), cx)
                })
                .map_err(|error| DraftPlanToolOutput::Error { error: error.to_string() })?
                .map_err(|error| DraftPlanToolOutput::Error { error: error.to_string() })?;

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
                             locked step is settled — its goal, rules, capture, file surface and where it leads \
                             were argued out, often in its own chat. Draw the plan again, \
                             restating those steps and their connections exactly as they are, and \
                             change only what is not locked. If one of them really does have to \
                             change, say so and ask the user. Once they agree, unlock it with \
                             `set_step_locks` and draw the plan again.",
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
            let mut problems = plan_validation_problems(&graph);
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
                    // planned on the canvas rather than carried out, so it is
                    // what puts the thread into Architect. Nothing else has to
                    // be asked or set.
                    thread.set_session_mode(crate::SessionMode::Architect, cx);
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

    #[gpui::test]
    async fn invalid_proposed_ids_leave_the_existing_plan_untouched(cx: &mut gpui::TestAppContext) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let mut before = ArchitectGraph::default();
        before.add_node(architect::ArchitectNode::new("keep", "Keep"));
        thread.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(before.clone()), cx);
            thread.set_session_mode(crate::SessionMode::Architect, cx);
        });
        let revision = thread.read_with(cx, |thread, _| thread.architect_revision());
        for nodes in [
            json!([{"id":"", "title":"Blank", "file_surface":[]}]),
            json!([{"id":" \t", "title":"Blank", "file_surface":[]}]),
            json!([
                {"id":"same", "title":"First", "file_surface":[]},
                {"id":"same", "title":"Second", "file_surface":[]}
            ]),
            json!([{"id":"parent", "title":"Parent", "file_surface":[], "steps":{"nodes":[
                {"id":"same", "title":"First", "file_surface":[]},
                {"id":"same", "title":"Second", "file_surface":[]}
            ]}}]),
        ] {
            let (events, _receiver) = ToolCallEventStream::test();
            let input = ToolInput::ready(json!({"nodes": nodes}));
            let error = cx
                .update(|cx| {
                    Arc::new(DraftPlanTool::new(thread.downgrade())).run(input, events, cx)
                })
                .await
                .expect_err("invalid IDs must not rewrite the plan");
            assert!(matches!(error, DraftPlanToolOutput::Error { .. }));
            thread.read_with(cx, |thread, _| {
                assert_eq!(thread.architect_graph(), Some(&before));
                assert_eq!(thread.architect_revision(), revision);
                assert_eq!(thread.session_mode(), crate::SessionMode::Architect);
            });
        }
    }

    #[test]
    fn nested_graphs_have_separate_proposed_id_namespaces() {
        let proposal: ProposedGraph = serde_json::from_value(json!({"nodes": [
            {"id":"left", "title":"Left", "file_surface":[], "steps":{"nodes":[
                {"id":"step", "title":"Step", "file_surface":[]}
            ]}},
            {"id":"right", "title":"Right", "file_surface":[], "steps":{"nodes":[
                {"id":"step", "title":"Step", "file_surface":[]}
            ]}}
        ]}))
        .unwrap();
        validate_proposed_ids(&proposal.nodes).expect("IDs are local to each graph");
    }

    #[gpui::test]
    async fn file_surface_paths_resolve_against_project_roots(cx: &mut gpui::TestAppContext) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let project = thread.read_with(cx, |thread, _| thread.project().clone());
        cx.update(|cx| {
            for input in [
                "src/main.rs",
                "./src/main.rs",
                "src\\main.rs",
                "a/src/main.rs",
            ] {
                assert_eq!(
                    resolve_file_surface(&[input.into()], &project, cx).expect("relative path"),
                    vec!["a/src/main.rs"]
                );
            }
            assert_eq!(
                resolve_file_surface(&["README.md".into()], &project, cx).expect("root file"),
                vec!["a/README.md"]
            );
            for invalid in ["../outside.rs", "/a/main.rs", "C:/project/main.rs"] {
                assert_eq!(
                    resolve_file_surface(&[invalid.into()], &project, cx)
                        .expect("retain invalid declaration"),
                    vec![invalid]
                );
            }
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/",
            json!({
                "backend": {"src": {"main.rs": "", "server.rs": ""}},
                "frontend": {"src": {"main.rs": ""}}
            }),
        )
        .await;
        let project = project::Project::test(
            fs,
            [
                std::path::Path::new("/backend"),
                std::path::Path::new("/frontend"),
            ],
            cx,
        )
        .await;
        cx.update(|cx| {
            assert_eq!(
                resolve_file_surface(&["src/server.rs".into()], &project, cx).expect("unique file"),
                vec!["backend/src/server.rs"]
            );
            assert_eq!(
                resolve_file_surface(&["frontend/src/main.rs".into()], &project, cx)
                    .expect("qualified file"),
                vec!["frontend/src/main.rs"]
            );
            let error = resolve_file_surface(&["src/main.rs".into()], &project, cx)
                .expect_err("ambiguous roots");
            assert!(error.to_string().contains("Use root/path"));
            assert!(resolve_file_surface(&["missing.rs".into()], &project, cx).is_err());
            let error = resolve_file_surface(&["backend/src".into()], &project, cx)
                .expect_err("directories cannot stand in for files");
            assert!(error.to_string().contains("is a directory"));
        });
    }

    #[gpui::test]
    async fn relative_surfaces_use_remote_worktree_paths(
        cx: &mut gpui::TestAppContext,
        host_cx: &mut gpui::TestAppContext,
    ) {
        let (_connection, _agent, _thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let client_fs = fs::FakeFs::new(cx.executor());
        client_fs.insert_tree("/", json!({"client-only": {}})).await;
        let host_fs = fs::FakeFs::new(host_cx.executor());
        host_fs
            .insert_tree("/", json!({"remote": {"src": {"main.rs": ""}}}))
            .await;
        let (project, _host) = project::Project::test_remote_worktrees(
            client_fs,
            host_fs,
            [std::path::Path::new("/remote")],
            cx,
            host_cx,
        )
        .await;
        cx.update(|cx| {
            assert_eq!(
                resolve_file_surface(&["src/main.rs".into()], &project, cx)
                    .expect("remote-relative path"),
                vec!["remote/src/main.rs"]
            );
            for directory in ["src", "remote/src"] {
                let error = resolve_file_surface(&[directory.into()], &project, cx)
                    .expect_err("remote directory is not a file surface");
                assert!(error.to_string().contains("is a directory"));
            }
        });
    }

    #[gpui::test]
    async fn relative_draft_surfaces_are_stored_qualified_and_cannot_hide_conflicts(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let (events, _receiver) = ToolCallEventStream::test();
        let input = ToolInput::ready(json!({"nodes": [
            {"id": "left", "title": "Left", "file_surface": ["src/main.rs"]},
            {"id": "right", "title": "Right", "file_surface": ["a/src/main.rs"]}
        ]}));
        let output = cx
            .update(|cx| Arc::new(DraftPlanTool::new(thread.downgrade())).run(input, events, cx))
            .await
            .expect("retain conflicting draft");
        let DraftPlanToolOutput::Success { problems, .. } = output else {
            panic!("expected draft");
        };
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("may run concurrently"))
        );
        thread.read_with(cx, |thread, _| {
            for node in &thread.architect_graph().expect("graph").nodes {
                assert_eq!(node.file_surface, Some(vec!["a/src/main.rs".into()]));
            }
        });
    }

    #[test]
    fn model_description_and_schema_keep_drafting_contracts() {
        let description = <DraftPlanTool as AgentTool>::description();
        let guidance = description.split_whitespace().collect::<Vec<_>>().join(" ");
        for required in [
            "edit_architect_plan",
            "refine_step",
            "control_architect_run",
            "archives the previous run",
            "not a resumable update",
            "empty rules",
            "file_surface always replaces",
            "project-relative paths",
            "README.md",
            "multiple roots",
            "connected remote projects",
            "Self-loops are supported",
            "from == to",
            "counts edge traversals",
            "first YES",
            "not an if/else",
            "every incoming prerequisite",
            "reachable successors",
            "Ignored files",
            "explicitly declared ignored files",
            "not filesystem write enforcement",
        ] {
            assert!(
                guidance.contains(required),
                "missing model guidance: {required}"
            );
        }
        let mut schema = DraftPlanTool::input_schema().to_value();
        language_model::tool_schema::normalize_tool_schema(&mut schema);
        for (field, required) in [
            ("nodes", "Complete nonempty replacement"),
            ("nodes", "requires file_surface"),
            ("edges", "local node IDs"),
            ("edges", "Self-loop"),
            ("edges", "edge traversals"),
        ] {
            let description = schema["properties"][field]["description"]
                .as_str()
                .expect(field)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(description.contains(required), "{field}: {required}");
        }
        let example = description
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("{\"from\""))
            .expect("self-loop JSON example");
        let value = serde_json::Deserializer::from_str(example)
            .into_iter::<serde_json::Value>()
            .next()
            .expect("example")
            .expect("valid JSON prefix");
        let edge: ProposedEdge = serde_json::from_value(value).expect("valid proposed edge");
        assert_eq!(edge.from, edge.to);
        assert_eq!(edge.max_repeats, Some(3));
    }

    #[test]
    fn validation_reports_nested_surfaces_with_their_own_paths() {
        let graph: ProposedGraph = serde_json::from_value(json!({"nodes": [{
            "id": "parent", "title": "Parent", "file_surface": [],
            "steps": {"nodes": [{
                "id": "child", "title": "Child", "file_surface": ["../outside.rs"]
            }]}
        }]}))
        .expect("draft");
        let problems = plan_validation_problems(&graph.into_graph());
        assert!(
            problems.iter().any(|problem| {
                problem.contains("inside parent: child") && problem.contains("invalid file surface")
            }),
            "{problems:?}"
        );
    }

    #[test]
    fn every_drafted_node_requires_an_explicit_file_surface() {
        for node in [
            json!({"id": "step", "title": "Step"}),
            json!({"id": "step", "title": "Step", "file_surface": null}),
            json!({"id": "parent", "title": "Parent", "file_surface": [],
                "steps": {"nodes": [{"id": "child", "title": "Child"}]}}),
        ] {
            assert!(
                serde_json::from_value::<DraftPlanToolInput>(json!({"nodes": [node]})).is_err()
            );
        }
        let schema =
            serde_json::to_value(schemars::schema_for!(DraftPlanToolInput)).expect("schema");
        assert!(
            schema["$defs"]["ProposedNode"]["required"]
                .as_array()
                .expect("required node fields")
                .contains(&json!("file_surface"))
        );
    }

    #[gpui::test]
    async fn invalid_and_conflicting_drafts_stay_visible_until_repaired(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        for (surface, expected_problem) in [
            (json!(["../outside.rs"]), "invalid file surface"),
            (json!(["a/shared.rs"]), "may run concurrently"),
        ] {
            let input = ToolInput::ready(json!({
                "nodes": [
                    {"id": "start", "title": "Start", "file_surface": []},
                    {"id": "left", "title": "Left", "file_surface": surface},
                    {"id": "right", "title": "Right", "file_surface": ["a/shared.rs"]},
                    {"id": "join", "title": "Join", "file_surface": []}
                ],
                "edges": [
                    {"from": "start", "to": "left"}, {"from": "start", "to": "right"},
                    {"from": "left", "to": "join"}, {"from": "right", "to": "join"}
                ]
            }));
            let (events, _receiver) = ToolCallEventStream::test();
            let output = cx
                .update(|cx| {
                    Arc::new(DraftPlanTool::new(thread.downgrade())).run(input, events, cx)
                })
                .await
                .expect("invalid drafts should still be saved");
            let DraftPlanToolOutput::Success { problems, .. } = output else {
                panic!("expected a saved draft");
            };
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.contains(expected_problem)),
                "{problems:?}"
            );
            thread.read_with(cx, |thread, _| {
                assert_eq!(thread.session_mode(), crate::SessionMode::Architect);
                let mut graph = thread.architect_graph().expect("retained draft").clone();
                assert_eq!(
                    json!(graph.node(&"left".into()).expect("left").file_surface),
                    surface
                );
                graph.lock_all();
                assert!(
                    architect::compile_spec(&graph).is_err(),
                    "locking must not bypass surface validation"
                );
                let repaired = architect::preview_graph_edits(
                    &graph,
                    &[architect::GraphEdit::SetFileSurface {
                        path: architect::NodePath::root("left".into()),
                        file_surface: vec!["a/left.rs".into()],
                    }],
                )
                .expect("surface repair");
                assert!(repaired.is_valid);
                assert!(!repaired.ready_to_run, "repair needs lock review");
                let mut repaired = repaired.graph;
                repaired.lock_all();
                assert!(architect::compile_spec(&repaired).is_ok());
            });
        }
    }

    #[test]
    fn whole_replacement_preserves_positions_only_for_matching_full_paths() {
        let proposal = json!({
            "nodes": [
                {"id": "left", "title": "Left", "file_surface": [], "steps": {
                    "nodes": [{"id": "shared", "title": "Shared", "file_surface": []}]
                }},
                {"id": "right", "title": "Right", "file_surface": [], "steps": {
                    "nodes": [{"id": "shared", "title": "Shared", "file_surface": []}]
                }}
            ]
        });
        let mut existing = serde_json::from_value::<ProposedGraph>(proposal.clone())
            .expect("existing proposal")
            .into_graph();
        let left_path = architect::NodePath(vec!["left".into(), "shared".into()]);
        let right_path = architect::NodePath(vec!["right".into(), "shared".into()]);
        existing
            .move_node_at(&left_path, architect::Position { x: -301.5, y: 77.0 })
            .expect("left position");
        existing
            .move_node_at(&right_path, architect::Position { x: 925.0, y: -41.5 })
            .expect("right position");
        existing.node_mut(&"left".into()).expect("left").position = None;
        let snapshot = existing.clone();
        let mut proposal = proposal;
        proposal["nodes"][1]["steps"]["nodes"]
            .as_array_mut()
            .expect("right children")
            .push(json!({"id": "new", "title": "New", "file_surface": []}));
        let draft = serde_json::from_value::<ProposedGraph>(proposal)
            .expect("replacement proposal")
            .into_graph();
        let new_path = architect::NodePath(vec!["right".into(), "new".into()]);
        let new_position = draft.node_at(&new_path).expect("new node").position;
        let merged = existing.merge_draft(draft).expect("merge replacement");
        assert_eq!(existing, snapshot);
        for path in [
            architect::NodePath::root("left".into()),
            architect::NodePath::root("right".into()),
            left_path,
            right_path,
        ] {
            assert_eq!(
                merged.graph.node_at(&path).expect("retained node").position,
                existing.node_at(&path).expect("existing node").position,
            );
        }
        assert_eq!(
            merged.graph.node_at(&new_path).expect("new node").position,
            new_position,
        );
        assert!(new_position.is_some());
    }

    #[test]
    fn draft_schema_and_nested_input_include_execution_models() {
        let schema = serde_json::to_value(schemars::schema_for!(DraftPlanToolInput))
            .expect("schema should serialize");
        let schema = schema.to_string();
        assert!(schema.contains("StepModel"));
        assert!(schema.contains("provider"));

        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [{
                "id": "parent", "title": "Parent", "file_surface": [],
                "model": {"provider": "test-provider", "model": "parent-model"},
                "steps": {"nodes": [{
                    "id": "child", "title": "Child", "file_surface": [],
                    "model": {"provider": "test-provider", "model": "child-model"}
                }]}
            }, {"id": "inherited", "title": "Inherited", "file_surface": []}]
        }))
        .expect("model-bearing drafts should deserialize");
        let graph = ProposedGraph {
            nodes: input.nodes,
            edges: input.edges,
        }
        .into_graph();
        let parent = graph.node(&"parent".into()).expect("parent should exist");
        assert_eq!(
            parent
                .model
                .as_ref()
                .expect("parent model should exist")
                .model,
            "parent-model"
        );
        let child = parent
            .subplan()
            .expect("nested plan should exist")
            .node(&"child".into())
            .expect("child should exist");
        assert_eq!(
            child
                .model
                .as_ref()
                .expect("child model should exist")
                .model,
            "child-model"
        );
        assert!(
            graph
                .node(&"inherited".into())
                .expect("step should exist")
                .model
                .is_none()
        );
    }

    #[gpui::test]
    fn model_validation_uses_registry_ids_not_display_names(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let provider = LanguageModelRegistry::test(cx);
            let model = provider.model("execution-model");
            let selected = StepModel {
                provider: model.provider_id().0.to_string(),
                model: model.id().0.to_string(),
            };
            assert!(validate_step_model(&selected, cx).is_ok());
            let display_names = StepModel {
                provider: "Fake".into(),
                model: "Missing Model".into(),
            };
            assert!(validate_step_model(&display_names, cx).is_err());
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.unregister_provider(model.provider_id(), cx);
            });
            assert!(validate_step_model(&selected, cx).is_err());
        });
    }

    #[test]
    fn saved_output_without_optional_lists_replays() {
        let saved = serde_json::to_value(DraftPlanToolOutput::Success {
            steps: 2,
            connections: 1,
            problems: Vec::new(),
            kept_existing_detail: Vec::new(),
        })
        .unwrap();
        assert_eq!(saved, json!({ "steps": 2, "connections": 1 }));
        assert!(matches!(
            serde_json::from_value::<DraftPlanToolOutput>(saved).unwrap(),
            DraftPlanToolOutput::Success { steps: 2, .. }
        ));
    }

    #[test]
    fn a_plan_the_model_might_send_deserializes() {
        let input: DraftPlanToolInput = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "reproduce",
                    "title": "Reproduce the failure",
                    "file_surface": [],
                    "intent": "Get a failing test that shows the bug",
                    "rules": ["Do not change behaviour yet"],
                    "capture": "The test file and the assertion that failed",
                },
                { "id": "fix", "title": "Fix it", "file_surface": [], "intent": "Make the test pass" },
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
                    "file_surface": [],
                    "intent": "Endpoints behave per the schema",
                    "capture": "Which endpoints you added and their status codes",
                    "steps": {
                        "nodes": [
                            { "id": "parse", "title": "Parse the body", "file_surface": [] },
                            { "id": "respond", "title": "Respond", "file_surface": [] },
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
                { "id": "first", "title": "First", "file_surface": [] },
                { "id": "second", "title": "Second", "file_surface": [] },
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
            "nodes": [{ "id": "only", "title": "Only step", "file_surface": [] }],
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
                    "file_surface": [],
                    "capture": "Every call site of the old API",
                },
                {
                    "id": "migrate",
                    "title": "Migrate the callers",
                    "file_surface": [],
                    "steps": {
                        "nodes": [
                            { "id": "rewrite", "title": "Rewrite them", "file_surface": [], "capture": "Files touched" },
                            { "id": "compile", "title": "Compile", "file_surface": [] },
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
                    "file_surface": [],
                    "capture": "The before and after timings, in milliseconds",
                },
                { "id": "report", "title": "Report", "file_surface": [] },
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
                { "id": "build", "title": "Build", "file_surface": [] },
                { "id": "ship", "title": "Ship", "file_surface": [] },
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
