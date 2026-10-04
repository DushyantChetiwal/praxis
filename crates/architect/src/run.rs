//! Driving a finished plan one step at a time, through nested plans and loops.
//!
//! Compiling a plan into a spec (see [`crate::compile_spec`]) hands the whole
//! graph to the model at once and trusts it to follow the control flow. That
//! trust is the weak point: a model reading "go back to step 2 if the tests
//! fail" may quietly decide it has done enough. This module keeps the control
//! flow out of the model's hands. The run holds the position in the graph, the
//! model is told about one step at a time, and every branch is a question put
//! to it in isolation with the answer read back here.
//!
//! A step that contains a plan of its own is not carried out directly: the run
//! descends into it, and the step is finished when its plan is. Which is why
//! position is a stack of frames rather than a single node.
//!
//! A step with several plain connections out of it forks: every branch runs,
//! each as a lane of its own (see [`PlanRun::fork_lanes`]), until the branches
//! meet at their join, where the run carries on once all of them are done. A
//! lane is itself a `PlanRun`, so a branch can nest, loop, and fork again.
//!
//! Nothing here talks to a model or a UI. It is a state machine: the caller asks
//! what to do next, does it, and reports what happened. That keeps the part that
//! is easy to get wrong testable without a model in the loop.

use crate::{ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, GraphProblem, NodeId, NodePath};
use collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt::{self, Display, Write};

/// How many steps a run may take before it is stopped, counted across every
/// level. A plan with loops has no natural length, so the run needs a ceiling.
pub const MAX_RUN_STEPS: usize = 200;

/// How many times a single step may be entered before the run is stopped. A
/// tighter bound on the failure that actually happens: a retry loop whose exit
/// condition is never satisfied.
pub const MAX_NODE_VISITS: usize = 25;

/// How deeply plans may nest before a run refuses to descend further.
pub const MAX_PLAN_DEPTH: usize = 5;

/// Why a run would not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunRefusal {
    NothingToRun,
    NotReady(Vec<GraphProblem>),
    /// A run was asked to start at a step the plan does not have.
    NoSuchStep(NodePath),
}

impl Display for RunRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunRefusal::NothingToRun => write!(formatter, "there are no steps to run"),
            RunRefusal::NoSuchStep(path) => write!(formatter, "the plan has no step {path}"),
            RunRefusal::NotReady(problems) => {
                let described: Vec<String> =
                    problems.iter().map(|problem| problem.to_string()).collect();
                write!(formatter, "{}", described.join("; "))
            }
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOutcome {
    Completed,
    StepLimit { steps: usize },
    NodeLimit { node: NodeId, visits: usize },
    DepthLimit { node: NodeId },
    Failed { message: String },
    Cancelled,
}

impl RunOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, RunOutcome::Completed)
    }

    /// Whether the run was cut short, by being stopped or by a step that could
    /// not run, rather than ending where the plan or its limits say. Only such
    /// a run is worth picking up where it left off.
    pub fn is_resumable(&self) -> bool {
        matches!(self, RunOutcome::Cancelled | RunOutcome::Failed { .. })
    }

    /// A sentence describing the end of the run, for the transcript.
    pub fn describe(&self, graph: &ArchitectGraph) -> String {
        let title = |id: &NodeId| find_title(graph, id).unwrap_or_else(|| id.0.clone());
        match self {
            RunOutcome::Completed => {
                "The plan is complete; every step has been carried out.".into()
            }
            RunOutcome::StepLimit { steps } => format!(
                "The run was stopped after {steps} steps, which means the plan is looping without \
                 ever reaching an end. Do not carry on; say which step kept repeating and what \
                 would have to change for it to finish."
            ),
            RunOutcome::NodeLimit { node, visits } => format!(
                "The run was stopped because \"{}\" was entered {visits} times without its exit \
                 condition ever being met. Do not carry on; say what kept failing and what would \
                 have to change for it to pass.",
                title(node)
            ),
            RunOutcome::DepthLimit { node } => format!(
                "The run was stopped because plans nested more than {MAX_PLAN_DEPTH} deep at \
                 \"{}\". Flatten that part of the plan before running it again.",
                title(node)
            ),
            RunOutcome::Failed { message } => format!("The run failed: {message}"),
            RunOutcome::Cancelled => "The run was cancelled before it finished.".into(),
        }
    }

    /// A short account of the end of the run for the user. `describe` is
    /// addressed to the model, and tells it what to do next.
    pub fn summary(&self, graph: &ArchitectGraph) -> String {
        let title = |id: &NodeId| find_title(graph, id).unwrap_or_else(|| id.0.clone());
        match self {
            RunOutcome::Completed => "Plan complete".into(),
            RunOutcome::StepLimit { steps } => {
                format!("Stopped after {steps} steps because the plan kept looping")
            }
            RunOutcome::NodeLimit { node, visits } => format!(
                "Stopped after \"{}\" ran {visits} times; a repeat limit lets its loop move on",
                title(node)
            ),
            RunOutcome::DepthLimit { node } => {
                format!("Stopped because plans nest too deep at \"{}\"", title(node))
            }
            RunOutcome::Failed { message } => format!("Run failed: {message}"),
            RunOutcome::Cancelled => "Run stopped".into(),
        }
    }
}

fn find_title(graph: &ArchitectGraph, id: &NodeId) -> Option<String> {
    if let Some(node) = graph.node(id) {
        return Some(node.title.clone());
    }
    graph
        .nodes
        .iter()
        .filter_map(|node| node.subplan())
        .find_map(|subplan| find_title(subplan, id))
}

/// One way out of the step the run is sitting on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub edge: EdgeId,
    pub to: NodeId,
    pub condition: EdgeCondition,
}

/// What the caller should do next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Carry out this step, then report it with [`PlanRun::finish_step`].
    Run(NodePath),
    /// Put this question to the agent, then report the answer with
    /// [`PlanRun::answer`].
    Ask(Branch),
    /// The step just finished leads on to several steps at once. Run a lane
    /// from [`PlanRun::fork_lanes`] for each branch, at the same time where the
    /// caller can, then report that all of them have finished with
    /// [`PlanRun::join`].
    Fork {
        /// The plan the branches are in.
        graph: NodePath,
        branches: Vec<(EdgeId, NodeId)>,
        /// Where the branches meet. It runs once, after every branch.
        join: Option<NodeId>,
    },
    /// The run is over.
    Done(RunOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Working,
    Deciding {
        remaining: Vec<Branch>,
        /// Every open plain connection, taken together once no condition
        /// holds.
        plain: Vec<(EdgeId, NodeId)>,
    },
    /// Waiting for the lanes of a fork to finish.
    Forked {
        branches: Vec<(EdgeId, NodeId)>,
        join: Option<NodeId>,
    },
}

/// A position within one plan. A run holds a stack of these, one per level.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Frame {
    /// The steps walked to reach this plan; empty for the top-level plan.
    parents: Vec<NodeId>,
    current: NodeId,
    /// Other entry points in this plan, in graph order. Architect runs each root
    /// component deterministically instead of silently dropping all but one.
    remaining_roots: Vec<NodeId>,
    phase: Phase,
    visits: HashMap<NodeId, usize>,
    /// How many times each connection in this plan has been taken, for
    /// connections with a repeat limit.
    edge_uses: HashMap<EdgeId, usize>,
    prerequisites: Option<Prerequisites>,
    dependency_fork: bool,
}

/// A dependency region has one owner, even when only some branches converge.
/// Edges become selected or closed only after their source's routing is settled.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Prerequisites {
    nodes: Vec<NodeId>,
    entries: Vec<NodeId>,
    completed: Vec<NodeId>,
    skipped: Vec<NodeId>,
    selected: Vec<EdgeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepReadiness {
    pub path: NodePath,
    pub status: String,
    pub reason: String,
}

impl Prerequisites {
    fn settled(&self, node: &NodeId) -> bool {
        self.completed.contains(node) || self.skipped.contains(node)
    }

    fn waiting(&self, graph: &ArchitectGraph, node: &NodeId) -> bool {
        graph.edges.iter().any(|edge| {
            &edge.to == node && self.nodes.contains(&edge.from) && !self.settled(&edge.from)
        })
    }

    fn activated(&self, graph: &ArchitectGraph, node: &NodeId) -> bool {
        self.entries.contains(node)
            || graph
                .edges
                .iter()
                .any(|edge| &edge.to == node && self.selected.contains(&edge.id))
    }

    fn close_skipped(&mut self, graph: &ArchitectGraph) {
        loop {
            let skipped = self
                .nodes
                .iter()
                .find(|node| {
                    !self.settled(node)
                        && !self.waiting(graph, node)
                        && !self.activated(graph, node)
                })
                .cloned();
            match skipped {
                Some(node) => self.skipped.push(node),
                None => break,
            }
        }
    }
}

impl Frame {
    fn path(&self) -> NodePath {
        let mut ids = self.parents.clone();
        ids.push(self.current.clone());
        NodePath(ids)
    }

    fn graph_path(&self) -> NodePath {
        NodePath(self.parents.clone())
    }
}

/// A position in a plan, and the history of how it got there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanRun {
    stack: Vec<Frame>,
    /// Every step entered, at any depth, in order and including repeats.
    history: Vec<NodePath>,
    outcome: Option<RunOutcome>,
    /// For a lane of a fork, the steps of the lane's own plan that it stops
    /// short of: where its branches meet, which run once every lane is done.
    stops: Vec<NodeId>,
    settled_readiness: Vec<StepReadiness>,
}

impl PlanRun {
    fn empty() -> Self {
        Self {
            stack: Vec::new(),
            history: Vec::new(),
            outcome: None,
            stops: Vec::new(),
            settled_readiness: Vec::new(),
        }
    }

    /// A saved or rebased draft may await review, but its addresses and routing
    /// must remain valid. Execution still uses the full readiness check.
    pub fn validate_structure(graph: &ArchitectGraph) -> Result<(), RunRefusal> {
        fn only_unlocked(problem: &GraphProblem) -> bool {
            match problem {
                GraphProblem::Unlocked(_) => true,
                GraphProblem::InSubplan { problem, .. } => only_unlocked(problem),
                _ => false,
            }
        }
        let problems: Vec<_> = graph
            .blocking_problems()
            .into_iter()
            .filter(|problem| !only_unlocked(problem))
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(RunRefusal::NotReady(problems))
        }
    }

    fn check_ready(graph: &ArchitectGraph) -> Result<(), RunRefusal> {
        if graph.is_empty() {
            return Err(RunRefusal::NothingToRun);
        }
        let problems = graph.blocking_problems();
        if !problems.is_empty() {
            return Err(RunRefusal::NotReady(problems));
        }
        Ok(())
    }

    /// Starts a run at the plan's entry point, or explains why it cannot.
    pub fn start(graph: &ArchitectGraph) -> Result<Self, RunRefusal> {
        Self::check_ready(graph)?;
        let entries = entries_of(graph);
        if entries.is_empty() {
            return Err(RunRefusal::NothingToRun);
        }
        let mut run = Self::empty();
        run.push_frame(Vec::new(), entries, graph);
        // The entry step may itself be a plan, so descend before handing back.
        run.descend_while_nested(graph);
        Ok(run)
    }

    /// Starts a run at a given step, at any depth, instead of at the plan's
    /// entry. The steps before it are not run again, so whatever they last
    /// reported is what the steps after it are told. When the step's own plan
    /// runs out, the run carries on outwards as it would have from there.
    pub fn start_at(graph: &ArchitectGraph, path: &NodePath) -> Result<Self, RunRefusal> {
        Self::check_ready(graph)?;
        if path.is_empty() || graph.node_at(path).is_none() {
            return Err(RunRefusal::NoSuchStep(path.clone()));
        }
        let mut run = Self::empty();
        for (depth, id) in path.iter().enumerate() {
            run.push_frame(path.0[..depth].to_vec(), vec![id.clone()], graph);
        }
        run.descend_while_nested(graph);
        Ok(run)
    }

    /// A lane of a fork: one branch, from `start`, within the plan at
    /// `graph_path`. The lane finishes when it would enter any of
    /// `stop_before` in that plan, or when that plan runs out of steps, rather
    /// than carrying on into the plans above it.
    pub fn branch(
        graph: &ArchitectGraph,
        graph_path: &NodePath,
        start: NodeId,
        stop_before: &[NodeId],
    ) -> Self {
        let mut run = Self::empty();
        run.stops = stop_before.to_vec();
        let exists = graph
            .graph_at(graph_path)
            .is_some_and(|local| local.node(&start).is_some());
        if !exists || stop_before.contains(&start) {
            run.outcome = Some(RunOutcome::Completed);
            return run;
        }
        run.push_frame(graph_path.0.clone(), vec![start], graph);
        run.descend_while_nested(graph);
        run
    }

    /// The lanes for the fork the run is waiting on: one for each branch that
    /// does not start at the join. A fork at a lane's own level also stops
    /// where that lane would, so no branch runs past the point where the
    /// branches above it meet.
    pub fn fork_lanes(&self, graph: &ArchitectGraph) -> Vec<PlanRun> {
        let Some(frame) = self.stack.last() else {
            return Vec::new();
        };
        let Phase::Forked { branches, join } = &frame.phase else {
            return Vec::new();
        };
        let graph_path = frame.graph_path();
        let mut stops: Vec<NodeId> = join.iter().cloned().collect();
        if self.stack.len() == 1 {
            stops.extend(self.stops.iter().cloned());
        }
        if frame.dependency_fork {
            let mut run = Self::empty();
            // This lane owns the entire remaining region, including the nominal
            // join. Otherwise a skipped conditional route could still run it.
            if self.stack.len() == 1 {
                run.stops = self.stops.clone();
            }
            let entries = branches.iter().map(|(_, node)| node.clone()).collect();
            run.push_frame(graph_path.0, entries, graph);
            if !run.is_finished()
                && run
                    .stack
                    .last()
                    .is_some_and(|frame| frame.prerequisites.is_none())
            {
                run.enable_prerequisites(graph);
            }
            if !run.is_finished() {
                run.descend_while_nested(graph);
            }
            return vec![run];
        }
        branches
            .iter()
            .filter(|(_, start)| Some(start) != join.as_ref())
            .map(|(_, start)| Self::branch(graph, &graph_path, start.clone(), &stops))
            .collect()
    }

    /// Reports that every lane of the fork has finished. The run carries on at
    /// the join, or, when the branches never meet, leaves the plan as if its
    /// steps had run out.
    pub fn join(&mut self, graph: &ArchitectGraph) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last() else {
            return self.finish(RunOutcome::Completed);
        };
        let Phase::Forked { join, .. } = &frame.phase else {
            return self.decide(graph);
        };
        if frame.dependency_fork {
            return self.leave_frame(graph);
        }
        match join.clone() {
            Some(join) => self.enter(join, graph),
            None => self.leave_frame(graph),
        }
    }

    /// Starts the current step over as a new attempt, for a run picked up after
    /// the step was interrupted or failed. Counting it keeps a step that fails
    /// every time within its visit limit. Anywhere but on a step, nothing
    /// changes.
    pub fn retry_step(&mut self, graph: &ArchitectGraph) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.decide(graph);
        };
        if frame.phase != Phase::Working {
            return self.decide(graph);
        }
        let node = frame.current.clone();
        let visits = {
            let visits = frame.visits.entry(node.clone()).or_insert(0);
            *visits += 1;
            *visits
        };
        if visits > MAX_NODE_VISITS {
            return self.finish(RunOutcome::NodeLimit { node, visits });
        }
        let path = frame.path();
        self.history.push(path);
        self.decide(graph)
    }

    fn push_frame(&mut self, parents: Vec<NodeId>, entries: Vec<NodeId>, graph: &ArchitectGraph) {
        let needs_prerequisites = graph
            .graph_at(&NodePath(parents.clone()))
            .is_some_and(|local| overlapping_regions(local, &entries, &[]));
        let mut entries = entries.into_iter();
        let Some(current) = entries.next() else {
            return;
        };
        let mut visits = HashMap::default();
        visits.insert(current.clone(), 1);
        let frame = Frame {
            parents,
            current,
            remaining_roots: entries.collect(),
            phase: Phase::Working,
            visits,
            edge_uses: HashMap::default(),
            prerequisites: None,
            dependency_fork: false,
        };
        self.history.push(frame.path());
        self.stack.push(frame);
        if needs_prerequisites {
            self.enable_prerequisites(graph);
        }
    }

    fn enable_prerequisites(&mut self, graph: &ArchitectGraph) {
        let stops = if self.stack.len() == 1 {
            self.stops.clone()
        } else {
            Vec::new()
        };
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let Some(local) = graph.graph_at(&frame.graph_path()) else {
            return;
        };
        let mut entries = vec![frame.current.clone()];
        entries.append(&mut frame.remaining_roots);
        let nodes = reachable_before(local, &entries, &stops);
        if local.edges.iter().any(|edge| {
            nodes.contains(&edge.from) && nodes.contains(&edge.to) && local.is_loop_edge(edge)
        }) {
            self.finish(RunOutcome::Failed {
                message: "Overlapping prerequisite branches contain a cycle. Split the loop into a nested plan before running.".into(),
            });
            return;
        }
        frame.prerequisites = Some(Prerequisites {
            nodes,
            entries,
            completed: Vec::new(),
            skipped: Vec::new(),
            selected: Vec::new(),
        });
        // A branch can start directly at a shared node. Pick a genuinely ready
        // entry rather than letting the order of outgoing edges bypass a parent.
        if let Some(prerequisites) = &frame.prerequisites
            && prerequisites.waiting(local, &frame.current)
            && let Some(ready) = prerequisites
                .nodes
                .iter()
                .find(|node| {
                    !prerequisites.waiting(local, node) && prerequisites.activated(local, node)
                })
                .cloned()
        {
            frame.visits.remove(&frame.current);
            frame.current = ready.clone();
            frame.visits.insert(ready, 1);
            if let Some(path) = self.history.last_mut() {
                *path = frame.path();
            }
        }
    }

    fn settle_prerequisites(&mut self, selected: Vec<EdgeId>, graph: &ArchitectGraph) -> Decision {
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };
        let Some(local) = graph.graph_at(&frame.graph_path()) else {
            return self.finish(RunOutcome::Failed {
                message: "The checkpoint plan is missing.".into(),
            });
        };
        let Some(prerequisites) = &mut frame.prerequisites else {
            return self.decide(graph);
        };
        prerequisites.completed.push(frame.current.clone());
        for edge in &selected {
            *frame.edge_uses.entry(edge.clone()).or_insert(0) += 1;
        }
        prerequisites.selected.extend(selected);
        prerequisites.close_skipped(local);
        let next = prerequisites
            .nodes
            .iter()
            .find(|node| {
                !prerequisites.settled(node)
                    && !prerequisites.waiting(local, node)
                    && prerequisites.activated(local, node)
            })
            .cloned();
        if let Some(next) = next {
            return self.enter(next, graph);
        }
        if prerequisites
            .nodes
            .iter()
            .any(|node| !prerequisites.settled(node))
        {
            return self.finish(RunOutcome::Failed {
                message: "No step is ready: unresolved cyclic prerequisites remain.".into(),
            });
        }
        self.leave_frame(graph)
    }

    /// Dependency state for active regions. Callers can combine this with lane
    /// positions and completed results without treating a summary as a verdict.
    pub fn readiness(&self, graph: &ArchitectGraph) -> Vec<StepReadiness> {
        let mut readiness = self.settled_readiness.clone();
        for frame in &self.stack {
            let Some(prerequisites) = &frame.prerequisites else {
                continue;
            };
            let Some(local) = graph.graph_at(&frame.graph_path()) else {
                continue;
            };
            for node in &prerequisites.nodes {
                let (status, reason) = if prerequisites.completed.contains(node) {
                    ("completed", "Step and outgoing routing completed")
                } else if prerequisites.skipped.contains(node) {
                    ("skipped", "Every incoming route was closed")
                } else if prerequisites.waiting(local, node) {
                    (
                        "waiting",
                        "Incoming prerequisites or branch decisions are unfinished",
                    )
                } else if prerequisites.activated(local, node) {
                    (
                        "ready",
                        "All incoming prerequisites are settled and a route is selected",
                    )
                } else {
                    ("waiting", "No incoming route has been selected")
                };
                let mut path = frame.parents.clone();
                path.push(node.clone());
                readiness.push(StepReadiness {
                    path: NodePath(path),
                    status: status.into(),
                    reason: reason.into(),
                });
            }
        }
        readiness
    }

    /// Rebuilds a flat acyclic checkpoint without reexecuting retained results.
    /// This does not authorize execution: the runner must gate resume on review.
    /// Conditional routing cannot be inferred from a completed summary.
    pub fn rebase_remaining(
        graph: &ArchitectGraph,
        completed: &[NodePath],
    ) -> Result<Self, String> {
        if graph.is_empty() {
            return Err(RunRefusal::NothingToRun.to_string());
        }
        Self::validate_structure(graph).map_err(|error| error.to_string())?;
        if graph.nodes.iter().any(|node| node.subplan().is_some())
            || graph.edges.iter().any(|edge| graph.is_loop_edge(edge))
        {
            return Err("Topology rebasing requires a flat acyclic plan; the original checkpoint was preserved".into());
        }
        let completed: Vec<NodeId> = completed
            .iter()
            .map(|path| {
                if path.depth() != 1 || graph.node_at(path).is_none() {
                    return Err(format!("Cannot retain missing completed step {path}"));
                }
                path.leaf()
                    .cloned()
                    .ok_or_else(|| "An empty step cannot be completed".into())
            })
            .collect::<Result<_, String>>()?;
        if graph
            .edges
            .iter()
            .any(|edge| completed.contains(&edge.from) && !edge.condition.is_always())
        {
            return Err("Topology rebasing cannot infer a completed branch verdict; invalidate that step explicitly".into());
        }
        let mut prerequisites = Prerequisites {
            nodes: graph.nodes.iter().map(|node| node.id.clone()).collect(),
            entries: entries_of(graph),
            selected: graph
                .edges
                .iter()
                .filter(|edge| completed.contains(&edge.from) && edge.max_repeats != Some(0))
                .map(|edge| edge.id.clone())
                .collect(),
            completed,
            skipped: Vec::new(),
        };
        prerequisites.close_skipped(graph);
        if prerequisites
            .completed
            .iter()
            .any(|node| prerequisites.waiting(graph, node) || !prerequisites.activated(graph, node))
        {
            return Err("Retained results have unfinished incoming prerequisites; invalidate those results first".into());
        }
        let next = prerequisites
            .nodes
            .iter()
            .find(|node| {
                !prerequisites.settled(node)
                    && !prerequisites.waiting(graph, node)
                    && prerequisites.activated(graph, node)
            })
            .cloned();
        let mut run = Self::empty();
        if let Some(next) = next {
            run.push_frame(Vec::new(), vec![next], graph);
            if let Some(frame) = run.stack.last_mut() {
                frame.prerequisites = Some(prerequisites);
            }
        } else if prerequisites
            .nodes
            .iter()
            .all(|node| prerequisites.settled(node))
        {
            run.outcome = Some(RunOutcome::Completed);
        } else {
            return Err("No remaining step has satisfied prerequisites".into());
        }
        Ok(run)
    }

    /// Changes only the next ready step in a dependency region, never its
    /// completed work. The caller must ensure the displaced step has not begun.
    pub fn prioritize_ready(
        &mut self,
        path: &NodePath,
        graph: &ArchitectGraph,
    ) -> Result<(), String> {
        if self.is_finished()
            || !self
                .stack
                .last()
                .is_some_and(|frame| frame.phase == Phase::Working)
        {
            return Err("The lane is not waiting on an executable step".into());
        }
        if &self.current() == path {
            return Ok(());
        }
        let frame = self
            .stack
            .last_mut()
            .ok_or_else(|| "The run has finished".to_string())?;
        let local = graph
            .graph_at(&frame.graph_path())
            .ok_or_else(|| "The plan is missing".to_string())?;
        let node = path.leaf().ok_or_else(|| "Select a step".to_string())?;
        let prerequisites = frame.prerequisites.as_ref().ok_or_else(|| {
            "Only the checkpoint's current step is ready in this lane".to_string()
        })?;
        if frame.phase != Phase::Working
            || path.0[..path.depth() - 1] != frame.parents
            || !prerequisites.nodes.contains(node)
            || prerequisites.settled(node)
            || prerequisites.waiting(local, node)
            || !prerequisites.activated(local, node)
        {
            return Err(
                "The selected step is completed, skipped, or still waiting for prerequisites"
                    .into(),
            );
        }
        if graph
            .node_at(path)
            .is_some_and(|node| node.subplan().is_some())
        {
            return Err("Resume a nested plan at its admitted leaf step".into());
        }
        frame.visits.remove(&frame.current);
        frame.current = node.clone();
        frame.visits.insert(node.clone(), 1);
        if let Some(previous) = self.history.last_mut() {
            *previous = path.clone();
        }
        Ok(())
    }

    /// Validates deserialized positions before any driver can execute them.
    pub fn validate_checkpoint(&self, graph: &ArchitectGraph) -> Result<(), String> {
        Self::validate_structure(graph).map_err(|error| error.to_string())?;
        if self.stack.len() > MAX_PLAN_DEPTH || self.history.len() > MAX_RUN_STEPS + MAX_NODE_VISITS
        {
            return Err("Checkpoint counters exceed run limits".into());
        }
        if self.outcome.is_none() && self.stack.is_empty() {
            return Err("An unfinished checkpoint has no position".into());
        }
        for path in &self.history {
            if path.is_empty() || path.depth() > MAX_PLAN_DEPTH || graph.node_at(path).is_none() {
                return Err(format!(
                    "Checkpoint history refers to an invalid or missing step {path}"
                ));
            }
        }
        for step in &self.settled_readiness {
            if step.path.is_empty()
                || step.path.depth() > MAX_PLAN_DEPTH
                || graph.node_at(&step.path).is_none()
                || !matches!(step.status.as_str(), "completed" | "skipped")
            {
                return Err("Checkpoint contains invalid settled readiness".into());
            }
        }
        for (index, frame) in self.stack.iter().enumerate() {
            if index > 0
                && self
                    .stack
                    .get(index - 1)
                    .is_none_or(|parent| parent.path().0 != frame.parents)
            {
                return Err("Checkpoint nesting does not match its parent step".into());
            }
            let local = graph
                .graph_at(&frame.graph_path())
                .ok_or_else(|| "Checkpoint contains a missing nested plan".to_string())?;
            if local.node(&frame.current).is_none()
                || !frame.visits.contains_key(&frame.current)
                || (index == 0 && self.stops.iter().any(|node| local.node(node).is_none()))
                || frame
                    .remaining_roots
                    .iter()
                    .enumerate()
                    .any(|(index, node)| {
                        local.node(node).is_none() || frame.remaining_roots[..index].contains(node)
                    })
                || frame.visits.iter().any(|(node, visits)| {
                    local.node(node).is_none() || *visits == 0 || *visits > MAX_NODE_VISITS + 1
                })
                || frame.edge_uses.iter().any(|(id, uses)| {
                    *uses == 0
                        || *uses > MAX_RUN_STEPS
                        || !local.edges.iter().any(|edge| &edge.id == id)
                })
            {
                return Err("Checkpoint contains invalid node or edge counters".into());
            }
            let valid_edge = |id: &EdgeId, to: &NodeId| {
                local
                    .edges
                    .iter()
                    .any(|edge| &edge.id == id && &edge.to == to && edge.from == frame.current)
            };
            match &frame.phase {
                Phase::Deciding { remaining, plain } => {
                    let mut edges = std::collections::HashSet::new();
                    if remaining
                        .iter()
                        .map(|branch| &branch.edge)
                        .chain(plain.iter().map(|(edge, _)| edge))
                        .any(|edge| !edges.insert(edge))
                    {
                        return Err("Checkpoint repeats a pending branch decision".into());
                    }
                    if remaining.iter().any(|branch| {
                        !valid_edge(&branch.edge, &branch.to)
                            || !local.edges.iter().any(|edge| {
                                edge.id == branch.edge && edge.condition == branch.condition
                            })
                    }) || plain.iter().any(|(edge, to)| !valid_edge(edge, to))
                    {
                        return Err("Checkpoint branch decision does not match the graph".into());
                    }
                }
                Phase::Forked { branches, join } => {
                    let mut edges = std::collections::HashSet::new();
                    if branches.iter().any(|(edge, _)| !edges.insert(edge)) {
                        return Err("Checkpoint repeats a fork branch".into());
                    }
                    if branches.is_empty()
                        || branches.iter().any(|(edge, to)| !valid_edge(edge, to))
                        || *join != join_of(local, &frame.current, branches)
                        || frame.dependency_fork != fork_needs_prerequisites(local, branches, join)
                    {
                        return Err("Checkpoint fork does not match the graph".into());
                    }
                }
                Phase::Working => {}
            }
            if let Some(prerequisites) = &frame.prerequisites {
                let stops = if index == 0 {
                    self.stops.as_slice()
                } else {
                    &[]
                };
                if !prerequisites.nodes.contains(&frame.current)
                    || prerequisites.nodes != reachable_before(local, &prerequisites.entries, stops)
                    || prerequisites
                        .entries
                        .iter()
                        .enumerate()
                        .any(|(index, node)| prerequisites.entries[..index].contains(node))
                    || prerequisites
                        .selected
                        .iter()
                        .enumerate()
                        .any(|(index, edge)| prerequisites.selected[..index].contains(edge))
                    || prerequisites
                        .completed
                        .iter()
                        .enumerate()
                        .any(|(index, node)| prerequisites.completed[..index].contains(node))
                    || prerequisites
                        .skipped
                        .iter()
                        .enumerate()
                        .any(|(index, node)| prerequisites.skipped[..index].contains(node))
                    || prerequisites
                        .nodes
                        .iter()
                        .any(|node| local.node(node).is_none())
                    || local.edges.iter().any(|edge| {
                        prerequisites.nodes.contains(&edge.from)
                            && prerequisites.nodes.contains(&edge.to)
                            && local.is_loop_edge(edge)
                    })
                    || prerequisites.completed.iter().any(|node| {
                        prerequisites.waiting(local, node) || !prerequisites.activated(local, node)
                    })
                    || prerequisites.skipped.iter().any(|node| {
                        prerequisites.waiting(local, node) || prerequisites.activated(local, node)
                    })
                    || prerequisites
                        .entries
                        .iter()
                        .chain(&prerequisites.completed)
                        .chain(&prerequisites.skipped)
                        .any(|node| !prerequisites.nodes.contains(node))
                    || prerequisites
                        .completed
                        .iter()
                        .any(|node| prerequisites.skipped.contains(node))
                    || prerequisites.selected.iter().any(|id| {
                        !local.edges.iter().any(|edge| {
                            &edge.id == id && prerequisites.completed.contains(&edge.from)
                        })
                    })
                    || (frame.phase == Phase::Working
                        && (prerequisites.settled(&frame.current)
                            || prerequisites.waiting(local, &frame.current)
                            || !prerequisites.activated(local, &frame.current)))
                {
                    return Err("Checkpoint prerequisite state is inconsistent".into());
                }
            }
        }
        Ok(())
    }

    /// The step the run is on, as a full path.
    pub fn current(&self) -> NodePath {
        self.stack.last().map(Frame::path).unwrap_or_default()
    }

    /// The steps entered, at any depth, in order.
    pub fn history(&self) -> &[NodePath] {
        &self.history
    }

    pub fn steps_taken(&self) -> usize {
        self.history.len()
    }

    /// How many times the current frame has entered a step. A step reached
    /// again by a loop is on attempt 2.
    pub fn attempt(&self, id: &NodeId) -> usize {
        self.stack
            .last()
            .and_then(|frame| frame.visits.get(id).copied())
            .unwrap_or(1)
    }

    /// Every step currently in progress, outermost first. The breadcrumb during
    /// a run: "Build API › Write handlers › Validate".
    pub fn active_path(&self) -> Vec<NodeId> {
        self.stack
            .last()
            .map(|frame| frame.path().0)
            .unwrap_or_default()
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn cancel(&mut self) {
        if !self.is_finished() {
            self.outcome = Some(RunOutcome::Cancelled);
        }
    }

    /// What to do next, given where the run currently is.
    pub fn decide(&mut self, graph: &ArchitectGraph) -> Decision {
        if let Some(outcome) = &self.outcome {
            return Decision::Done(outcome.clone());
        }
        let Some(frame) = self.stack.last() else {
            return self.finish(RunOutcome::Completed);
        };
        match &frame.phase {
            Phase::Working => Decision::Run(frame.path()),
            Phase::Deciding { .. } => self.next_question(graph),
            Phase::Forked { branches, join } => Decision::Fork {
                graph: frame.graph_path(),
                branches: branches.clone(),
                join: join.clone(),
            },
        }
    }

    /// Reports that the current step has been carried out, moving the run on to
    /// deciding which way to leave it.
    pub fn finish_step(&mut self, graph: &ArchitectGraph) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };
        if matches!(frame.phase, Phase::Forked { .. }) {
            return self.decide(graph);
        }

        let graph_path = frame.graph_path();
        let current = frame.current.clone();
        let Some(local) = graph.graph_at(&graph_path) else {
            return self.finish(RunOutcome::Completed);
        };

        let mut remaining = Vec::new();
        let mut plain = Vec::new();
        let mut repeat = None;
        for edge in local.edges_from(&current) {
            // A connection to a step that is not there cannot be followed. The
            // plan could not have been started with one, but a step's own chat
            // can rewrite routing while a run is in flight.
            if local.node(&edge.to).is_none() {
                continue;
            }
            // A connection that has used up its repeats is closed for the rest
            // of this plan's run, which is how a limited loop lets go.
            let uses = frame.edge_uses.get(&edge.id).copied().unwrap_or(0);
            if let Some(limit) = edge.max_repeats
                && uses >= limit as usize
            {
                continue;
            }
            if edge.condition.is_always() {
                if edge.max_repeats.is_some() && local.is_loop_edge(edge) {
                    // A plain loop with a repeat limit says "do that again this
                    // many times", so while it has repeats left it is taken on
                    // its own rather than alongside the way on.
                    repeat.get_or_insert_with(|| (edge.id.clone(), edge.to.clone()));
                } else {
                    // Plain connections are the "otherwise" route, and all of
                    // them are taken: several plain connections out of one step
                    // are steps meant to happen alongside each other.
                    plain.push((edge.id.clone(), edge.to.clone()));
                }
            } else {
                remaining.push(Branch {
                    edge: edge.id.clone(),
                    to: edge.to.clone(),
                    condition: edge.condition.clone(),
                });
            }
        }

        let plain = match repeat {
            Some(repeat) => vec![repeat],
            None => plain,
        };
        frame.phase = Phase::Deciding { remaining, plain };
        self.next_question(graph)
    }

    /// Reports the answer to the question [`Decision::Ask`] posed.
    pub fn answer(&mut self, graph: &ArchitectGraph, taken: bool) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };
        let Phase::Deciding { remaining, plain } = &mut frame.phase else {
            return self.decide(graph);
        };
        if remaining.is_empty() {
            return self.next_question(graph);
        }

        let branch = remaining.remove(0);
        if taken {
            return self.take(branch.edge, branch.to, graph);
        }
        let plain = plain.clone();
        if remaining.is_empty() {
            return self.follow_plain(plain, graph);
        }
        self.next_question(graph)
    }

    fn next_question(&mut self, graph: &ArchitectGraph) -> Decision {
        let Some(frame) = self.stack.last() else {
            return self.finish(RunOutcome::Completed);
        };
        let Phase::Deciding { remaining, plain } = &frame.phase else {
            return self.decide(graph);
        };
        if let Some(branch) = remaining.first() {
            return Decision::Ask(branch.clone());
        }
        let plain = plain.clone();
        self.follow_plain(plain, graph)
    }

    /// Leaves the current step by its plain connections: nowhere, one step,
    /// or several at once.
    fn follow_plain(&mut self, plain: Vec<(EdgeId, NodeId)>, graph: &ArchitectGraph) -> Decision {
        if self
            .stack
            .last()
            .is_some_and(|frame| frame.prerequisites.is_some())
        {
            return self
                .settle_prerequisites(plain.into_iter().map(|(edge, _)| edge).collect(), graph);
        }
        let mut branches: Vec<(EdgeId, NodeId)> = Vec::new();
        for (edge, to) in plain {
            if !branches.iter().any(|(_, seen)| seen == &to) {
                branches.push((edge, to));
            }
        }
        if branches.len() > 1 {
            return self.fork(branches, graph);
        }
        match branches.pop() {
            Some((edge, to)) => self.take(edge, to, graph),
            None => self.leave_frame(graph),
        }
    }

    /// Sets the current step waiting on several branches at once.
    fn fork(&mut self, branches: Vec<(EdgeId, NodeId)>, graph: &ArchitectGraph) -> Decision {
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };
        let graph_path = frame.graph_path();
        let join = graph
            .graph_at(&graph_path)
            .and_then(|local| join_of(local, &frame.current, &branches));
        for (edge, _) in &branches {
            *frame.edge_uses.entry(edge.clone()).or_insert(0) += 1;
        }
        frame.dependency_fork = graph
            .graph_at(&graph_path)
            .is_some_and(|local| fork_needs_prerequisites(local, &branches, &join));
        frame.phase = Phase::Forked {
            branches: branches.clone(),
            join: join.clone(),
        };
        Decision::Fork {
            graph: graph_path,
            branches,
            join,
        }
    }

    /// Follows a connection out of the current step, counting it against any
    /// repeat limit it has.
    fn take(&mut self, edge: EdgeId, to: NodeId, graph: &ArchitectGraph) -> Decision {
        if self
            .stack
            .last()
            .is_some_and(|frame| frame.prerequisites.is_some())
        {
            return self.settle_prerequisites(vec![edge], graph);
        }
        if let Some(frame) = self.stack.last_mut() {
            *frame.edge_uses.entry(edge).or_insert(0) += 1;
        }
        self.enter(to, graph)
    }

    /// Moves onto a step within the current plan, descending into it if it is
    /// itself a plan.
    fn enter(&mut self, id: NodeId, graph: &ArchitectGraph) -> Decision {
        // A lane ends where its branches meet; the join is run once, by the
        // run the lane was forked from.
        if self.stack.len() == 1 && self.stops.contains(&id) {
            return self.finish(RunOutcome::Completed);
        }
        if self.history.len() >= MAX_RUN_STEPS {
            return self.finish(RunOutcome::StepLimit {
                steps: self.history.len(),
            });
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };

        let visits = frame.visits.entry(id.clone()).or_insert(0);
        *visits += 1;
        if *visits > MAX_NODE_VISITS {
            let visits = *visits;
            return self.finish(RunOutcome::NodeLimit { node: id, visits });
        }

        frame.current = id;
        frame.phase = Phase::Working;
        self.history.push(frame.path());
        self.descend_while_nested(graph);
        self.decide(graph)
    }

    /// A step that contains a plan is not carried out itself; the run descends
    /// into it. Repeated, because the first step of that plan may nest too.
    fn descend_while_nested(&mut self, graph: &ArchitectGraph) {
        loop {
            if self.is_finished() {
                return;
            }
            let Some(frame) = self.stack.last() else {
                return;
            };
            let path = frame.path();
            let Some(node) = graph.node_at(&path) else {
                return;
            };
            let Some(subplan) = node.subplan() else {
                return;
            };
            // Counted from the path rather than the stack, since a lane's
            // stack starts at the level it was forked at.
            if path.depth() >= MAX_PLAN_DEPTH {
                self.finish(RunOutcome::DepthLimit {
                    node: node.id.clone(),
                });
                return;
            }
            let entries = entries_of(subplan);
            if entries.is_empty() {
                return;
            }
            self.settled_readiness
                .retain(|step| !step.path.as_slice().starts_with(path.as_slice()));
            self.push_frame(path.0, entries, graph);
            if self.is_finished() {
                return;
            }
        }
    }

    /// The current plan has run out of steps. If it is a nested plan, the step
    /// that contained it is now finished, and the level above carries on.
    fn leave_frame(&mut self, graph: &ArchitectGraph) -> Decision {
        let has_next_root = self
            .stack
            .last()
            .is_some_and(|frame| !frame.remaining_roots.is_empty());
        if has_next_root && self.history.len() >= MAX_RUN_STEPS {
            return self.finish(RunOutcome::StepLimit {
                steps: self.history.len(),
            });
        }

        let next_root_path = self.stack.last_mut().and_then(|frame| {
            if frame.remaining_roots.is_empty() {
                return None;
            }
            let next = frame.remaining_roots.remove(0);
            let visits = {
                let visits = frame.visits.entry(next.clone()).or_insert(0);
                *visits += 1;
                *visits
            };
            frame.current = next;
            frame.phase = Phase::Working;
            Some((frame.path(), visits))
        });
        if let Some((path, visits)) = next_root_path {
            if visits > MAX_NODE_VISITS {
                let node = path
                    .leaf()
                    .cloned()
                    .expect("a root path always contains a node");
                return self.finish(RunOutcome::NodeLimit { node, visits });
            }
            self.history.push(path);
            self.descend_while_nested(graph);
            return self.decide(graph);
        }

        let depth = self.stack.last().map(|frame| frame.parents.len() + 1);
        let settled: Vec<_> = self
            .readiness(graph)
            .into_iter()
            .filter(|step| {
                Some(step.path.depth()) == depth
                    && matches!(step.status.as_str(), "completed" | "skipped")
                    && !self
                        .settled_readiness
                        .iter()
                        .any(|previous| previous.path == step.path)
            })
            .collect();
        self.settled_readiness.extend(settled);
        // A fork lane ends at its join, not at the end of its containing plan.
        // Only a frame with an owned parent can complete that container.
        if self.stack.len() > 1
            && let Some(frame) = self.stack.last()
        {
            let path = frame.graph_path();
            self.settled_readiness.retain(|step| step.path != path);
            self.settled_readiness.push(StepReadiness {
                path,
                status: "completed".into(),
                reason: "All selected nested work and routing completed".into(),
            });
        }
        self.stack.pop();
        if self.stack.is_empty() {
            return self.finish(RunOutcome::Completed);
        }
        self.finish_step(graph)
    }

    fn finish(&mut self, outcome: RunOutcome) -> Decision {
        self.outcome = Some(outcome.clone());
        Decision::Done(outcome)
    }
}

fn reachable_before(graph: &ArchitectGraph, entries: &[NodeId], stops: &[NodeId]) -> Vec<NodeId> {
    let mut reached = Vec::new();
    let mut queue = VecDeque::from(entries.to_vec());
    while let Some(node) = queue.pop_front() {
        if stops.contains(&node) || reached.contains(&node) {
            continue;
        }
        reached.push(node.clone());
        queue.extend(graph.edges_from(&node).map(|edge| edge.to.clone()));
    }
    graph
        .nodes
        .iter()
        .filter(|node| reached.contains(&node.id))
        .map(|node| node.id.clone())
        .collect()
}

fn fork_needs_prerequisites(
    graph: &ArchitectGraph,
    branches: &[(EdgeId, NodeId)],
    join: &Option<NodeId>,
) -> bool {
    let entries: Vec<_> = branches.iter().map(|(_, node)| node.clone()).collect();
    let stops: Vec<_> = join.iter().cloned().collect();
    let region = reachable_before(graph, &entries, &[]);
    let before_join = reachable_before(graph, &entries, &stops);
    let after_join = reachable_before(graph, &stops, &[]);
    overlapping_regions(graph, &entries, &stops)
        || before_join.iter().any(|node| after_join.contains(node))
        || graph.edges.iter().any(|edge| {
            region.contains(&edge.from)
                && (!edge.condition.is_always() || edge.max_repeats.is_some())
                && !graph.is_loop_edge(edge)
        })
}

/// Pairs whose repeated visits cannot be ordered by reachability alone.
/// Keep the proof beside the runner's fork/join and exclusive-repeat semantics.
/// Both pair orientations are returned for callers iterating in graph order.
pub(crate) fn potentially_concurrent_loop_steps(
    graph: &ArchitectGraph,
) -> HashSet<(NodeId, NodeId)> {
    // Disabled routes cannot establish a join or a loop. Only routing is needed;
    // copying nested plans and recorded results would make this proof expensive.
    let graph = ArchitectGraph {
        nodes: graph
            .nodes
            .iter()
            .map(|node| ArchitectNode::new(node.id.clone(), ""))
            .collect(),
        edges: graph
            .edges
            .iter()
            .filter(|edge| edge.max_repeats != Some(0))
            .cloned()
            .collect(),
    };
    let loop_edges: HashSet<_> = graph
        .edges
        .iter()
        .filter(|edge| graph.is_loop_edge(edge))
        .map(|edge| (&edge.from, &edge.to))
        .collect();
    let mut concurrent = HashSet::default();
    if loop_edges.is_empty() {
        return concurrent;
    }
    for node in &graph.nodes {
        let conditional_destinations: HashSet<_> = graph
            .edges_from(&node.id)
            .filter(|edge| !edge.condition.is_always())
            .map(|edge| &edge.to)
            .collect();
        let mut branches: Vec<(EdgeId, NodeId)> = Vec::new();
        for edge in graph.edges_from(&node.id) {
            // finish_step takes bounded plain repeats alone; answer takes a
            // successful retry directly, not alongside its fallback exits.
            // Multiple conditional destinations remain conservative candidates,
            // rather than assuming their predicates are mutually exclusive.
            let exclusive_repeat = loop_edges.contains(&(&edge.from, &edge.to))
                && ((edge.condition.is_always() && edge.max_repeats.is_some())
                    || (!edge.condition.is_always() && conditional_destinations.len() == 1));
            if !exclusive_repeat && !branches.iter().any(|(_, target)| target == &edge.to) {
                branches.push((edge.id.clone(), edge.to.clone()));
            }
        }
        if branches.len() < 2 {
            continue;
        }
        let entries: Vec<_> = branches.iter().map(|(_, target)| target.clone()).collect();
        let region = reachable_before(&graph, &entries, &[]);
        if !graph
            .edges
            .iter()
            .any(|edge| region.contains(&edge.from) && loop_edges.contains(&(&edge.from, &edge.to)))
        {
            continue;
        }
        let join = join_of(&graph, &node.id, &branches);
        // Structured lanes stop at their join, so earlier work and work beyond
        // that barrier do not conflict with the loop. Overlapping cyclic regions
        // cannot use the prerequisite scheduler; keep those branches fail-closed.
        let stops: Vec<_> = if fork_needs_prerequisites(&graph, &branches, &join) {
            Vec::new()
        } else {
            join.into_iter().collect()
        };
        let regions: Vec<_> = entries
            .iter()
            .map(|entry| reachable_before(&graph, std::slice::from_ref(entry), &stops))
            .collect();
        for (index, first_region) in regions.iter().enumerate() {
            for second_region in regions.iter().skip(index + 1) {
                for first in first_region {
                    for second in second_region {
                        if first != second {
                            concurrent.insert((first.clone(), second.clone()));
                            concurrent.insert((second.clone(), first.clone()));
                        }
                    }
                }
            }
        }
    }
    concurrent
}

fn overlapping_regions(graph: &ArchitectGraph, entries: &[NodeId], stops: &[NodeId]) -> bool {
    let mut seen = Vec::new();
    for entry in entries {
        let region = reachable_before(graph, std::slice::from_ref(entry), stops);
        if region.iter().any(|node| seen.contains(node)) {
            return true;
        }
        seen.extend(region);
    }
    false
}

/// Where a plan begins. Several entry points are valid; all are retained in
/// graph order so disconnected root components run deterministically.
fn entries_of(graph: &ArchitectGraph) -> Vec<NodeId> {
    let mut roots = graph.roots();
    roots.sort_by_key(|id| graph.node_index(id).unwrap_or(usize::MAX));
    roots
}

/// Where the branches of a fork meet: of the steps every branch can reach, the
/// one the slowest branch reaches soonest, ties going to the step first in the
/// plan. The forking step never counts, so a branch looping back to it does not
/// make it the meeting point.
fn join_of(
    local: &ArchitectGraph,
    forking: &NodeId,
    branches: &[(EdgeId, NodeId)],
) -> Option<NodeId> {
    let distances: Vec<HashMap<NodeId, usize>> = branches
        .iter()
        .map(|(_, start)| distances_from(local, start))
        .collect();
    let (first, rest) = distances.split_first()?;
    first
        .iter()
        .filter(|(id, _)| *id != forking)
        .filter_map(|(id, distance)| {
            let mut longest = *distance;
            for other in rest {
                longest = longest.max(*other.get(id)?);
            }
            Some((longest, local.node_index(id).unwrap_or(usize::MAX), id))
        })
        .min()
        .map(|(_, _, id)| id.clone())
}

/// How many connections it takes to reach each step from `start`, following
/// any connection.
fn distances_from(local: &ArchitectGraph, start: &NodeId) -> HashMap<NodeId, usize> {
    let mut distances = HashMap::default();
    if local.node(start).is_none() {
        return distances;
    }
    distances.insert(start.clone(), 0);
    let mut queue = VecDeque::from([start.clone()]);
    while let Some(id) = queue.pop_front() {
        let distance = distances.get(&id).copied().unwrap_or(0);
        for edge in local.edges_from(&id) {
            if local.node(&edge.to).is_none() || distances.contains_key(&edge.to) {
                continue;
            }
            distances.insert(edge.to.clone(), distance + 1);
            queue.push_back(edge.to.clone());
        }
    }
    distances
}

/// What a step running alongside others is told about them.
///
/// Parallel steps share one working tree. Their anticipated file surfaces
/// provide coordination context, not permissions or modification restrictions.
pub fn parallel_steps_prompt(graph: &ArchitectGraph, others: &[NodePath]) -> String {
    if others.is_empty() {
        return String::new();
    }
    let mut prompt = String::from(
        "\n## Running at the same time\n\nThese steps of the plan are being carried out at the \
         same time as this one, in conversations of their own and in the same working tree:\n",
    );
    for path in others {
        let Some(node) = graph.node_at(path) else {
            continue;
        };
        let intent = node.intent.trim();
        if intent.is_empty() {
            let _ = writeln!(prompt, "- {}", node.title);
        } else {
            let _ = writeln!(prompt, "- {}: {intent}", node.title);
        }
        let _ = writeln!(
            prompt,
            "  Existing-file surface: {}",
            node.file_surface_description()
        );
        if node.has_subplan() {
            match graph.effective_file_surface(path) {
                Ok(files) => {
                    let _ = writeln!(prompt, "  Including nested steps: {files:?}");
                }
                Err(reason) => {
                    let _ = writeln!(prompt, "  Nested file surface is not ready: {reason}");
                }
            }
        }
    }
    prompt.push_str(
        "\nThese surfaces describe anticipated scope for advisory scheduling, not file \
         permissions. Account for concurrent work when planning edits or repository-wide \
         commands such as git commit, checkout, reset, stash, rebase, formatting, regeneration, \
         or dependency installation. Report scope changes to keep coordination current.\n",
    );
    prompt
}

/// The brief handed to the agent for one step.
///
/// Only this step is described, plus what the steps feeding into it reported.
/// The agent is not shown the rest of the plan: a model that can see step 4
/// tends to start on it while it is still meant to be doing step 3, and the
/// run, not the model, decides what comes next.
pub fn step_prompt(
    graph: &ArchitectGraph,
    path: &NodePath,
    step_number: usize,
    attempt: usize,
) -> String {
    let Some(node) = graph.node_at(path) else {
        return format!("Step {step_number} is missing from the plan; stop and say so.");
    };
    let local = path
        .parent()
        .and_then(|parent| graph.graph_at(&parent))
        .or_else(|| graph.graph_at(&NodePath::default()))
        .unwrap_or(graph);

    let mut prompt = String::new();

    if path.depth() > 1 {
        let ancestry: Vec<String> = path.0[..path.0.len() - 1]
            .iter()
            .map(|id| find_title(graph, id).unwrap_or_else(|| id.0.clone()))
            .collect();
        let _ = writeln!(prompt, "You are inside: {}.\n", ancestry.join(" › "));
    }

    let _ = writeln!(prompt, "## Step {step_number}: {}", node.title);
    let _ = writeln!(
        prompt,
        "\nExisting-file surface: {}",
        node.file_surface_description()
    );
    prompt.push_str(
        "This surface describes anticipated file assignments for advisory scheduling, \
         including paths of files to create. Files need not exist. It is \
         not an edit allowlist or a restriction on which files you may modify. [] means no \
         file writes are anticipated. Report scope changes using worktree-qualified \
         paths (worktree/path) to keep scheduling information current. Report every newly \
         created file by its worktree-qualified path so it can be added to downstream steps.\n",
    );

    let handed_on = incoming_summaries(local, node);
    if !handed_on.is_empty() {
        prompt.push_str("\nWhat earlier steps reported:\n");
        for (title, summary) in handed_on {
            let _ = writeln!(prompt, "- {title}: {summary}");
        }
    }

    if attempt > 1
        && let Some(result) = &node.result
    {
        let _ = write!(
            prompt,
            "\nYou have been here before. On attempt {}, you reported:\n{}\n\nThat did not settle \
             it, which is why you are back. Do not repeat what you already tried.\n",
            result.attempt.max(1),
            result.summary.trim()
        );
    }

    if !node.responsibility.trim().is_empty() {
        prompt.push_str("\nResponsibility: ");
        prompt.push_str(node.responsibility.trim());
        prompt.push('\n');
    }

    if !node.intent.trim().is_empty() {
        let _ = write!(prompt, "\nGoal: {}\n", node.intent.trim());
    }

    if !node.rules.is_empty() {
        prompt.push_str("\nRules, which hold however you carry this out:\n");
        for rule in &node.rules {
            let _ = writeln!(prompt, "- {rule}");
        }
    }

    if !node.capture.trim().is_empty() {
        let _ = write!(
            prompt,
            "\nCapture in your summary: {}\n",
            node.capture.trim()
        );
    }

    prompt.push_str(
        "\nCarry out this step now, and only this step. Do not start any later step: what happens \
         next is decided once this one is done. When it is finished, report what you did as your \
         summary.\n",
    );
    prompt
}

/// What the steps leading into this one reported, plus anything pinned.
///
/// Pinned steps are included wherever they are in the plan, which is the whole
/// point of pinning: a decision taken early that everything after it depends on
/// should not fall out of view once it is no longer a direct predecessor.
fn incoming_summaries(local: &ArchitectGraph, node: &ArchitectNode) -> Vec<(String, String)> {
    // Pinned first, so a standing decision is read before the detail of
    // whatever happened to run immediately before this step.
    let mut candidates: Vec<&ArchitectNode> = local.pinned_nodes().collect();
    for edge in local.edges_into(&node.id) {
        if let Some(source) = local.node(&edge.from) {
            candidates.push(source);
        }
    }

    let mut seen: Vec<&NodeId> = Vec::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        if candidate.id == node.id || seen.contains(&&candidate.id) {
            continue;
        }
        let Some(result) = candidate.handoff_result() else {
            continue;
        };
        if result.summary.trim().is_empty() {
            continue;
        }
        seen.push(&candidate.id);
        out.push((candidate.title.clone(), result.summary.trim().to_string()));
    }
    out
}

/// The question that decides whether a branch is taken.
///
/// The answer is parsed from the reply, so the shape of the reply is not
/// optional; asking for the verdict first also stops a model reasoning its way
/// into an answer it would not have given up front.
pub fn branch_prompt(graph: &ArchitectGraph, branch: &Branch) -> String {
    let target = find_title(graph, &branch.to).unwrap_or_else(|| branch.to.0.clone());

    let question = match &branch.condition {
        EdgeCondition::Always => "Should the plan continue?".to_string(),
        EdgeCondition::Objective { statement } => format!(
            "Check whether this is true right now: {statement}\n\nCheck it, rather than \
             recalling what was true earlier."
        ),
        EdgeCondition::LlmEvaluated { question } => format!("Answer this question: {question}"),
    };

    format!(
        "{question}\n\nAnswering YES means the plan moves on to \"{target}\". Reply with YES or NO \
         on the first line by itself, then one sentence saying why.\n"
    )
}

/// Reads a yes-or-no verdict out of a reply.
///
/// Only the opening of the reply is considered. A model that has answered "NO"
/// and then explains what a "yes" would have looked like must not be read as
/// having said yes, and the first word is the only part of a free-text reply
/// whose meaning is unambiguous.
pub fn parse_verdict(reply: &str) -> Option<bool> {
    for line in reply.lines() {
        let line = line.trim().trim_start_matches(['*', '#', '-', '>', ' ']);
        if line.is_empty() {
            continue;
        }
        let word: String = line
            .chars()
            .take_while(|character| character.is_ascii_alphabetic())
            .collect();
        return match word.to_ascii_lowercase().as_str() {
            "yes" | "true" => Some(true),
            "no" | "false" => Some(false),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchitectEdge, StepResult};

    fn lock_deeply(graph: &mut ArchitectGraph) {
        for node in &mut graph.nodes {
            node.locked = true;
            if let Some(subplan) = node.subplan.as_deref_mut() {
                lock_deeply(subplan);
            }
        }
    }

    /// plan → edit → test, with test looping back to edit when it fails.
    fn locked_graph() -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        let mut plan = ArchitectNode::new("plan", "Plan the change");
        plan.intent = "Decide which files need to change".into();
        plan.rules = vec!["Do not edit anything yet".into()];
        plan.capture = "Which files you intend to change".into();
        graph.add_node(plan);
        graph.add_node(ArchitectNode::new("edit", "Make the edit"));
        graph.add_node(ArchitectNode::new("test", "Run the tests"));
        graph.connect("plan", "edit");
        graph.connect("edit", "test");
        graph
            .edges
            .push(ArchitectEdge::new("retry", "test", "edit").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Did the tests fail?".into(),
                },
            ));
        lock_deeply(&mut graph);
        graph
    }

    fn path(ids: &[&str]) -> NodePath {
        NodePath(ids.iter().map(|id| NodeId((*id).into())).collect())
    }

    #[test]
    fn failure_outcomes_preserve_actionable_context() {
        let outcome = RunOutcome::Failed {
            message: "provider disconnected".to_string(),
        };
        assert_eq!(
            outcome.describe(&ArchitectGraph::default()),
            "The run failed: provider disconnected"
        );
        assert!(!outcome.is_success());
    }

    #[test]
    fn a_run_starts_at_the_step_nothing_leads_to() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.current(), path(&["plan"]));
        assert_eq!(run.decide(&graph), Decision::Run(path(&["plan"])));
    }

    #[test]
    fn every_root_component_runs_in_graph_order() {
        let mut graph = ArchitectGraph::default();
        for id in ["first-root", "first-end", "second-root", "second-end"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        graph.connect("first-root", "first-end");
        graph.connect("second-root", "second-end");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.decide(&graph), Decision::Run(path(&["first-root"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["first-end"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["second-root"]))
        );
        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["second-end"]))
        );
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
        assert_eq!(
            run.history(),
            &[
                path(&["first-root"]),
                path(&["first-end"]),
                path(&["second-root"]),
                path(&["second-end"]),
            ]
        );
    }

    #[test]
    fn an_unconditional_connection_is_followed_without_asking_anything() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["edit"])));
    }

    #[test]
    fn answering_yes_takes_the_branch_back_round_the_loop() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.finish_step(&graph);
        run.finish_step(&graph);
        assert_eq!(run.answer(&graph, true), Decision::Run(path(&["edit"])));
        assert_eq!(run.attempt(&NodeId("edit".into())), 2);
    }

    #[test]
    fn answering_no_with_nowhere_else_to_go_completes_the_plan() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.finish_step(&graph);
        run.finish_step(&graph);
        assert_eq!(
            run.answer(&graph, false),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn a_plain_connection_is_taken_when_every_condition_says_no() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("check", "Check the build"));
        graph.add_node(ArchitectNode::new("fix", "Fix the build"));
        graph.add_node(ArchitectNode::new("ship", "Ship it"));
        graph
            .edges
            .push(ArchitectEdge::new("broken", "check", "fix").with_condition(
                EdgeCondition::Objective {
                    statement: "the build failed".into(),
                },
            ));
        graph.connect("check", "ship");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(run.answer(&graph, false), Decision::Run(path(&["ship"])));
    }

    // ── nesting ───────────────────────────────────────────────────────────

    fn nested_graph() -> ArchitectGraph {
        let mut inner = ArchitectGraph::default();
        inner.add_node(ArchitectNode::new("parse", "Parse body"));
        inner.add_node(ArchitectNode::new("respond", "Respond"));
        inner.connect("parse", "respond");

        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("schema", "Define schema"));
        let mut handlers = ArchitectNode::new("handlers", "Write handlers");
        handlers.subplan = Some(Box::new(inner));
        graph.add_node(handlers);
        graph.add_node(ArchitectNode::new("ship", "Ship it"));
        graph.connect("schema", "handlers");
        graph.connect("handlers", "ship");
        lock_deeply(&mut graph);
        graph
    }

    #[test]
    fn entering_a_step_that_contains_a_plan_descends_into_it() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();

        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["handlers", "parse"]))
        );
        assert_eq!(run.depth(), 2, "the run should be inside the sub-plan");
    }

    #[test]
    fn finishing_a_sub_plan_finishes_the_step_that_contained_it() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();

        run.finish_step(&graph); // schema -> handlers, descends to parse
        run.finish_step(&graph); // parse -> respond
        assert_eq!(run.current(), path(&["handlers", "respond"]));

        // Respond is the last of the sub-plan, so finishing it should pop back
        // out and carry on with the step after the one that contained it.
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["ship"])));
        assert_eq!(run.depth(), 1);
        assert!(
            run.readiness(&graph)
                .iter()
                .any(|step| { step.path == path(&["handlers"]) && step.status == "completed" })
        );
        let restored: PlanRun =
            serde_json::from_value(serde_json::to_value(&run).expect("checkpoint"))
                .expect("restore checkpoint");
        assert!(
            restored
                .readiness(&graph)
                .iter()
                .any(|step| { step.path == path(&["handlers"]) && step.status == "completed" })
        );
    }

    #[test]
    fn the_breadcrumb_shows_every_level_in_progress() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);

        assert_eq!(
            run.active_path(),
            vec![NodeId("handlers".into()), NodeId("parse".into())]
        );
    }

    #[test]
    fn a_plan_nested_too_deep_is_refused_rather_than_run() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("leaf", "Leaf"));
        for depth in 0..MAX_PLAN_DEPTH + 1 {
            let mut outer = ArchitectGraph::default();
            let mut node = ArchitectNode::new(format!("level-{depth}"), format!("Level {depth}"));
            node.subplan = Some(Box::new(graph));
            outer.add_node(node);
            graph = outer;
        }
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert!(
            matches!(
                run.decide(&graph),
                Decision::Done(RunOutcome::DepthLimit { .. })
            ),
            "a plan nested past the limit should stop the run"
        );
    }

    #[test]
    fn a_sub_plan_with_an_unlocked_step_blocks_the_whole_run() {
        let mut graph = nested_graph();
        graph
            .node_mut(&NodeId("handlers".into()))
            .unwrap()
            .subplan
            .as_deref_mut()
            .unwrap()
            .set_locked(&NodeId("parse".into()), false);

        let refusal = PlanRun::start(&graph).unwrap_err();
        let RunRefusal::NotReady(problems) = refusal else {
            panic!("expected the unlocked nested step to be reported");
        };
        assert!(
            problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::InSubplan { .. })),
            "got {problems:?}"
        );
    }

    #[test]
    fn a_step_cannot_be_locked_while_its_sub_plan_is_still_open() {
        let mut graph = nested_graph();
        assert!(graph.can_lock(&NodeId("handlers".into())));

        graph
            .node_mut(&NodeId("handlers".into()))
            .unwrap()
            .subplan
            .as_deref_mut()
            .unwrap()
            .set_locked(&NodeId("parse".into()), false);
        assert!(!graph.can_lock(&NodeId("handlers".into())));
    }

    // ── handoff ───────────────────────────────────────────────────────────

    fn with_result(graph: &mut ArchitectGraph, id: &str, summary: &str, attempt: usize) {
        graph.node_mut(&NodeId(id.into())).unwrap().result = Some(StepResult {
            summary: summary.into(),
            attempt,
        });
    }

    #[test]
    fn a_step_is_told_what_the_steps_feeding_it_reported() {
        let mut graph = locked_graph();
        with_result(&mut graph, "plan", "Will change calc.py only.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 2, 1);
        assert!(prompt.contains("What earlier steps reported"));
        assert!(prompt.contains("Plan the change: Will change calc.py only."));
    }

    #[test]
    fn a_step_is_not_told_what_unrelated_steps_reported() {
        let mut graph = locked_graph();
        with_result(&mut graph, "test", "Tests failed.", 1);

        // `test` does not lead into `edit` except by the retry loop, which does
        // feed it — so use `plan`, which nothing leads into.
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(
            !prompt.contains("Tests failed."),
            "a step should not receive summaries from steps that do not feed it:\n{prompt}"
        );
    }

    #[test]
    fn a_pinned_step_is_told_to_every_later_step() {
        let mut graph = locked_graph();
        graph.node_mut(&NodeId("plan".into())).unwrap().pinned = true;
        with_result(&mut graph, "plan", "Decided to keep the public API.", 1);

        let prompt = step_prompt(&graph, &path(&["test"]), 3, 1);
        assert!(
            prompt.contains("Decided to keep the public API."),
            "a pinned step should reach steps it does not directly feed:\n{prompt}"
        );
    }

    #[test]
    fn coming_round_a_loop_reminds_the_step_what_it_already_tried() {
        let mut graph = locked_graph();
        with_result(&mut graph, "edit", "Widened the clock skew.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 4, 2);
        assert!(prompt.contains("You have been here before"));
        assert!(prompt.contains("Widened the clock skew."));
        assert!(prompt.contains("Do not repeat what you already tried"));
    }

    #[test]
    fn a_first_attempt_is_not_told_about_a_previous_one() {
        let mut graph = locked_graph();
        with_result(&mut graph, "edit", "Widened the clock skew.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 2, 1);
        assert!(!prompt.contains("You have been here before"), "{prompt}");
    }

    #[test]
    fn a_step_states_what_its_summary_must_capture() {
        let graph = locked_graph();
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(prompt.contains("Capture in your summary: Which files you intend to change"));
    }

    #[test]
    fn a_nested_step_is_told_which_step_it_is_inside() {
        let graph = nested_graph();
        let prompt = step_prompt(&graph, &path(&["handlers", "parse"]), 2, 1);
        assert!(
            prompt.contains("You are inside: Write handlers"),
            "{prompt}"
        );
    }

    #[test]
    fn a_successor_receives_the_completed_nested_plan_as_its_handoff() {
        let mut graph = nested_graph();
        let handlers = graph.node_mut(&NodeId::from("handlers")).unwrap();
        let subplan = handlers.subplan.as_deref_mut().unwrap();
        with_result(subplan, "parse", "Read and validated the request body.", 1);
        with_result(subplan, "respond", "Returned the documented response.", 1);

        let prompt = step_prompt(&graph, &path(&["ship"]), 4, 1);
        assert!(prompt.contains("Write handlers:"), "{prompt}");
        assert!(
            prompt.contains("Parse body: Read and validated the request body."),
            "{prompt}"
        );
        assert!(
            prompt.contains("Respond: Returned the documented response."),
            "{prompt}"
        );
    }

    #[test]
    fn a_step_prompt_never_mentions_later_steps() {
        let graph = locked_graph();
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(!prompt.contains("Run the tests"), "{prompt}");
    }

    #[test]
    fn steps_that_lead_somewhere_without_saying_what_they_hand_on_are_reported() {
        let graph = locked_graph();
        let missing = graph.steps_without_capture();
        assert!(missing.contains(&NodeId("edit".into())));
        assert!(
            !missing.contains(&NodeId("plan".into())),
            "plan states a capture, so it should not be reported"
        );
    }

    // ── budgets and endings ───────────────────────────────────────────────

    #[test]
    fn a_loop_that_never_exits_is_stopped_rather_than_run_forever() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("work", "Work"));
        graph
            .edges
            .push(ArchitectEdge::new("again", "work", "work").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Again?".into(),
                },
            ));
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        let outcome = loop {
            match run.finish_step(&graph) {
                Decision::Ask(_) => match run.answer(&graph, true) {
                    Decision::Done(outcome) => break outcome,
                    _ => continue,
                },
                Decision::Done(outcome) => break outcome,
                Decision::Run(_) => continue,
                Decision::Fork { .. } => panic!("a single loop never forks"),
            }
        };
        assert_eq!(
            outcome,
            RunOutcome::NodeLimit {
                node: NodeId("work".into()),
                visits: MAX_NODE_VISITS + 1,
            }
        );
    }

    #[test]
    fn a_limited_loop_moves_on_once_its_repeats_are_spent() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("edit", "Edit"));
        graph.add_node(ArchitectNode::new("test", "Test"));
        graph.add_node(ArchitectNode::new("ship", "Ship"));
        graph.connect("edit", "test");
        let mut retry = ArchitectEdge::new("retry", "test", "edit");
        retry.condition = EdgeCondition::LlmEvaluated {
            question: "Did the tests fail?".into(),
        };
        retry.max_repeats = Some(2);
        graph.edges.push(retry);
        graph.connect("test", "ship");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        for _ in 0..2 {
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["test"])));
            assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
            assert_eq!(run.answer(&graph, true), Decision::Run(path(&["edit"])));
        }
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["test"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["ship"])),
            "with its repeats spent, the loop is not offered again"
        );
        assert_eq!(run.attempt(&NodeId("edit".into())), 3);
    }

    #[test]
    fn an_unconditional_loop_with_a_limit_repeats_then_moves_on() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("draft", "Draft"));
        graph.add_node(ArchitectNode::new("polish", "Polish"));
        graph.add_node(ArchitectNode::new("publish", "Publish"));
        graph.connect("draft", "polish");
        let mut again = ArchitectEdge::new("again", "polish", "draft");
        again.max_repeats = Some(1);
        graph.edges.push(again);
        graph.connect("polish", "publish");
        lock_deeply(&mut graph);
        assert_eq!(graph.blocking_problems(), vec![]);

        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["polish"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["draft"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["polish"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["publish"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn an_empty_plan_refuses_to_run() {
        assert_eq!(
            PlanRun::start(&ArchitectGraph::default()).unwrap_err(),
            RunRefusal::NothingToRun
        );
    }

    #[test]
    fn cancelling_ends_the_run_wherever_it_had_got_to() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.cancel();
        assert_eq!(run.decide(&graph), Decision::Done(RunOutcome::Cancelled));
    }

    #[test]
    fn an_outcome_summary_names_the_step_and_the_way_out() {
        let graph = nested_graph();
        let summary = RunOutcome::NodeLimit {
            node: NodeId("parse".into()),
            visits: 25,
        }
        .summary(&graph);
        assert!(summary.contains("Parse body"), "{summary}");
        assert!(summary.contains("repeat limit"), "{summary}");
    }

    #[test]
    fn an_outcome_describes_itself_using_the_step_title() {
        let graph = nested_graph();
        let described = RunOutcome::NodeLimit {
            node: NodeId("parse".into()),
            visits: 25,
        }
        .describe(&graph);
        assert!(
            described.contains("Parse body"),
            "a nested step should still be named: {described}"
        );
    }

    // ── forks ─────────────────────────────────────────────────────────────

    fn plain_graph(ids: &[&str], connections: &[(&str, &str)]) -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for id in ids {
            graph.add_node(ArchitectNode::new(*id, id.to_uppercase()));
        }
        for (from, to) in connections {
            graph.connect(*from, *to);
        }
        lock_deeply(&mut graph);
        graph
    }

    fn id(id: &str) -> NodeId {
        NodeId(id.into())
    }

    /// Runs a lane to its end, answering every question no, and returns the
    /// steps it ran.
    fn run_lane(lane: &mut PlanRun, graph: &ArchitectGraph) -> (Vec<NodePath>, RunOutcome) {
        let mut ran = Vec::new();
        let mut decision = lane.decide(graph);
        loop {
            decision = match decision {
                Decision::Run(path) => {
                    ran.push(path);
                    lane.finish_step(graph)
                }
                Decision::Ask(_) => lane.answer(graph, false),
                Decision::Fork { .. } => panic!("unexpected fork in {ran:?}"),
                Decision::Done(outcome) => return (ran, outcome),
            };
        }
    }

    fn fork_join(decision: &Decision) -> Option<NodeId> {
        match decision {
            Decision::Fork { join, .. } => join.clone(),
            other => panic!("expected a fork, got {other:?}"),
        }
    }

    #[test]
    fn several_plain_connections_fork_and_meet_at_the_join() {
        let graph = plain_graph(
            &["a", "b", "c", "d"],
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let decision = run.finish_step(&graph);
        let Decision::Fork {
            graph: at,
            branches,
            join,
        } = &decision
        else {
            panic!("a step with two plain connections should fork, got {decision:?}");
        };
        assert_eq!(at, &NodePath::default());
        let starts: Vec<NodeId> = branches.iter().map(|(_, to)| to.clone()).collect();
        assert_eq!(starts, vec![id("b"), id("c")]);
        assert_eq!(join, &Some(id("d")));

        let mut lanes = run.fork_lanes(&graph);
        assert_eq!(lanes.len(), 2);
        let (ran_b, outcome_b) = run_lane(&mut lanes[0], &graph);
        let (ran_c, outcome_c) = run_lane(&mut lanes[1], &graph);
        assert_eq!(ran_b, vec![path(&["b"])], "a lane stops before the join");
        assert_eq!(ran_c, vec![path(&["c"])]);
        assert_eq!(outcome_b, RunOutcome::Completed);
        assert_eq!(outcome_c, RunOutcome::Completed);

        assert_eq!(run.join(&graph), Decision::Run(path(&["d"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn a_fork_whose_branches_never_meet_has_no_join() {
        let graph = plain_graph(
            &["a", "b", "b2", "c"],
            &[("a", "b"), ("a", "c"), ("b", "b2")],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let decision = run.finish_step(&graph);
        assert_eq!(fork_join(&decision), None);

        let mut lanes = run.fork_lanes(&graph);
        let (ran_b, _) = run_lane(&mut lanes[0], &graph);
        let (ran_c, _) = run_lane(&mut lanes[1], &graph);
        assert_eq!(ran_b, vec![path(&["b"]), path(&["b2"])]);
        assert_eq!(ran_c, vec![path(&["c"])]);
        assert_eq!(
            run.join(&graph),
            Decision::Done(RunOutcome::Completed),
            "with nowhere to meet, the plan ends once every branch has"
        );
    }

    #[test]
    fn a_branch_that_starts_at_the_join_contributes_no_lane() {
        let graph = plain_graph(&["a", "b", "d"], &[("a", "b"), ("a", "d"), ("b", "d")]);
        let mut run = PlanRun::start(&graph).unwrap();
        let decision = run.finish_step(&graph);
        assert_eq!(fork_join(&decision), Some(id("d")));

        let mut lanes = run.fork_lanes(&graph);
        assert_eq!(lanes.len(), 1, "only the branch through b needs a lane");
        let (ran, _) = run_lane(&mut lanes[0], &graph);
        assert_eq!(ran, vec![path(&["b"])]);
        assert_eq!(run.join(&graph), Decision::Run(path(&["d"])));
    }

    #[test]
    fn the_join_is_where_the_slowest_branch_arrives_soonest() {
        let graph = plain_graph(
            &["a", "b", "b2", "c", "d", "e"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "b2"),
                ("b2", "d"),
                ("c", "d"),
                ("d", "e"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let decision = run.finish_step(&graph);
        assert_eq!(fork_join(&decision), Some(id("d")));

        let mut lanes = run.fork_lanes(&graph);
        let (ran, _) = run_lane(&mut lanes[0], &graph);
        assert_eq!(
            ran,
            vec![path(&["b"]), path(&["b2"])],
            "the longer branch runs all of its own steps, and stops before the join"
        );
        assert_eq!(run.join(&graph), Decision::Run(path(&["d"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["e"])));
    }

    #[test]
    fn equally_distant_joins_are_chosen_in_graph_order() {
        let graph = plain_graph(
            &["a", "b", "c", "y", "x"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "x"),
                ("b", "y"),
                ("c", "x"),
                ("c", "y"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let decision = run.finish_step(&graph);
        assert_eq!(fork_join(&decision), Some(id("y")));
    }

    #[test]
    fn a_lane_loops_within_its_branch_before_stopping_at_the_join() {
        let mut graph = plain_graph(
            &["a", "b", "check", "c", "d"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "check"),
                ("check", "d"),
                ("c", "d"),
            ],
        );
        graph
            .edges
            .push(ArchitectEdge::new("again", "check", "b").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Should b run again?".into(),
                },
            ));
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(fork_join(&run.finish_step(&graph)), Some(id("d")));

        let mut lanes = run.fork_lanes(&graph);
        let lane = &mut lanes[0];
        assert_eq!(lane.decide(&graph), Decision::Run(path(&["b"])));
        assert_eq!(lane.finish_step(&graph), Decision::Run(path(&["check"])));
        assert!(matches!(lane.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(lane.answer(&graph, true), Decision::Run(path(&["b"])));
        assert_eq!(lane.attempt(&id("b")), 2, "a lane counts its own visits");
        assert_eq!(lane.finish_step(&graph), Decision::Run(path(&["check"])));
        assert!(matches!(lane.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(
            lane.answer(&graph, false),
            Decision::Done(RunOutcome::Completed),
            "leaving the loop leads to the join, which the lane stops before"
        );
    }

    #[test]
    fn a_fork_inside_a_branch_stops_where_its_parent_lane_would() {
        let graph = plain_graph(
            &["a", "b", "e", "f", "g", "c", "d"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "e"),
                ("b", "f"),
                ("e", "d"),
                ("f", "g"),
                ("c", "d"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(fork_join(&run.finish_step(&graph)), Some(id("d")));
        let mut lanes = run.fork_lanes(&graph);

        let lane_b = &mut lanes[0];
        assert_eq!(lane_b.decide(&graph), Decision::Run(path(&["b"])));
        let inner = lane_b.finish_step(&graph);
        assert_eq!(fork_join(&inner), None, "e and f never meet on their own");
        let mut inner_lanes = lane_b.fork_lanes(&graph);
        let (ran_e, _) = run_lane(&mut inner_lanes[0], &graph);
        let (ran_f, _) = run_lane(&mut inner_lanes[1], &graph);
        assert_eq!(
            ran_e,
            vec![path(&["e"])],
            "a nested branch must not run the outer join"
        );
        assert_eq!(ran_f, vec![path(&["f"]), path(&["g"])]);
        assert_eq!(lane_b.join(&graph), Decision::Done(RunOutcome::Completed));

        let (ran_c, _) = run_lane(&mut lanes[1], &graph);
        assert_eq!(ran_c, vec![path(&["c"])]);
        assert_eq!(run.join(&graph), Decision::Run(path(&["d"])));
    }

    #[test]
    fn a_fork_inside_a_nested_plan_runs_and_joins_within_it() {
        let inner = plain_graph(
            &["split", "left", "right", "merge"],
            &[
                ("split", "left"),
                ("split", "right"),
                ("left", "merge"),
                ("right", "merge"),
            ],
        );
        let mut graph = ArchitectGraph::default();
        let mut outer = ArchitectNode::new("outer", "Outer");
        outer.subplan = Some(Box::new(inner));
        graph.add_node(outer);
        graph.add_node(ArchitectNode::new("after", "After"));
        graph.connect("outer", "after");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        let first = run.decide(&graph);
        assert_eq!(first, Decision::Run(path(&["outer", "split"])));
        let decision = run.finish_step(&graph);
        let Decision::Fork {
            graph: at, join, ..
        } = &decision
        else {
            panic!("expected a fork inside the nested plan, got {decision:?}");
        };
        assert_eq!(at, &path(&["outer"]));
        assert_eq!(join, &Some(id("merge")));

        let mut lanes = run.fork_lanes(&graph);
        let (ran_left, _) = run_lane(&mut lanes[0], &graph);
        let (ran_right, _) = run_lane(&mut lanes[1], &graph);
        assert_eq!(ran_left, vec![path(&["outer", "left"])]);
        assert_eq!(ran_right, vec![path(&["outer", "right"])]);
        assert_eq!(run.join(&graph), Decision::Run(path(&["outer", "merge"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["after"])),
            "finishing the nested plan's join finishes the step that holds it"
        );
    }

    #[test]
    fn a_branch_that_leaves_its_nested_plan_ends_its_lane() {
        let inner = plain_graph(
            &["split", "left", "right"],
            &[("split", "left"), ("split", "right")],
        );
        let mut graph = ArchitectGraph::default();
        let mut outer = ArchitectNode::new("outer", "Outer");
        outer.subplan = Some(Box::new(inner));
        graph.add_node(outer);
        graph.add_node(ArchitectNode::new("after", "After"));
        graph.connect("outer", "after");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(fork_join(&run.finish_step(&graph)), None);
        let mut lanes = run.fork_lanes(&graph);
        let (ran, outcome) = run_lane(&mut lanes[0], &graph);
        assert_eq!(
            ran,
            vec![path(&["outer", "left"])],
            "a lane never climbs out of the plan it was forked in"
        );
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(run.join(&graph), Decision::Run(path(&["after"])));
    }

    #[test]
    fn an_outer_retry_restarts_all_nested_roots_after_a_local_retry_finishes() {
        let mut nested = plain_graph(
            &["before", "source", "next", "sibling"],
            &[("before", "source"), ("source", "next")],
        );
        let mut local_retry = ArchitectEdge::new("local-retry", "next", "source");
        local_retry.max_repeats = Some(1);
        nested.edges.push(local_retry);
        let mut graph = plain_graph(&["parent", "after"], &[("parent", "after")]);
        graph.node_mut(&id("parent")).expect("parent").subplan = Some(Box::new(nested));
        let mut outer_retry = ArchitectEdge::new("outer-retry", "after", "parent");
        outer_retry.max_repeats = Some(1);
        graph.edges.push(outer_retry);
        let mut run = PlanRun::start(&graph).expect("nested retry plan");
        let (steps, outcome) = run_lane(&mut run, &graph);
        let iteration = vec![
            path(&["parent", "before"]),
            path(&["parent", "source"]),
            path(&["parent", "next"]),
            path(&["parent", "source"]),
            path(&["parent", "next"]),
            path(&["parent", "sibling"]),
            path(&["after"]),
        ];
        assert_eq!(steps, [iteration.clone(), iteration].concat());
        assert_eq!(outcome, RunOutcome::Completed);
    }

    #[test]
    fn serial_retries_can_share_files_with_their_predecessors_and_successors() {
        for conditional in [false, true] {
            let mut graph = plain_graph(
                &["before", "edit", "review", "after", "tail"],
                &[
                    ("before", "edit"),
                    ("edit", "review"),
                    ("review", "after"),
                    ("after", "tail"),
                ],
            );
            for node in &mut graph.nodes {
                node.file_surface = Some(vec!["worktree/src/shared.rs".into()]);
            }
            let mut retry = ArchitectEdge::new("retry", "review", "edit");
            retry.max_repeats = Some(1);
            if conditional {
                retry.condition = EdgeCondition::Objective {
                    statement: "Needs another edit".into(),
                };
            }
            graph.edges.push(retry);
            assert!(graph.file_surface_problems().is_empty());
            assert!(crate::compile_spec(&graph).is_ok());
            let mut run = PlanRun::start(&graph).expect("serial file sharing");
            assert_eq!(run.current(), path(&["before"]));
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["edit"])));
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["review"])));
            let mut decision = run.finish_step(&graph);
            if conditional {
                assert!(matches!(decision, Decision::Ask(_)));
                decision = run.answer(&graph, true);
            }
            assert_eq!(decision, Decision::Run(path(&["edit"])));
            run.validate_checkpoint(&graph)
                .expect("serial loop checkpoint");
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["review"])));
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["after"])));
            assert_eq!(run.finish_step(&graph), Decision::Run(path(&["tail"])));
            assert_eq!(
                run.finish_step(&graph),
                Decision::Done(RunOutcome::Completed)
            );
        }
    }

    #[test]
    fn a_loop_lane_conflicts_with_its_peer_but_not_with_work_before_or_after_the_fork() {
        let mut graph = plain_graph(
            &["before", "split", "edit", "review", "peer", "join", "after"],
            &[
                ("before", "split"),
                ("split", "edit"),
                ("split", "peer"),
                ("edit", "review"),
                ("review", "join"),
                ("peer", "join"),
                ("join", "after"),
            ],
        );
        let mut retry = ArchitectEdge::new("retry", "review", "edit");
        retry.max_repeats = Some(1);
        graph.edges.push(retry);
        for node in &mut graph.nodes {
            node.file_surface = Some(vec!["worktree/src/shared.rs".into()]);
        }
        assert_eq!(
            graph.file_surface_problems(),
            vec![
                GraphProblem::FileSurfaceOverlap {
                    first: id("edit"),
                    second: id("peer"),
                    file: "worktree/src/shared.rs".into(),
                },
                GraphProblem::FileSurfaceOverlap {
                    first: id("review"),
                    second: id("peer"),
                    file: "worktree/src/shared.rs".into(),
                },
            ]
        );
        graph.node_mut(&id("peer")).expect("peer").file_surface =
            Some(vec!["worktree/peer.rs".into()]);
        assert!(graph.file_surface_problems().is_empty());
        let mut run = PlanRun::start(&graph).expect("disjoint loop lane");
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["split"])));
        assert!(matches!(run.finish_step(&graph), Decision::Fork { .. }));
        let mut lanes = run.fork_lanes(&graph);
        assert_eq!(lanes.len(), 2);
        let (steps, outcome) = run_lane(lanes.first_mut().expect("loop lane"), &graph);
        assert_eq!(
            steps,
            vec![
                path(&["edit"]),
                path(&["review"]),
                path(&["edit"]),
                path(&["review"])
            ]
        );
        assert_eq!(outcome, RunOutcome::Completed);
        let (steps, outcome) = run_lane(lanes.get_mut(1).expect("peer lane"), &graph);
        assert_eq!(steps, vec![path(&["peer"])]);
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(run.join(&graph), Decision::Run(path(&["join"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["after"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn overlapping_loop_branches_remain_fail_closed_without_tainting_predecessors() {
        let mut graph = plain_graph(
            &["before", "split", "left", "right", "join", "after"],
            &[
                ("before", "split"),
                ("split", "left"),
                ("split", "right"),
                ("left", "join"),
                ("right", "join"),
                ("join", "after"),
            ],
        );
        let mut retry = ArchitectEdge::new("retry", "join", "left");
        retry.max_repeats = Some(1);
        graph.edges.push(retry);
        for node in &mut graph.nodes {
            node.file_surface = Some(vec!["worktree/src/shared.rs".into()]);
        }
        let problems = graph.file_surface_problems();
        assert!(problems.contains(&GraphProblem::FileSurfaceOverlap {
            first: id("left"),
            second: id("right"),
            file: "worktree/src/shared.rs".into(),
        }));
        assert!(!problems.iter().any(|problem| matches!(problem,
            GraphProblem::FileSurfaceOverlap { first, second, .. }
                if [id("before"), id("split")].contains(first)
                    || [id("before"), id("split")].contains(second)
        )));
        assert!(PlanRun::start(&graph).is_err());
    }

    #[test]
    fn a_limited_plain_loop_repeats_on_its_own_before_the_fork() {
        let mut graph = plain_graph(
            &["draft", "polish", "left", "right"],
            &[("draft", "polish"), ("polish", "left"), ("polish", "right")],
        );
        let mut again = ArchitectEdge::new("again", "polish", "draft");
        again.max_repeats = Some(1);
        graph.edges.push(again);
        for node in &mut graph.nodes {
            node.file_surface = Some(vec![if node.id == id("right") {
                "worktree/right.rs".into()
            } else {
                "worktree/shared.rs".into()
            }]);
        }
        assert_eq!(graph.blocking_problems(), vec![]);

        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["polish"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["draft"])));
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["polish"])));
        assert!(matches!(run.finish_step(&graph), Decision::Fork { .. }));
    }

    #[test]
    fn a_branch_that_loops_back_to_its_fork_forever_is_an_endless_loop() {
        let graph = plain_graph(&["a", "b", "c"], &[("a", "c"), ("a", "b"), ("b", "a")]);
        assert!(
            graph
                .problems()
                .iter()
                .any(|problem| matches!(problem, GraphProblem::EndlessLoop(_))),
            "got {:?}",
            graph.problems()
        );
    }

    #[test]
    fn a_parallel_step_receives_advisory_coordination_context() {
        let mut graph = plain_graph(&["a", "b", "c"], &[("a", "b"), ("a", "c")]);
        graph.node_mut(&id("c")).unwrap().intent = "Write the migration".into();
        let prompt = parallel_steps_prompt(&graph, &[path(&["c"])]);
        assert!(prompt.contains("C: Write the migration"), "{prompt}");
        assert!(prompt.contains("git commit"), "{prompt}");
        assert!(
            prompt.contains("anticipated scope for advisory scheduling"),
            "{prompt}"
        );
        assert!(!prompt.contains("Do not change files"), "{prompt}");
        assert_eq!(parallel_steps_prompt(&graph, &[]), "");
    }

    // ── pausing, resuming, and starting part way ──────────────────────────

    #[test]
    fn deciding_again_picks_up_exactly_where_the_run_was_left() {
        let graph = plain_graph(
            &["a", "b", "c", "d"],
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let fork = run.finish_step(&graph);
        assert_eq!(
            run.decide(&graph),
            fork,
            "a paused fork is still the same fork"
        );

        let mut lanes = run.fork_lanes(&graph);
        assert_eq!(lanes[0].decide(&graph), Decision::Run(path(&["b"])));
        assert_eq!(
            lanes[0].decide(&graph),
            Decision::Run(path(&["b"])),
            "a step not yet reported is still the one to run"
        );
        assert_eq!(
            lanes[0].finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
        assert_eq!(
            lanes[0].decide(&graph),
            Decision::Done(RunOutcome::Completed)
        );
        assert_eq!(lanes[1].decide(&graph), Decision::Run(path(&["c"])));
    }

    #[test]
    fn an_interrupted_step_is_run_again_as_a_new_attempt() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["edit"])));
        let before = run.steps_taken();

        assert_eq!(run.retry_step(&graph), Decision::Run(path(&["edit"])));
        assert_eq!(run.attempt(&id("edit")), 2);
        assert_eq!(run.steps_taken(), before + 1);
        assert_eq!(
            run.finish_step(&graph),
            Decision::Run(path(&["test"])),
            "after the retry the run carries on as usual"
        );
    }

    #[test]
    fn a_step_that_keeps_failing_runs_into_its_visit_limit() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        let mut decision = run.decide(&graph);
        for _ in 0..MAX_NODE_VISITS {
            decision = run.retry_step(&graph);
        }
        assert_eq!(
            decision,
            Decision::Done(RunOutcome::NodeLimit {
                node: id("plan"),
                visits: MAX_NODE_VISITS + 1,
            })
        );
    }

    #[test]
    fn a_run_can_start_part_way_through() {
        let graph = locked_graph();
        let mut run = PlanRun::start_at(&graph, &path(&["test"])).unwrap();
        assert_eq!(run.decide(&graph), Decision::Run(path(&["test"])));
        assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(
            run.answer(&graph, false),
            Decision::Done(RunOutcome::Completed),
            "steps before the starting point are not run"
        );
    }

    #[test]
    fn a_run_started_inside_a_nested_plan_carries_on_outwards() {
        let graph = nested_graph();
        let start = path(&["handlers", "respond"]);
        let mut run = PlanRun::start_at(&graph, &start).unwrap();
        assert_eq!(run.decide(&graph), Decision::Run(start));
        assert_eq!(run.depth(), 2);
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["ship"])));

        let mut run = PlanRun::start_at(&graph, &path(&["handlers"])).unwrap();
        assert_eq!(
            run.decide(&graph),
            Decision::Run(path(&["handlers", "parse"])),
            "starting at a step with a plan starts at that plan's entry"
        );
    }

    #[test]
    fn a_run_cannot_start_at_a_step_the_plan_does_not_have() {
        let graph = locked_graph();
        assert_eq!(
            PlanRun::start_at(&graph, &path(&["missing"])).unwrap_err(),
            RunRefusal::NoSuchStep(path(&["missing"]))
        );
    }

    // ── verdicts ──────────────────────────────────────────────────────────

    fn run_all(run: &mut PlanRun, graph: &ArchitectGraph) -> Vec<NodePath> {
        let mut paths = Vec::new();
        let mut decision = run.decide(graph);
        loop {
            decision = match decision {
                Decision::Run(path) => {
                    paths.push(path);
                    run.finish_step(graph)
                }
                Decision::Ask(_) => run.answer(graph, false),
                Decision::Fork { .. } => {
                    for mut lane in run.fork_lanes(graph) {
                        paths.extend(run_all(&mut lane, graph));
                    }
                    run.join(graph)
                }
                Decision::Done(outcome) => {
                    assert_eq!(outcome, RunOutcome::Completed);
                    return paths;
                }
            };
        }
    }

    #[test]
    fn separate_roots_wait_for_every_incoming_prerequisite() {
        let graph = plain_graph(&["a", "join", "b"], &[("a", "join"), ("b", "join")]);
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["b"])));
        let readiness = run.readiness(&graph);
        assert_eq!(
            readiness
                .iter()
                .find(|step| step.path == path(&["join"]))
                .unwrap()
                .status,
            "waiting"
        );
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["join"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
        assert_eq!(
            run.history(),
            &[path(&["a"]), path(&["b"]), path(&["join"])]
        );
    }

    #[test]
    fn a_partial_join_has_one_owner_and_waits_for_both_branches() {
        let graph = plain_graph(
            &["a", "b", "e", "c", "f", "d"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("a", "d"),
                ("b", "e"),
                ("c", "e"),
                ("e", "f"),
                ("d", "f"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(
            run_all(&mut run, &graph),
            vec![
                path(&["a"]),
                path(&["b"]),
                path(&["c"]),
                path(&["e"]),
                path(&["d"]),
                path(&["f"]),
            ]
        );
    }

    #[test]
    fn a_branch_bypassing_the_nominal_join_cannot_run_its_successors_early() {
        let graph = plain_graph(
            &["a", "b", "e", "f", "k", "later", "c", "join"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "e"),
                ("b", "f"),
                ("e", "join"),
                ("f", "k"),
                ("k", "later"),
                ("c", "join"),
                ("join", "later"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        let paths = run_all(&mut run, &graph);
        assert_eq!(
            paths
                .iter()
                .filter(|node| **node == path(&["later"]))
                .count(),
            1
        );
        assert!(
            paths.iter().position(|node| *node == path(&["later"]))
                > paths.iter().position(|node| *node == path(&["join"]))
        );
    }

    #[test]
    fn multiple_shared_successors_each_run_once() {
        let graph = plain_graph(
            &["a", "b", "c", "y", "x"],
            &[
                ("a", "b"),
                ("a", "c"),
                ("b", "x"),
                ("b", "y"),
                ("c", "x"),
                ("c", "y"),
            ],
        );
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(
            run_all(&mut run, &graph),
            vec![
                path(&["a"]),
                path(&["b"]),
                path(&["c"]),
                path(&["y"]),
                path(&["x"]),
            ]
        );
    }

    #[test]
    fn a_closed_conditional_prerequisite_is_skipped_not_executed() {
        let mut graph = plain_graph(
            &["a", "b", "join", "root"],
            &[("a", "b"), ("b", "join"), ("root", "join")],
        );
        graph.edges[0].condition = EdgeCondition::LlmEvaluated {
            question: "Run b?".into(),
        };
        let mut run = PlanRun::start(&graph).unwrap();
        assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(run.answer(&graph, false), Decision::Run(path(&["root"])));
        assert_eq!(
            run.readiness(&graph)
                .iter()
                .find(|step| step.path == path(&["b"]))
                .unwrap()
                .status,
            "skipped"
        );
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["join"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
        assert_eq!(
            run.readiness(&graph)
                .iter()
                .find(|step| step.path == path(&["b"]))
                .unwrap()
                .status,
            "skipped"
        );
    }

    #[test]
    fn a_disabled_internal_loop_preserves_structured_fork_lanes() {
        let mut graph = plain_graph(
            &["start", "a", "check", "b", "join"],
            &[
                ("start", "a"),
                ("start", "b"),
                ("a", "check"),
                ("check", "join"),
                ("b", "join"),
            ],
        );
        let mut disabled = ArchitectEdge::new("disabled", "check", "a");
        disabled.max_repeats = Some(0);
        graph.edges.push(disabled);
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(fork_join(&run.finish_step(&graph)), Some(id("join")));
        let mut lanes = run.fork_lanes(&graph);
        assert_eq!(lanes.len(), 2);
        let mut ran = Vec::new();
        for lane in &mut lanes {
            ran.extend(run_all(lane, &graph));
        }
        assert_eq!(ran, vec![path(&["a"]), path(&["check"]), path(&["b"])]);
        assert_eq!(run.join(&graph), Decision::Run(path(&["join"])));
        assert_eq!(
            run.finish_step(&graph),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn repeat_limits_gate_a_nominal_join_even_for_unconditional_routes() {
        for limits in [
            (Some(0), Some(0)),
            (Some(1), Some(0)),
            (Some(0), None),
            (Some(1), Some(1)),
        ] {
            let mut graph = plain_graph(
                &["start", "a", "b", "join"],
                &[("start", "a"), ("start", "b"), ("a", "join"), ("b", "join")],
            );
            for edge in &mut graph.edges {
                if edge.from == id("a") {
                    edge.max_repeats = limits.0;
                } else if edge.from == id("b") {
                    edge.max_repeats = limits.1;
                }
            }
            let mut run = PlanRun::start(&graph).unwrap();
            assert_eq!(fork_join(&run.finish_step(&graph)), Some(id("join")));
            let mut lanes = run.fork_lanes(&graph);
            assert_eq!(lanes.len(), 1, "the dependency region must own the join");
            let mut lane = lanes.remove(0);
            assert_eq!(lane.finish_step(&graph), Decision::Run(path(&["b"])));
            assert_eq!(
                lane.readiness(&graph)
                    .iter()
                    .find(|step| step.path == path(&["join"]))
                    .unwrap()
                    .status,
                "waiting",
                "a selected route still waits for the other branch"
            );
            let next = lane.finish_step(&graph);
            if limits == (Some(0), Some(0)) {
                assert_eq!(next, Decision::Done(RunOutcome::Completed));
                assert_eq!(
                    lane.readiness(&graph)
                        .iter()
                        .find(|step| step.path == path(&["join"]))
                        .unwrap()
                        .status,
                    "skipped"
                );
                assert!(!lane.history().contains(&path(&["join"])));
            } else {
                assert_eq!(next, Decision::Run(path(&["join"])));
                assert_eq!(
                    lane.finish_step(&graph),
                    Decision::Done(RunOutcome::Completed)
                );
                assert_eq!(
                    lane.history()
                        .iter()
                        .filter(|step| **step == path(&["join"]))
                        .count(),
                    1
                );
            }
            assert_eq!(run.join(&graph), Decision::Done(RunOutcome::Completed));
            assert!(
                !run.history().contains(&path(&["join"])),
                "the parent must not run it again"
            );
        }
    }

    #[test]
    fn a_nominal_join_is_skipped_when_every_branch_closes_its_route() {
        let mut graph = plain_graph(
            &["a", "b", "c", "join"],
            &[("a", "b"), ("a", "c"), ("b", "join"), ("c", "join")],
        );
        for edge in &mut graph.edges {
            if edge.to == id("join") {
                edge.condition = EdgeCondition::LlmEvaluated {
                    question: "Continue?".into(),
                };
            }
        }
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(
            run_all(&mut run, &graph),
            vec![path(&["a"]), path(&["b"]), path(&["c"])]
        );
    }

    #[test]
    fn overlapping_cyclic_prerequisites_fail_closed() {
        let mut graph = plain_graph(
            &["a", "root", "b", "check", "done"],
            &[("a", "b"), ("root", "b"), ("b", "check"), ("check", "done")],
        );
        graph
            .edges
            .push(ArchitectEdge::new("again", "check", "b").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Again?".into(),
                },
            ));
        let mut run = PlanRun::start(&graph).unwrap();
        assert!(matches!(
            run.decide(&graph),
            Decision::Done(RunOutcome::Failed { .. })
        ));
    }

    #[test]
    fn dependency_checkpoint_roundtrip_keeps_decisions_and_completed_steps() {
        let mut graph = plain_graph(
            &["a", "b", "join", "root"],
            &[("a", "b"), ("b", "join"), ("root", "join")],
        );
        graph.edges[0].condition = EdgeCondition::LlmEvaluated {
            question: "Run b?".into(),
        };
        let mut run = PlanRun::start(&graph).unwrap();
        let question = run.finish_step(&graph);
        let value = serde_json::to_value(&run).unwrap();
        let mut restored: PlanRun = serde_json::from_value(value).unwrap();
        restored.validate_checkpoint(&graph).unwrap();
        assert_eq!(restored.decide(&graph), question);
        assert_eq!(restored.answer(&graph, true), Decision::Run(path(&["b"])));
        assert_eq!(restored.finish_step(&graph), Decision::Run(path(&["root"])));
        let value = serde_json::to_value(&restored).unwrap();
        let mut restored: PlanRun = serde_json::from_value(value).unwrap();
        restored.validate_checkpoint(&graph).unwrap();
        assert_eq!(
            run_all(&mut restored, &graph),
            vec![path(&["root"]), path(&["join"])]
        );
    }

    #[test]
    fn loop_checkpoint_roundtrip_keeps_attempts_and_repeat_counters() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.finish_step(&graph);
        assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(run.answer(&graph, true), Decision::Run(path(&["edit"])));
        let value = serde_json::to_value(&run).unwrap();
        let mut restored: PlanRun = serde_json::from_value(value.clone()).unwrap();
        restored.validate_checkpoint(&graph).unwrap();
        assert_eq!(restored.attempt(&id("edit")), 2);
        assert_eq!(serde_json::to_value(&restored).unwrap(), value);
        assert_eq!(restored.finish_step(&graph), Decision::Run(path(&["test"])));
        assert!(matches!(restored.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(
            restored.answer(&graph, false),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn selecting_a_ready_root_never_admits_its_shared_successor() {
        let graph = plain_graph(&["a", "join", "b"], &[("a", "join"), ("b", "join")]);
        let mut run = PlanRun::start(&graph).unwrap();
        assert!(run.prioritize_ready(&path(&["join"]), &graph).is_err());
        run.prioritize_ready(&path(&["b"]), &graph).unwrap();
        run.validate_checkpoint(&graph).unwrap();
        assert_eq!(
            run_all(&mut run, &graph),
            vec![path(&["b"]), path(&["a"]), path(&["join"])]
        );
    }

    #[test]
    fn unlocked_draft_can_be_rebased_but_cannot_start_before_review() {
        let mut graph = plain_graph(&["a", "join", "b"], &[("a", "join"), ("b", "join")]);
        graph.node_mut(&id("b")).unwrap().locked = false;
        let run = PlanRun::rebase_remaining(&graph, &[path(&["a"])]).unwrap();
        run.validate_checkpoint(&graph).unwrap();
        assert_eq!(run.current(), path(&["b"]));
        assert!(!graph.node(&id("b")).unwrap().locked);
        assert!(matches!(
            PlanRun::start(&graph),
            Err(RunRefusal::NotReady(_))
        ));
        // Capture warnings remain attached to the draft; structural validation
        // neither approves locks nor rewrites the requirements being reviewed.
        assert_eq!(graph.steps_without_capture(), vec![id("a"), id("b")]);
        graph.edges.push(ArchitectEdge::new("bad", "b", "missing"));
        assert!(PlanRun::validate_structure(&graph).is_err());
    }

    #[test]
    fn rebase_retains_completed_prerequisites_without_replaying_them() {
        let graph = plain_graph(&["a", "join", "b"], &[("a", "join"), ("b", "join")]);
        let mut run = PlanRun::rebase_remaining(&graph, &[path(&["a"])]).unwrap();
        run.validate_checkpoint(&graph).unwrap();
        assert_eq!(
            run_all(&mut run, &graph),
            vec![path(&["b"]), path(&["join"])]
        );
    }

    #[test]
    fn checkpoint_rejects_a_forged_ready_shared_node() {
        let graph = plain_graph(&["a", "join", "b"], &[("a", "join"), ("b", "join")]);
        let run = PlanRun::start(&graph).unwrap();
        let mut value = serde_json::to_value(&run).unwrap();
        value["stack"][0]["current"] = serde_json::json!("join");
        value["stack"][0]["visits"]["join"] = serde_json::json!(1);
        let restored: PlanRun = serde_json::from_value(value).unwrap();
        assert!(restored.validate_checkpoint(&graph).is_err());
    }

    #[test]
    fn starting_and_restoring_checkpoints_refuse_missing_or_conflicting_surfaces() {
        let mut graph = plain_graph(
            &["root", "left", "right"],
            &[("root", "left"), ("root", "right")],
        );
        let run = PlanRun::start(&graph).expect("empty explicit surfaces are runnable");
        let saved = serde_json::to_value(&run).expect("save checkpoint");
        let restored: PlanRun = serde_json::from_value(saved).expect("restore checkpoint");
        for surface in [None, Some(vec!["worktree/../invalid.rs".into()])] {
            graph.node_mut(&id("left")).expect("left").file_surface = surface;
            assert!(PlanRun::start(&graph).is_err());
            assert!(PlanRun::start_at(&graph, &path(&["left"])).is_err());
            assert!(restored.validate_checkpoint(&graph).is_err());
        }
        for step in ["left", "right"] {
            graph.node_mut(&id(step)).expect("branch").file_surface =
                Some(vec!["worktree/shared.rs".into()]);
        }
        assert!(PlanRun::start(&graph).is_err());
        assert!(PlanRun::validate_structure(&graph).is_err());
        assert!(restored.validate_checkpoint(&graph).is_err());
        assert!(PlanRun::rebase_remaining(&graph, &[]).is_err());
        graph.node_mut(&id("right")).expect("right").file_surface = Some(Vec::new());
        restored
            .validate_checkpoint(&graph)
            .expect("corrected surface");
    }

    #[test]
    fn step_and_parallel_prompts_include_existing_file_contracts() {
        let mut graph = plain_graph(&["left", "right"], &[]);
        graph.node_mut(&id("left")).expect("left").file_surface =
            Some(vec!["worktree/src/left.rs".into()]);
        let prompt = step_prompt(&graph, &path(&["left"]), 1, 1);
        assert!(prompt.contains("worktree/src/left.rs"));
        assert!(prompt.contains("anticipated file assignments for advisory scheduling"));
        assert!(prompt.contains("including paths of files to create"));
        assert!(prompt.contains("Files need not exist"));
        assert!(prompt.contains("not an edit allowlist or a restriction"));
        assert!(!prompt.contains("Only touch existing files"));
        assert!(!prompt.contains("stop and request a surface correction"));
        assert!(prompt.contains("created file by its worktree-qualified path"));
        let prompt = parallel_steps_prompt(&graph, &[path(&["left"])]);
        assert!(prompt.contains("worktree/src/left.rs"));
        assert!(
            step_prompt(&graph, &path(&["right"]), 1, 1).contains("no file writes anticipated")
        );
        graph.node_mut(&id("right")).expect("right").file_surface = None;
        assert!(step_prompt(&graph, &path(&["right"]), 1, 1).contains("MISSING"));
    }

    #[test]
    fn a_verdict_is_read_from_the_first_word_of_the_reply() {
        assert_eq!(parse_verdict("YES\nThe tests failed."), Some(true));
        assert_eq!(parse_verdict("no, everything passed"), Some(false));
        assert_eq!(parse_verdict("**YES** - it failed"), Some(true));
    }

    #[test]
    fn a_no_that_goes_on_to_mention_yes_is_still_a_no() {
        assert_eq!(
            parse_verdict("NO\nIf they had failed the answer would be yes."),
            Some(false)
        );
    }

    #[test]
    fn a_reply_that_does_not_start_with_a_verdict_is_not_guessed_at() {
        assert_eq!(parse_verdict("I think the tests failed, so yes"), None);
        assert_eq!(parse_verdict(""), None);
    }
}
