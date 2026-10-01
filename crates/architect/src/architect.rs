//! The graph model behind Architect mode.
//!
//! A graph is a plan: each node is a step the agent will carry out, and each
//! edge is the condition under which one step leads to another. The model
//! proposes the shape, the user reshapes it on a canvas, and once every step is
//! locked the graph is compiled into an ordered spec for the agent to follow.
//!
//! This crate is deliberately free of UI and app dependencies so the graph can
//! be stored alongside a thread and exercised in plain unit tests.

mod edit;
mod layout;
mod run;
mod spec;

pub use edit::{
    GraphEdit, GraphEditError, GraphEditPreview, NodePosition, preview_graph_edits,
    preview_graph_replacement,
};
pub use layout::{COLUMN_SPACING, Position, ROW_SPACING, layout_positions};
pub use run::{
    Branch, Decision, MAX_NODE_VISITS, MAX_PLAN_DEPTH, MAX_RUN_STEPS, PlanRun, RunOutcome,
    RunRefusal, branch_prompt, parallel_steps_prompt, parse_verdict, step_prompt,
};
pub use spec::compile_spec;

use agent_client_protocol::schema::v1 as acp;
use collections::{HashMap, HashSet};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fmt::{self, Display};

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct NodeId(#[schemars(length(min = 1))] pub String);

impl Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for NodeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct EdgeId(#[schemars(length(min = 1))] pub String);

impl From<&str> for EdgeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// When one step leads to another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeCondition {
    /// The step always follows.
    Always,
    /// An externally observable statement, such as whether a command succeeded.
    /// The built-in integration asks the model to evaluate it from the completed
    /// step summary; this is not an executable expression or a deterministic
    /// evaluator. The old serialized name `deterministic` and field `expression`
    /// remain accepted so saved plans continue to load.
    #[serde(alias = "deterministic")]
    Objective {
        #[serde(alias = "expression")]
        statement: String,
    },
    /// A judgement the model has to make, phrased as a yes-or-no question.
    LlmEvaluated { question: String },
}

impl EdgeCondition {
    pub fn label(&self) -> Option<&str> {
        match self {
            EdgeCondition::Always => None,
            EdgeCondition::Objective { statement } => Some(statement),
            EdgeCondition::LlmEvaluated { question } => Some(question),
        }
    }

    pub fn is_always(&self) -> bool {
        matches!(self, EdgeCondition::Always)
    }
}

/// What a step reported when it finished.
///
/// This is what travels along an edge to the steps that follow, and what the
/// step itself is reminded of when a loop brings it round again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepResult {
    /// The step's own account of what it did, answering its `capture`.
    pub summary: String,
    /// Which attempt produced this, counting from 1. A step reached twice by a
    /// loop is on attempt 2.
    #[serde(default)]
    pub attempt: usize,
}

/// Registry identifiers for the model that executes a step, not display names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepModel {
    pub provider: String,
    pub model: String,
}

/// A single step in the plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArchitectNode {
    pub id: NodeId,
    pub title: String,
    /// Overrides the plan's execution model. Omitted steps inherit the plan model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<StepModel>,
    /// The area of work this step owns.
    #[serde(default)]
    pub responsibility: String,
    /// Existing files this step anticipates modifying. Tool inputs accept
    /// project-relative paths such as ["src/main.rs", "README.md"], or root/path
    /// to disambiguate multiple roots. Stored graphs use root-qualified paths.
    /// `None` is an unreviewed legacy declaration; `Some([])` anticipates no existing files.
    #[serde(default)]
    pub file_surface: Option<Vec<String>>,
    /// What this step has to accomplish.
    #[serde(default)]
    pub intent: String,
    /// Constraints this step must honour, refined in the node's own chat.
    #[serde(default)]
    pub rules: Vec<String>,
    /// What the step's summary has to contain, in the author's words. This is
    /// the contract between one step and the steps that follow it: whatever is
    /// named here is what they will be told.
    #[serde(default)]
    pub capture: String,
    /// Forces this step's summary into every later step, not just the ones it
    /// leads to directly. For the decision early on that everything downstream
    /// depends on.
    #[serde(default)]
    pub pinned: bool,
    /// Where the node sits on the canvas. `None` until it has been laid out.
    #[serde(default)]
    #[schemars(with = "Option<NodePosition>")]
    pub position: Option<Position>,
    /// A locked node is finished being deliberated and can no longer be edited.
    #[serde(default)]
    pub locked: bool,
    /// The thread used to deliberate this step, created on first use.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub chat: Option<acp::SessionId>,
    /// A plan nested inside this step. Running the step runs this plan, and the
    /// step is done when the plan is.
    #[serde(default)]
    pub subplan: Option<Box<ArchitectGraph>>,
    /// What the step reported the last time it ran.
    #[serde(default)]
    pub result: Option<StepResult>,
}

impl ArchitectNode {
    pub fn new(id: impl Into<NodeId>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            model: None,
            responsibility: String::new(),
            file_surface: Some(Vec::new()),
            intent: String::new(),
            rules: Vec::new(),
            capture: String::new(),
            pinned: false,
            position: None,
            locked: false,
            chat: None,
            subplan: None,
            result: None,
        }
    }

    pub(crate) fn file_surface_description(&self) -> String {
        match &self.file_surface {
            None => "MISSING — declare existing files before running".into(),
            Some(files) if files.is_empty() => "[] — no existing files anticipated".into(),
            Some(files) => format!("{files:?}"),
        }
    }

    /// The nested plan, if this step has one worth running.
    pub fn subplan(&self) -> Option<&ArchitectGraph> {
        self.subplan.as_deref().filter(|graph| !graph.is_empty())
    }

    pub fn has_subplan(&self) -> bool {
        self.subplan().is_some()
    }

    /// The result successors should receive from this step.
    ///
    /// Nested steps are not run directly, so older callers may not have stored a
    /// result on the containing node. In that case the completed child results
    /// are composed into a handoff instead of making the containing step appear
    /// to have produced nothing.
    pub fn handoff_result(&self) -> Option<StepResult> {
        self.result
            .as_ref()
            .filter(|result| !result.summary.trim().is_empty())
            .cloned()
            .or_else(|| self.subplan()?.completion_result(1))
    }
}

impl From<&str> for ArchitectNode {
    fn from(value: &str) -> Self {
        Self::new(value, value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArchitectEdge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    #[serde(default = "always_condition")]
    pub condition: EdgeCondition,
    /// The most times a run may take this connection before treating it as
    /// closed and moving on. This is how a loop says "retry at most three
    /// times": once the limit is spent, the next way out is taken instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repeats: Option<u32>,
}

fn always_condition() -> EdgeCondition {
    EdgeCondition::Always
}

impl ArchitectEdge {
    pub fn new(id: impl Into<EdgeId>, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        Self {
            id: id.into(),
            from: from.into(),
            to: to.into(),
            condition: EdgeCondition::Always,
            max_repeats: None,
        }
    }

    pub fn with_condition(mut self, condition: EdgeCondition) -> Self {
        self.condition = condition;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArchitectGraph {
    #[serde(default)]
    pub nodes: Vec<ArchitectNode>,
    #[serde(default)]
    pub edges: Vec<ArchitectEdge>,
}

/// Something wrong with the graph that the user should see before running it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum GraphProblem {
    InvalidNodeId(NodeId),
    InvalidEdgeId(EdgeId),
    DuplicateNode(NodeId),
    DuplicateEdge(EdgeId),
    /// An edge referring to a node that is not in the graph.
    DanglingEdge {
        edge: EdgeId,
        missing: NodeId,
    },
    /// A node no entry point can reach, so it would never run.
    Unreachable(NodeId),
    /// A conditional connection with no statement or question to evaluate.
    EmptyCondition(EdgeId),
    /// A step still open for deliberation.
    Unlocked(NodeId),
    /// A loop that always goes round again: every step in it goes on only by
    /// unconditional, unlimited connections, all of which a run takes, so it
    /// could never leave the loop.
    EndlessLoop(NodeId),
    MissingFileSurface(NodeId),
    InvalidFileSurface {
        node: NodeId,
        file: String,
        reason: String,
    },
    FileSurfaceOverlap {
        first: NodeId,
        second: NodeId,
        file: String,
    },
    /// Something wrong inside a step's nested plan.
    InSubplan {
        node: NodeId,
        problem: Box<GraphProblem>,
    },
}

impl Display for GraphProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphProblem::InvalidNodeId(id) => write!(
                formatter,
                "step identifier {:?} is blank; use a non-blank stable id",
                id.0
            ),
            GraphProblem::InvalidEdgeId(id) => write!(
                formatter,
                "connection identifier {:?} is blank; use a non-blank stable id",
                id.0
            ),
            GraphProblem::DuplicateNode(id) => {
                write!(formatter, "more than one step uses the id {id}")
            }
            GraphProblem::DuplicateEdge(id) => {
                write!(formatter, "more than one connection uses the id {}", id.0)
            }
            GraphProblem::DanglingEdge { edge, missing } => write!(
                formatter,
                "connection {} points at a step that does not exist: {missing}",
                edge.0
            ),
            GraphProblem::Unreachable(id) => {
                write!(formatter, "nothing leads to {id}, so it would never run")
            }
            GraphProblem::EmptyCondition(id) => {
                write!(formatter, "connection {} has an empty condition", id.0)
            }
            GraphProblem::Unlocked(id) => write!(formatter, "{id} is not locked yet"),
            GraphProblem::EndlessLoop(id) => write!(
                formatter,
                "{id} is in a loop with no way out; give one of its connections a condition or a \
                 repeat limit"
            ),
            GraphProblem::MissingFileSurface(node) => write!(
                formatter,
                "{node} needs an explicit file_surface; list existing files as worktree/path, or [] if none are anticipated"
            ),
            GraphProblem::InvalidFileSurface { node, file, reason } => {
                write!(
                    formatter,
                    "{node} has an invalid file surface path {file:?}: {reason}"
                )
            }
            GraphProblem::FileSurfaceOverlap {
                first,
                second,
                file,
            } => write!(
                formatter,
                "{first} and {second} may run concurrently and both declare {file}; serialize these steps or give them disjoint file surfaces (including nested steps)"
            ),
            GraphProblem::InSubplan { node, problem } => {
                write!(formatter, "inside {node}: {problem}")
            }
        }
    }
}

impl GraphProblem {
    /// The problem as the user should read it, naming steps by title. The
    /// `Display` form keeps ids, which is what the model needs to fix a plan.
    pub fn describe(&self, graph: &ArchitectGraph) -> String {
        let title = |id: &NodeId| {
            graph
                .node(id)
                .map(|node| node.title.trim())
                .filter(|title| !title.is_empty())
                .map_or_else(|| id.0.clone(), |title| title.to_string())
        };
        let edge_ends = |id: &EdgeId| {
            graph
                .edges
                .iter()
                .find(|edge| &edge.id == id)
                .map(|edge| (title(&edge.from), title(&edge.to)))
        };
        match self {
            GraphProblem::InvalidNodeId(id) => {
                format!("\"{}\" needs a non-blank step ID", title(id))
            }
            GraphProblem::InvalidEdgeId(edge) => match edge_ends(edge) {
                Some((from, to)) => {
                    format!("The connection from \"{from}\" to \"{to}\" needs a non-blank ID")
                }
                None => "A connection needs a non-blank ID".to_string(),
            },
            GraphProblem::DuplicateEdge(id) => {
                format!("More than one connection uses the id {}", id.0)
            }
            GraphProblem::DuplicateNode(id) => format!("More than one step uses the id {id}"),
            GraphProblem::DanglingEdge { edge, .. } => match edge_ends(edge) {
                Some((from, _)) => format!("A connection from \"{from}\" leads to a missing step"),
                None => "A connection leads to a missing step".to_string(),
            },
            GraphProblem::Unreachable(id) => {
                format!("Nothing leads to \"{}\", so it would never run", title(id))
            }
            GraphProblem::EmptyCondition(edge) => match edge_ends(edge) {
                Some((from, to)) => {
                    format!("The connection from \"{from}\" to \"{to}\" needs its condition")
                }
                None => "A connection needs its condition".to_string(),
            },
            GraphProblem::Unlocked(id) => format!("\"{}\" is not locked yet", title(id)),
            GraphProblem::EndlessLoop(id) => format!(
                "\"{}\" is in a loop with no way out; give one of its connections a condition \
                 or a repeat limit",
                title(id)
            ),
            GraphProblem::MissingFileSurface(node) => format!(
                "\"{}\" needs an existing-file surface. List worktree/path files, or [] if none are anticipated",
                title(node)
            ),
            GraphProblem::InvalidFileSurface { node, file, reason } => format!(
                "\"{}\" has an invalid file path {file:?}: {reason}",
                title(node)
            ),
            GraphProblem::FileSurfaceOverlap {
                first,
                second,
                file,
            } => format!(
                "\"{}\" and \"{}\" may run concurrently and both touch {file}. Serialize them or declare disjoint file surfaces, including nested steps",
                title(first),
                title(second)
            ),
            GraphProblem::InSubplan { node, problem } => {
                let inner = graph
                    .node(node)
                    .and_then(ArchitectNode::subplan)
                    .map_or_else(|| problem.to_string(), |subplan| problem.describe(subplan));
                format!("Inside \"{}\": {inner}", title(node))
            }
        }
    }
}

/// Where a step sits, as the ids walked from the top-level plan down to it.
///
/// A bare `NodeId` stops meaning anything once plans nest, because the same id
/// may exist in several sub-plans.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct NodePath(pub Vec<NodeId>);

impl NodePath {
    pub fn root(id: NodeId) -> Self {
        Self(vec![id])
    }

    pub fn child(&self, id: NodeId) -> Self {
        let mut path = self.0.clone();
        path.push(id);
        Self(path)
    }

    pub fn parent(&self) -> Option<NodePath> {
        (self.0.len() > 1).then(|| NodePath(self.0[..self.0.len() - 1].to_vec()))
    }

    pub fn leaf(&self) -> Option<&NodeId> {
        self.0.last()
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[NodeId] {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = &NodeId> {
        self.0.iter()
    }
}

impl From<NodeId> for NodePath {
    fn from(id: NodeId) -> Self {
        Self::root(id)
    }
}

impl From<Vec<NodeId>> for NodePath {
    fn from(ids: Vec<NodeId>) -> Self {
        Self(ids)
    }
}

impl Display for NodePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joined: Vec<&str> = self.0.iter().map(|id| id.0.as_str()).collect();
        write!(formatter, "{}", joined.join(" / "))
    }
}

/// Why a checked graph mutation could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationError {
    EmptyPath,
    NodeNotFound { path: NodePath },
    MissingSubplan { path: NodePath },
    Locked { path: NodePath },
    NestedPlanUnlocked { path: NodePath },
    EdgeNotFound { graph: NodePath, edge: EdgeId },
}

impl std::error::Error for GraphMutationError {}

impl Display for GraphMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphMutationError::EmptyPath => write!(formatter, "a node path cannot be empty"),
            GraphMutationError::NodeNotFound { path } => {
                write!(formatter, "the step at {path} does not exist")
            }
            GraphMutationError::MissingSubplan { path } => {
                write!(formatter, "the step at {path} does not contain a plan")
            }
            GraphMutationError::Locked { path } => {
                write!(formatter, "the step at {path} is locked")
            }
            GraphMutationError::NestedPlanUnlocked { path } => write!(
                formatter,
                "the step at {path} cannot be locked while its nested plan is unlocked"
            ),
            GraphMutationError::EdgeNotFound { graph, edge } => {
                if graph.is_empty() {
                    write!(formatter, "the connection {} does not exist", edge.0)
                } else {
                    write!(
                        formatter,
                        "the connection {} does not exist in the plan at {graph}",
                        edge.0
                    )
                }
            }
        }
    }
}

/// Locked steps a new draft would have discarded.
///
/// Locking is the user's declaration that a step is settled, so a draft that
/// drops or rewrites one is throwing away work they explicitly finished. That is
/// refused rather than merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedStepsWouldChange {
    /// The steps, named as the user knows them.
    pub steps: Vec<String>,
}

/// A draft, merged over the plan it replaces.
#[derive(Clone, Debug)]
pub struct MergedDraft {
    pub graph: ArchitectGraph,
    /// Steps that kept something the draft did not carry: settled detail, their
    /// own conversation, what they reported when they ran, or a plan inside.
    pub preserved: Vec<NodeId>,
}

/// Where a step leads, and on what terms, in a form two graphs can be compared
/// by. Ordered, so the same routing written in a different order still matches.
type Route = (String, EdgeCondition, Option<u32>);

fn routes_from(graph: &ArchitectGraph, id: &NodeId) -> Vec<Route> {
    let mut routes: Vec<Route> = graph
        .edges_from(id)
        .map(|edge| (edge.to.0.clone(), edge.condition.clone(), edge.max_repeats))
        .collect();
    routes.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.label().unwrap_or("").cmp(b.1.label().unwrap_or("")))
    });
    routes
}

// This is a lexical comparison spelling, not OS canonicalization. Filesystem
// aliases (including symlinks and short names) need host-specific resolution.
fn canonical_file_surface_path(path: &str) -> Result<String, String> {
    let path = path.replace('\\', "/");
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
        return Err(
            "use a relative file path qualified as worktree/path, not a directory or absolute path"
                .into(),
        );
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err("parent traversal is ambiguous; spell the file directly as worktree/path without '..'".into());
        }
        if component
            .chars()
            .any(|character| character.is_control() || "<>:\"|?*".contains(character))
            || component.ends_with([' ', '.'])
        {
            return Err("remove control characters, Windows-reserved characters, or trailing spaces or dots from the file path".into());
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        {
            return Err("Windows device names are not portable file paths; name a regular worktree/path file".into());
        }
        components.push(component);
    }
    if components.len() < 2 {
        return Err(
            "include the worktree name and a file, for example worktree/src/main.rs".into(),
        );
    }
    Ok(components.join("/"))
}

fn file_surface_declaration_identity(path: &str) -> Result<String, String> {
    let canonical = canonical_file_surface_path(path)?;
    let (root, relative) = canonical
        .split_once('/')
        .ok_or_else(|| "include the project root and file path".to_string())?;
    // Root names identify the open project namespace. Paths within that root
    // may name distinct files on a case-sensitive host.
    Ok(format!("{}/{relative}", root.to_lowercase()))
}

impl ArchitectGraph {
    /// Conservative comparison identity for a worktree-qualified file path.
    /// Normalizes separators, '.' components, and Unicode lowercase spelling.
    /// This is not full Unicode case folding, Unicode normalization, or OS
    /// canonicalization: symlinks, short names, and filesystem aliases are unresolved.
    pub fn normalize_file_surface_path(path: &str) -> Result<String, String> {
        canonical_file_surface_path(path).map(|path| path.to_lowercase())
    }

    /// Sorted, deduplicated identities suitable for comparing file surfaces.
    /// Do not use these identities to open files on a case-sensitive filesystem.
    pub fn normalize_file_surface(files: &[String]) -> Result<Vec<String>, String> {
        files
            .iter()
            .map(|file| Self::normalize_file_surface_path(file))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()
            .map(|files| files.into_iter().collect())
    }

    /// Includes a composite step's own declaration and every nested declaration.
    pub fn effective_file_surface(&self, path: &NodePath) -> Result<Vec<String>, String> {
        let node = self
            .node_at(path)
            .ok_or_else(|| format!("No step at {path}"))?;
        let files = node
            .file_surface
            .as_ref()
            .ok_or_else(|| format!("Step {path} needs an explicit file_surface before running"))?;
        let mut surface: std::collections::BTreeSet<_> =
            Self::normalize_file_surface(files)?.into_iter().collect();
        if let Some(subplan) = node.subplan() {
            for child in &subplan.nodes {
                surface.extend(subplan.effective_file_surface(&NodePath::root(child.id.clone()))?);
            }
        }
        Ok(surface.into_iter().collect())
    }

    /// Surface blockers at every depth, including effective composite scopes.
    pub fn file_surface_problems(&self) -> Vec<GraphProblem> {
        let mut problems = self.local_file_surface_problems();
        for node in &self.nodes {
            if let Some(subplan) = node.subplan() {
                problems.extend(subplan.file_surface_problems().into_iter().map(|problem| {
                    GraphProblem::InSubplan {
                        node: node.id.clone(),
                        problem: Box::new(problem),
                    }
                }));
            }
        }
        problems
    }

    fn local_file_surface_problems(&self) -> Vec<GraphProblem> {
        let mut problems = Vec::new();
        for node in &self.nodes {
            let Some(files) = &node.file_surface else {
                problems.push(GraphProblem::MissingFileSurface(node.id.clone()));
                continue;
            };
            let mut identities = HashSet::default();
            for file in files {
                let problem = match canonical_file_surface_path(file).and_then(|canonical| {
                    file_surface_declaration_identity(&canonical)
                        .map(|identity| (canonical, identity))
                }) {
                    Err(reason) => Some(reason),
                    Ok((canonical, identity)) => {
                        if !identities.insert(identity) {
                            Some("this file path is already declared; remove the duplicate".into())
                        } else if &canonical != file {
                            Some(format!(
                                "use the canonical spelling {canonical:?}, with '/' separators and no empty or '.' components"
                            ))
                        } else {
                            None
                        }
                    }
                };
                if let Some(reason) = problem {
                    problems.push(GraphProblem::InvalidFileSurface {
                        node: node.id.clone(),
                        file: file.clone(),
                        reason,
                    });
                }
            }
        }

        // A disabled edge cannot prove ordering. Conditional edges are dependencies,
        // not exclusive choices: both branches can be selected by the runner.
        let reachable: Vec<HashSet<NodeId>> = self
            .nodes
            .iter()
            .map(|node| {
                let mut reached = HashSet::default();
                let mut pending: Vec<_> = self
                    .edges_from(&node.id)
                    .filter(|edge| edge.max_repeats != Some(0))
                    .map(|edge| edge.to.clone())
                    .collect();
                while let Some(id) = pending.pop() {
                    if reached.insert(id.clone()) {
                        pending.extend(
                            self.edges_from(&id)
                                .filter(|edge| edge.max_repeats != Some(0))
                                .map(|edge| edge.to.clone()),
                        );
                    }
                }
                reached
            })
            .collect();
        // Repeated visits invalidate ordering only across potentially concurrent
        // loop branches, not every predecessor and successor of a serial retry.
        let concurrent_loop_steps = run::potentially_concurrent_loop_steps(self);
        // Invalid or missing declarations already have actionable problems at
        // their own depth. Only complete scopes can be compared for overlap.
        let surfaces: Vec<_> = self
            .nodes
            .iter()
            .map(|node| self.effective_file_surface(&NodePath::root(node.id.clone())))
            .collect();
        for (first_index, first) in self.nodes.iter().enumerate() {
            for (second_index, second) in self.nodes.iter().enumerate().skip(first_index + 1) {
                let ordered = !concurrent_loop_steps
                    .contains(&(first.id.clone(), second.id.clone()))
                    && (reachable
                        .get(first_index)
                        .is_some_and(|nodes| nodes.contains(&second.id))
                        || reachable
                            .get(second_index)
                            .is_some_and(|nodes| nodes.contains(&first.id)));
                if ordered {
                    continue;
                }
                if let (Some(Ok(first_surface)), Some(Ok(second_surface))) =
                    (surfaces.get(first_index), surfaces.get(second_index))
                {
                    for file in first_surface
                        .iter()
                        .filter(|file| second_surface.contains(file))
                    {
                        problems.push(GraphProblem::FileSurfaceOverlap {
                            first: first.id.clone(),
                            second: second.id.clone(),
                            file: file.clone(),
                        });
                    }
                }
            }
        }
        problems
    }

    /// Widens downstream declarations after creation, including nested successors
    /// and containing scopes, without reopening locks or discarding live results.
    /// Returns changed paths in stable order. Missing legacy surfaces stay missing.
    /// Invalid paths are retained verbatim so validation fails rather than silently
    /// losing a created file. Callers must revalidate before scheduling more work.
    pub fn record_created_files(&mut self, source: &NodePath, files: &[String]) -> Vec<NodePath> {
        if self.node_at(source).is_none() || files.is_empty() {
            return Vec::new();
        }
        let mut pending = vec![(source.clone(), false, false)];
        let mut visited = HashSet::default();
        let mut changed = std::collections::BTreeSet::new();
        while let Some((path, descend, record)) = pending.pop() {
            if !visited.insert((path.clone(), descend, record)) {
                continue;
            }
            if record
                && let Some(node) = self.node_at_mut(&path)
                && let Some(surface) = &mut node.file_surface
            {
                for file in files {
                    let canonical =
                        canonical_file_surface_path(file).unwrap_or_else(|_| file.clone());
                    let identity = file_surface_declaration_identity(&canonical);
                    if !surface.iter().any(|existing| {
                        existing == &canonical
                            || identity.as_ref().is_ok_and(|identity| {
                                file_surface_declaration_identity(existing).as_ref() == Ok(identity)
                            })
                    }) {
                        surface.push(canonical);
                        changed.insert(path.clone());
                    }
                }
            }
            if descend && let Some(subplan) = self.node_at(&path).and_then(ArchitectNode::subplan) {
                // An edge re-entering a composite starts its subplan afresh,
                // including earlier children. Merely ascending to widen a parent
                // scope never sets descend, even during a local child retry.
                pending.extend(
                    subplan
                        .nodes
                        .iter()
                        .map(|node| (path.child(node.id.clone()), true, true)),
                );
            }
            let parent = path.parent().unwrap_or_default();
            if !parent.is_empty() {
                // Widen the parent scope, not unrelated siblings inside it.
                pending.push((parent.clone(), false, true));
            }
            if let Some(id) = path.leaf()
                && let Some(local) = self.graph_at(&parent)
            {
                pending.extend(
                    local
                        .edges_from(id)
                        .map(|edge| (parent.child(edge.to.clone()), true, true)),
                );
            }
        }
        changed.into_iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn step_count_deeply(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| {
                1 + node
                    .subplan()
                    .map(ArchitectGraph::step_count_deeply)
                    .unwrap_or_default()
            })
            .sum()
    }

    /// How many steps here and in every nested plan are locked, counted the
    /// same way as `step_count_deeply`.
    pub fn locked_step_count_deeply(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| {
                let nested = node
                    .subplan()
                    .map(ArchitectGraph::locked_step_count_deeply)
                    .unwrap_or_default();
                usize::from(node.locked) + nested
            })
            .sum()
    }

    /// Clears results from this plan and every nested plan before a new run.
    pub fn clear_results(&mut self) {
        for node in &mut self.nodes {
            node.result = None;
            if let Some(subplan) = node.subplan.as_deref_mut() {
                subplan.clear_results();
            }
        }
    }

    /// Composes the results produced by a completed plan into the handoff its
    /// containing step gives to successors.
    pub fn completion_result(&self, attempt: usize) -> Option<StepResult> {
        let mut summary = String::new();
        for node in &self.nodes {
            let Some(result) = node.handoff_result() else {
                continue;
            };
            let result_summary = result.summary.trim();
            if result_summary.is_empty() {
                continue;
            }
            let _ = writeln!(summary, "- {}: {result_summary}", node.title);
        }

        (!summary.is_empty()).then(|| StepResult {
            summary: summary.trim_end().to_string(),
            attempt: attempt.max(1),
        })
    }

    /// Lays a fresh draft over this plan, keeping what a draft cannot know.
    ///
    /// Drafting a plan replaces it wholesale, which used to mean that redrawing a
    /// plan silently destroyed everything settled since it was first drawn: every
    /// goal argued out in a step's own chat, every rule, every recorded result,
    /// the layout the user arranged, and the step conversations themselves.
    ///
    /// Structure is the draft's business — which steps exist and what leads where.
    /// Everything that was decided about a step, rather than proposed, is this
    /// plan's business and is carried across.
    pub fn merge_draft(
        &self,
        mut draft: ArchitectGraph,
    ) -> Result<MergedDraft, LockedStepsWouldChange> {
        let mut refused = Vec::new();
        for settled in self.nodes.iter().filter(|node| node.locked) {
            let rewritten = match draft.node(&settled.id) {
                None => true,
                Some(drafted) => {
                    drafted.title != settled.title
                        || drafted
                            .model
                            .as_ref()
                            .is_some_and(|model| Some(model) != settled.model.as_ref())
                        || drafted.responsibility.trim() != settled.responsibility.trim()
                        || drafted.intent.trim() != settled.intent.trim()
                        || drafted.file_surface != settled.file_surface
                        || drafted.rules != settled.rules
                        || drafted.capture.trim() != settled.capture.trim()
                        || routes_from(&draft, &settled.id) != routes_from(self, &settled.id)
                }
            };
            if rewritten {
                refused.push(settled.title.clone());
            }
        }
        if !refused.is_empty() {
            return Err(LockedStepsWouldChange { steps: refused });
        }

        let mut preserved = Vec::new();
        for node in &mut draft.nodes {
            let Some(existing) = self.node(&node.id) else {
                continue;
            };

            // None of this can be re-derived from a draft: whether the user
            // settled the step, where they put it, the conversation they settled
            // it in, what it reported when it ran, and the plan inside it.
            node.locked = existing.locked;
            node.pinned = existing.pinned;
            node.position = existing.position;
            node.chat = existing.chat.clone();
            node.result = existing.result.clone();
            let mut kept_detail = false;
            match (node.subplan.as_deref_mut(), existing.subplan.as_deref()) {
                (Some(drafted), Some(existing)) => {
                    let merged = existing.merge_draft(drafted.clone())?;
                    kept_detail = !merged.preserved.is_empty();
                    *drafted = merged.graph;
                }
                (None, _) => node.subplan = existing.subplan.clone(),
                _ => {}
            }

            // Detail a draft leaves blank is detail it did not mean to remove.
            // A redraw that only changes the shape of the plan should not empty
            // out the steps it keeps.
            if node.model.is_none() && existing.model.is_some() {
                node.model = existing.model.clone();
                kept_detail = true;
            }
            if node.responsibility.trim().is_empty() && !existing.responsibility.trim().is_empty() {
                node.responsibility = existing.responsibility.clone();
                kept_detail = true;
            }
            if node.intent.trim().is_empty() && !existing.intent.trim().is_empty() {
                node.intent = existing.intent.clone();
                kept_detail = true;
            }
            if node.rules.is_empty() && !existing.rules.is_empty() {
                node.rules = existing.rules.clone();
                kept_detail = true;
            }
            if node.capture.trim().is_empty() && !existing.capture.trim().is_empty() {
                node.capture = existing.capture.clone();
                kept_detail = true;
            }

            if kept_detail || existing.chat.is_some() || existing.result.is_some() {
                preserved.push(node.id.clone());
            }
        }

        // Compare composite scopes only after omitted nested plans have been
        // restored; omission is preservation, not removal of a child's files.
        for settled in self.nodes.iter().filter(|node| node.locked) {
            let path = NodePath::root(settled.id.clone());
            if draft.effective_file_surface(&path) != self.effective_file_surface(&path) {
                refused.push(settled.title.clone());
            }
        }
        if !refused.is_empty() {
            return Err(LockedStepsWouldChange { steps: refused });
        }
        Ok(MergedDraft {
            graph: draft,
            preserved,
        })
    }

    /// The plan as the model should read it before redrawing it.
    ///
    /// Without this the model drafts blind: it cannot preserve a goal it has
    /// never seen, and it cannot match a step it does not know the id of. Ids and
    /// lock state are the load-bearing parts, so a redraw is an edit rather than
    /// an invention.
    pub fn outline(&self) -> String {
        let mut out = String::new();
        self.write_outline(&mut out, 0);
        out
    }

    fn write_outline(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        for node in &self.nodes {
            let _ = writeln!(
                out,
                "{pad}- {} — \"{}\"{}",
                node.id.0,
                node.title,
                if node.locked {
                    " [locked: settled, do not rewrite]"
                } else {
                    ""
                }
            );
            if let Some(model) = &node.model {
                out.push_str(&format!(
                    "{pad}  model: {}/{}\n",
                    model.provider, model.model
                ));
            } else {
                out.push_str(&format!("{pad}  model: inherit plan\n"));
            }
            if !node.responsibility.trim().is_empty() {
                out.push_str(&format!(
                    "{pad}  responsibility: {}\n",
                    node.responsibility.trim()
                ));
            }
            out.push_str(&format!(
                "{pad}  file_surface: {}\n",
                node.file_surface_description()
            ));
            if !node.intent.trim().is_empty() {
                let _ = writeln!(out, "{pad}  goal: {}", node.intent.trim());
            }
            for rule in &node.rules {
                let _ = writeln!(out, "{pad}  rule: {rule}");
            }
            if !node.capture.trim().is_empty() {
                let _ = writeln!(out, "{pad}  capture: {}", node.capture.trim());
            }
            if node.pinned {
                let _ = writeln!(out, "{pad}  told to every later step");
            }
            for edge in self.edges_from(&node.id) {
                let when = match &edge.condition {
                    EdgeCondition::Always => "always".to_string(),
                    EdgeCondition::Objective { statement } => format!("if {statement}"),
                    EdgeCondition::LlmEvaluated { question } => {
                        format!("model decides: {question}")
                    }
                };
                let limit = edge
                    .max_repeats
                    .map(|limit| format!(", at most {limit} times"))
                    .unwrap_or_default();
                let _ = writeln!(out, "{pad}  leads to {} ({when}{limit})", edge.to.0);
            }
            if let Some(result) = &node.result {
                let _ = writeln!(
                    out,
                    "{pad}  reported on attempt {}: {}",
                    result.attempt.max(1),
                    result.summary.trim()
                );
            }
            if node.chat.is_some() {
                let _ = writeln!(
                    out,
                    "{pad}  has its own conversation, where this step was argued out"
                );
            }
            if let Some(subplan) = node.subplan() {
                let _ = writeln!(out, "{pad}  contains a plan:");
                subplan.write_outline(out, depth + 2);
            }
        }
    }

    pub fn node(&self, id: &NodeId) -> Option<&ArchitectNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    pub fn node_mut(&mut self, id: &NodeId) -> Option<&mut ArchitectNode> {
        self.nodes.iter_mut().find(|node| &node.id == id)
    }

    pub fn node_index(&self, id: &NodeId) -> Option<usize> {
        self.nodes.iter().position(|node| &node.id == id)
    }

    /// Adds a node, giving it a unique id if the requested one is taken.
    pub fn add_node(&mut self, mut node: ArchitectNode) -> NodeId {
        if self.node(&node.id).is_some() {
            node.id = self.unique_node_id(&node.id);
        }
        let id = node.id.clone();
        self.nodes.push(node);
        id
    }

    fn unique_node_id(&self, desired: &NodeId) -> NodeId {
        let mut suffix = 2;
        loop {
            let candidate = NodeId(format!("{}-{suffix}", desired.0));
            if self.node(&candidate).is_none() {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Removes a node along with every connection that touched it, so the graph
    /// cannot be left with edges pointing at nothing.
    pub fn remove_node(&mut self, id: &NodeId) {
        self.nodes.retain(|node| &node.id != id);
        self.edges.retain(|edge| &edge.from != id && &edge.to != id);
    }

    pub fn connect(&mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) -> EdgeId {
        let from = from.into();
        let to = to.into();
        let base = format!("{}->{}", from.0, to.0);
        let mut id = EdgeId(base.clone());
        let mut suffix = self.edges.len();
        while self.edges.iter().any(|edge| edge.id == id) {
            id = EdgeId(format!("{base}-{suffix}"));
            suffix += 1;
        }
        self.edges.push(ArchitectEdge {
            id: id.clone(),
            from,
            to,
            condition: EdgeCondition::Always,
            max_repeats: None,
        });
        id
    }

    pub fn connect_with(
        &mut self,
        from: impl Into<NodeId>,
        to: impl Into<NodeId>,
        condition: EdgeCondition,
    ) -> EdgeId {
        let id = self.connect(from, to);
        if let Some(edge) = self.edges.iter_mut().find(|edge| edge.id == id) {
            edge.condition = condition;
        }
        id
    }

    pub fn disconnect(&mut self, id: &EdgeId) {
        self.edges.retain(|edge| &edge.id != id);
    }

    pub fn edges_from<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a ArchitectEdge> {
        self.edges.iter().filter(move |edge| &edge.from == id)
    }

    pub fn edges_into<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a ArchitectEdge> {
        self.edges.iter().filter(move |edge| &edge.to == id)
    }

    /// Every step in deterministic semantic execution order.
    ///
    /// Entries are walked breadth-first in node and edge insertion order. Cycles
    /// are visited once, and disconnected components follow in node order so a
    /// malformed draft remains fully inspectable.
    pub fn execution_order(&self) -> Vec<NodeId> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut visited = HashSet::default();
        let mut pending = Vec::with_capacity(self.nodes.len());

        let mut enqueue_component =
            |entry: NodeId, order: &mut Vec<NodeId>, visited: &mut HashSet<NodeId>| {
                pending.clear();
                pending.push(entry);
                let mut next = 0;
                while let Some(id) = pending.get(next).cloned() {
                    next += 1;
                    if !visited.insert(id.clone()) {
                        continue;
                    }
                    order.push(id.clone());
                    pending.extend(
                        self.edges_from(&id)
                            .filter(|edge| self.node(&edge.to).is_some())
                            .map(|edge| edge.to.clone()),
                    );
                }
            };

        for root in self.roots() {
            enqueue_component(root, &mut order, &mut visited);
        }
        for node in &self.nodes {
            if !visited.contains(&node.id) {
                enqueue_component(node.id.clone(), &mut order, &mut visited);
            }
        }

        order
    }

    /// Where a run begins.
    ///
    /// Normally these are the steps nothing leads to. A plan whose last step
    /// loops back to an earlier one has no such step at all, so in that case
    /// the loop is broken at the connection that closes it and the entry is
    /// whatever the loop is entered through.
    pub fn roots(&self) -> Vec<NodeId> {
        let unentered: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|node| self.edges_into(&node.id).next().is_none())
            .map(|node| node.id.clone())
            .collect();
        if !unentered.is_empty() {
            return unentered;
        }

        let adjacency = self.adjacency();
        let back_edges = layout::back_edges(&adjacency);
        let mut entered = vec![false; self.nodes.len()];
        for (from, targets) in adjacency.iter().enumerate() {
            for &to in targets {
                if !back_edges.contains(&(from, to)) {
                    entered[to] = true;
                }
            }
        }

        self.nodes
            .iter()
            .enumerate()
            .filter(|(ix, _)| !entered[*ix])
            .map(|(_, node)| node.id.clone())
            .collect()
    }

    pub fn is_fully_locked(&self) -> bool {
        !self.nodes.is_empty() && self.nodes.iter().all(|node| node.locked)
    }

    pub fn set_locked(&mut self, id: &NodeId, locked: bool) {
        if let Some(node) = self.node_mut(id) {
            node.locked = locked;
        }
    }

    /// The step a path addresses, walking down through nested plans.
    pub fn node_at(&self, path: &NodePath) -> Option<&ArchitectNode> {
        let (last, parents) = path.0.split_last()?;
        let mut graph = self;
        for id in parents {
            graph = graph.node(id)?.subplan()?;
        }
        graph.node(last)
    }

    pub fn node_at_mut(&mut self, path: &NodePath) -> Option<&mut ArchitectNode> {
        let (last, parents) = path.0.split_last()?;
        let mut graph = self;
        for id in parents {
            graph = graph.node_mut(id)?.subplan.as_deref_mut()?;
        }
        graph.node_mut(last)
    }

    /// Mutates a nested plan only when every step containing it is unlocked.
    /// An empty path addresses this top-level graph.
    pub fn mutate_graph_at<R>(
        &mut self,
        path: &NodePath,
        update: impl FnOnce(&mut ArchitectGraph) -> R,
    ) -> Result<R, GraphMutationError> {
        let mut graph = self;
        let mut walked = Vec::with_capacity(path.depth());
        for id in &path.0 {
            walked.push(id.clone());
            let parent_path = NodePath(walked.clone());
            let parent = graph
                .node_mut(id)
                .ok_or_else(|| GraphMutationError::NodeNotFound {
                    path: parent_path.clone(),
                })?;
            if parent.locked {
                return Err(GraphMutationError::Locked { path: parent_path });
            }
            graph = parent.subplan.as_deref_mut().ok_or_else(|| {
                GraphMutationError::MissingSubplan {
                    path: NodePath(walked.clone()),
                }
            })?;
        }
        Ok(update(graph))
    }

    /// Mutates one unlocked step addressed by its complete path.
    ///
    /// Unlike [`ArchitectGraph::node_at_mut`], this enforces the lock boundary:
    /// neither the target nor a containing step may be locked.
    pub fn mutate_node_at<R>(
        &mut self,
        path: &NodePath,
        update: impl FnOnce(&mut ArchitectNode) -> R,
    ) -> Result<R, GraphMutationError> {
        let (graph, id) = self.containing_graph_mut(path)?;
        let node = graph
            .node_mut(&id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        if node.locked {
            return Err(GraphMutationError::Locked { path: path.clone() });
        }
        Ok(update(node))
    }

    /// Moves a step on the canvas. Where a step sits is layout, not part of
    /// what was settled, so a locked step, or one inside a locked step, can
    /// still be moved.
    pub fn move_node_at(
        &mut self,
        path: &NodePath,
        position: Position,
    ) -> Result<(), GraphMutationError> {
        let (last, parents) = path.0.split_last().ok_or(GraphMutationError::EmptyPath)?;
        let mut graph = self;
        for (depth, id) in parents.iter().enumerate() {
            graph = graph
                .node_mut(id)
                .and_then(|node| node.subplan.as_deref_mut())
                .ok_or_else(|| GraphMutationError::MissingSubplan {
                    path: NodePath(parents[..=depth].to_vec()),
                })?;
        }
        let node = graph
            .node_mut(last)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        node.position = Some(position);
        Ok(())
    }

    /// Replaces one unlocked step's outgoing routes, ignoring and returning
    /// destinations that do not exist in that step's own plan.
    pub fn replace_outgoing_at(
        &mut self,
        path: &NodePath,
        routes: Vec<(NodeId, EdgeCondition)>,
    ) -> Result<Vec<NodeId>, GraphMutationError> {
        let (graph, id) = self.containing_graph_mut(path)?;
        let node = graph
            .node(&id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        if node.locked {
            return Err(GraphMutationError::Locked { path: path.clone() });
        }

        let mut accepted = Vec::new();
        let mut unknown = Vec::new();
        for (to, condition) in routes {
            if graph.node(&to).is_some() {
                accepted.push((to, condition));
            } else {
                unknown.push(to);
            }
        }

        // A refined route to the same step keeps the repeat limit the user set,
        // since refining a step's routing is not a request to lift it.
        let limits: HashMap<NodeId, u32> = graph
            .edges_from(&id)
            .filter_map(|edge| Some((edge.to.clone(), edge.max_repeats?)))
            .collect();
        graph.edges.retain(|edge| edge.from != id);
        for (to, condition) in accepted {
            let limit = limits.get(&to).copied();
            let edge_id = graph.connect_with(id.clone(), to, condition);
            if let Some(edge) = graph.edges.iter_mut().find(|edge| edge.id == edge_id) {
                edge.max_repeats = limit;
            }
        }
        Ok(unknown)
    }

    /// Removes one unlocked step and every connection that touches it.
    pub fn remove_node_at(&mut self, path: &NodePath) -> Result<(), GraphMutationError> {
        let (graph, id) = self.containing_graph_mut(path)?;
        let node = graph
            .node(&id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        if node.locked {
            return Err(GraphMutationError::Locked { path: path.clone() });
        }
        graph.remove_node(&id);
        Ok(())
    }

    /// Adds an outgoing connection from one unlocked step in its containing plan.
    pub fn connect_from_at(
        &mut self,
        from: &NodePath,
        to: NodeId,
        condition: EdgeCondition,
    ) -> Result<EdgeId, GraphMutationError> {
        let (graph, from_id) = self.containing_graph_mut(from)?;
        let source = graph
            .node(&from_id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: from.clone() })?;
        if source.locked {
            return Err(GraphMutationError::Locked { path: from.clone() });
        }
        if graph.node(&to).is_none() {
            let mut target = from.0[..from.0.len() - 1].to_vec();
            target.push(to);
            return Err(GraphMutationError::NodeNotFound {
                path: NodePath(target),
            });
        }
        Ok(graph.connect_with(from_id, to, condition))
    }

    /// Changes a connection condition unless its source step or a containing
    /// step is locked.
    pub fn set_edge_condition_at(
        &mut self,
        graph_path: &NodePath,
        edge_id: &EdgeId,
        condition: EdgeCondition,
    ) -> Result<(), GraphMutationError> {
        let edge = self
            .graph_at(graph_path)
            .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == edge_id))
            .cloned()
            .ok_or_else(|| GraphMutationError::EdgeNotFound {
                graph: graph_path.clone(),
                edge: edge_id.clone(),
            })?;
        let source_path = graph_path.child(edge.from);
        let (graph, source_id) = self.containing_graph_mut(&source_path)?;
        let source = graph
            .node(&source_id)
            .ok_or_else(|| GraphMutationError::NodeNotFound {
                path: source_path.clone(),
            })?;
        if source.locked {
            return Err(GraphMutationError::Locked { path: source_path });
        }
        let target = graph
            .edges
            .iter_mut()
            .find(|edge| &edge.id == edge_id)
            .ok_or_else(|| GraphMutationError::EdgeNotFound {
                graph: graph_path.clone(),
                edge: edge_id.clone(),
            })?;
        target.condition = condition;
        Ok(())
    }

    /// Limits how many times a run may take a connection, or lifts the limit,
    /// unless its source step or a containing step is locked. A limit of zero
    /// would close the connection outright, so it is stored as one.
    pub fn set_edge_max_repeats_at(
        &mut self,
        graph_path: &NodePath,
        edge_id: &EdgeId,
        max_repeats: Option<u32>,
    ) -> Result<(), GraphMutationError> {
        let edge = self
            .graph_at(graph_path)
            .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == edge_id))
            .cloned()
            .ok_or_else(|| GraphMutationError::EdgeNotFound {
                graph: graph_path.clone(),
                edge: edge_id.clone(),
            })?;
        let source_path = graph_path.child(edge.from);
        let (graph, source_id) = self.containing_graph_mut(&source_path)?;
        let source = graph
            .node(&source_id)
            .ok_or_else(|| GraphMutationError::NodeNotFound {
                path: source_path.clone(),
            })?;
        if source.locked {
            return Err(GraphMutationError::Locked { path: source_path });
        }
        let target = graph
            .edges
            .iter_mut()
            .find(|edge| &edge.id == edge_id)
            .ok_or_else(|| GraphMutationError::EdgeNotFound {
                graph: graph_path.clone(),
                edge: edge_id.clone(),
            })?;
        target.max_repeats = max_repeats.map(|limit| limit.max(1));
        Ok(())
    }

    /// Removes a connection unless its source step or a containing step is locked.
    pub fn disconnect_at(
        &mut self,
        graph_path: &NodePath,
        edge_id: &EdgeId,
    ) -> Result<(), GraphMutationError> {
        let edge = self
            .graph_at(graph_path)
            .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == edge_id))
            .cloned()
            .ok_or_else(|| GraphMutationError::EdgeNotFound {
                graph: graph_path.clone(),
                edge: edge_id.clone(),
            })?;
        let source_path = graph_path.child(edge.from);
        let (graph, source_id) = self.containing_graph_mut(&source_path)?;
        let source = graph
            .node(&source_id)
            .ok_or_else(|| GraphMutationError::NodeNotFound {
                path: source_path.clone(),
            })?;
        if source.locked {
            return Err(GraphMutationError::Locked { path: source_path });
        }
        graph.disconnect(edge_id);
        Ok(())
    }

    /// Changes a step's lock state while preserving nested-plan invariants.
    ///
    /// Unlocking the addressed step is allowed, but a child cannot be unlocked
    /// through a locked containing step. Locking a step is refused until every
    /// nested step is locked.
    pub fn set_locked_at(
        &mut self,
        path: &NodePath,
        locked: bool,
    ) -> Result<(), GraphMutationError> {
        let (graph, id) = self.containing_graph_mut(path)?;
        let node = graph
            .node_mut(&id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        if locked
            && node
                .subplan()
                .is_some_and(|subplan| !subplan.is_fully_locked_deeply())
        {
            return Err(GraphMutationError::NestedPlanUnlocked { path: path.clone() });
        }
        node.locked = locked;
        Ok(())
    }

    /// Locks a step together with every step in its nested plans, so a step
    /// holding a plan can be settled in one go. Returns how many steps changed.
    ///
    /// Like [`Self::set_locked_at`], it cannot reach through a locked
    /// containing step. Unlocking goes through `set_locked_at`, which reopens
    /// only the addressed step.
    pub fn lock_deeply_at(&mut self, path: &NodePath) -> Result<usize, GraphMutationError> {
        let (graph, id) = self.containing_graph_mut(path)?;
        let node = graph
            .node_mut(&id)
            .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
        let mut changed = usize::from(!node.locked);
        node.locked = true;
        if let Some(subplan) = node.subplan.as_deref_mut() {
            changed += subplan.lock_all();
        }
        Ok(changed)
    }

    /// Locks every step here and in every nested plan. Returns how many steps
    /// changed.
    pub fn lock_all(&mut self) -> usize {
        self.nodes
            .iter_mut()
            .map(|node| {
                let changed = usize::from(!node.locked);
                node.locked = true;
                let nested = node
                    .subplan
                    .as_deref_mut()
                    .map_or(0, ArchitectGraph::lock_all);
                changed + nested
            })
            .sum()
    }

    /// Unlocks every step here and in every nested plan. Returns how many
    /// steps changed.
    pub fn unlock_all(&mut self) -> usize {
        self.nodes
            .iter_mut()
            .map(|node| {
                let changed = usize::from(node.locked);
                node.locked = false;
                let nested = node
                    .subplan
                    .as_deref_mut()
                    .map_or(0, ArchitectGraph::unlock_all);
                changed + nested
            })
            .sum()
    }

    /// Resolves the plan containing a node and rejects traversal through a
    /// locked containing step.
    fn containing_graph_mut(
        &mut self,
        path: &NodePath,
    ) -> Result<(&mut ArchitectGraph, NodeId), GraphMutationError> {
        let (last, parents) = path.0.split_last().ok_or(GraphMutationError::EmptyPath)?;
        let mut graph = self;
        let mut walked = Vec::with_capacity(path.depth());
        for id in parents {
            walked.push(id.clone());
            let parent_path = NodePath(walked.clone());
            let parent = graph
                .node_mut(id)
                .ok_or_else(|| GraphMutationError::NodeNotFound {
                    path: parent_path.clone(),
                })?;
            if parent.locked {
                return Err(GraphMutationError::Locked { path: parent_path });
            }
            graph = parent.subplan.as_deref_mut().ok_or_else(|| {
                GraphMutationError::MissingSubplan {
                    path: NodePath(walked.clone()),
                }
            })?;
        }
        Ok((graph, last.clone()))
    }

    /// The plan a path addresses: the nested plan inside the addressed step, or
    /// this graph itself for an empty path.
    pub fn graph_at(&self, path: &NodePath) -> Option<&ArchitectGraph> {
        let mut graph = self;
        for id in &path.0 {
            graph = graph.node(id)?.subplan()?;
        }
        Some(graph)
    }

    /// Gives a step a plan of its own, or hands back the one it already has.
    pub fn subplan_mut(&mut self, id: &NodeId) -> Option<&mut ArchitectGraph> {
        let node = self.node_mut(id)?;
        Some(node.subplan.get_or_insert_with(Default::default))
    }

    /// Whether a step may be locked. A step that contains a plan cannot be
    /// settled while any part of that plan is still being argued about.
    pub fn can_lock(&self, id: &NodeId) -> bool {
        self.node(id)
            .and_then(|node| node.subplan())
            .is_none_or(|subplan| subplan.is_fully_locked_deeply())
    }

    /// Whether every step here and in every nested plan is locked.
    pub fn is_fully_locked_deeply(&self) -> bool {
        !self.nodes.is_empty()
            && self.nodes.iter().all(|node| {
                node.locked
                    && node
                        .subplan()
                        .is_none_or(ArchitectGraph::is_fully_locked_deeply)
            })
    }

    /// Steps whose summary every later step is told about, in graph order.
    pub fn pinned_nodes(&self) -> impl Iterator<Item = &ArchitectNode> {
        self.nodes.iter().filter(|node| node.pinned)
    }

    /// Steps that lead somewhere but never say what they hand on. Not an error:
    /// plenty of steps have nothing worth passing forward. Worth pointing at,
    /// though, because it is usually an oversight.
    pub fn steps_without_capture(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|node| {
                node.capture.trim().is_empty() && self.edges_from(&node.id).next().is_some()
            })
            .map(|node| node.id.clone())
            .collect()
    }

    /// Everything that has to be fixed before a plan can be run.
    ///
    /// This is the structural problems plus the steps still open for
    /// deliberation: running a half-argued plan is how a plan stops being worth
    /// making. Compiling a spec and driving a run both gate on this, so the two
    /// can never disagree about what "ready" means.
    pub fn blocking_problems(&self) -> Vec<GraphProblem> {
        let mut problems = self.problems();
        problems.extend(
            self.nodes
                .iter()
                .filter(|node| !node.locked)
                .map(|node| GraphProblem::Unlocked(node.id.clone())),
        );
        for node in &self.nodes {
            let Some(subplan) = node.subplan() else {
                continue;
            };
            problems.extend(subplan.blocking_problems().into_iter().map(|problem| {
                GraphProblem::InSubplan {
                    node: node.id.clone(),
                    problem: Box::new(problem),
                }
            }));
        }
        problems
    }

    /// Every problem worth surfacing, in a stable order.
    pub fn problems(&self) -> Vec<GraphProblem> {
        let mut problems = Vec::new();

        let mut seen = HashSet::default();
        for node in &self.nodes {
            if node.id.0.trim().is_empty() {
                problems.push(GraphProblem::InvalidNodeId(node.id.clone()));
            }
            if !seen.insert(node.id.clone()) {
                problems.push(GraphProblem::DuplicateNode(node.id.clone()));
            }
        }

        let mut edge_ids = HashSet::default();
        for edge in &self.edges {
            if edge.id.0.trim().is_empty() {
                problems.push(GraphProblem::InvalidEdgeId(edge.id.clone()));
            }
            if !edge_ids.insert(edge.id.clone()) {
                problems.push(GraphProblem::DuplicateEdge(edge.id.clone()));
            }
            for endpoint in [&edge.from, &edge.to] {
                if self.node(endpoint).is_none() {
                    problems.push(GraphProblem::DanglingEdge {
                        edge: edge.id.clone(),
                        missing: endpoint.clone(),
                    });
                }
            }
        }

        for id in self.unreachable_nodes() {
            problems.push(GraphProblem::Unreachable(id));
        }

        for id in self.endless_loops() {
            problems.push(GraphProblem::EndlessLoop(id));
        }

        problems.extend(self.local_file_surface_problems());
        problems.extend(self.edges.iter().filter_map(|edge| {
            edge.condition
                .label()
                .is_some_and(|label| label.trim().is_empty())
                .then(|| GraphProblem::EmptyCondition(edge.id.clone()))
        }));

        problems
    }

    /// Steps no entry point leads to, such as a branch left over from an
    /// earlier draft. They would never run.
    fn unreachable_nodes(&self) -> Vec<NodeId> {
        let roots = self.roots();
        let mut reached: HashSet<NodeId> = HashSet::default();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if !reached.insert(id.clone()) {
                continue;
            }
            for edge in self.edges_from(&id) {
                if !reached.contains(&edge.to) {
                    stack.push(edge.to.clone());
                }
            }
        }

        self.nodes
            .iter()
            .filter(|node| !reached.contains(&node.id))
            .map(|node| node.id.clone())
            .collect()
    }

    /// Whether taking this connection can lead back to where it started, which
    /// is what makes it part of a loop and a repeat limit meaningful.
    pub fn is_loop_edge(&self, edge: &ArchitectEdge) -> bool {
        let mut reached: HashSet<&NodeId> = HashSet::default();
        let mut stack = vec![&edge.to];
        while let Some(id) = stack.pop() {
            if id == &edge.from {
                return true;
            }
            if !reached.insert(id) {
                continue;
            }
            stack.extend(self.edges_from(id).map(|next| &next.to));
        }
        false
    }

    /// Where a run must go after this step, when it has no say in the matter:
    /// with no conditional way out, every plain connection without a repeat
    /// limit is taken, since several plain connections run side by side.
    /// Empty whenever a condition decides.
    fn forced_successors(&self, id: &NodeId) -> Vec<&NodeId> {
        let mut forced: Vec<&NodeId> = Vec::new();
        // Not `edges_from`, whose result borrows `id`: the successors returned
        // here have to outlive the id they were looked up by.
        for edge in self.edges.iter().filter(|edge| &edge.from == id) {
            if self.node(&edge.to).is_none() {
                continue;
            }
            if !edge.condition.is_always() {
                return Vec::new();
            }
            if edge.max_repeats.is_none() && !forced.contains(&&edge.to) {
                forced.push(&edge.to);
            }
        }
        forced
    }

    /// Loops a run could never leave, each reported once by its first step in
    /// plan order. Steps nothing leads to are already reported as unreachable.
    fn endless_loops(&self) -> Vec<NodeId> {
        let unreachable: HashSet<NodeId> = self.unreachable_nodes().into_iter().collect();
        let mut in_reported_loop: HashSet<NodeId> = HashSet::default();
        let mut loops = Vec::new();
        for node in &self.nodes {
            if unreachable.contains(&node.id) || in_reported_loop.contains(&node.id) {
                continue;
            }
            // Depth first along forced connections. A step with several of
            // them forks, so a loop through any one of them is endless.
            let mut walk = vec![node.id.clone()];
            let mut pending = vec![self.forced_successors(&node.id).into_iter()];
            let mut explored: HashSet<NodeId> = HashSet::default();
            while let Some(successors) = pending.last_mut() {
                let Some(next) = successors.next() else {
                    pending.pop();
                    explored.extend(walk.pop());
                    continue;
                };
                if let Some(start) = walk.iter().position(|id| id == next) {
                    let cycle = &walk[start..];
                    let is_new = cycle.iter().all(|id| !in_reported_loop.contains(id));
                    let first = cycle
                        .iter()
                        .min_by_key(|id| self.node_index(id).unwrap_or(usize::MAX));
                    if is_new {
                        loops.extend(first.cloned());
                    }
                    in_reported_loop.extend(cycle.iter().cloned());
                    continue;
                }
                if explored.contains(next) || walk.len() > self.nodes.len() {
                    continue;
                }
                walk.push(next.clone());
                pending.push(self.forced_successors(next).into_iter());
            }
        }
        loops
    }

    /// Gives a position to every node that does not have one yet, leaving nodes
    /// the user has already placed exactly where they are.
    pub fn place_unpositioned_nodes(&mut self) {
        if self.nodes.iter().all(|node| node.position.is_some()) {
            return;
        }
        let positions = layout_positions(self);
        for node in &mut self.nodes {
            if node.position.is_none() {
                node.position = positions.get(&node.id).copied();
            }
        }
    }

    /// Re-runs the layout over the whole graph, discarding manual placement.
    pub fn relayout(&mut self) {
        let positions = layout_positions(self);
        for node in &mut self.nodes {
            node.position = positions.get(&node.id).copied();
        }
    }

    /// The adjacency of the graph in node order, which keeps every derived
    /// computation deterministic regardless of hash ordering.
    pub(crate) fn adjacency(&self) -> Vec<Vec<usize>> {
        let index: HashMap<&NodeId, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(ix, node)| (&node.id, ix))
            .collect();

        let mut adjacency = vec![Vec::new(); self.nodes.len()];
        for edge in &self.edges {
            let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) else {
                continue;
            };
            adjacency[from].push(to);
        }
        adjacency
    }
}

/// What a model proposes when it drafts a plan.
///
/// Positions, lock state and chats are all owned by the user rather than the
/// model, so they are absent here: asking a model to invent canvas coordinates
/// produces overlapping nodes, and it has no business locking a step the user
/// has not reviewed.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposedNode {
    /// A non-blank stable identifier, such as `run-tests`, unique within this
    /// graph. Nested graphs have separate ID namespaces; never rely on renaming.
    pub id: NodeId,
    /// A short human-readable name for the step.
    pub title: String,
    /// Execution model using exact provider and model ids from the available
    /// models. Omit to inherit the plan model for a new step, or to preserve an
    /// existing step's override when redrafting. Never invent model ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<StepModel>,
    /// The area of work this step owns, such as `Authentication` or `Tests`.
    #[serde(default)]
    pub responsibility: String,
    /// Required existing-file surface. Example: ["src/main.rs", "README.md"].
    /// Paths are project-relative; use "backend/src/main.rs" to disambiguate
    /// multiple open roots. Root-prefixed paths also work in single-root projects.
    /// Local and connected remote projects use the same rules. No absolute paths,
    /// directories, globs, or '..'. List modifications, renames, and deletions,
    /// not read-only access. Use [] when no existing files will be affected.
    /// Concurrent steps must have disjoint surfaces, including nested children.
    pub file_surface: Vec<String>,
    /// What this step has to accomplish.
    #[serde(default)]
    pub intent: String,
    /// Constraints this step must honour.
    #[serde(default)]
    pub rules: Vec<String>,
    /// What this step's summary must contain, so the steps that follow it know
    /// what they will be told. Leave out for a step that hands nothing on.
    #[serde(default)]
    pub capture: String,
    /// A plan nested inside this step, for work that is one step at this level
    /// but several once you look closely.
    #[serde(default)]
    pub steps: Option<Box<ProposedGraph>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposedEdge {
    /// The id of the step this connection leaves.
    pub from: NodeId,
    /// The id of the step this connection enters. Pointing back at an earlier
    /// step forms a loop, which is allowed.
    pub to: NodeId,
    /// When this connection is taken. Defaults to always.
    #[serde(default)]
    pub condition: Option<EdgeCondition>,
    /// For a connection that loops back: the most times it may be taken before
    /// the plan moves on another way. Leave out for no limit.
    #[serde(default)]
    pub max_repeats: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposedGraph {
    pub nodes: Vec<ProposedNode>,
    #[serde(default)]
    pub edges: Vec<ProposedEdge>,
}

impl ProposedGraph {
    /// Builds a graph from a proposal, laying it out so it is readable the
    /// moment it appears.
    pub fn into_graph(self) -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for node in self.nodes {
            graph.nodes.push(ArchitectNode {
                id: node.id,
                title: node.title,
                model: node.model,
                responsibility: node.responsibility,
                file_surface: Some(node.file_surface),
                intent: node.intent,
                rules: node.rules,
                capture: node.capture,
                pinned: false,
                position: None,
                locked: false,
                chat: None,
                subplan: node
                    .steps
                    .map(|steps| Box::new(steps.into_graph()))
                    .filter(|graph| !graph.is_empty()),
                result: None,
            });
        }
        for (ix, edge) in self.edges.into_iter().enumerate() {
            graph.edges.push(ArchitectEdge {
                id: EdgeId(format!("{}->{}-{ix}", edge.from.0, edge.to.0)),
                from: edge.from,
                to: edge.to,
                condition: edge.condition.unwrap_or(EdgeCondition::Always),
                max_repeats: edge.max_repeats.map(|limit| limit.max(1)),
            });
        }
        graph.place_unpositioned_nodes();
        graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain with a loop back to an earlier step, which is the shape the
    /// layout and validation both have to tolerate.
    fn surface_graph(nodes: &[&str], edges: &[(&str, &str)]) -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for id in nodes {
            let mut node = ArchitectNode::new(*id, *id);
            node.file_surface = Some(vec!["worktree/src/shared.rs".into()]);
            node.locked = true;
            graph.add_node(node);
        }
        for (from, to) in edges {
            graph.connect(*from, *to);
        }
        graph
    }

    #[test]
    fn proposed_surfaces_are_required_in_serde_and_schema_at_every_depth() {
        let missing = serde_json::json!({"id": "step", "title": "Step"});
        assert!(serde_json::from_value::<ProposedNode>(missing.clone()).is_err());
        let mut explicit = missing;
        explicit["file_surface"] = serde_json::json!([]);
        let proposed: ProposedNode = serde_json::from_value(explicit.clone()).expect("explicit []");
        assert!(proposed.file_surface.is_empty());
        explicit["file_surface"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<ProposedNode>(explicit).is_err());
        let nested = serde_json::json!({"nodes": [{
            "id": "parent", "title": "Parent", "file_surface": [],
            "steps": {"nodes": [{"id": "child", "title": "Child"}]}
        }]});
        assert!(serde_json::from_value::<ProposedGraph>(nested).is_err());
        let schema = serde_json::to_value(schemars::schema_for!(ProposedNode)).expect("schema");
        assert!(
            schema["required"]
                .as_array()
                .expect("required fields")
                .contains(&serde_json::json!("file_surface"))
        );
        assert!(
            schema["properties"]["file_surface"]
                .get("default")
                .is_none()
        );
    }

    #[test]
    fn legacy_missing_surfaces_load_but_never_authorize_execution() {
        let graph: ArchitectGraph = serde_json::from_value(serde_json::json!({"nodes": [{
            "id": "legacy", "title": "Legacy", "locked": true
        }]}))
        .expect("legacy graph loads");
        assert_eq!(
            graph.node(&"legacy".into()).expect("legacy").file_surface,
            None
        );
        assert_eq!(
            graph.file_surface_problems(),
            vec![GraphProblem::MissingFileSurface("legacy".into())]
        );
        assert!(PlanRun::start(&graph).is_err());
        assert!(compile_spec(&graph).is_err());
        let restored: ArchitectGraph =
            serde_json::from_value(serde_json::to_value(&graph).expect("serialize"))
                .expect("restore");
        assert_eq!(graph, restored);
        assert!(restored.outline().contains("MISSING"));
        assert_eq!(
            ArchitectNode::new("new", "New").file_surface,
            Some(Vec::new())
        );
    }

    #[test]
    fn file_surface_identities_are_portable_and_invalid_paths_are_actionable() {
        for alias in [
            "Worktree/Src/File.rs",
            "worktree\\src\\file.rs",
            "./worktree//src/./file.rs",
        ] {
            assert_eq!(
                ArchitectGraph::normalize_file_surface_path(alias).expect("identity"),
                "worktree/src/file.rs"
            );
        }
        assert_eq!(
            ArchitectGraph::normalize_file_surface(&[
                "Worktree/Src/File.rs".into(),
                "worktree\\src\\file.rs".into(),
            ])
            .expect("deduplicate"),
            vec!["worktree/src/file.rs"]
        );
        for invalid in [
            "",
            "file.rs",
            "/worktree/file.rs",
            "C:\\worktree\\file.rs",
            "//server/share/file.rs",
            "worktree/../file.rs",
            "worktree/src/",
            "worktree/*.rs",
            "worktree/file.rs:stream",
            "worktree/NUL.txt",
            "worktree/COM1",
            "worktree/file.",
            "worktree/file ",
            "worktree/file\n.rs",
            "worktree/COM¹.txt",
            "worktree/LPT²",
        ] {
            assert!(
                ArchitectGraph::normalize_file_surface_path(invalid).is_err(),
                "{invalid:?}"
            );
        }
        let mut graph = surface_graph(&["a", "b"], &[]);
        graph.node_mut(&"a".into()).expect("a").file_surface =
            Some(vec!["worktree\\src\\shared.rs".into()]);
        let problems = graph.file_surface_problems();
        assert!(problems.iter().any(|problem| matches!(problem,
            GraphProblem::InvalidFileSurface { reason, .. } if reason.contains("canonical spelling")
        )));
        assert!(
            problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::FileSurfaceOverlap { .. }))
        );
        graph.node_mut(&"a".into()).expect("a").file_surface = Some(vec![
            "Worktree/src/shared.rs".into(),
            "worktree/src/shared.rs".into(),
        ]);
        assert!(
            graph
                .file_surface_problems()
                .iter()
                .any(|problem| matches!(problem,
                    GraphProblem::InvalidFileSurface { reason, .. } if reason.contains("duplicate")
                ))
        );
    }

    #[test]
    fn one_step_can_declare_distinct_case_variants_without_weakening_parallel_checks() {
        let mut graph = surface_graph(&["step"], &[]);
        graph.node_mut(&"step".into()).unwrap().file_surface = Some(vec![
            "worktree/README.md".into(),
            "worktree/readme.md".into(),
        ]);
        assert!(graph.file_surface_problems().is_empty());
        let mut parallel = ArchitectNode::new("parallel", "Parallel");
        parallel.file_surface = Some(vec!["worktree/README.md".into()]);
        graph.add_node(parallel);
        assert!(
            graph
                .file_surface_problems()
                .iter()
                .any(|problem| { matches!(problem, GraphProblem::FileSurfaceOverlap { .. }) })
        );
    }

    #[test]
    fn created_case_variant_is_added_even_when_another_spelling_is_declared() {
        let mut graph = surface_graph(&["source", "after"], &[("source", "after")]);
        graph.node_mut(&"after".into()).unwrap().file_surface =
            Some(vec!["worktree/README.md".into()]);
        graph.record_created_files(
            &NodePath::root("source".into()),
            &["worktree/readme.md".into()],
        );
        assert_eq!(
            graph.node(&"after".into()).unwrap().file_surface,
            Some(vec![
                "worktree/README.md".into(),
                "worktree/readme.md".into()
            ])
        );
        assert!(graph.file_surface_problems().is_empty());
    }

    #[test]
    fn unicode_surfaces_preserve_spelling_and_compare_lowercase_aliases() {
        for (path, identity) in [
            ("Проект/Данные/É.TXT", "проект/данные/é.txt"),
            ("项目/源/ 文件~.rs", "项目/源/ 文件~.rs"),
            ("Worktree/ file~.rs", "worktree/ file~.rs"),
            ("Worktree/SHORT~1.rs", "worktree/short~1.rs"),
            ("Worktree/İ.rs", "worktree/i\u{307}.rs"),
        ] {
            assert_eq!(
                ArchitectGraph::normalize_file_surface_path(path).expect("valid path"),
                identity
            );
        }
        // This lexical comparison does not claim Unicode normalization or OS
        // alias resolution, and must not rewrite display/open-file spellings.
        assert_ne!(
            ArchitectGraph::normalize_file_surface_path("worktree/é.rs"),
            ArchitectGraph::normalize_file_surface_path("worktree/e\u{301}.rs")
        );
        let mut graph = surface_graph(&["left", "right"], &[]);
        graph.node_mut(&"left".into()).expect("left").file_surface =
            Some(vec!["Données/É.rs".into()]);
        graph.node_mut(&"right".into()).expect("right").file_surface =
            Some(vec!["données/é.rs".into()]);
        assert_eq!(
            graph.file_surface_problems(),
            vec![GraphProblem::FileSurfaceOverlap {
                first: "left".into(),
                second: "right".into(),
                file: "données/é.rs".into(),
            }]
        );
        graph.connect("left", "right");
        assert!(graph.file_surface_problems().is_empty());
        let files = vec!["项目/源/ 文件~.rs".into()];
        graph.record_created_files(&NodePath::root("left".into()), &files);
        assert!(
            graph
                .node(&"right".into())
                .expect("right")
                .file_surface
                .as_ref()
                .expect("surface")
                .contains(&files[0])
        );
        assert!(graph.file_surface_problems().is_empty());
    }

    #[test]
    fn parallel_surfaces_conflict_but_serial_and_join_surfaces_do_not() {
        let mut graph = surface_graph(
            &["root", "left", "right", "join"],
            &[
                ("root", "left"),
                ("root", "right"),
                ("left", "join"),
                ("right", "join"),
            ],
        );
        assert_eq!(
            graph.file_surface_problems(),
            vec![GraphProblem::FileSurfaceOverlap {
                first: "left".into(),
                second: "right".into(),
                file: "worktree/src/shared.rs".into(),
            }]
        );
        graph.connect("left", "right");
        assert!(graph.file_surface_problems().is_empty());
        assert!(PlanRun::start(&graph).is_ok());
        let mut disjoint = surface_graph(&["left", "right"], &[]);
        disjoint
            .node_mut(&"right".into())
            .expect("right")
            .file_surface = Some(vec!["other/src/shared.rs".into()]);
        assert!(disjoint.file_surface_problems().is_empty());
    }

    #[test]
    fn conditional_branches_disabled_routes_and_loops_do_not_hide_conflicts() {
        let mut graph = surface_graph(
            &["root", "left", "right"],
            &[("root", "left"), ("root", "right")],
        );
        for edge in &mut graph.edges {
            edge.condition = EdgeCondition::Objective {
                statement: "both may be true".into(),
            };
        }
        assert_eq!(graph.file_surface_problems().len(), 1);
        let edge = graph.connect("left", "right");
        graph
            .edges
            .iter_mut()
            .find(|candidate| candidate.id == edge)
            .expect("edge")
            .max_repeats = Some(0);
        assert_eq!(graph.file_surface_problems().len(), 1);
        let mut looping = surface_graph(&["a", "b"], &[("a", "b"), ("b", "a")]);
        for edge in &mut looping.edges {
            edge.max_repeats = Some(2);
        }
        assert!(
            looping.file_surface_problems().is_empty(),
            "bounded repeats run serially"
        );
        // A disabled back edge must not turn a proven serial chain into a loop.
        looping
            .edges
            .iter_mut()
            .find(|edge| edge.from == NodeId::from("b"))
            .expect("back edge")
            .max_repeats = Some(0);
        assert!(looping.file_surface_problems().is_empty());
    }

    #[test]
    fn composite_surfaces_union_children_without_conflicting_with_their_own_steps() {
        let mut graph = surface_graph(&["parent", "peer"], &[]);
        let parent = graph.node_mut(&"parent".into()).expect("parent");
        parent.file_surface = Some(Vec::new());
        parent.subplan = Some(Box::new(surface_graph(
            &["first", "second"],
            &[("first", "second")],
        )));
        assert_eq!(
            graph.effective_file_surface(&NodePath::root("parent".into())),
            Ok(vec!["worktree/src/shared.rs".into()])
        );
        assert_eq!(graph.file_surface_problems().len(), 1);
        graph.connect("parent", "peer");
        assert!(graph.file_surface_problems().is_empty());
        let nested = graph.subplan_mut(&"parent".into()).expect("nested");
        nested.edges.clear();
        assert!(graph.file_surface_problems().iter().any(|problem| matches!(problem,
            GraphProblem::InSubplan { problem, .. } if matches!(problem.as_ref(), GraphProblem::FileSurfaceOverlap { .. })
        )));
        graph
            .node_at_mut(&NodePath::root("parent".into()).child("first".into()))
            .expect("first")
            .file_surface = None;
        assert!(graph.file_surface_problems().iter().any(|problem| matches!(problem,
            GraphProblem::InSubplan { problem, .. } if matches!(problem.as_ref(), GraphProblem::MissingFileSurface(_))
        )));
        assert!(PlanRun::start(&graph).is_err());
    }

    #[test]
    fn proposals_keep_conflicting_drafts_inspectable_and_locked_surfaces_are_protected() {
        let proposal: ProposedGraph = serde_json::from_value(serde_json::json!({"nodes": [
            {"id": "left", "title": "Left", "file_surface": ["Worktree/src/shared.rs"]},
            {"id": "right", "title": "Right", "file_surface": ["worktree/src/shared.rs"]}
        ]}))
        .expect("valid proposal payload");
        let mut graph = proposal.into_graph();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.file_surface_problems().len(), 1);
        assert!(graph.outline().contains("Worktree/src/shared.rs"));
        graph.lock_all();
        assert!(PlanRun::start(&graph).is_err());
        let mut changed = graph.clone();
        changed.node_mut(&"left".into()).expect("left").file_surface = Some(Vec::new());
        assert!(graph.merge_draft(changed.clone()).is_err());
        graph.unlock_all();
        let merged = graph
            .merge_draft(changed)
            .expect("unlocked correction")
            .graph;
        assert_eq!(
            merged.node(&"left".into()).expect("left").file_surface,
            Some(Vec::new())
        );
        assert!(merged.file_surface_problems().is_empty());
    }

    #[test]
    fn a_locked_composite_cannot_gain_a_new_child_file_surface_during_merge() {
        let mut graph = surface_graph(&["parent"], &[]);
        let mut nested = surface_graph(&["child"], &[]);
        let mut leaf = surface_graph(&["leaf"], &[]);
        leaf.node_mut(&"leaf".into()).expect("leaf").file_surface =
            Some(vec!["worktree/leaf.rs".into()]);
        nested.node_mut(&"child".into()).expect("child").subplan = Some(Box::new(leaf));
        graph.node_mut(&"parent".into()).expect("parent").subplan = Some(Box::new(nested));
        let mut omitted_child = graph.clone();
        omitted_child
            .node_at_mut(&NodePath::root("parent".into()).child("child".into()))
            .expect("child")
            .subplan = None;
        assert_eq!(
            graph
                .merge_draft(omitted_child)
                .expect("deep omission preserves files")
                .graph,
            graph
        );
        let mut omitted = graph.clone();
        omitted.node_mut(&"parent".into()).expect("parent").subplan = None;
        assert_eq!(
            graph
                .merge_draft(omitted)
                .expect("omission preserves children")
                .graph,
            graph
        );
        let mut draft = graph.clone();
        let mut child = ArchitectNode::new("new-child", "New child");
        child.file_surface = Some(vec!["worktree/new.rs".into()]);
        draft
            .subplan_mut(&"parent".into())
            .expect("subplan")
            .add_node(child);
        assert!(graph.merge_draft(draft).is_err());
    }

    #[test]
    fn created_files_reach_nested_successors_and_outer_successors_without_touching_siblings() {
        let mut graph = surface_graph(
            &["before", "parent", "after", "unrelated"],
            &[("before", "parent"), ("parent", "after")],
        );
        let mut nested = surface_graph(&["source", "next", "sibling"], &[("source", "next")]);
        nested.node_mut(&"next".into()).expect("next").subplan =
            Some(Box::new(surface_graph(&["deep"], &[])));
        graph.node_mut(&"parent".into()).expect("parent").subplan = Some(Box::new(nested));
        graph.node_mut(&"after".into()).expect("after").subplan =
            Some(Box::new(surface_graph(&["finish"], &[])));
        let source = NodePath::root("parent".into()).child("source".into());
        let snapshot = graph.clone();
        // Root-name and separator aliases deduplicate; filename case may differ on the host.
        let files = vec![
            "worktree\\src\\created.rs".into(),
            "WORKTREE/src/created.rs".into(),
        ];
        let changed = graph.record_created_files(&source, &files);
        let mut expected = vec![
            NodePath::root("parent".into()),
            NodePath::root("parent".into()).child("next".into()),
            NodePath::root("parent".into())
                .child("next".into())
                .child("deep".into()),
            NodePath::root("after".into()),
            NodePath::root("after".into()).child("finish".into()),
        ];
        expected.sort();
        assert_eq!(changed, expected);
        for path in &changed {
            let node = graph.node_at(path).expect("changed step");
            assert_eq!(
                node.file_surface
                    .as_ref()
                    .expect("declared")
                    .last()
                    .map(String::as_str),
                Some("worktree/src/created.rs")
            );
            assert_eq!(
                node.locked,
                snapshot.node_at(path).expect("previous").locked
            );
            assert_eq!(
                node.result,
                snapshot.node_at(path).expect("previous").result
            );
        }
        for path in [
            source.clone(),
            NodePath::root("before".into()),
            NodePath::root("unrelated".into()),
            NodePath::root("parent".into()).child("sibling".into()),
        ] {
            assert_eq!(graph.node_at(&path), snapshot.node_at(&path));
        }
        assert!(graph.record_created_files(&source, &files).is_empty());
        assert!(
            graph
                .record_created_files(&NodePath::root("missing".into()), &files)
                .is_empty()
        );
        assert!(graph.record_created_files(&source, &[]).is_empty());
    }

    #[test]
    fn local_retries_preserve_siblings_but_outer_reentry_widens_future_children() {
        let mut nested = surface_graph(
            &["before", "source", "next", "sibling"],
            &[("before", "source"), ("source", "next"), ("next", "source")],
        );
        nested
            .edges
            .iter_mut()
            .find(|edge| edge.from == NodeId::from("next"))
            .expect("local retry")
            .max_repeats = Some(1);
        nested
            .node_mut(&"sibling".into())
            .expect("sibling")
            .file_surface = Some(vec!["worktree/sibling.rs".into()]);
        let mut graph = surface_graph(&["parent", "after"], &[("parent", "after")]);
        graph.node_mut(&"parent".into()).expect("parent").subplan = Some(Box::new(nested));
        let source = NodePath::root("parent".into()).child("source".into());
        let before = NodePath::root("parent".into()).child("before".into());
        let sibling = NodePath::root("parent".into()).child("sibling".into());
        let snapshot = graph.clone();
        let files = vec!["项目/新文件.rs".into()];
        let changed = graph.record_created_files(&source, &files);
        assert!(
            changed.contains(&source),
            "the local loop revisits the source"
        );
        assert!(changed.contains(&NodePath::root("after".into())));
        assert!(!changed.contains(&before));
        assert!(!changed.contains(&sibling));
        assert_eq!(graph.node_at(&before), snapshot.node_at(&before));
        assert_eq!(graph.node_at(&sibling), snapshot.node_at(&sibling));
        assert!(graph.record_created_files(&source, &files).is_empty());

        let mut retry = ArchitectEdge::new("outer-retry", "after", "parent");
        retry.max_repeats = Some(1);
        graph.edges.push(retry);
        let changed = graph.record_created_files(&source, &files);
        assert!(
            changed.contains(&before),
            "outer reentry restarts the nested plan"
        );
        assert!(
            changed.contains(&sibling),
            "all nested roots run on reentry"
        );
        for path in [&before, &sibling] {
            let node = graph.node_at(path).expect("future nested step");
            assert!(
                node.file_surface
                    .as_ref()
                    .expect("surface")
                    .contains(&files[0])
            );
            assert_eq!(
                node.locked,
                snapshot.node_at(path).expect("previous").locked
            );
            assert_eq!(
                node.result,
                snapshot.node_at(path).expect("previous").result
            );
        }
        assert!(graph.record_created_files(&source, &files).is_empty());
    }

    #[test]
    fn created_files_can_require_serializing_previously_disjoint_successors() {
        let mut graph = ArchitectGraph::default();
        for id in ["source", "left", "right"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        graph.connect("source", "left");
        graph.connect("source", "right");
        graph.lock_all();
        assert!(PlanRun::start(&graph).is_ok());
        let changed = graph.record_created_files(
            &NodePath::root("source".into()),
            &["worktree/new.rs".into()],
        );
        assert_eq!(
            changed,
            vec![
                NodePath::root("left".into()),
                NodePath::root("right".into())
            ]
        );
        assert!(graph.is_fully_locked_deeply());
        assert!(PlanRun::start(&graph).is_err());
        graph.connect("left", "right");
        assert!(PlanRun::start(&graph).is_ok());
    }

    #[test]
    fn creation_propagation_preserves_missing_surfaces_and_fails_closed_on_invalid_paths() {
        let mut graph = surface_graph(
            &["source", "legacy", "after"],
            &[
                ("source", "legacy"),
                ("legacy", "after"),
                ("after", "source"),
            ],
        );
        graph
            .node_mut(&"legacy".into())
            .expect("legacy")
            .file_surface = None;
        let source = NodePath::root("source".into());
        let changed = graph.record_created_files(&source, &["worktree/../invalid.rs".into()]);
        assert!(
            changed.contains(&source),
            "a loop revisits the creating step"
        );
        assert!(changed.contains(&NodePath::root("after".into())));
        assert_eq!(
            graph.node(&"legacy".into()).expect("legacy").file_surface,
            None
        );
        assert!(
            graph
                .file_surface_problems()
                .iter()
                .any(|problem| matches!(problem, GraphProblem::InvalidFileSurface { .. }))
        );
    }

    fn looping_graph() -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for id in ["plan", "edit", "test", "ship"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        graph.connect("plan", "edit");
        graph.connect("edit", "test");
        graph.connect("test", "ship");
        // Failing tests send us back to editing.
        graph
            .edges
            .push(ArchitectEdge::new("retry", "test", "edit").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Did the tests fail?".into(),
                },
            ));
        graph
    }

    #[test]
    fn connecting_after_deletions_never_reuses_a_live_edge_id() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("a", "A"));
        graph.add_node(ArchitectNode::new("b", "B"));
        let first = graph.connect("a", "b");
        let removed = graph.connect("a", "b");
        let retained = graph.connect("a", "b");
        graph.disconnect(&removed);
        let added = graph.connect("a", "b");
        assert_ne!(added, retained);
        assert_ne!(added, first);
        let ids: HashSet<_> = graph.edges.iter().map(|edge| &edge.id).collect();
        assert_eq!(ids.len(), graph.edges.len());
        assert!(graph.edges.iter().any(|edge| edge.id == retained));
    }

    #[test]
    fn blank_and_duplicate_graph_ids_block_execution_at_every_depth() {
        for blank in ["", " \t"] {
            let mut graph = ArchitectGraph::default();
            graph.add_node(ArchitectNode::new(blank, "Invalid step"));
            graph.lock_all();
            assert!(
                graph.problems().iter().any(|problem| matches!(problem, GraphProblem::InvalidNodeId(_)))
            );
            assert!(PlanRun::start(&graph).is_err());

            let mut nested = ArchitectGraph::default();
            nested.add_node(ArchitectNode::new("a", "A"));
            nested.add_node(ArchitectNode::new("b", "B"));
            nested.edges.push(ArchitectEdge::new(blank, "a", "b"));
            nested.lock_all();
            assert!(
                nested.problems().iter().any(|problem| matches!(problem, GraphProblem::InvalidEdgeId(_)))
            );
            graph.nodes.clear();
            let mut parent = ArchitectNode::new("parent", "Parent");
            parent.subplan = Some(Box::new(nested));
            graph.add_node(parent);
            graph.lock_all();
            assert!(PlanRun::start(&graph).is_err());
        }
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("a", "A"));
        graph.add_node(ArchitectNode::new("b", "B"));
        graph.edges.push(ArchitectEdge::new("same", "a", "b"));
        graph.edges.push(ArchitectEdge::new("same", "a", "b"));
        graph.lock_all();
        assert!(
            graph.problems().contains(&GraphProblem::DuplicateEdge("same".into()))
        );
        assert!(PlanRun::start(&graph).is_err());
        assert!(PlanRun::validate_structure(&graph).is_err());
    }

    #[test]
    fn proposals_do_not_silently_rename_duplicate_ids() {
        let proposal: ProposedGraph = serde_json::from_value(serde_json::json!({"nodes": [
            {"id":"same", "title":"First", "file_surface":[]},
            {"id":"same", "title":"Second", "file_surface":[]}
        ]})).unwrap();
        let graph = proposal.into_graph();
        assert!(
            graph.problems().contains(&GraphProblem::DuplicateNode("same".into()))
        );
        assert!(graph.node(&"same-2".into()).is_none());
        let node_schema = serde_json::to_value(schemars::schema_for!(NodeId)).unwrap();
        let edge_schema = serde_json::to_value(schemars::schema_for!(EdgeId)).unwrap();
        assert_eq!(node_schema["minLength"], 1);
        assert_eq!(edge_schema["minLength"], 1);
    }

    #[test]
    fn legacy_deterministic_conditions_load_as_objective_statements() {
        let condition: EdgeCondition = serde_json::from_value(serde_json::json!({
            "kind": "deterministic",
            "expression": "the build failed",
        }))
        .unwrap();
        assert_eq!(
            condition,
            EdgeCondition::Objective {
                statement: "the build failed".into(),
            }
        );

        let serialized = serde_json::to_value(condition).unwrap();
        assert_eq!(serialized["kind"], "objective");
        assert_eq!(serialized["statement"], "the build failed");
        assert!(serialized.get("expression").is_none());
    }

    #[test]
    fn removing_a_node_takes_its_connections_with_it() {
        let mut graph = looping_graph();
        graph.remove_node(&"test".into());

        assert!(graph.node(&"test".into()).is_none());
        assert!(
            graph
                .edges
                .iter()
                .all(|edge| edge.from.0 != "test" && edge.to.0 != "test"),
            "no edge should still refer to the removed step: {:?}",
            graph.edges
        );
        assert!(
            graph.problems().is_empty(),
            "removing a node should not leave the graph broken: {:?}",
            graph.problems()
        );
    }

    #[test]
    fn adding_a_node_with_a_taken_id_gets_a_fresh_one() {
        let mut graph = ArchitectGraph::default();
        let first = graph.add_node(ArchitectNode::new("edit", "Edit"));
        let second = graph.add_node(ArchitectNode::new("edit", "Edit again"));

        assert_eq!(first, NodeId("edit".into()));
        assert_ne!(second, first);
        assert_eq!(graph.nodes.len(), 2);
    }

    #[test]
    fn a_loop_does_not_make_a_reachable_graph_look_unreachable() {
        let graph = looping_graph();
        assert_eq!(graph.problems(), vec![]);
    }

    #[test]
    fn deep_step_count_includes_nested_plans() {
        let mut nested = ArchitectGraph::default();
        let mut locked_child = ArchitectNode::new("child-a", "Child A");
        locked_child.locked = true;
        nested.add_node(locked_child);
        nested.add_node(ArchitectNode::new("child-b", "Child B"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        let mut sibling = ArchitectNode::new("sibling", "Sibling");
        sibling.locked = true;
        graph.add_node(sibling);

        assert_eq!(graph.step_count_deeply(), 4);
        assert_eq!(graph.locked_step_count_deeply(), 2);
    }

    #[test]
    fn execution_order_is_stable_for_branches_cycles_and_disconnected_steps() {
        let mut graph = ArchitectGraph::default();
        for id in ["start", "left", "right", "finish", "detached"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        graph.connect("start", "left");
        graph.connect("start", "right");
        graph.connect("left", "finish");
        graph.connect("right", "finish");
        graph.connect("finish", "left");

        assert_eq!(
            graph.execution_order(),
            vec!["start", "left", "right", "finish", "detached"]
                .into_iter()
                .map(NodeId::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn step_models_are_optional_and_survive_nested_json_round_trips() {
        let legacy: ArchitectNode = serde_json::from_value(serde_json::json!({
            "id": "legacy", "title": "Saved before step models"
        }))
        .expect("legacy steps should load");
        assert!(legacy.model.is_none());
        let serialized = serde_json::to_value(&legacy).expect("step should serialize");
        assert!(serialized.get("model").is_none());

        let graph = draft(serde_json::json!({
            "nodes": [{
                "id": "parent", "title": "Parent",
                "steps": {"nodes": [{
                    "id": "child", "title": "Child",
                    "model": {"provider": "test-provider", "model": "test-model"}
                }]}
            }]
        }));
        let path = NodePath::from(vec!["parent".into(), "child".into()]);
        let model = graph
            .node_at(&path)
            .expect("child should exist")
            .model
            .as_ref()
            .expect("proposal should retain the model");
        assert_eq!(model.provider, "test-provider");
        assert_eq!(model.model, "test-model");
        let saved = serde_json::to_string(&graph).expect("plan should serialize");
        let restored: ArchitectGraph = serde_json::from_str(&saved).expect("plan should load");
        assert_eq!(restored, graph);
        assert!(
            restored
                .outline()
                .contains("model: test-provider/test-model")
        );
        assert!(restored.outline().contains("model: inherit plan"));
    }

    #[test]
    fn redrafting_preserves_omitted_models_and_protects_locked_models_deeply() {
        let proposal = serde_json::json!({
            "nodes": [{
                "id": "parent", "title": "Parent",
                "steps": {"nodes": [{"id": "child", "title": "Child"}]}
            }]
        });
        let mut graph = draft(proposal.clone());
        let path = NodePath::from(vec!["parent".into(), "child".into()]);
        let model = StepModel {
            provider: "test-provider".into(),
            model: "test-model".into(),
        };
        graph
            .mutate_node_at(&path, |node| node.model = Some(model.clone()))
            .expect("child should be editable");
        for locked in [false, true] {
            graph
                .set_locked_at(&path, locked)
                .expect("lock should change");
            let merged = graph
                .merge_draft(draft(proposal.clone()))
                .expect("omission should preserve");
            assert_eq!(
                merged
                    .graph
                    .node_at(&path)
                    .expect("child should exist")
                    .model,
                Some(model.clone())
            );
        }
        let mut replacement = draft(proposal);
        replacement
            .mutate_node_at(&path, |node| {
                node.model = Some(StepModel {
                    provider: "test-provider".into(),
                    model: "other-model".into(),
                });
            })
            .expect("draft should be editable");
        assert!(graph.merge_draft(replacement.clone()).is_err());
        graph
            .set_locked_at(&path, false)
            .expect("child should unlock");
        let merged = graph
            .merge_draft(replacement)
            .expect("unlocked override can change");
        assert_eq!(
            merged
                .graph
                .node_at(&path)
                .expect("child should exist")
                .model
                .as_ref()
                .expect("override should exist")
                .model,
            "other-model"
        );
    }

    #[test]
    fn responsibility_is_optional_for_saved_and_proposed_graphs() {
        let saved: ArchitectNode = serde_json::from_value(serde_json::json!({
            "id": "inspect",
            "title": "Inspect",
        }))
        .unwrap();
        assert!(saved.responsibility.is_empty());

        let proposed = draft(serde_json::json!({
            "nodes": [{
                "id": "inspect",
                "title": "Inspect",
                "responsibility": "Diagnostics"
            }]
        }));
        assert_eq!(proposed.nodes[0].responsibility, "Diagnostics");
    }

    /// A plan that ends by looping back to its first step has nothing with a
    /// clear "nothing leads here" entry, but it is perfectly ordinary.
    #[test]
    fn a_plan_that_loops_back_to_its_first_step_is_fine() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("reproduce", "Reproduce"));
        graph.add_node(ArchitectNode::new("fix", "Fix"));
        graph.connect("reproduce", "fix");
        graph.connect_with(
            "fix",
            "reproduce",
            EdgeCondition::LlmEvaluated {
                question: "Does it still fail?".into(),
            },
        );

        assert_eq!(graph.roots(), vec![NodeId("reproduce".into())]);
        assert_eq!(graph.problems(), vec![]);
    }

    #[test]
    fn a_locked_step_can_be_moved_but_not_edited() {
        let mut graph = ArchitectGraph::default();
        let mut settled = ArchitectNode::new("settled", "Settled");
        settled.locked = true;
        graph.add_node(settled);
        let path = NodePath::root(NodeId("settled".into()));
        let position = Position { x: 40.0, y: 80.0 };

        graph.move_node_at(&path, position).unwrap();
        assert_eq!(graph.node_at(&path).unwrap().position, Some(position));
        let edit = graph.mutate_node_at(&path, |node| node.title.clear());
        assert!(edit.is_err(), "a locked step keeps its brief");
    }

    #[test]
    fn problems_describe_steps_by_title() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("draft-step", "Draft the change"));
        assert_eq!(
            GraphProblem::Unlocked(NodeId("draft-step".into())).describe(&graph),
            "\"Draft the change\" is not locked yet"
        );
    }

    #[test]
    fn a_loop_with_no_way_out_is_reported_until_it_gets_one() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("start", "Start"));
        graph.add_node(ArchitectNode::new("draft", "Draft"));
        graph.add_node(ArchitectNode::new("review", "Review"));
        graph.connect("start", "draft");
        graph.connect("draft", "review");
        let back = graph.connect("review", "draft");

        assert_eq!(
            graph.problems(),
            vec![GraphProblem::EndlessLoop(NodeId("draft".into()))],
            "the loop should be reported once, by its first step"
        );
        let loop_edges: Vec<&EdgeId> = graph
            .edges
            .iter()
            .filter(|edge| graph.is_loop_edge(edge))
            .map(|edge| &edge.id)
            .collect();
        assert_eq!(
            loop_edges.len(),
            2,
            "only the connections inside the loop are loop edges"
        );

        graph
            .set_edge_max_repeats_at(&NodePath::default(), &back, Some(0))
            .unwrap();
        let limited = graph.edges.iter().find(|edge| edge.id == back).unwrap();
        assert_eq!(
            limited.max_repeats,
            Some(1),
            "a limit of zero is stored as one"
        );
        assert_eq!(graph.problems(), vec![], "a repeat limit is a way out");
    }

    #[test]
    fn reports_a_step_nothing_leads_to() {
        let mut graph = looping_graph();
        graph.add_node(ArchitectNode::new("orphan", "Orphan"));
        // An orphan with no incoming edge is a root, so give it one from
        // another orphan to make it genuinely unreachable.
        graph.add_node(ArchitectNode::new("orphan-source", "Orphan source"));
        graph.connect("orphan-source", "orphan");
        graph.connect("orphan", "orphan-source");

        let problems = graph.problems();
        assert!(
            problems.contains(&GraphProblem::Unreachable(NodeId("orphan".into()))),
            "expected the orphan to be reported, got {problems:?}"
        );
    }

    #[test]
    fn reports_an_empty_routing_condition() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("plan", "Plan"));
        graph.add_node(ArchitectNode::new("build", "Build"));
        let edge = graph.connect_with(
            "plan",
            "build",
            EdgeCondition::Objective {
                statement: "  ".into(),
            },
        );

        assert!(
            graph
                .problems()
                .contains(&GraphProblem::EmptyCondition(edge))
        );
    }

    #[test]
    fn reports_an_edge_pointing_at_a_missing_step() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("plan", "Plan"));
        graph.connect("plan", "nowhere");

        let problems = graph.problems();
        assert!(
            problems.iter().any(|problem| matches!(
                problem,
                GraphProblem::DanglingEdge { missing, .. } if missing.0 == "nowhere"
            )),
            "expected a dangling edge, got {problems:?}"
        );
    }

    #[test]
    fn locking_reports_when_the_graph_is_ready() {
        let mut graph = looping_graph();
        assert!(!graph.is_fully_locked());

        let ids: Vec<NodeId> = graph.nodes.iter().map(|node| node.id.clone()).collect();
        for id in &ids {
            graph.set_locked(id, true);
        }
        assert!(graph.is_fully_locked());

        graph.set_locked(&ids[0], false);
        assert!(!graph.is_fully_locked());
    }

    #[test]
    fn an_empty_graph_is_never_ready_to_run() {
        let graph = ArchitectGraph::default();
        assert!(!graph.is_fully_locked());
    }

    #[test]
    fn a_completed_plan_composes_child_results_in_graph_order() {
        let mut graph = ArchitectGraph::default();
        let mut first = ArchitectNode::new("inspect", "Inspect");
        first.result = Some(StepResult {
            summary: "Found the failing boundary.".into(),
            attempt: 1,
        });
        graph.add_node(first);
        let mut second = ArchitectNode::new("fix", "Fix");
        second.result = Some(StepResult {
            summary: "Corrected the boundary check.".into(),
            attempt: 1,
        });
        graph.add_node(second);

        let result = graph.completion_result(2).unwrap();
        assert_eq!(result.attempt, 2);
        assert_eq!(
            result.summary,
            "- Inspect: Found the failing boundary.\n- Fix: Corrected the boundary check."
        );
    }

    #[test]
    fn clearing_results_descends_into_nested_plans() {
        let mut nested = ArchitectGraph::default();
        let mut child = ArchitectNode::new("child", "Child");
        child.result = Some(StepResult {
            summary: "old child result".into(),
            attempt: 1,
        });
        nested.add_node(child);
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.result = Some(StepResult {
            summary: "old parent result".into(),
            attempt: 1,
        });
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);

        graph.clear_results();

        let parent = graph.node(&NodeId::from("parent")).unwrap();
        assert!(parent.result.is_none());
        assert!(
            parent
                .subplan()
                .unwrap()
                .node(&NodeId::from("child"))
                .unwrap()
                .result
                .is_none()
        );
    }

    #[test]
    fn checked_mutation_uses_the_full_nested_path() {
        let mut first = ArchitectGraph::default();
        first.add_node(ArchitectNode::new("same", "First same"));
        let mut second = ArchitectGraph::default();
        second.add_node(ArchitectNode::new("same", "Second same"));

        let mut graph = ArchitectGraph::default();
        let mut first_parent = ArchitectNode::new("first", "First");
        first_parent.subplan = Some(Box::new(first));
        graph.add_node(first_parent);
        let mut second_parent = ArchitectNode::new("second", "Second");
        second_parent.subplan = Some(Box::new(second));
        graph.add_node(second_parent);

        graph
            .mutate_node_at(
                &NodePath::from(vec!["second".into(), "same".into()]),
                |node| {
                    node.intent = "Only the second nested step".into();
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
            graph
                .node_at(&NodePath::from(vec!["second".into(), "same".into()]))
                .unwrap()
                .intent,
            "Only the second nested step"
        );
    }

    #[test]
    fn checked_mutation_refuses_locked_targets_and_ancestors() {
        let mut inner = ArchitectGraph::default();
        inner.add_node(ArchitectNode::new("child", "Child"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(inner));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);

        let child = NodePath::from(vec!["parent".into(), "child".into()]);
        graph.set_locked_at(&child, true).unwrap();
        assert_eq!(
            graph.mutate_node_at(&child, |_| ()),
            Err(GraphMutationError::Locked {
                path: child.clone()
            })
        );

        graph
            .set_locked_at(&NodePath::from(NodeId::from("parent")), true)
            .unwrap();
        assert_eq!(
            graph.mutate_node_at(&child, |_| ()),
            Err(GraphMutationError::Locked {
                path: NodePath::from(NodeId::from("parent"))
            })
        );
    }

    #[test]
    fn checked_route_edits_refuse_locked_sources() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("from", "From"));
        graph.add_node(ArchitectNode::new("to", "To"));
        let edge = graph.connect("from", "to");
        let from = NodePath::root("from".into());
        graph.set_locked_at(&from, true).unwrap();

        assert_eq!(
            graph.disconnect_at(&NodePath::default(), &edge),
            Err(GraphMutationError::Locked { path: from.clone() })
        );
        assert_eq!(
            graph.set_edge_condition_at(
                &NodePath::default(),
                &edge,
                EdgeCondition::Objective {
                    statement: "tests passed".into(),
                },
            ),
            Err(GraphMutationError::Locked { path: from.clone() })
        );
        assert_eq!(
            graph.connect_from_at(&from, "to".into(), EdgeCondition::Always),
            Err(GraphMutationError::Locked { path: from })
        );
    }

    #[test]
    fn nested_graph_mutation_refuses_a_locked_container() {
        let mut nested = ArchitectGraph::default();
        nested.add_node(ArchitectNode::new("child", "Child"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        let parent = NodePath::root("parent".into());
        graph
            .set_locked_at(&parent.child("child".into()), true)
            .unwrap();
        graph.set_locked_at(&parent, true).unwrap();

        assert_eq!(
            graph.mutate_graph_at(&parent, |nested| nested.nodes.clear()),
            Err(GraphMutationError::Locked { path: parent })
        );
    }

    #[test]
    fn locking_a_parent_requires_its_nested_plan_to_be_settled() {
        let mut inner = ArchitectGraph::default();
        inner.add_node(ArchitectNode::new("child", "Child"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(inner));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        let parent = NodePath::from(NodeId::from("parent"));

        assert_eq!(
            graph.set_locked_at(&parent, true),
            Err(GraphMutationError::NestedPlanUnlocked {
                path: parent.clone()
            })
        );
        graph
            .set_locked_at(&NodePath::from(vec!["parent".into(), "child".into()]), true)
            .unwrap();
        graph.set_locked_at(&parent, true).unwrap();
        assert!(graph.node(&NodeId::from("parent")).unwrap().locked);
    }

    fn two_level_plan() -> ArchitectGraph {
        let mut grandchild_plan = ArchitectGraph::default();
        grandchild_plan.add_node(ArchitectNode::new("grandchild", "Grandchild"));
        let mut child = ArchitectNode::new("child", "Child");
        child.subplan = Some(Box::new(grandchild_plan));
        let mut inner = ArchitectGraph::default();
        inner.add_node(child);
        inner.add_node(ArchitectNode::new("sibling", "Sibling"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(inner));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        graph.add_node(ArchitectNode::new("other", "Other"));
        graph
    }

    #[test]
    fn locking_deeply_settles_a_step_and_everything_inside_it() {
        let mut graph = two_level_plan();
        let parent = NodePath::from(NodeId::from("parent"));
        graph
            .set_locked_at(
                &NodePath::from(vec!["parent".into(), "sibling".into()]),
                true,
            )
            .unwrap();

        assert_eq!(graph.lock_deeply_at(&parent), Ok(3));
        let inner = graph.graph_at(&parent).unwrap();
        assert!(inner.is_fully_locked_deeply());
        assert!(graph.node(&NodeId::from("parent")).unwrap().locked);
        assert!(
            !graph.node(&NodeId::from("other")).unwrap().locked,
            "steps outside the one locked are left alone"
        );
        assert_eq!(
            graph.lock_deeply_at(&parent),
            Ok(0),
            "locking again changes nothing"
        );

        graph.set_locked_at(&parent, false).unwrap();
        assert!(
            graph.graph_at(&parent).unwrap().is_fully_locked_deeply(),
            "unlocking reopens only the step itself"
        );
        assert_eq!(
            graph.lock_deeply_at(&NodePath::from(vec!["other".into(), "missing".into()])),
            Err(GraphMutationError::MissingSubplan {
                path: NodePath::from(NodeId::from("other"))
            })
        );
    }

    #[test]
    fn a_nested_step_cannot_be_locked_through_a_locked_parent() {
        let mut graph = two_level_plan();
        let parent = NodePath::from(NodeId::from("parent"));
        graph.lock_deeply_at(&parent).unwrap();
        let child = NodePath::from(vec!["parent".into(), "child".into()]);
        assert_eq!(
            graph.lock_deeply_at(&child),
            Err(GraphMutationError::Locked { path: parent })
        );
    }

    #[test]
    fn lock_all_settles_every_step_at_every_depth() {
        let mut graph = two_level_plan();
        assert_eq!(graph.lock_all(), graph.step_count_deeply());
        assert!(graph.is_fully_locked_deeply());
        assert_eq!(graph.lock_all(), 0);
    }

    #[test]
    fn unlock_all_reopens_every_step_at_every_depth() {
        let mut graph = two_level_plan();
        let total = graph.lock_all();
        assert_eq!(graph.unlock_all(), total);
        assert_eq!(graph.locked_step_count_deeply(), 0);
        assert_eq!(graph.unlock_all(), 0, "unlocking again changes nothing");
    }

    /// A settled plan, as it would be after the user argued the steps out and
    /// locked one of them.
    fn settled_plan() -> ArchitectGraph {
        let mut graph: ArchitectGraph = ArchitectGraph::default();
        graph.add_node(ArchitectNode {
            intent: "Every migration is reversible".into(),
            rules: vec!["Do not change the token format".into()],
            capture: "The migration number".into(),
            locked: true,
            chat: Some(acp::SessionId::new("chat-schema")),
            result: Some(StepResult {
                summary: "Added expires_at, migration 0007".into(),
                attempt: 2,
            }),
            position: Some(Position { x: 40.0, y: 80.0 }),
            pinned: true,
            ..ArchitectNode::new("schema", "Define schema")
        });
        graph.add_node(ArchitectNode {
            intent: "Endpoints behave per schema".into(),
            capture: "Which endpoints changed".into(),
            chat: Some(acp::SessionId::new("chat-handlers")),
            ..ArchitectNode::new("handlers", "Write handlers")
        });
        graph
            .edges
            .push(ArchitectEdge::new("schema->handlers", "schema", "handlers"));
        graph
    }

    fn draft(mut json: serde_json::Value) -> ArchitectGraph {
        // These historical merge fixtures exercise other fields; their steps
        // explicitly anticipate no existing files. Required serde is tested raw.
        fn declare_surfaces(graph: &mut serde_json::Value) {
            if let Some(nodes) = graph
                .get_mut("nodes")
                .and_then(serde_json::Value::as_array_mut)
            {
                for node in nodes {
                    if node.get("file_surface").is_none() {
                        node["file_surface"] = serde_json::json!([]);
                    }
                    if let Some(steps) = node.get_mut("steps") {
                        declare_surfaces(steps);
                    }
                }
            }
        }
        declare_surfaces(&mut json);
        serde_json::from_value::<ProposedGraph>(json)
            .unwrap()
            .into_graph()
    }

    #[test]
    fn a_redraw_that_drops_a_locked_step_is_refused() {
        // The failure this guards against lost an afternoon of deliberation to a
        // single "revise the plan", with nothing on screen to say it had gone.
        let settled = settled_plan();
        let refusal = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [{ "id": "handlers", "title": "Write handlers" }],
            })))
            .expect_err("dropping a locked step must be refused");

        assert_eq!(refusal.steps, vec!["Define schema"]);
    }

    #[test]
    fn a_redraw_that_rewrites_a_locked_step_is_refused() {
        let settled = settled_plan();
        let refusal = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [
                    { "id": "schema", "title": "Define schema", "intent": "Something else" },
                    { "id": "handlers", "title": "Write handlers" },
                ],
            })))
            .expect_err("rewriting a locked step must be refused");

        assert_eq!(refusal.steps, vec!["Define schema"]);
    }

    #[test]
    fn a_redraw_that_reroutes_a_locked_step_is_refused() {
        // Where a settled step leads was settled with it.
        let settled = settled_plan();
        let refusal = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [
                    {
                        "id": "schema",
                        "title": "Define schema",
                        "intent": "Every migration is reversible",
                        "rules": ["Do not change the token format"],
                        "capture": "The migration number",
                    },
                    { "id": "handlers", "title": "Write handlers" },
                    { "id": "tests", "title": "Add tests" },
                ],
                "edges": [{ "from": "schema", "to": "tests" }],
            })))
            .expect_err("rerouting a locked step must be refused");

        assert_eq!(refusal.steps, vec!["Define schema"]);
    }

    #[test]
    fn a_redraw_may_reshape_the_plan_around_a_locked_step() {
        // Restating the locked step exactly is allowed: that is what preserving
        // it looks like. Everything else about the plan is the draft's business.
        let settled = settled_plan();
        let merged = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [
                    {
                        "id": "schema",
                        "title": "Define schema",
                        "intent": "Every migration is reversible",
                        "rules": ["Do not change the token format"],
                        "capture": "The migration number",
                    },
                    { "id": "handlers", "title": "Write handlers" },
                    { "id": "ship", "title": "Ship it" },
                ],
                "edges": [
                    { "from": "schema", "to": "handlers" },
                    { "from": "handlers", "to": "ship" },
                ],
            })))
            .expect("reshaping around a locked step is allowed");

        assert_eq!(merged.graph.nodes.len(), 3);
        assert!(merged.graph.node(&NodeId::from("ship")).is_some());
    }

    #[test]
    fn a_redraw_keeps_what_a_draft_cannot_know() {
        let settled = settled_plan();
        let merged = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [
                    {
                        "id": "schema",
                        "title": "Define schema",
                        "intent": "Every migration is reversible",
                        "rules": ["Do not change the token format"],
                        "capture": "The migration number",
                    },
                    { "id": "handlers", "title": "Write handlers" },
                ],
                "edges": [{ "from": "schema", "to": "handlers" }],
            })))
            .unwrap();

        let schema = merged.graph.node(&NodeId::from("schema")).unwrap();
        assert!(schema.locked, "a settled step must come back settled");
        assert!(schema.pinned);
        assert_eq!(schema.position, Some(Position { x: 40.0, y: 80.0 }));
        assert_eq!(schema.chat, Some(acp::SessionId::new("chat-schema")));
        assert_eq!(
            schema.result.as_ref().map(|result| result.attempt),
            Some(2),
            "what a step reported when it ran is not the draft's to discard"
        );

        // The draft said nothing about this step beyond its title, which is not
        // the same as saying it has no goal.
        let handlers = merged.graph.node(&NodeId::from("handlers")).unwrap();
        assert_eq!(handlers.intent, "Endpoints behave per schema");
        assert_eq!(handlers.capture, "Which endpoints changed");
        assert_eq!(
            handlers.chat,
            Some(acp::SessionId::new("chat-handlers")),
            "redrawing a plan must not orphan a step's conversation"
        );

        assert!(merged.preserved.contains(&NodeId::from("handlers")));
    }

    #[test]
    fn a_redraw_may_still_replace_detail_it_states() {
        // Preserving blanks must not become refusing to edit.
        let settled = settled_plan();
        let merged = settled
            .merge_draft(draft(serde_json::json!({
                "nodes": [
                    {
                        "id": "schema",
                        "title": "Define schema",
                        "intent": "Every migration is reversible",
                        "rules": ["Do not change the token format"],
                        "capture": "The migration number",
                    },
                    {
                        "id": "handlers",
                        "title": "Write handlers",
                        "intent": "A better goal",
                    },
                ],
                "edges": [{ "from": "schema", "to": "handlers" }],
            })))
            .unwrap();

        assert_eq!(
            merged.graph.node(&NodeId::from("handlers")).unwrap().intent,
            "A better goal"
        );
    }

    #[test]
    fn the_outline_gives_the_model_ids_and_what_is_settled() {
        let outline = settled_plan().outline();

        assert!(
            outline.contains("schema"),
            "ids are how a redraw matches up"
        );
        assert!(outline.contains("[locked: settled, do not rewrite]"));
        assert!(outline.contains("goal: Every migration is reversible"));
        assert!(outline.contains("rule: Do not change the token format"));
        assert!(outline.contains("leads to handlers (always)"));
        assert!(
            outline.contains("has its own conversation"),
            "the model has to know a step was argued out elsewhere"
        );
    }

    #[test]
    fn a_proposal_arrives_laid_out() {
        let proposal: ProposedGraph = serde_json::from_value(serde_json::json!({
            "nodes": [
                { "id": "plan", "title": "Plan the change", "file_surface": [] },
                { "id": "edit", "title": "Make the edit", "file_surface": [], "rules": ["Keep the public API stable"] },
            ],
            "edges": [
                { "from": "plan", "to": "edit" },
            ],
        }))
        .unwrap();

        let graph = proposal.into_graph();

        assert_eq!(graph.nodes.len(), 2);
        assert!(
            graph.nodes.iter().all(|node| node.position.is_some()),
            "every proposed node should be placed"
        );
        assert_eq!(graph.nodes[1].rules, vec!["Keep the public API stable"]);
        assert!(graph.edges[0].condition.is_always());
        assert!(
            graph.nodes.iter().all(|node| !node.locked),
            "a proposal should never arrive locked"
        );
    }

    #[test]
    fn a_graph_survives_a_round_trip_through_json() {
        let mut graph = looping_graph();
        graph.place_unpositioned_nodes();
        graph.set_locked(&"plan".into(), true);

        let json = serde_json::to_string(&graph).unwrap();
        let restored: ArchitectGraph = serde_json::from_str(&json).unwrap();

        assert_eq!(graph, restored);
    }

    #[test]
    fn placing_nodes_leaves_positions_the_user_chose() {
        let mut graph = looping_graph();
        let pinned = Position { x: -500.0, y: 42.0 };
        graph.node_mut(&"edit".into()).unwrap().position = Some(pinned);

        graph.place_unpositioned_nodes();

        assert_eq!(graph.node(&"edit".into()).unwrap().position, Some(pinned));
        assert!(graph.nodes.iter().all(|node| node.position.is_some()));
    }
}
