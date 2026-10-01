use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::{ArchitectGraph, GraphMutationError, NodeId, NodePath};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolCapability, ToolInput};

const RUN_IN_PROGRESS: &str = "A run of this plan is in progress or paused, and locks cannot \
     change under it. Nothing was changed. Ask the user to stop the run first.";

const NO_PLAN: &str = "There is no plan on the canvas yet, so there is nothing to lock or unlock.";

/// Lock or unlock steps of the plan on the Architect canvas.
///
/// A locked step is settled: `draft_plan` has to restate it exactly, and it
/// cannot be edited on the canvas until it is unlocked. Locking is the user's
/// signal that deliberation over a step is over, not yours. Only lock or unlock
/// when the user has asked for it in this conversation. Never lock a step
/// because you think it is finished, and never unlock one just so you can
/// redraw it: if a locked step has to change, say why and ask the user.
///
/// Use it when the user says things like "lock step X", "lock everything",
/// "unlock the tests step", or "reopen X so we can change it".
///
/// ### Naming steps
/// Name each step by its id from the plan shown to you. A bare id is found at
/// any depth. When the same id appears in more than one nested plan, name the
/// one you mean by its full path from the top-level plan, with the ids joined
/// by `/`, such as `handlers/parse`.
///
/// ### What locking and unlocking do
/// - Locking a step that contains a plan locks everything inside it too.
/// - Unlocking reopens only the step named, not the steps inside it.
/// - A step inside a locked step cannot be unlocked on its own. When the user
///   wants to reopen something inside a locked step, unlock the containing
///   step as well, in the same call.
/// - Set `all` to lock or unlock every step of the plan, at every depth.
///
/// Nothing changes if any step named cannot be changed. Locks cannot change
/// while a run of the plan is in progress or paused.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SetStepLocksToolInput {
    /// Whether to lock the steps (`true`) or unlock them (`false`).
    pub locked: bool,
    /// The steps to change, each by its id, or by its full path with the ids
    /// joined by `/`. Leave empty when `all` is set.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Change every step of the plan, at every depth, instead of the steps
    /// named.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SetStepLocksToolOutput {
    Success {
        locked: bool,
        /// Every step whose lock changed, including the steps inside a step
        /// that was locked.
        changed: Vec<String>,
        /// Steps named that were already locked or unlocked as asked.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unchanged: Vec<String>,
        /// How many steps of the whole plan, at every depth, are now locked.
        locked_steps: usize,
        total_steps: usize,
    },
    Error {
        error: String,
    },
}

impl From<SetStepLocksToolOutput> for LanguageModelToolResultContent {
    fn from(output: SetStepLocksToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|error| format!("Failed to serialize set_step_locks output: {error}"))
            .into()
    }
}

pub struct SetStepLocksTool {
    thread: WeakEntity<Thread>,
}

impl SetStepLocksTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

/// What a successful change did to the plan.
#[derive(Debug, PartialEq, Eq)]
struct LockChange {
    changed: Vec<String>,
    unchanged: Vec<String>,
    locked_steps: usize,
    total_steps: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum LockRefusal {
    NothingNamed,
    UnknownSteps(Vec<String>),
    /// Each name that matched several steps, with the full paths it matched.
    AmbiguousSteps(Vec<(String, Vec<String>)>),
    InsideLockedStep {
        step: String,
        container: String,
        container_path: String,
    },
    Graph(GraphMutationError),
}

impl fmt::Display for LockRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockRefusal::NothingNamed => formatter.write_str(
                "No steps were named. List them in `steps`, or set `all` to change every step.",
            ),
            LockRefusal::UnknownSteps(names) => write!(
                formatter,
                "No step has the id or path {}. Nothing was changed. Use the ids from the plan \
                 shown to you; a nested step can also be named by its full path, such as \
                 `outer/inner`.",
                quoted_list(names),
            ),
            LockRefusal::AmbiguousSteps(names) => {
                for (name, paths) in names {
                    write!(
                        formatter,
                        "`{name}` names more than one step: {}. ",
                        quoted_list(paths)
                    )?;
                }
                formatter.write_str("Nothing was changed. Name the one you mean by its full path.")
            }
            LockRefusal::InsideLockedStep {
                step,
                container,
                container_path,
            } => write!(
                formatter,
                "{step} sits inside {container}, which is locked, so it cannot be unlocked on \
                 its own. Nothing was changed. If the user wants to reopen it, unlock \
                 `{container_path}` as well, in the same call."
            ),
            LockRefusal::Graph(error) => write!(formatter, "Nothing was changed: {error}."),
        }
    }
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A path written the way the model is asked to write one, with `/` between
/// the ids.
fn slash_path(path: &NodePath) -> String {
    path.iter()
        .map(|id| id.0.as_str())
        .collect::<Vec<_>>()
        .join("/")
}

fn locked_at(graph: &ArchitectGraph, path: &NodePath) -> bool {
    graph.node_at(path).is_some_and(|node| node.locked)
}

fn describe_step(graph: &ArchitectGraph, path: &NodePath) -> String {
    let title = graph.node_at(path).map_or("", |node| node.title.as_str());
    format!("\"{title}\" ({})", slash_path(path))
}

/// Every step of the plan at every depth, each containing step before the
/// steps inside it.
fn step_paths(graph: &ArchitectGraph) -> Vec<NodePath> {
    fn collect(graph: &ArchitectGraph, prefix: &NodePath, paths: &mut Vec<NodePath>) {
        for node in &graph.nodes {
            let path = prefix.child(node.id.clone());
            if let Some(subplan) = node.subplan() {
                paths.push(path.clone());
                collect(subplan, &path, paths);
            } else {
                paths.push(path);
            }
        }
    }

    let mut paths = Vec::new();
    collect(graph, &NodePath::default(), &mut paths);
    paths
}

#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    Found(NodePath),
    Unknown,
    Ambiguous(Vec<NodePath>),
}

fn resolve_step(graph: &ArchitectGraph, every_step: &[NodePath], name: &str) -> Resolution {
    let name = name.trim();
    if name.contains('/') {
        let path = NodePath(
            name.split('/')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(NodeId::from)
                .collect(),
        );
        if graph.node_at(&path).is_some() {
            return Resolution::Found(path);
        }
    }

    let matches: Vec<&NodePath> = every_step
        .iter()
        .filter(|path| path.leaf().is_some_and(|leaf| leaf.0 == name))
        .collect();
    match matches.as_slice() {
        [] => Resolution::Unknown,
        [only] => Resolution::Found((*only).clone()),
        several => Resolution::Ambiguous(several.iter().copied().cloned().collect()),
    }
}

/// Locks or unlocks the steps asked for, the way the canvas does: locking
/// settles a step together with everything inside it, and unlocking reopens
/// only the step itself. The plan is written only if every step could be
/// changed.
fn apply_step_locks(
    graph: &mut ArchitectGraph,
    input: &SetStepLocksToolInput,
) -> Result<LockChange, LockRefusal> {
    let mut revised = graph.clone();
    let mut named = Vec::new();

    if input.all {
        if input.locked {
            revised.lock_all();
        } else {
            revised.unlock_all();
        }
    } else {
        if input.steps.is_empty() {
            return Err(LockRefusal::NothingNamed);
        }
        let every_step = step_paths(graph);
        let mut unknown = Vec::new();
        let mut ambiguous = Vec::new();
        for name in &input.steps {
            match resolve_step(graph, &every_step, name) {
                Resolution::Found(path) => {
                    if !named.contains(&path) {
                        named.push(path);
                    }
                }
                Resolution::Unknown => unknown.push(name.trim().to_string()),
                Resolution::Ambiguous(paths) => {
                    let paths: Vec<String> = paths.iter().map(slash_path).collect();
                    ambiguous.push((name.trim().to_string(), paths));
                }
            }
        }
        if !unknown.is_empty() {
            return Err(LockRefusal::UnknownSteps(unknown));
        }
        if !ambiguous.is_empty() {
            return Err(LockRefusal::AmbiguousSteps(ambiguous));
        }

        // Containing steps go first, so reopening a step together with
        // something inside it does not trip over the container's lock.
        named.sort_by_key(NodePath::depth);
        for path in &named {
            let Some(node) = revised.node_at(path) else {
                return Err(LockRefusal::Graph(GraphMutationError::NodeNotFound {
                    path: path.clone(),
                }));
            };
            if input.locked {
                // A step inside a locked step is already locked, and locking
                // it again would have to reach through its container.
                let settled = node.locked
                    && node
                        .subplan()
                        .is_none_or(ArchitectGraph::is_fully_locked_deeply);
                if !settled {
                    revised.lock_deeply_at(path).map_err(LockRefusal::Graph)?;
                }
            } else if node.locked {
                revised
                    .set_locked_at(path, false)
                    .map_err(|error| match error {
                        GraphMutationError::Locked { path: container } => {
                            LockRefusal::InsideLockedStep {
                                step: describe_step(graph, path),
                                container: describe_step(graph, &container),
                                container_path: slash_path(&container),
                            }
                        }
                        error => LockRefusal::Graph(error),
                    })?;
            }
        }
    }

    let changed = step_paths(graph)
        .iter()
        .filter(|path| locked_at(graph, path) != locked_at(&revised, path))
        .map(|path| describe_step(graph, path))
        .collect();
    let unchanged = named
        .iter()
        .filter(|path| locked_at(graph, path) == locked_at(&revised, path))
        .map(|path| describe_step(graph, path))
        .collect();
    let change = LockChange {
        changed,
        unchanged,
        locked_steps: revised.locked_step_count_deeply(),
        total_steps: revised.step_count_deeply(),
    };
    *graph = revised;
    Ok(change)
}

impl AgentTool for SetStepLocksTool {
    type Input = SetStepLocksToolInput;
    type Output = SetStepLocksToolOutput;

    const NAME: &'static str = "set_step_locks";

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
        let Ok(input) = input else {
            return "Change which steps are locked".into();
        };
        let verb = if input.locked { "Lock" } else { "Unlock" };
        if input.all {
            return format!("{verb} every step").into();
        }
        match input.steps.as_slice() {
            [] => format!("{verb} steps").into(),
            // Initial titles are built while the owning Thread is updating,
            // including during replay. Reading its graph here re-enters it.
            [only] => format!("{verb} “{}”", only.trim()).into(),
            steps => format!("{verb} {} steps", steps.len()).into(),
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
                .map_err(|error| SetStepLocksToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;

            let outcome = self
                .thread
                .update(cx, |thread, cx| {
                    // A paused run is still running, and the canvas refuses
                    // lock changes under a run too, so the two agree.
                    if let Some(run) = thread.architect_run()
                        && run.is_running()
                    {
                        return Err(RUN_IN_PROGRESS.to_string());
                    }
                    let Some(mut graph) = thread
                        .architect_graph()
                        .filter(|graph| !graph.is_empty())
                        .cloned()
                    else {
                        return Err(NO_PLAN.to_string());
                    };
                    let change =
                        apply_step_locks(&mut graph, &input).map_err(|error| error.to_string())?;
                    // A request that changes nothing leaves the thread alone,
                    // rather than marking it edited.
                    if !change.changed.is_empty() {
                        thread.update_architect_graph(|plan| *plan = graph, cx);
                    }
                    Ok(change)
                })
                .map_err(|error| SetStepLocksToolOutput::Error {
                    error: format!("The thread this plan belongs to is gone: {error}"),
                })?;

            let change = match outcome {
                Ok(change) => change,
                Err(error) => return Err(SetStepLocksToolOutput::Error { error }),
            };
            Ok(SetStepLocksToolOutput::Success {
                locked: input.locked,
                changed: change.changed,
                unchanged: change.unchanged,
                locked_steps: change.locked_steps,
                total_steps: change.total_steps,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::ArchitectNode;
    use serde_json::json;

    /// `handlers` holds `parse` and `check`, and `tests` holds another
    /// `check`, so `check` alone is ambiguous.
    fn nested_plan() -> ArchitectGraph {
        let mut handlers_plan = ArchitectGraph::default();
        handlers_plan.add_node(ArchitectNode::new("parse", "Parse the body"));
        handlers_plan.add_node(ArchitectNode::new("check", "Check the input"));
        let mut handlers = ArchitectNode::new("handlers", "Write handlers");
        handlers.subplan = Some(Box::new(handlers_plan));

        let mut tests_plan = ArchitectGraph::default();
        tests_plan.add_node(ArchitectNode::new("check", "Check the output"));
        let mut tests = ArchitectNode::new("tests", "Write tests");
        tests.subplan = Some(Box::new(tests_plan));

        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("schema", "Define schema"));
        graph.add_node(handlers);
        graph.add_node(tests);
        graph
    }

    fn path(ids: &[&str]) -> NodePath {
        NodePath(ids.iter().copied().map(NodeId::from).collect())
    }

    fn is_locked(graph: &ArchitectGraph, ids: &[&str]) -> bool {
        graph.node_at(&path(ids)).unwrap().locked
    }

    fn request(locked: bool, steps: &[&str]) -> SetStepLocksToolInput {
        SetStepLocksToolInput {
            locked,
            steps: steps.iter().map(|step| step.to_string()).collect(),
            all: false,
        }
    }

    #[test]
    fn the_input_a_model_would_send_deserializes() {
        let input: SetStepLocksToolInput = serde_json::from_value(json!({
            "locked": false,
            "steps": ["handlers", "handlers/parse"],
        }))
        .unwrap();
        assert!(!input.locked);
        assert_eq!(input.steps.len(), 2);
        assert!(!input.all, "`all` must default to off");

        let input: SetStepLocksToolInput = serde_json::from_value(json!({
            "locked": true,
            "all": true,
        }))
        .unwrap();
        assert!(input.all);
        assert!(input.steps.is_empty());
    }

    #[test]
    fn a_bare_id_is_found_at_any_depth_and_a_path_names_one_step() {
        let graph = nested_plan();
        let every_step = step_paths(&graph);

        assert_eq!(
            resolve_step(&graph, &every_step, "schema"),
            Resolution::Found(path(&["schema"]))
        );
        assert_eq!(
            resolve_step(&graph, &every_step, "parse"),
            Resolution::Found(path(&["handlers", "parse"])),
            "a bare id reaches into nested plans"
        );
        assert_eq!(
            resolve_step(&graph, &every_step, " tests / check "),
            Resolution::Found(path(&["tests", "check"]))
        );
        let handlers_check = path(&["handlers", "check"]);
        let tests_check = path(&["tests", "check"]);
        assert_eq!(
            resolve_step(&graph, &every_step, "check"),
            Resolution::Ambiguous(vec![handlers_check, tests_check])
        );
        assert_eq!(
            resolve_step(&graph, &every_step, "deploy"),
            Resolution::Unknown
        );
        assert_eq!(
            resolve_step(&graph, &every_step, "schema/parse"),
            Resolution::Unknown,
            "a path must lead somewhere real"
        );
    }

    #[test]
    fn an_ambiguous_id_is_refused_with_the_paths_to_use() {
        let mut graph = nested_plan();
        let before = graph.clone();

        let input = request(true, &["schema", "check"]);
        let refusal = apply_step_locks(&mut graph, &input).unwrap_err();

        let matched = vec!["handlers/check".to_string(), "tests/check".to_string()];
        assert_eq!(
            refusal,
            LockRefusal::AmbiguousSteps(vec![("check".to_string(), matched)])
        );
        let message = refusal.to_string();
        assert!(message.contains("`handlers/check`, `tests/check`"));
        assert_eq!(graph, before, "nothing changes when a name is ambiguous");
    }

    #[test]
    fn unknown_ids_are_refused_and_listed() {
        let mut graph = nested_plan();
        let before = graph.clone();

        let input = request(true, &["schema", "deploy", "ship"]);
        let refusal = apply_step_locks(&mut graph, &input).unwrap_err();

        assert_eq!(
            refusal,
            LockRefusal::UnknownSteps(vec!["deploy".into(), "ship".into()])
        );
        let message = refusal.to_string();
        assert!(message.contains("`deploy`, `ship`"));
        assert_eq!(graph, before);
    }

    #[test]
    fn naming_nothing_is_refused() {
        let mut graph = nested_plan();
        assert_eq!(
            apply_step_locks(&mut graph, &request(true, &[])),
            Err(LockRefusal::NothingNamed)
        );
    }

    #[test]
    fn locking_a_step_locks_everything_inside_it() {
        let mut graph = nested_plan();

        let input = request(true, &["handlers"]);
        let change = apply_step_locks(&mut graph, &input).unwrap();

        assert!(is_locked(&graph, &["handlers"]));
        assert!(is_locked(&graph, &["handlers", "parse"]));
        assert!(is_locked(&graph, &["handlers", "check"]));
        assert!(!is_locked(&graph, &["schema"]));
        assert!(!is_locked(&graph, &["tests", "check"]));
        assert_eq!(
            change,
            LockChange {
                changed: vec![
                    "\"Write handlers\" (handlers)".into(),
                    "\"Parse the body\" (handlers/parse)".into(),
                    "\"Check the input\" (handlers/check)".into(),
                ],
                unchanged: Vec::new(),
                locked_steps: 3,
                total_steps: 6,
            }
        );

        let input = request(true, &["handlers", "parse"]);
        let again = apply_step_locks(&mut graph, &input).unwrap();
        assert!(again.changed.is_empty());
        assert_eq!(
            again.unchanged.len(),
            2,
            "a step already locked, even inside a locked step, is reported rather than refused"
        );
    }

    #[test]
    fn unlocking_reopens_only_the_step_named() {
        let mut graph = nested_plan();
        graph.lock_all();

        let input = request(false, &["handlers"]);
        let change = apply_step_locks(&mut graph, &input).unwrap();

        assert!(!is_locked(&graph, &["handlers"]));
        assert!(is_locked(&graph, &["handlers", "parse"]));
        assert!(is_locked(&graph, &["handlers", "check"]));
        assert_eq!(
            change.changed,
            vec!["\"Write handlers\" (handlers)".to_string()]
        );
        assert_eq!(change.locked_steps, 5);
        assert_eq!(change.total_steps, 6);
    }

    #[test]
    fn a_step_inside_a_locked_step_needs_its_container_unlocked_too() {
        let mut graph = nested_plan();
        graph.lock_all();
        let before = graph.clone();

        let input = request(false, &["schema", "handlers/parse"]);
        let refusal = apply_step_locks(&mut graph, &input).unwrap_err();
        assert_eq!(
            refusal,
            LockRefusal::InsideLockedStep {
                step: "\"Parse the body\" (handlers/parse)".into(),
                container: "\"Write handlers\" (handlers)".into(),
                container_path: "handlers".into(),
            }
        );
        let message = refusal.to_string();
        assert!(message.contains("unlock `handlers` as well"));
        assert_eq!(graph, before, "nothing is unlocked");

        // Named child first, the container still goes first.
        let input = request(false, &["handlers/parse", "handlers"]);
        let change = apply_step_locks(&mut graph, &input).unwrap();
        assert!(!is_locked(&graph, &["handlers"]));
        assert!(!is_locked(&graph, &["handlers", "parse"]));
        assert!(is_locked(&graph, &["handlers", "check"]));
        assert_eq!(change.changed.len(), 2);
    }

    #[test]
    fn all_locks_and_unlocks_every_step_at_every_depth() {
        let mut graph = nested_plan();
        let every = SetStepLocksToolInput {
            locked: true,
            steps: Vec::new(),
            all: true,
        };

        let change = apply_step_locks(&mut graph, &every).unwrap();
        assert!(graph.is_fully_locked_deeply());
        assert_eq!(change.changed.len(), 6);
        assert_eq!(change.locked_steps, 6);

        let unlock_every = SetStepLocksToolInput {
            locked: false,
            ..every
        };
        let change = apply_step_locks(&mut graph, &unlock_every).unwrap();
        assert_eq!(graph.locked_step_count_deeply(), 0);
        assert_eq!(change.changed.len(), 6);
        assert_eq!(change.locked_steps, 0);
    }

    #[test]
    fn saved_output_without_unchanged_steps_replays() {
        let saved = serde_json::to_value(SetStepLocksToolOutput::Success {
            locked: true,
            changed: vec!["\"Define schema\" (schema)".into()],
            unchanged: Vec::new(),
            locked_steps: 1,
            total_steps: 6,
        })
        .unwrap();
        assert!(saved.get("unchanged").is_none());
        assert!(matches!(
            serde_json::from_value::<SetStepLocksToolOutput>(saved).unwrap(),
            SetStepLocksToolOutput::Success { locked: true, .. }
        ));
    }
}
