//! The graph model behind Architect mode.
//!
//! A graph is a plan: each node is a step the agent will carry out, and each
//! edge is the condition under which one step leads to another. The model
//! proposes the shape, the user reshapes it on a canvas, and once every step is
//! locked the graph is compiled into an ordered spec for the agent to follow.
//!
//! This crate is deliberately free of UI and app dependencies so the graph can
//! be stored alongside a thread and exercised in plain unit tests.

mod layout;
mod run;
mod spec;

pub use layout::{COLUMN_SPACING, Position, ROW_SPACING, layout_positions};
pub use run::{
    Branch, Decision, MAX_NODE_VISITS, MAX_PLAN_DEPTH, MAX_RUN_STEPS, PlanRun, RunOutcome,
    RunRefusal, branch_prompt, parse_verdict, step_prompt,
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
pub struct NodeId(pub String);

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EdgeId(pub String);

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
    /// A condition that can be checked without asking a model, such as a
    /// command's exit status or whether a file exists.
    Deterministic { expression: String },
    /// A judgement the model has to make, phrased as a yes-or-no question.
    LlmEvaluated { question: String },
}

impl EdgeCondition {
    pub fn label(&self) -> Option<&str> {
        match self {
            EdgeCondition::Always => None,
            EdgeCondition::Deterministic { expression } => Some(expression),
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    /// The step's own account of what it did, answering its `capture`.
    pub summary: String,
    /// Which attempt produced this, counting from 1. A step reached twice by a
    /// loop is on attempt 2.
    #[serde(default)]
    pub attempt: usize,
}

/// A single step in the plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchitectNode {
    pub id: NodeId,
    pub title: String,
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
    pub position: Option<Position>,
    /// A locked node is finished being deliberated and can no longer be edited.
    #[serde(default)]
    pub locked: bool,
    /// The thread used to deliberate this step, created on first use.
    #[serde(default)]
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

    /// The nested plan, if this step has one worth running.
    pub fn subplan(&self) -> Option<&ArchitectGraph> {
        self.subplan.as_deref().filter(|graph| !graph.is_empty())
    }

    pub fn has_subplan(&self) -> bool {
        self.subplan().is_some()
    }
}

impl From<&str> for ArchitectNode {
    fn from(value: &str) -> Self {
        Self::new(value, value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchitectEdge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    #[serde(default = "always_condition")]
    pub condition: EdgeCondition,
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
        }
    }

    pub fn with_condition(mut self, condition: EdgeCondition) -> Self {
        self.condition = condition;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchitectGraph {
    #[serde(default)]
    pub nodes: Vec<ArchitectNode>,
    #[serde(default)]
    pub edges: Vec<ArchitectEdge>,
}

/// Something wrong with the graph that the user should see before running it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphProblem {
    DuplicateNode(NodeId),
    /// An edge referring to a node that is not in the graph.
    DanglingEdge {
        edge: EdgeId,
        missing: NodeId,
    },
    /// A node no entry point can reach, so it would never run.
    Unreachable(NodeId),
    /// A step still open for deliberation.
    Unlocked(NodeId),
    /// Something wrong inside a step's nested plan.
    InSubplan {
        node: NodeId,
        problem: Box<GraphProblem>,
    },
}

impl Display for GraphProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphProblem::DuplicateNode(id) => {
                write!(formatter, "more than one step uses the id {id}")
            }
            GraphProblem::DanglingEdge { edge, missing } => write!(
                formatter,
                "connection {} points at a step that does not exist: {missing}",
                edge.0
            ),
            GraphProblem::Unreachable(id) => {
                write!(formatter, "nothing leads to {id}, so it would never run")
            }
            GraphProblem::Unlocked(id) => write!(formatter, "{id} is not locked yet"),
            GraphProblem::InSubplan { node, problem } => {
                write!(formatter, "inside {node}: {problem}")
            }
        }
    }
}

/// Where a step sits, as the ids walked from the top-level plan down to it.
///
/// A bare `NodeId` stops meaning anything once plans nest, because the same id
/// may exist in several sub-plans.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
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
}

impl Display for NodePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joined: Vec<&str> = self.0.iter().map(|id| id.0.as_str()).collect();
        write!(formatter, "{}", joined.join(" / "))
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
fn routes_from(graph: &ArchitectGraph, id: &NodeId) -> Vec<(String, EdgeCondition)> {
    let mut routes: Vec<(String, EdgeCondition)> = graph
        .edges_from(id)
        .map(|edge| (edge.to.0.clone(), edge.condition.clone()))
        .collect();
    routes.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.label().unwrap_or("").cmp(b.1.label().unwrap_or("")))
    });
    routes
}

impl ArchitectGraph {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
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
                        || drafted.intent.trim() != settled.intent.trim()
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
            if node.subplan.is_none() {
                node.subplan = existing.subplan.clone();
            }

            // Detail a draft leaves blank is detail it did not mean to remove.
            // A redraw that only changes the shape of the plan should not empty
            // out the steps it keeps.
            let mut kept_detail = false;
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
                    EdgeCondition::Deterministic { expression } => format!("if {expression}"),
                    EdgeCondition::LlmEvaluated { question } => {
                        format!("model decides: {question}")
                    }
                };
                let _ = writeln!(out, "{pad}  leads to {} ({when})", edge.to.0);
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
        let id = EdgeId(format!("{}->{}", from.0, to.0));
        let id = if self.edges.iter().any(|edge| edge.id == id) {
            EdgeId(format!("{}-{}", id.0, self.edges.len()))
        } else {
            id
        };
        self.edges.push(ArchitectEdge {
            id: id.clone(),
            from,
            to,
            condition: EdgeCondition::Always,
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

    /// The plan a path addresses: the nested plan inside the addressed step, or
    /// this graph itself for an empty path.
    pub fn graph_at(&self, path: &NodePath) -> Option<&ArchitectGraph> {
        let mut graph = self;
        for id in &path.0 {
            graph = graph.node(id)?.subplan()?;
        }
        Some(graph)
    }

    pub fn graph_at_mut(&mut self, path: &NodePath) -> Option<&mut ArchitectGraph> {
        let mut graph = self;
        for id in &path.0 {
            graph = graph.node_mut(id)?.subplan.as_deref_mut()?;
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
            if !seen.insert(node.id.clone()) {
                problems.push(GraphProblem::DuplicateNode(node.id.clone()));
            }
        }

        for edge in &self.edges {
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
    /// A short stable identifier, such as `run-tests`.
    pub id: NodeId,
    /// A short human-readable name for the step.
    pub title: String,
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
            graph.add_node(ArchitectNode {
                id: node.id,
                title: node.title,
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

    /// A plan that ends by looping back to its first step has nothing with a
    /// clear "nothing leads here" entry, but it is perfectly ordinary.
    #[test]
    fn a_plan_that_loops_back_to_its_first_step_is_fine() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("reproduce", "Reproduce"));
        graph.add_node(ArchitectNode::new("fix", "Fix"));
        graph.connect("reproduce", "fix");
        graph.connect("fix", "reproduce");

        assert_eq!(graph.roots(), vec![NodeId("reproduce".into())]);
        assert_eq!(graph.problems(), vec![]);
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

    fn draft(json: serde_json::Value) -> ArchitectGraph {
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
                { "id": "plan", "title": "Plan the change" },
                { "id": "edit", "title": "Make the edit", "rules": ["Keep the public API stable"] },
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
