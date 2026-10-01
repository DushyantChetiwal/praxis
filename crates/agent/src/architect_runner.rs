use std::cell::RefCell;
use std::rc::Rc;

use acp_thread::{AcpThread, AgentConnection as _, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema::v1 as acp;
use architect::{
    ArchitectGraph, Branch, Decision, MAX_NODE_VISITS, MAX_PLAN_DEPTH, MAX_RUN_STEPS, NodePath,
    PlanRun, RunOutcome, RunRefusal,
};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{LocalBoxFuture, join_all};
use gpui::{App, AsyncApp, Context, Entity, SharedString, WeakEntity};
use language_model::{LanguageModel, LanguageModelProviderId, LanguageModelRegistry};
use serde::{Deserialize, Serialize};

use crate::{
    ArchitectRun, ArchitectRunOutcome, ArchitectStepVisitId, NativeAgentConnection, SessionMode,
    Thread,
};

#[derive(Debug)]
pub enum ArchitectRunStartError {
    AlreadyRunning,
    Refused(RunRefusal),
    /// There is no paused, stopped, or failed run to pick up.
    NotResumable,
    /// Lock review cannot authorize unrelated edits to the execution snapshot.
    ReviewMismatch,
}

impl std::fmt::Display for ArchitectRunStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => formatter.write_str("An Architect run is already in progress."),
            Self::Refused(RunRefusal::NothingToRun) => {
                formatter.write_str("There is no plan to run yet.")
            }
            Self::Refused(refusal @ RunRefusal::NotReady(_)) => {
                write!(formatter, "This plan is not ready to run: {refusal}")
            }
            Self::Refused(refusal @ RunRefusal::NoSuchStep(_)) => {
                write!(formatter, "The run cannot start there: {refusal}.")
            }
            Self::NotResumable => {
                formatter.write_str("There is no paused or interrupted run to resume.")
            }
            Self::ReviewMismatch => formatter.write_str(
                "The draft differs from the execution checkpoint. Apply the approved graph update before reviewing and resuming it.",
            ),
        }
    }
}

impl std::error::Error for ArchitectRunStartError {}

impl From<RunRefusal> for ArchitectRunStartError {
    fn from(refusal: RunRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Where a run is in every branch it has going, and the turns it is waiting
/// on.
///
/// Shared by the lanes driving the run and kept on the thread, which is how
/// stopping the run reaches every turn in flight, however many branches are
/// running.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunState {
    /// The run's own copy of the plan, with what each step has reported, so
    /// edits made while the run is going cannot change its control flow.
    graph: ArchitectGraph,
    /// Lane 0 is the run itself. The rest run the branches of forks.
    lanes: Vec<Lane>,
    /// Steps started so far across every lane. It numbers them, and bounds the
    /// whole run however many ways it forks.
    steps: usize,
    attempts: Vec<(NodePath, usize)>,
    // Weak, as everything here is, because the thread keeps this after a run
    // is stopped, and a strong conversation would keep that thread alive.
    #[serde(skip)]
    in_flight: Vec<(usize, WeakEntity<AcpThread>)>,
    /// Set once a lane fails, so the others stop instead of carrying on.
    halted: bool,
    /// While set, no lane starts a new turn.
    paused: bool,
    /// Lanes held back by the pause, woken when the run is resumed.
    #[serde(skip)]
    waiters: Vec<oneshot::Sender<()>>,
    /// Rebased checkpoints remain available for auditing invalidated work.
    archived_checkpoints: Vec<serde_json::Value>,
    rebase_error: Option<String>,
    invalidated: Vec<NodePath>,
    rebased: bool,
}

// Containers and not-yet-selected branches do not each consume a model turn,
// but a saved graph must still have a finite validation and allocation budget.
const MAX_CHECKPOINT_GRAPH_NODES: usize = MAX_RUN_STEPS * MAX_PLAN_DEPTH;
const MAX_CHECKPOINT_LANES: usize = MAX_RUN_STEPS * MAX_RUN_STEPS + 1;
const MAX_CHECKPOINT_VALUES: usize = 1_000_000;

fn validate_graph_budget(graph: &ArchitectGraph) -> anyhow::Result<()> {
    let mut pending = vec![(graph, 1)];
    let mut nodes = 0;
    let mut edges = 0;
    while let Some((graph, depth)) = pending.pop() {
        anyhow::ensure!(
            depth <= MAX_PLAN_DEPTH,
            "Checkpoint graph exceeds the nesting limit"
        );
        nodes += graph.nodes.len();
        edges += graph.edges.len();
        anyhow::ensure!(
            nodes <= MAX_CHECKPOINT_GRAPH_NODES,
            "Checkpoint graph exceeds the total node budget"
        );
        anyhow::ensure!(
            edges <= MAX_RUN_STEPS * MAX_RUN_STEPS,
            "Checkpoint graph exceeds the total edge budget"
        );
        let mut edge_ids = std::collections::HashSet::new();
        for edge in &graph.edges {
            anyhow::ensure!(
                !edge.id.0.is_empty() && edge_ids.insert(&edge.id),
                "Checkpoint graph contains an empty or duplicate edge ID"
            );
        }
        for node in &graph.nodes {
            anyhow::ensure!(
                !node.id.0.is_empty(),
                "Checkpoint graph contains an empty node ID"
            );
            anyhow::ensure!(
                node.result
                    .as_ref()
                    .is_none_or(|result| result.attempt <= MAX_NODE_VISITS),
                "Checkpoint result exceeds the attempt budget"
            );
            if let Some(nested) = node.subplan() {
                pending.push((nested, depth + 1));
            }
        }
    }
    Ok(())
}

fn validate_checkpoint_value(value: &serde_json::Value) -> anyhow::Result<()> {
    validate_checkpoint_value_with_budget(value, MAX_CHECKPOINT_VALUES)
}

fn validate_checkpoint_value_with_budget(
    value: &serde_json::Value,
    budget: usize,
) -> anyhow::Result<()> {
    let mut pending = vec![(value, 0)];
    let mut values = 0;
    while let Some((value, depth)) = pending.pop() {
        values += 1;
        anyhow::ensure!(
            values <= budget && depth <= 128,
            "Checkpoint exceeds the serialized value-count or nesting budget"
        );
        match value {
            serde_json::Value::Array(items) => {
                anyhow::ensure!(
                    items.len() <= budget,
                    "Checkpoint array exceeds the allocation budget"
                );
                pending.extend(items.iter().map(|value| (value, depth + 1)));
            }
            serde_json::Value::Object(items) => {
                anyhow::ensure!(
                    items.len() <= budget,
                    "Checkpoint object exceeds the allocation budget"
                );
                pending.extend(items.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

impl RunState {
    fn new(graph: ArchitectGraph, run: PlanRun) -> Self {
        let lane = Lane::new(run, &graph);
        Self {
            graph,
            lanes: vec![lane],
            steps: 0,
            attempts: Vec::new(),
            in_flight: Vec::new(),
            halted: false,
            paused: false,
            waiters: Vec::new(),
            archived_checkpoints: Vec::new(),
            rebase_error: None,
            invalidated: Vec::new(),
            rebased: false,
        }
    }

    pub(crate) fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "state": self,
            "in_flight": self.in_flight.iter().map(|(lane, _)| *lane).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn from_checkpoint(value: serde_json::Value) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Checkpoint {
            version: u32,
            state: RunState,
            in_flight: Vec<usize>,
        }
        validate_checkpoint_value(&value)?;
        let checkpoint: Checkpoint = serde_json::from_value(value)?;
        anyhow::ensure!(
            checkpoint.version == 1,
            "Unsupported Architect checkpoint version {}",
            checkpoint.version
        );
        let mut state = checkpoint.state;
        anyhow::ensure!(
            !state.lanes.is_empty() && state.lanes.len() <= MAX_CHECKPOINT_LANES,
            "The checkpoint has no root lane or exceeds the total lane budget"
        );
        validate_graph_budget(&state.graph)?;
        anyhow::ensure!(!state.graph.is_empty(), "The checkpoint graph is empty");
        anyhow::ensure!(
            state.invalidated.len() <= 2 * MAX_CHECKPOINT_GRAPH_NODES
                && state
                    .invalidated
                    .iter()
                    .all(|path| !path.is_empty() && path.depth() <= MAX_PLAN_DEPTH),
            "Checkpoint invalidation paths exceed the graph budget"
        );
        anyhow::ensure!(
            state.steps <= MAX_RUN_STEPS,
            "The checkpoint exceeds the run step limit"
        );
        let mut attempts = std::collections::HashSet::new();
        let mut total_attempts = 0usize;
        for (path, count) in &state.attempts {
            anyhow::ensure!(
                !path.is_empty()
                    && path.depth() <= MAX_PLAN_DEPTH
                    && *count > 0
                    && *count <= MAX_NODE_VISITS
                    && attempts.insert(path),
                "The checkpoint contains invalid attempt counters"
            );
            total_attempts = total_attempts
                .checked_add(*count)
                .ok_or_else(|| anyhow::anyhow!("Checkpoint attempt counters overflow"))?;
            anyhow::ensure!(
                total_attempts <= MAX_RUN_STEPS,
                "Checkpoint attempts exceed the whole-run step budget"
            );
        }
        anyhow::ensure!(
            total_attempts == state.steps,
            "Checkpoint attempt counters disagree with the total steps started"
        );
        PlanRun::validate_structure(&state.graph).map_err(anyhow::Error::msg)?;
        let mut owned = std::collections::HashSet::new();
        let mut history_steps = 0usize;
        for (index, lane) in state.lanes.iter().enumerate() {
            lane.run
                .validate_checkpoint(&state.graph)
                .map_err(anyhow::Error::msg)?;
            history_steps = history_steps
                .checked_add(lane.run.steps_taken())
                .ok_or_else(|| anyhow::anyhow!("Checkpoint lane counters overflow"))?;
            anyhow::ensure!(
                history_steps <= (MAX_RUN_STEPS + state.lanes.len()) * MAX_PLAN_DEPTH,
                "Checkpoint histories exceed the aggregate step and pending-lane budget"
            );
            anyhow::ensure!(
                lane.run.clone().decide(&state.graph) == lane.next,
                "Lane {index} has an inconsistent next decision"
            );
            anyhow::ensure!(
                lane.children.is_empty() || matches!(lane.next, Decision::Fork { .. }),
                "Only a fork can own child lanes"
            );
            if !lane.children.is_empty() {
                anyhow::ensure!(
                    lane.children.len() == lane.run.fork_lanes(&state.graph).len(),
                    "Checkpoint fork is missing prerequisite lanes"
                );
            }
            for child in &lane.children {
                anyhow::ensure!(
                    *child > index && *child < state.lanes.len() && owned.insert(*child),
                    "The checkpoint lane tree is cyclic or contains an invalid child"
                );
            }
        }
        let mut admitted = std::collections::HashSet::new();
        for (index, lane) in state.lanes.iter().enumerate() {
            anyhow::ensure!(
                index == 0 || owned.contains(&index) || matches!(lane.next, Decision::Done(_)),
                "The checkpoint contains an unfinished orphan lane"
            );
            anyhow::ensure!(
                !lane.interrupted || matches!(lane.next, Decision::Run(_)),
                "Only an executable step can have an interrupted attempt"
            );
            if let Decision::Run(path) = &lane.next {
                anyhow::ensure!(
                    admitted.insert(path),
                    "Two lanes admit the same step before it completes"
                );
            }
        }
        let mut in_flight = std::collections::HashSet::new();
        for index in checkpoint.in_flight {
            anyhow::ensure!(
                in_flight.insert(index),
                "Checkpoint repeats an in-flight lane"
            );
            let lane = state
                .lanes
                .get_mut(index)
                .ok_or_else(|| anyhow::anyhow!("An in-flight checkpoint lane is missing"))?;
            anyhow::ensure!(
                matches!(lane.next, Decision::Run(_) | Decision::Ask(_)),
                "An in-flight lane is not on a turn"
            );
            if matches!(lane.next, Decision::Run(_)) {
                lane.interrupted = true;
            }
        }
        for archived in &state.archived_checkpoints {
            let value = archived
                .get("checkpoint")
                .ok_or_else(|| anyhow::anyhow!("Archived checkpoint is missing its saved state"))?;
            anyhow::ensure!(
                value
                    .pointer("/state/archived_checkpoints")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(Vec::is_empty),
                "Archived checkpoints must not recursively embed history"
            );
            Self::from_checkpoint(value.clone())?;
        }
        // Loading is never permission to execute. The owning Thread exposes an
        // interrupted run and only an explicit resume may create a driver.
        state.halted = true;
        state.paused = false;
        Ok(state)
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused
    }

    /// Compares execution identity without adopting root state or approving it.
    /// Persisted results, canvas positions, and review locks may legitimately lag
    /// behind the authoritative root; every other field and ordering must match.
    pub(crate) fn restore_is_compatible(&self, graph: &ArchitectGraph) -> bool {
        fn normalize(graph: &mut ArchitectGraph) {
            for node in &mut graph.nodes {
                node.result = None;
                node.position = None;
                node.locked = false;
                if let Some(nested) = node.subplan.as_deref_mut() {
                    normalize(nested);
                }
            }
        }
        let mut root = graph.clone();
        let mut checkpoint = self.graph.clone();
        normalize(&mut root);
        normalize(&mut checkpoint);
        root == checkpoint
    }

    pub(crate) fn validate_restore_graph(&self, graph: &ArchitectGraph) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.restore_is_compatible(graph),
            "The saved checkpoint does not match the current plan. Its history is retained, but it cannot be resumed."
        );
        Ok(())
    }

    fn validate_review(&self, live: &ArchitectGraph) -> Result<(), ArchitectRunStartError> {
        if self.rebase_error.is_some() {
            return Err(ArchitectRunStartError::NotResumable);
        }
        // Normalize copies only. Relocking the live plan must never smuggle a
        // different brief, result, model, or route into a frozen checkpoint.
        let mut reviewed = live.clone();
        let mut frozen = self.graph.clone();
        reviewed.lock_all();
        frozen.lock_all();
        if reviewed != frozen {
            return Err(ArchitectRunStartError::ReviewMismatch);
        }
        let problems = live.blocking_problems();
        if !problems.is_empty() {
            return Err(RunRefusal::NotReady(problems).into());
        }
        Ok(())
    }

    /// Only lock review is adopted from the live plan at resume. All other
    /// execution edits must already have passed apply_architect_graph_update.
    fn prepare_resume(&mut self, live: &ArchitectGraph) -> Result<(), ArchitectRunStartError> {
        self.validate_review(live)?;
        for lane in &self.lanes {
            if let Decision::Run(path) = &lane.next
                && self.graph.node_at(path).is_none()
            {
                return Err(RunRefusal::NoSuchStep(path.clone()).into());
            }
        }
        self.graph = live.clone();
        self.halted = false;
        self.paused = false;
        self.rebased = false;
        self.in_flight.clear();
        self.waiters.clear();
        for lane in &mut self.lanes {
            if lane.interrupted {
                lane.interrupted = false;
                lane.next = lane.run.retry_step(&self.graph);
            }
        }
        Ok(())
    }

    /// The steps other lanes are carrying out, or are about to, which a step
    /// starting now runs alongside.
    fn running_alongside(&self, lane: usize) -> Vec<NodePath> {
        self.lanes
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != lane)
            .filter_map(|(_, other)| match &other.next {
                Decision::Run(path) => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn restore_interrupted_results(&self, graph: &mut ArchitectGraph) {
        for lane in &self.lanes {
            if lane.interrupted
                && let Decision::Run(path) = &lane.next
                && let Some(step) = graph.node_at_mut(path)
            {
                // A tool report is provisional until its turn finishes successfully.
                step.result = self
                    .graph
                    .node_at(path)
                    .and_then(|step| step.result.clone());
            }
        }
    }

    /// Hands over every turn in flight, for stopping them.
    pub(crate) fn take_in_flight(&mut self) -> Vec<Entity<AcpThread>> {
        self.in_flight
            .drain(..)
            .filter_map(|(_, turn)| turn.upgrade())
            .collect()
    }
}

/// One line of work through the plan: the run itself, or a branch of a fork.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lane {
    run: PlanRun,
    /// What the lane does next. It stays on a step until that step has been
    /// reported, so a lane that is interrupted knows which step it was on.
    next: Decision,
    /// The lanes running the branches of the fork this lane is waiting on.
    children: Vec<usize>,
    /// Whether the step `next` names was started and never finished, so a run
    /// picked up again counts running it once more as another attempt.
    interrupted: bool,
    /// Where the lane's latest step ran. The question deciding its way out is
    /// put there, because that conversation saw the work it is about.
    #[serde(skip)]
    last_step_thread: Option<WeakEntity<AcpThread>>,
    last_step_session_id: Option<acp::SessionId>,
}

impl Lane {
    fn new(mut run: PlanRun, graph: &ArchitectGraph) -> Self {
        let next = run.decide(graph);
        Self {
            run,
            next,
            children: Vec::new(),
            interrupted: false,
            last_step_thread: None,
            last_step_session_id: None,
        }
    }
}

/// Starts the application-owned execution of an Architect graph.
///
/// The graph is copied before this function is called, so edits made while a
/// run is in flight cannot change its control flow. The thread owns the task,
/// allowing execution to continue when the canvas is closed.
pub fn start_architect_run(
    thread: Entity<Thread>,
    acp_thread: Entity<AcpThread>,
    graph: ArchitectGraph,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    start_run(thread, acp_thread, graph, None, cx)
}

/// Starts a run at one step of the plan, at any depth, instead of at its
/// entry. What every step last reported is kept, so the steps from there on
/// are told what the steps before them did.
pub fn start_architect_run_from(
    thread: Entity<Thread>,
    acp_thread: Entity<AcpThread>,
    graph: ArchitectGraph,
    from: NodePath,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    start_run(thread, acp_thread, graph, Some(from), cx)
}

fn start_run(
    thread: Entity<Thread>,
    acp_thread: Entity<AcpThread>,
    mut graph: ArchitectGraph,
    from: Option<NodePath>,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    if thread
        .read(cx)
        .architect_run()
        .is_some_and(ArchitectRun::is_running)
    {
        return Err(ArchitectRunStartError::AlreadyRunning);
    }

    let plan_run = match &from {
        Some(from) => PlanRun::start_at(&graph, from)?,
        None => PlanRun::start(&graph)?,
    };
    let first_step = plan_run.current();
    let first_title = step_title(&graph, &first_step);

    // A run from the start must not inherit summaries from an earlier attempt.
    // Clear both the immutable run snapshot and the live graph that
    // complete_step writes into. A run started part way keeps them, since the
    // steps it skips are what the steps it runs build on.
    let fresh = from.is_none();
    if fresh {
        graph.clear_results();
    }
    thread.update(cx, |thread, cx| {
        if fresh {
            thread.update_architect_graph(|graph| graph.clear_results(), cx);
        }
        thread.set_session_mode(SessionMode::Build, cx);
    });

    let state = Rc::new(RefCell::new(RunState::new(graph, plan_run)));
    let driver = Driver::new(&thread, acp_thread, state.clone(), cx);
    let task = cx.spawn(async move |cx| driver.run_to_end(cx).await);
    thread.update(cx, |thread, cx| {
        thread.start_architect_run(first_step, first_title, task, cx);
        thread.set_architect_run_control(state);
        thread.checkpoint_architect_run(cx);
    });
    Ok(())
}

/// Revises an execution brief after explicit coordinator approval, without changing
/// topology, results, or the run checkpoint. `goal` replaces `ArchitectNode::intent`;
/// omitted fields are preserved and supplied empty values clear those fields.
///
/// This is an authorized execution change, not a drafting edit: existing locks on
/// the step and its ancestors stay intact. Callers must obtain approval before
/// invoking it. Active steps (including branch decisions) require stopping first;
/// completed steps and containers with completed descendants cannot be revised.
pub fn update_architect_step(
    thread: &Entity<Thread>,
    path: &NodePath,
    goal: Option<String>,
    rules: Option<Vec<String>>,
    capture: Option<String>,
    cx: &mut App,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        goal.is_some() || rules.is_some() || capture.is_some(),
        "Provide a goal, rules, or capture requirement to revise."
    );
    let owner = thread.read(cx);
    let step = owner
        .architect_graph()
        .and_then(|graph| graph.node_at(path))
        .ok_or_else(|| anyhow::anyhow!("The plan has no step {path}."))?;
    ensure_architect_step_inactive(owner, path)?;
    anyhow::ensure!(
        !has_completed_architect_work(step),
        "Step {path} already has completed work. Its execution brief cannot be rewritten."
    );
    let control = owner
        .architect_run()
        .and_then(ArchitectRun::control)
        .cloned();
    let apply = |step: &mut architect::ArchitectNode| {
        if let Some(goal) = &goal {
            step.intent = goal.clone();
        }
        if let Some(rules) = &rules {
            step.rules = rules.clone();
        }
        if let Some(capture) = &capture {
            step.capture = capture.clone();
        }
    };
    if let Some(control) = &control {
        let mut state = control.borrow_mut();
        let step = state.graph.node_at_mut(path).ok_or_else(|| {
            anyhow::anyhow!(
                "Step {path} is not in the run checkpoint. Start a new run to change its topology."
            )
        })?;
        anyhow::ensure!(
            !has_completed_architect_work(step),
            "Step {path} already has completed work in the checkpoint. Its execution brief cannot be rewritten."
        );
        apply(step);
    }
    thread.update(cx, |thread, cx| {
        thread.update_architect_graph(
            |graph| {
                if let Some(step) = graph.node_at_mut(path) {
                    apply(step);
                }
            },
            cx,
        );
    });
    Ok(())
}

fn has_completed_architect_work(step: &architect::ArchitectNode) -> bool {
    step.result.is_some()
        || step
            .subplan()
            .is_some_and(|graph| graph.nodes.iter().any(has_completed_architect_work))
}

fn ensure_architect_step_inactive(thread: &Thread, path: &NodePath) -> anyhow::Result<()> {
    if let Some(run) = thread.architect_run().filter(|run| run.is_running()) {
        let active = run
            .running_steps()
            .iter()
            .any(|step| step.path.as_slice().starts_with(path.as_slice()))
            || run.control().is_some_and(|control| {
                control.borrow().lanes.iter().any(|lane| {
                    matches!(lane.next, Decision::Ask(_))
                        && lane.run.current().as_slice().starts_with(path.as_slice())
                })
            });
        anyhow::ensure!(
            !active,
            "Stop the run before revising active step {path} or its enclosing brief."
        );
    }
    Ok(())
}

/// Changes the model for subsequent visits to a step without changing run topology.
/// Stop the run before changing a step whose execution or branch decision is active.
/// Completed visits and their conversations retain the model they actually used;
/// changing a completed step's selection only affects later visits or runs.
pub fn set_architect_step_model(
    thread: &Entity<Thread>,
    path: &NodePath,
    model: Option<architect::StepModel>,
    cx: &mut App,
) -> anyhow::Result<()> {
    set_architect_step_models(thread, &[(path.clone(), model)], cx)
}

/// Validates the entire batch before changing either the live or frozen plan.
pub fn set_architect_step_models(
    thread: &Entity<Thread>,
    models: &[(NodePath, Option<architect::StepModel>)],
    cx: &mut App,
) -> anyhow::Result<()> {
    let owner = thread.read(cx);
    let control = owner
        .architect_run()
        .and_then(ArchitectRun::control)
        .cloned();
    let mut seen = std::collections::HashSet::new();
    for (path, model) in models {
        anyhow::ensure!(
            seen.insert(path.clone()),
            "Step {path} appears more than once in the model batch."
        );
        anyhow::ensure!(
            owner
                .architect_graph()
                .and_then(|graph| graph.node_at(path))
                .is_some(),
            "The plan has no step {path}."
        );
        ensure_architect_step_inactive(owner, path)?;
        if let Some(control) = &control {
            anyhow::ensure!(
                control.borrow().graph.node_at(path).is_some(),
                "Step {path} is not in the frozen checkpoint. Apply the graph update first."
            );
        }
        if let Some(model) = model {
            resolve_step_model(model, cx)?;
        }
    }
    let apply = |graph: &mut ArchitectGraph| {
        for (path, model) in models {
            if let Some(step) = graph.node_at_mut(path) {
                step.model = model.clone();
            }
        }
    };
    if let Some(control) = &control {
        apply(&mut control.borrow_mut().graph);
    }
    thread.update(cx, |thread, cx| {
        thread.update_architect_graph(apply, cx);
    });
    Ok(())
}

fn graph_paths(graph: &ArchitectGraph) -> Vec<NodePath> {
    fn visit(graph: &ArchitectGraph, parents: &NodePath, paths: &mut Vec<NodePath>) {
        for node in &graph.nodes {
            let mut path = parents.clone();
            path.0.push(node.id.clone());
            paths.push(path.clone());
            if let Some(nested) = node.subplan() {
                visit(nested, &path, paths);
            }
        }
    }
    let mut paths = Vec::new();
    visit(graph, &NodePath::default(), &mut paths);
    paths
}

fn topology(graph: &ArchitectGraph) -> serde_json::Value {
    serde_json::json!({
        "edges": graph.edges,
        "nodes": graph.nodes.iter().map(|node| serde_json::json!({
            "id": node.id,
            "subplan": node.subplan().map(topology),
        })).collect::<Vec<_>>(),
    })
}

/// Canonical inspection of the frozen scheduler, not speculative draft routing.
/// Terminal checkpoints are decoded for inspection only, never installed as control.
pub fn architect_run_readiness(thread: &Thread) -> serde_json::Value {
    let run = thread.architect_run();
    let control = run.and_then(ArchitectRun::control);
    let live = control.map(|control| control.borrow());
    let saved = if live.is_none() {
        run.and_then(ArchitectRun::saved_checkpoint)
            .map(|value| RunState::from_checkpoint(value.clone()))
    } else {
        None
    };
    let checkpoint_error = saved
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(|error| error.to_string());
    let state = live
        .as_deref()
        .or_else(|| saved.as_ref().and_then(|result| result.as_ref().ok()));
    let legacy_snapshot = run.filter(|_| state.is_none()).map(ArchitectRun::snapshot);
    let completed_run =
        run.is_some_and(|run| run.outcome.as_ref().is_some_and(RunOutcome::is_success));
    let read_only = run.is_some() && control.is_none();
    let graph = state
        .map(|state| &state.graph)
        .or_else(|| {
            legacy_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.graph.as_ref())
        })
        .or_else(|| thread.architect_graph());
    let Some(graph) = graph else {
        return serde_json::json!({ "steps": [], "reason": "No plan exists" });
    };
    let mut steps: Vec<_> = graph_paths(graph).into_iter().map(|path| {
        let completed_result = graph.node_at(&path).is_some_and(|node| node.result.is_some());
        let completed_history = completed_run && run.is_some_and(|run| {
            run.history().iter().any(|step| step.path.as_slice().starts_with(path.as_slice()))
        });
        let (status, reason) = if completed_result {
            ("completed", "A completed result is retained")
        } else if completed_history {
            ("completed", "Completed visit history records this step or its nested work")
        } else if run.is_some() && state.is_none() {
            ("unknown", "Saved routing state is unavailable; history cannot distinguish skipped from unfinished work")
        } else {
            ("waiting", "Waiting for incoming prerequisites or enclosing plan")
        };
        serde_json::json!({ "path": path, "status": status, "reason": reason })
    }).collect();
    let mut apply = |path: &NodePath, status: &str, reason: &str| {
        if let Some(step) = steps
            .iter_mut()
            .find(|step| step["path"] == serde_json::json!(path))
        {
            step["status"] = status.into();
            step["reason"] = reason.into();
        }
    };
    if let Some(state) = &state {
        for lane in &state.lanes {
            for ready in lane.run.readiness(graph) {
                apply(&ready.path, &ready.status, &ready.reason);
            }
        }
        for (index, lane) in state.lanes.iter().enumerate() {
            match &lane.next {
                Decision::Run(path) => apply(
                    path,
                    if state.in_flight.iter().any(|(active, _)| *active == index) {
                        "running"
                    } else {
                        "ready"
                    },
                    if lane.interrupted {
                        "Interrupted attempt; prerequisites remain satisfied"
                    } else {
                        "The scheduler has admitted this step"
                    },
                ),
                Decision::Ask(_) => apply(
                    &lane.run.current(),
                    "waiting",
                    "The step's branch verdict is unresolved",
                ),
                _ => {}
            }
        }
    } else if run.is_none()
        && let Ok(mut fresh) = PlanRun::start(graph)
    {
        for ready in fresh.readiness(graph) {
            apply(&ready.path, &ready.status, &ready.reason);
        }
        if let Decision::Run(path) = fresh.decide(graph) {
            apply(&path, "ready", "The plan entry is ready");
        }
    }
    let run_problems: Vec<_> = graph
        .blocking_problems()
        .into_iter()
        .map(|problem| problem.to_string())
        .collect();
    if !run_problems.is_empty() {
        for step in &mut steps {
            if step["status"] == "ready" {
                step["status"] = "waiting".into();
                step["reason"] =
                    "Resolve the run problems and review the plan before resuming".into();
            }
        }
    }
    if let Some(state) = &state {
        let complete = state
            .lanes
            .first()
            .is_some_and(|lane| matches!(lane.next, Decision::Done(RunOutcome::Completed)));
        for step in &mut steps {
            if state.rebase_error.is_some()
                && state
                    .invalidated
                    .iter()
                    .any(|path| step["path"] == serde_json::json!(path))
            {
                step["status"] = "invalidated".into();
                step["reason"] =
                    "The approved graph update invalidated this result; history is retained".into();
            } else if state.rebase_error.is_some() && step["status"] != "completed" {
                step["status"] = "waiting".into();
                step["reason"] =
                    "The graph update invalidated this checkpoint; review rebase_error".into();
            } else if complete && step["status"] == "waiting" {
                let visited = state.lanes.iter().any(|lane| {
                    lane.run
                        .history()
                        .iter()
                        .any(|path| step["path"] == serde_json::json!(path))
                });
                step["status"] = if visited { "completed" } else { "skipped" }.into();
                step["reason"] = if visited {
                    "The enclosing plan completed"
                } else {
                    "The completed run did not select this route"
                }
                .into();
            }
        }
    }
    if read_only {
        for step in &mut steps {
            if step["status"] == "ready" || step["status"] == "running" {
                step["status"] = "unknown".into();
                step["reason"] =
                    "This is a read-only saved run, not an executable pending visit".into();
            }
        }
    }
    serde_json::json!({
        "steps": steps,
        "read_only": read_only,
        "checkpoint_error": checkpoint_error,
        "ready_to_run": !read_only && !graph.is_empty() && run_problems.is_empty()
            && state.as_ref().is_none_or(|state| state.rebase_error.is_none()
                && !state.lanes.first().is_some_and(|lane| matches!(lane.next, Decision::Done(_)))),
        "run_problems": run_problems,
        "paused": state.as_ref().is_some_and(|state| state.paused),
        "halted": state.as_ref().is_some_and(|state| state.halted),
        "rebase_error": state.as_ref().and_then(|state| state.rebase_error.as_deref()),
        "invalidated_steps": state.as_ref().map(|state| &state.invalidated),
        "steps_started": state.as_ref().map(|state| state.steps).unwrap_or(0),
        "attempts": state.as_ref().map(|state| &state.attempts),
    })
}

/// Prioritizes an unfinished, admitted step without discarding any other lane.
/// Stop first: a paused driver can still own a suspended turn or decision.
pub fn resume_architect_run_at(
    thread: Entity<Thread>,
    acp: Entity<AcpThread>,
    path: NodePath,
    cx: &mut App,
) -> anyhow::Result<()> {
    let owner = thread.read(cx);
    let run = owner
        .architect_run()
        .ok_or_else(|| anyhow::anyhow!("There is no checkpoint to resume."))?;
    anyhow::ensure!(
        !run.is_running(),
        "Stop the run before selecting a resume step."
    );
    let control = run
        .control()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("The run has no recoverable checkpoint."))?;
    {
        let mut state = control.borrow_mut();
        anyhow::ensure!(
            state.rebase_error.is_none(),
            "{}",
            state.rebase_error.as_deref().unwrap_or_default()
        );
        anyhow::ensure!(
            run.outcome.as_ref().is_some_and(RunOutcome::is_resumable) || state.rebased,
            "This run is not resumable."
        );
        anyhow::ensure!(
            state.graph.node_at(&path).is_some(),
            "The checkpoint has no step {path}."
        );
        // A loop's pending visit may retain an earlier attempt's result. Only
        // scheduler admission, not the presence of that summary, proves readiness.
        let live = owner
            .architect_graph()
            .ok_or(ArchitectRunStartError::NotResumable)?;
        state.validate_review(live)?;
        let candidate = state
            .lanes
            .iter()
            .enumerate()
            .find_map(|(index, lane)| {
                let Decision::Run(current) = &lane.next else {
                    return None;
                };
                if lane.interrupted && current != &path {
                    return None;
                }
                let mut run = lane.run.clone();
                run.prioritize_ready(&path, &state.graph).ok()?;
                Some((index, run))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Step {path} is skipped or still waiting for incoming prerequisites."
                )
            })?;
        let (index, run) = candidate;
        state.lanes[index].run = run;
        state.lanes[index].next = Decision::Run(path);
    }
    resume_architect_run(thread, acp, cx)?;
    Ok(())
}

fn can_retain_lanes(state: &RunState, affected: &[NodePath]) -> bool {
    affected.iter().all(|path| {
        if state
            .graph
            .node_at(path)
            .is_some_and(|node| node.result.is_some())
        {
            return false;
        }
        state.lanes.iter().all(|lane| {
            if !lane.run.history().contains(path) {
                return true;
            }
            let current = lane.run.current();
            !lane.run.is_finished()
                && current.as_slice().starts_with(path.as_slice())
                && (current != *path || matches!(lane.next, Decision::Run(_)))
        })
    })
}

struct PreparedGraphUpdate {
    graph: ArchitectGraph,
    changed_steps: Vec<NodePath>,
    affected_locks: Vec<NodePath>,
    invalidated: Vec<NodePath>,
    run: Option<PlanRun>,
    rebase_error: Option<String>,
    checkpoint: Option<RunState>,
    checkpoint_error: Option<String>,
}

fn stage_graph_update(state: &RunState, prepared: &PreparedGraphUpdate) -> RunState {
    let mut archived_checkpoints = state.archived_checkpoints.clone();
    if prepared.run.is_some() || !prepared.invalidated.is_empty() {
        let mut archive = serde_json::json!({
            "checkpoint": state.checkpoint(),
            "invalidated": prepared.invalidated,
        });
        archive["checkpoint"]["state"]["archived_checkpoints"] = serde_json::json!([]);
        archived_checkpoints.push(archive);
    }
    RunState {
        graph: prepared.graph.clone(),
        lanes: match &prepared.run {
            Some(run) => vec![Lane::new(run.clone(), &prepared.graph)],
            None => state.lanes.clone(),
        },
        steps: state.steps,
        attempts: state.attempts.clone(),
        in_flight: Vec::new(),
        halted: true,
        paused: state.paused,
        waiters: Vec::new(),
        archived_checkpoints,
        rebase_error: None,
        invalidated: prepared.invalidated.clone(),
        rebased: prepared.run.is_some() || state.rebased,
    }
}

#[cfg(test)]
fn prepare_graph_update(
    old: &ArchitectGraph,
    graph: &ArchitectGraph,
    invalidated: &[NodePath],
    state: Option<&RunState>,
) -> anyhow::Result<PreparedGraphUpdate> {
    prepare_graph_update_with_budget(old, graph, invalidated, state, MAX_CHECKPOINT_VALUES)
}

fn prepare_graph_update_with_budget(
    old: &ArchitectGraph,
    graph: &ArchitectGraph,
    invalidated: &[NodePath],
    state: Option<&RunState>,
    checkpoint_budget: usize,
) -> anyhow::Result<PreparedGraphUpdate> {
    validate_graph_budget(old)?;
    validate_graph_budget(graph)?;
    let proposed = graph;
    let impact = architect::preview_graph_replacement(old, proposed).map_err(anyhow::Error::msg)?;
    PlanRun::validate_structure(&impact.graph).map_err(anyhow::Error::msg)?;
    let mut graph = impact.graph;
    let new_paths = graph_paths(&graph);
    let mut affected: std::collections::BTreeSet<_> =
        impact.invalidated_steps.into_iter().collect();
    for path in invalidated {
        anyhow::ensure!(
            old.node_at(path).is_some() || graph.node_at(path).is_some(),
            "Cannot invalidate missing step {path}."
        );
        affected.insert(path.clone());
    }
    let affected: Vec<_> = affected.into_iter().collect();
    // Locks and results supplied by the caller are not proof that invalidated
    // work remains usable. Preserve only the frozen results outside core impact.
    for path in &affected {
        if let Some(node) = graph.node_at_mut(path) {
            node.locked = false;
        }
    }
    let affected_locks = affected
        .iter()
        .filter(|path| {
            (old.node_at(path).is_some_and(|node| node.locked)
                || proposed.node_at(path).is_some_and(|node| node.locked))
                && graph.node_at(path).is_none_or(|node| !node.locked)
        })
        .cloned()
        .collect();
    graph.clear_results();
    for path in &new_paths {
        if !affected.contains(path)
            && let Some(result) = old.node_at(path).and_then(|node| node.result.clone())
            && let Some(node) = graph.node_at_mut(path)
        {
            node.result = Some(result);
        }
    }
    let mut run = None;
    let mut rebase_error = None;
    if let Some(state) = state {
        if topology(old) == topology(&graph) && can_retain_lanes(state, &affected) {
            for lane in &state.lanes {
                lane.run
                    .validate_checkpoint(&graph)
                    .map_err(anyhow::Error::msg)?;
            }
        } else {
            let completed: Vec<_> = new_paths
                .iter()
                .filter(|path| {
                    graph
                        .node_at(path)
                        .is_some_and(|node| node.result.is_some())
                })
                .cloned()
                .collect();
            match PlanRun::rebase_remaining(&graph, &completed) {
                Ok(rebased) => run = Some(rebased),
                Err(error) => rebase_error = Some(error),
            }
        }
    }
    let mut prepared = PreparedGraphUpdate {
        graph,
        changed_steps: impact.changed_steps,
        affected_locks,
        invalidated: affected,
        run,
        rebase_error,
        checkpoint: None,
        checkpoint_error: None,
    };
    if prepared.rebase_error.is_none()
        && let Some(state) = state
    {
        let prospective = stage_graph_update(state, &prepared);
        let checkpoint = prospective.checkpoint();
        match validate_checkpoint_value_with_budget(&checkpoint, checkpoint_budget) {
            Ok(()) => prepared.checkpoint = Some(prospective),
            Err(error) => {
                prepared.checkpoint_error = Some(format!(
                    "The proposed checkpoint, including preserved history, cannot be saved: {error}. The graph and checkpoint were not changed; no history was truncated."
                ))
            }
        }
    }
    Ok(prepared)
}

/// A dry run using exactly the same invalidation and checkpoint analysis as apply.
/// `lost_checkpoint` describes loss of resumability if the proposal were forced;
/// apply rejects that proposal without discarding the original checkpoint.
/// `changed_steps` reports direct frozen-to-proposed changes from core analysis.
/// `invalidated_paths` is the full frozen-to-candidate core impact, unioned with
/// the requested keys. `affected_locks` includes invalidated paths locked in
/// either input that the prepared graph reopens or removes, including parents.
/// `checkpoint_fits` includes the complete preserved archive;
/// `checkpoint_error` explains a rejected prospective checkpoint without mutation.
/// Model availability requires an App and is checked separately at apply time.
pub fn preview_architect_graph_update(
    thread: &Thread,
    graph: &ArchitectGraph,
    invalidated: &[NodePath],
) -> serde_json::Value {
    preview_architect_graph_update_with_budget(thread, graph, invalidated, MAX_CHECKPOINT_VALUES)
}

fn preview_architect_graph_update_with_budget(
    thread: &Thread,
    graph: &ArchitectGraph,
    invalidated: &[NodePath],
    checkpoint_budget: usize,
) -> serde_json::Value {
    let control = thread.architect_run().and_then(ArchitectRun::control);
    let state = control.map(|control| control.borrow());
    let old = state
        .as_ref()
        .map(|state| &state.graph)
        .or_else(|| thread.architect_graph());
    let prepared = old
        .ok_or_else(|| anyhow::anyhow!("There is no plan to update."))
        .and_then(|old| {
            prepare_graph_update_with_budget(
                old,
                graph,
                invalidated,
                state.as_deref(),
                checkpoint_budget,
            )
        });
    match prepared {
        Ok(prepared) => {
            let active = thread.architect_run().is_some_and(ArchitectRun::is_running)
                || state
                    .as_ref()
                    .is_some_and(|state| !state.in_flight.is_empty());
            let can_rebase = prepared.rebase_error.is_none();
            let checkpoint_fits = can_rebase && prepared.checkpoint_error.is_none();
            let run_problems: Vec<_> = prepared
                .graph
                .blocking_problems()
                .into_iter()
                .map(|problem| problem.to_string())
                .collect();
            let requires_review = graph_paths(&prepared.graph).iter().any(|path| {
                prepared
                    .graph
                    .node_at(path)
                    .is_some_and(|node| !node.locked)
            });
            let reason = if active {
                Some("Stop the run before applying a graph update.".to_string())
            } else {
                prepared
                    .rebase_error
                    .clone()
                    .or_else(|| prepared.checkpoint_error.clone())
            };
            let retained: Vec<_> = graph_paths(&prepared.graph)
                .into_iter()
                .filter(|path| {
                    prepared
                        .graph
                        .node_at(path)
                        .is_some_and(|node| node.result.is_some())
                })
                .collect();
            serde_json::json!({
                "can_rebase": can_rebase,
                "can_apply": can_rebase && checkpoint_fits && !active,
                "ready_to_run": !prepared.graph.is_empty() && run_problems.is_empty() && can_rebase && checkpoint_fits,
                "requires_review": requires_review,
                "run_problems": run_problems,
                "changed_steps": prepared.changed_steps,
                "affected_locks": prepared.affected_locks,
                "invalidated_paths": prepared.invalidated,
                "retained_results": retained,
                "lost_checkpoint": !can_rebase,
                "requires_restart": !can_rebase,
                "history_preserved": true,
                "reason": reason,
                "rebase_error": prepared.rebase_error,
                "checkpoint_fits": checkpoint_fits,
                "checkpoint_error": prepared.checkpoint_error,
                "model_validation": "checked_at_apply",
            })
        }
        Err(error) => serde_json::json!({
            "can_rebase": false,
            "can_apply": false,
            "ready_to_run": false,
            "requires_review": false,
            "run_problems": [],
            "changed_steps": [],
            "affected_locks": [],
            "invalidated_paths": [],
            "retained_results": [],
            "lost_checkpoint": false,
            "requires_restart": false,
            "history_preserved": true,
            "reason": error.to_string(),
            "rebase_error": null,
            "checkpoint_fits": false,
            "checkpoint_error": null,
            "model_validation": "checked_at_apply",
        }),
    }
}

/// Applies an approved clone atomically. Unsupported rebases and oversized
/// checkpoints are refused before any mutation or history truncation.
pub fn apply_architect_graph_update(
    thread: &Entity<Thread>,
    graph: ArchitectGraph,
    invalidated: &[NodePath],
    cx: &mut App,
) -> anyhow::Result<()> {
    apply_architect_graph_update_with_budget(thread, graph, invalidated, MAX_CHECKPOINT_VALUES, cx)
}

fn apply_architect_graph_update_with_budget(
    thread: &Entity<Thread>,
    graph: ArchitectGraph,
    invalidated: &[NodePath],
    checkpoint_budget: usize,
    cx: &mut App,
) -> anyhow::Result<()> {
    let owner = thread.read(cx);
    anyhow::ensure!(
        !owner.architect_run().is_some_and(ArchitectRun::is_running),
        "Stop the run before applying a graph update."
    );
    let control = owner
        .architect_run()
        .and_then(ArchitectRun::control)
        .cloned();
    let prepared = {
        let state = control.as_ref().map(|control| control.borrow());
        anyhow::ensure!(
            state
                .as_ref()
                .is_none_or(|state| state.in_flight.is_empty()),
            "Wait for interrupted turns to stop before updating the graph."
        );
        let old = state
            .as_ref()
            .map(|state| &state.graph)
            .or_else(|| owner.architect_graph())
            .ok_or_else(|| anyhow::anyhow!("There is no plan to update."))?;
        prepare_graph_update_with_budget(
            old,
            &graph,
            invalidated,
            state.as_deref(),
            checkpoint_budget,
        )?
    };
    if let Some(error) = &prepared.rebase_error {
        anyhow::bail!(
            "Cannot safely rebase this checkpoint: {error}. The graph and completed history were not changed. Preview and approve a separate restart instead."
        );
    }
    if let Some(error) = &prepared.checkpoint_error {
        anyhow::bail!("{error}");
    }
    for path in graph_paths(&prepared.graph) {
        if let Some(model) = prepared
            .graph
            .node_at(&path)
            .and_then(|step| step.model.as_ref())
        {
            resolve_step_model(model, cx)?;
        }
    }
    if let Some(control) = &control {
        let checkpoint = prepared.checkpoint.ok_or_else(|| {
            anyhow::anyhow!("The proposed checkpoint was not validated; the graph was not changed.")
        })?;
        *control.borrow_mut() = checkpoint;
    }
    thread.update(cx, |thread, cx| {
        thread.update_architect_graph(|graph| *graph = prepared.graph, cx);
    });
    Ok(())
}

pub(crate) fn resolve_step_model(
    selection: &architect::StepModel,
    cx: &App,
) -> anyhow::Result<LanguageModel> {
    let provider_id = LanguageModelProviderId::from(selection.provider.clone());
    let provider = LanguageModelRegistry::read_global(cx)
        .provider(&provider_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Model provider \"{}\" is unavailable. Configure it or choose another step model.",
                selection.provider
            )
        })?;
    provider
        .provided_models(cx)
        .into_iter()
        .find(|model| model.id().0.as_ref() == selection.model.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Model \"{}/{}\" is unavailable. Configure it or choose another step model.",
                selection.provider,
                selection.model
            )
        })
}

/// Pauses a run: no lane starts another step or question, and the turns
/// already under way are left to finish. The run then waits, keeping its
/// place in every branch, until it is resumed.
pub fn pause_architect_run(thread: &Entity<Thread>, cx: &mut App) {
    let Some(run) = thread.read(cx).architect_run() else {
        return;
    };
    let Some(control) = run.control().filter(|_| run.is_running()).cloned() else {
        return;
    };
    control.borrow_mut().paused = true;
    thread.update(cx, |thread, cx| thread.checkpoint_architect_run(cx));
}

/// Picks a run up where it left off. A paused run carries on. A run that was
/// stopped or failed starts again from the steps it was on, each counted as a
/// new attempt, keeping what every step before them reported.
pub fn resume_architect_run(
    thread: Entity<Thread>,
    acp_thread: Entity<AcpThread>,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    let run = thread
        .read(cx)
        .architect_run()
        .ok_or(ArchitectRunStartError::NotResumable)?;
    let control = run
        .control()
        .cloned()
        .ok_or(ArchitectRunStartError::NotResumable)?;

    if run.is_running() {
        let waiters = {
            let mut state = control.borrow_mut();
            if !state.paused {
                return Err(ArchitectRunStartError::AlreadyRunning);
            }
            let problems = state.graph.blocking_problems();
            if !problems.is_empty() {
                return Err(RunRefusal::NotReady(problems).into());
            }
            state.paused = false;
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            if waiter.send(()).is_err() {
                log::debug!("Architect: a paused lane stopped waiting before the run resumed");
            }
        }
        thread.update(cx, |thread, cx| thread.checkpoint_architect_run(cx));
        return Ok(());
    }

    if !run.outcome.as_ref().is_some_and(RunOutcome::is_resumable) && !control.borrow().rebased {
        return Err(ArchitectRunStartError::NotResumable);
    }
    let live = thread
        .read(cx)
        .architect_graph()
        .cloned()
        .ok_or(ArchitectRunStartError::NotResumable)?;
    control.borrow_mut().prepare_resume(&live)?;

    let driver = Driver::new(&thread, acp_thread, control, cx);
    let task = cx.spawn(async move |cx| driver.run_to_end(cx).await);
    thread.update(cx, |thread, cx| {
        thread.set_session_mode(SessionMode::Build, cx);
        thread.reopen_architect_run(task, cx);
    });
    Ok(())
}

/// Stops execution while preserving its checkpoint. Passing `None` never cancels
/// the owning conversation, so its coordinator can interrupt a run from a tool.
/// Pass the owning ACP thread only to also cancel legacy shared-thread execution.
pub fn stop_architect_run(
    thread: &Entity<Thread>,
    acp_thread: Option<&Entity<AcpThread>>,
    cx: &mut App,
) {
    // Only the turns the run is waiting on are interrupted. A step running in
    // a thread of its own leaves the plan's conversation alone, since the user
    // may be talking in it while the run goes on.
    let run = thread.read(cx).architect_run();
    let control = run.and_then(ArchitectRun::control);
    let working: Vec<Entity<AcpThread>> = match control {
        Some(control) => {
            let mut state = control.borrow_mut();
            state.halted = true;
            state.waiters.clear();
            state.take_in_flight()
        }
        None if run.is_some_and(ArchitectRun::is_running) => {
            let step_thread = run.and_then(ArchitectRun::step_thread);
            step_thread
                .or_else(|| acp_thread.cloned())
                .into_iter()
                .collect()
        }
        None => Vec::new(),
    };
    let owner_session_id = thread.read(cx).id().clone();
    thread.update(cx, |thread, cx| thread.stop_architect_run(cx));
    for working_thread in working {
        if acp_thread.is_none() && working_thread.read(cx).session_id() == &owner_session_id {
            continue;
        }
        working_thread
            .update(cx, |thread, cx| thread.cancel(cx))
            .detach();
    }
}

/// What every lane of a run shares.
#[derive(Clone)]
struct Driver {
    state: Rc<RefCell<RunState>>,
    // Weak, because the thread owns the task. Keeping a strong entity here
    // would form a cycle that keeps the thread alive forever.
    thread: WeakEntity<Thread>,
    plan_thread: Entity<AcpThread>,
    /// With the native agent each step runs in a fresh thread of its own, so a
    /// long plan never has to fit into one context window, and the branches of
    /// a fork run at the same time. Anything else runs every step in the plan's
    /// own conversation, one at a time.
    step_threads: Option<(Rc<NativeAgentConnection>, acp::SessionId)>,
    plan_title: SharedString,
}

impl Driver {
    fn new(
        thread: &Entity<Thread>,
        plan_thread: Entity<AcpThread>,
        state: Rc<RefCell<RunState>>,
        cx: &App,
    ) -> Self {
        let step_threads = plan_thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<NativeAgentConnection>()
            .map(|connection| (connection, plan_thread.read(cx).session_id().clone()));
        let plan_title = thread
            .read(cx)
            .title()
            .unwrap_or_else(|| SharedString::from("Untitled plan"));
        Self {
            state,
            thread: thread.downgrade(),
            plan_thread,
            step_threads,
            plan_title,
        }
    }

    async fn run_to_end(self, cx: &mut AsyncApp) {
        let outcome = drive_lane(self.clone(), 0, cx.clone()).await;

        if let Err(error) = self.thread.update(cx, |thread, cx| {
            thread.finish_architect_run(outcome, cx);
        }) {
            log::info!("Architect: run finished after its thread was closed: {error}");
        }
    }

    async fn drive(&self, lane: usize, cx: &mut AsyncApp) -> RunOutcome {
        loop {
            if self.state.borrow().halted {
                return RunOutcome::Cancelled;
            }
            let next = self.state.borrow().lanes[lane].next.clone();
            let starts_turn = matches!(next, Decision::Run(_) | Decision::Ask(_));
            if starts_turn && let Some(outcome) = self.wait_while_paused(cx).await {
                return outcome;
            }
            let next = match next {
                Decision::Run(node) => self.run_step(lane, node, cx).await,
                Decision::Ask(branch) => self.decide_branch(lane, branch, cx).await,
                Decision::Fork { .. } => self.run_fork(lane, cx).await,
                Decision::Done(outcome) => return outcome,
            };
            match next {
                Ok(next) => {
                    self.state.borrow_mut().lanes[lane].next = next;
                    if let Err(outcome) = self.checkpoint(cx) {
                        return outcome;
                    }
                }
                Err(outcome) => return outcome,
            }
        }
    }

    /// Carries out one step, and reports it to the lane's run, which says what
    /// the lane does next.
    async fn run_step(
        &self,
        lane: usize,
        node: NodePath,
        cx: &mut AsyncApp,
    ) -> Result<Decision, RunOutcome> {
        let concurrent = self.step_threads.is_some();
        let (step_number, attempt, title, mut prompt, model, note) = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            if state.steps >= MAX_RUN_STEPS {
                let steps = state.steps;
                return Err(RunOutcome::StepLimit { steps });
            }
            let attempt = state
                .attempts
                .iter()
                .find(|(path, _)| path == &node)
                .map(|(_, count)| count + 1)
                .unwrap_or(1);
            if attempt > MAX_NODE_VISITS {
                return Err(RunOutcome::NodeLimit {
                    node: node.leaf().cloned().ok_or_else(|| RunOutcome::Failed {
                        message: "The scheduler admitted an empty step path.".into(),
                    })?,
                    visits: attempt,
                });
            }
            state.steps += 1;
            let step_number = state.steps;
            if let Some((_, count)) = state.attempts.iter_mut().find(|(path, _)| path == &node) {
                *count = attempt;
            } else {
                state.attempts.push((node.clone(), attempt));
            }
            state.lanes[lane].interrupted = true;
            let title = step_title(&state.graph, &node);
            let prompt = architect::step_prompt(&state.graph, &node, step_number, attempt);
            let alongside = if concurrent {
                state.running_alongside(lane)
            } else {
                Vec::new()
            };
            let note = architect::parallel_steps_prompt(&state.graph, &alongside);
            let model = state
                .graph
                .node_at(&node)
                .and_then(|step| step.model.clone());
            (step_number, attempt, title, prompt, model, note)
        };

        let visit = self.update_thread(cx, |thread, cx| {
            thread.note_architect_run_position(
                node.clone(),
                title.clone(),
                step_number,
                attempt,
                cx,
            )
        })?;

        let step_thread = match &self.step_threads {
            Some((connection, plan_session_id)) => {
                let label = SharedString::from(format!("Step {step_number}: {title}"));
                let created = cx.update(|cx| {
                    connection.create_architect_run_step_thread(
                        plan_session_id,
                        label,
                        visit,
                        model.as_ref(),
                        cx,
                    )
                });
                match created {
                    Ok(step_thread) => {
                        prompt = format!(
                            "You are carrying out one step of the plan \"{}\". Earlier steps ran \
                             in conversations of their own; what they reported is included \
                             below.\n\n{prompt}{note}",
                            self.plan_title
                        );
                        step_thread
                    }
                    Err(error) => {
                        let message =
                            format!("No thread could be made for step \"{node}\": {error:#}");
                        return Err(self.fail_step(visit, message, cx));
                    }
                }
            }
            None if model.is_some() => {
                return Err(self.fail_step(
                    visit,
                    "Per-step models require the Praxis Agent. Choose it and resume the run."
                        .into(),
                    cx,
                ));
            }
            None => self.plan_thread.clone(),
        };

        // Tells the plan's conversation about the step's thread, so the UI can
        // show it and surface anything it asks to be allowed to do.
        if step_thread != self.plan_thread {
            let step_session_id = session_id(&step_thread, cx);
            self.plan_thread.update(cx, |thread, cx| {
                thread.subagent_spawned(step_session_id, cx);
            });
        }
        let step_session_id = session_id(&step_thread, cx);
        {
            let mut state = self.state.borrow_mut();
            state.lanes[lane].last_step_thread = Some(step_thread.downgrade());
            state.lanes[lane].last_step_session_id = Some(step_session_id);
        }
        self.update_thread(cx, |thread, cx| {
            thread.set_architect_run_step_thread(visit, &step_thread, cx);
        })?;

        self.begin_turn(lane, &step_thread, cx)?;
        let sent = send_and_wait(&step_thread, prompt, cx).await;
        let halted = self.end_turn(lane, cx)?;
        let reported_on_visit =
            self.update_thread(cx, |thread, _cx| thread.clear_architect_step_visit(visit))?;
        if halted || matches!(sent, Err(RunOutcome::Cancelled)) {
            self.finish_visit(visit, None, ArchitectRunOutcome::Cancelled, cx)?;
            return Err(RunOutcome::Cancelled);
        }
        if let Err(outcome) = sent {
            return Err(match outcome {
                RunOutcome::Failed { message } => self.fail_step(
                    visit,
                    format!("Step \"{node}\" could not run: {message}"),
                    cx,
                ),
                outcome => outcome,
            });
        }

        // Only a report made on this visit counts. The step may still carry
        // one from an earlier run, which says nothing about this one.
        let reported = if reported_on_visit {
            self.read_thread(cx, |thread, _| reported_summary(thread, &node))?
        } else {
            None
        };
        let summary = match reported {
            Some(summary) => summary,
            // Another lane failed and stopped this step before it reported,
            // so it is left to be run again.
            None if halted => {
                self.finish_visit(visit, None, ArchitectRunOutcome::Cancelled, cx)?;
                return Err(RunOutcome::Cancelled);
            }
            None => {
                log::warn!(
                    "Architect: {node} ended without calling complete_step; using its closing \
                     message as the summary"
                );
                let summary = closing_message(&step_thread, cx).trim().to_string();
                let path = node.clone();
                let recorded = summary.clone();
                self.update_thread(cx, |thread, cx| {
                    thread.update_architect_graph(
                        move |graph| {
                            if let Some(step) = graph.node_at_mut(&path) {
                                step.result = Some(architect::StepResult {
                                    summary: recorded,
                                    attempt,
                                });
                            }
                        },
                        cx,
                    );
                })?;
                summary
            }
        };

        let next = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            if let Some(step) = state.graph.node_at_mut(&node) {
                step.result = Some(architect::StepResult {
                    summary: summary.clone(),
                    attempt,
                });
            }
            let current = &mut state.lanes[lane];
            current.interrupted = false;
            let next = current.run.finish_step(&state.graph);
            current.next = next.clone();
            next
        };
        self.finish_visit(
            visit,
            Some(summary.into()),
            ArchitectRunOutcome::Completed,
            cx,
        )?;
        Ok(next)
    }

    /// Puts the question deciding a way out of the lane's last step to the
    /// conversation that carried that step out.
    async fn decide_branch(
        &self,
        lane: usize,
        branch: Branch,
        cx: &mut AsyncApp,
    ) -> Result<Decision, RunOutcome> {
        let saved_conversation = {
            let state = self.state.borrow();
            let lane = &state.lanes[lane];
            if lane
                .last_step_thread
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .is_none()
            {
                lane.last_step_session_id.clone()
            } else {
                None
            }
        };
        // Keep the loaded entity strong until the question has finished.
        let restored_conversation = if let (Some(session_id), Some((connection, _))) =
            (saved_conversation, &self.step_threads)
        {
            let task = cx.update(|cx| {
                let (project, work_dirs) = {
                    let plan = self.plan_thread.read(cx);
                    (
                        plan.project().clone(),
                        plan.work_dirs().cloned().unwrap_or_default(),
                    )
                };
                connection
                    .clone()
                    .load_session(session_id, project, work_dirs, None, cx)
            });
            self.checkpoint(cx)?;
            let conversation = task.await.map_err(|error| RunOutcome::Failed {
                message: format!("The saved branch conversation could not be restored: {error:#}. Restore it before resuming."),
            })?;
            self.state.borrow_mut().lanes[lane].last_step_thread = Some(conversation.downgrade());
            self.checkpoint(cx)?;
            Some(conversation)
        } else {
            None
        };
        let (prompt, asked, model) = {
            let state = self.state.borrow();
            let last_step_thread = state.lanes[lane].last_step_thread.as_ref();
            let asked = match last_step_thread.and_then(WeakEntity::upgrade) {
                Some(thread) => thread,
                None if self.step_threads.is_some() => {
                    return Err(RunOutcome::Failed {
                        message: "The step conversation is no longer available to decide its branch. Run that step again.".into(),
                    });
                }
                None => self.plan_thread.clone(),
            };
            let model = state
                .graph
                .node_at(&state.lanes[lane].run.current())
                .and_then(|step| step.model.clone());
            (
                architect::branch_prompt(&state.graph, &branch),
                asked,
                model,
            )
        };
        if let Some((connection, _)) = &self.step_threads {
            cx.update(|cx| -> anyhow::Result<()> {
                let model = match &model {
                    Some(model) => resolve_step_model(model, cx)?,
                    None => self
                        .thread
                        .read_with(cx, |thread, _| thread.model().cloned())?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Select a model for the plan before resuming.")
                        })?,
                };
                let step_thread = connection
                    .thread(asked.read(cx).session_id(), cx)
                    .ok_or_else(|| anyhow::anyhow!("The step conversation is no longer open."))?;
                step_thread.update(cx, |thread, cx| thread.set_model(model, cx));
                Ok(())
            })
            .map_err(|error| RunOutcome::Failed {
                message: format!("The branch model could not be selected: {error:#}"),
            })?;
        }

        self.begin_turn(lane, &asked, cx)?;
        let sent = send_and_wait(&asked, prompt, cx).await;
        if self.end_turn(lane, cx)? {
            return Err(RunOutcome::Cancelled);
        }
        sent?;

        let reply = closing_message(&asked, cx);
        drop(restored_conversation);
        let taken = architect::parse_verdict(&reply).ok_or_else(|| RunOutcome::Failed {
            message: format!("The reply for branch {} did not begin with YES or NO. Resume to ask again; no route was selected.", branch.edge.0),
        })?;
        let mut guard = self.state.borrow_mut();
        let state = &mut *guard;
        Ok(state.lanes[lane].run.answer(&state.graph, taken))
    }

    /// Runs every branch of the fork the lane is waiting on, then carries the
    /// lane on from where they meet.
    async fn run_fork(&self, lane: usize, cx: &mut AsyncApp) -> Result<Decision, RunOutcome> {
        let children = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            if state.lanes[lane].children.is_empty() {
                let branches = state.lanes[lane].run.fork_lanes(&state.graph);
                let first = state.lanes.len();
                for branch in branches {
                    let child = Lane::new(branch, &state.graph);
                    state.lanes.push(child);
                }
                state.lanes[lane].children = (first..state.lanes.len()).collect();
            }
            state.lanes[lane].children.clone()
        };
        self.checkpoint(cx)?;

        let outcome = if self.step_threads.is_some() {
            let lanes = children
                .iter()
                .map(|child| drive_lane(self.clone(), *child, cx.clone()));
            combine(join_all(lanes).await)
        } else {
            // Every step shares the plan's own conversation here, and it can
            // only take one at a time, so the branches run one after another.
            let mut outcome = RunOutcome::Completed;
            for child in children {
                outcome = drive_lane(self.clone(), child, cx.clone()).await;
                if !outcome.is_success() {
                    break;
                }
            }
            outcome
        };
        if !outcome.is_success() {
            return Err(outcome);
        }

        let mut guard = self.state.borrow_mut();
        let state = &mut *guard;
        state.lanes[lane].children.clear();
        Ok(state.lanes[lane].run.join(&state.graph))
    }

    /// Stops every other turn the run has in flight once a lane has failed.
    /// The run ends with that failure, and the other lanes' interrupted steps
    /// are left to be run again.
    fn halt(&self, cx: &mut AsyncApp) {
        let turns = {
            let mut state = self.state.borrow_mut();
            state.halted = true;
            state.waiters.clear();
            state.take_in_flight()
        };
        if let Err(outcome) = self.checkpoint(cx) {
            log::debug!("Architect: could not checkpoint a halted run: {outcome:?}");
        }
        for turn in turns {
            turn.update(cx, |thread, cx| thread.cancel(cx)).detach();
        }
    }

    /// Holds a lane back while the run is paused. Turns already under way are
    /// left alone; this only keeps new ones from starting.
    async fn wait_while_paused(&self, cx: &mut AsyncApp) -> Option<RunOutcome> {
        loop {
            let resumed = {
                let mut state = self.state.borrow_mut();
                if state.halted {
                    return Some(RunOutcome::Cancelled);
                }
                if !state.paused {
                    return None;
                }
                let (sender, receiver) = oneshot::channel();
                state.waiters.push(sender);
                receiver
            };
            if let Err(outcome) = self.checkpoint(cx) {
                return Some(outcome);
            }
            let cancelled = resumed.await.is_err();
            if let Err(outcome) = self.checkpoint(cx) {
                return Some(outcome);
            }
            if cancelled {
                return Some(RunOutcome::Cancelled);
            }
        }
    }

    fn begin_turn(
        &self,
        lane: usize,
        thread: &Entity<AcpThread>,
        cx: &mut AsyncApp,
    ) -> Result<(), RunOutcome> {
        self.state
            .borrow_mut()
            .in_flight
            .push((lane, thread.downgrade()));
        self.checkpoint(cx)
    }

    /// Notes that a lane's turn ended, and says whether the run was halted
    /// while it was in flight.
    fn end_turn(&self, lane: usize, cx: &mut AsyncApp) -> Result<bool, RunOutcome> {
        let halted = {
            let mut state = self.state.borrow_mut();
            state.in_flight.retain(|(turn_lane, _)| *turn_lane != lane);
            state.halted
        };
        self.checkpoint(cx)?;
        Ok(halted)
    }

    fn checkpoint(&self, cx: &mut AsyncApp) -> Result<(), RunOutcome> {
        self.update_thread(cx, |thread, cx| thread.checkpoint_architect_run(cx))
    }

    /// Closes a step that could not be carried out, recording why.
    fn fail_step(
        &self,
        visit: ArchitectStepVisitId,
        message: String,
        cx: &mut AsyncApp,
    ) -> RunOutcome {
        log::error!("Architect: {message}");
        if let Err(outcome) = self.finish_visit(
            visit,
            Some(message.clone().into()),
            ArchitectRunOutcome::Failed {
                message: message.clone(),
            },
            cx,
        ) {
            return outcome;
        }
        RunOutcome::Failed { message }
    }

    fn finish_visit(
        &self,
        visit: ArchitectStepVisitId,
        summary: Option<SharedString>,
        outcome: ArchitectRunOutcome,
        cx: &mut AsyncApp,
    ) -> Result<(), RunOutcome> {
        self.update_thread(cx, |thread, cx| {
            thread.finish_architect_run_step_with_outcome(visit, summary, outcome, cx);
        })
    }

    /// Updates the plan's thread. Once that thread is closed there is nowhere
    /// left to record the run, so it ends.
    fn update_thread<R>(
        &self,
        cx: &mut AsyncApp,
        update: impl FnOnce(&mut Thread, &mut Context<Thread>) -> R,
    ) -> Result<R, RunOutcome> {
        self.thread.update(cx, update).map_err(|error| {
            log::info!("Architect: stopped a run whose thread was closed: {error}");
            RunOutcome::Cancelled
        })
    }

    fn read_thread<R>(
        &self,
        cx: &mut AsyncApp,
        read: impl FnOnce(&Thread, &App) -> R,
    ) -> Result<R, RunOutcome> {
        self.thread.read_with(cx, read).map_err(|error| {
            log::info!("Architect: stopped a run whose thread was closed: {error}");
            RunOutcome::Cancelled
        })
    }
}

/// Drives one lane to its end. Boxed, because a fork drives lanes of its own.
fn drive_lane(
    driver: Driver,
    lane: usize,
    mut cx: AsyncApp,
) -> LocalBoxFuture<'static, RunOutcome> {
    async move {
        let outcome = driver.drive(lane, &mut cx).await;
        if !outcome.is_success() {
            driver.halt(&mut cx);
        }
        outcome
    }
    .boxed_local()
}

/// How a fork ended, given how each of its lanes did. A real failure wins,
/// since the other lanes were only stopped because of it.
fn combine(outcomes: Vec<RunOutcome>) -> RunOutcome {
    let mut cancelled = false;
    for outcome in outcomes {
        match outcome {
            RunOutcome::Completed => {}
            RunOutcome::Cancelled => cancelled = true,
            failure => return failure,
        }
    }
    if cancelled {
        RunOutcome::Cancelled
    } else {
        RunOutcome::Completed
    }
}

/// What a step reported through `complete_step`, if anything.
fn reported_summary(thread: &Thread, node: &NodePath) -> Option<String> {
    thread
        .architect_graph()
        .and_then(|graph| graph.node_at(node))
        .and_then(|step| step.result.as_ref())
        .map(|result| result.summary.trim().to_string())
        .filter(|summary| !summary.is_empty())
}

fn step_title(graph: &ArchitectGraph, path: &NodePath) -> SharedString {
    graph
        .node_at(path)
        .map(|node| SharedString::from(node.title.clone()))
        .or_else(|| path.leaf().map(|id| SharedString::from(id.0.clone())))
        .unwrap_or_else(|| SharedString::from("Step"))
}

fn session_id(thread: &Entity<AcpThread>, cx: &mut AsyncApp) -> acp::SessionId {
    thread.read_with(cx, |thread, _cx| thread.session_id().clone())
}

fn closing_message(thread: &Entity<AcpThread>, cx: &mut AsyncApp) -> String {
    thread.read_with(cx, |thread, cx| last_assistant_text(thread, cx))
}

async fn send_and_wait(
    thread: &Entity<AcpThread>,
    prompt: String,
    cx: &mut AsyncApp,
) -> Result<(), RunOutcome> {
    let send = thread.update(cx, |thread, cx| {
        thread.send(
            vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
            cx,
        )
    });
    match send.await {
        Ok(Some(response)) => match response.stop_reason {
            acp::StopReason::EndTurn => Ok(()),
            acp::StopReason::Cancelled => Err(RunOutcome::Cancelled),
            reason => Err(RunOutcome::Failed {
                message: format!(
                    "The step ended without completing ({reason:?}). Check its conversation and resume the run."
                ),
            }),
        },
        Ok(None) => Err(RunOutcome::Cancelled),
        Err(error) => Err(RunOutcome::Failed {
            message: format!("{error:#}. Check the model's configuration and resume the run."),
        }),
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use acp_thread::AgentConnection as _;
    use architect::{ArchitectNode, EdgeCondition, NodeId};
    use gpui::{AppContext as _, TestAppContext};
    use language_model::fake_provider::FakeLanguageModelProvider;
    use std::path::Path;
    use std::sync::Arc;
    use util::path_list::PathList;

    async fn native_session(
        cx: &mut TestAppContext,
    ) -> (
        Rc<NativeAgentConnection>,
        Entity<Thread>,
        Entity<AcpThread>,
        Arc<FakeLanguageModelProvider>,
    ) {
        let fake = crate::tests::init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/", serde_json::json!({ "a": {} })).await;
        let project = project::Project::test(fs.clone(), [Path::new("/a")], cx).await;
        let store = cx.new(|cx| crate::ThreadStore::new(cx));
        let agent = cx.update(|cx| crate::NativeAgent::new(store, crate::Templates::new(), fs, cx));
        let connection = Rc::new(NativeAgentConnection(agent));
        let acp_thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[Path::new("/a")]), cx)
            })
            .await
            .unwrap();
        let thread = cx.update(|cx| {
            connection
                .thread(acp_thread.read(cx).session_id(), cx)
                .unwrap()
        });
        thread.update(cx, |thread, cx| thread.set_model(fake.model("fake"), cx));
        (connection, thread, acp_thread, fake)
    }

    fn finish_pending_step(fake: &FakeLanguageModelProvider) {
        let pending: Vec<_> = fake
            .pending_completions()
            .into_iter()
            .filter(|request| {
                request
                    .messages
                    .last()
                    .is_some_and(|message| message.string_contents().contains("## Step"))
            })
            .collect();
        assert_eq!(
            pending.len(),
            1,
            "expected one admitted step, not a premature shared successor"
        );
        let model = fake.model("fake");
        fake.send_text(&model, &pending[0], "Step completed successfully.");
        fake.end_stream(&model, &pending[0]);
    }

    fn assert_running(thread: &Entity<Thread>, expected: &str, cx: &TestAppContext) {
        thread.read_with(cx, |thread, _| {
            let paths: Vec<_> = thread
                .architect_run()
                .unwrap()
                .running_steps()
                .iter()
                .map(|step| step.path.clone())
                .collect();
            assert_eq!(paths, vec![path(expected)]);
        });
    }

    fn assert_finished_readiness(
        thread: &Entity<Thread>,
        expected: &[(&str, &str)],
        cx: &TestAppContext,
    ) -> serde_json::Value {
        thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert_eq!(run.outcome, Some(RunOutcome::Completed));
            assert!(run.control().is_none());
            assert!(!run.can_resume());
            let checkpoint = run.saved_checkpoint().cloned();
            let sequence = thread.architect_event_sequence();
            let readiness = architect_run_readiness(thread);
            assert_eq!(readiness["ready_to_run"], false);
            assert_eq!(readiness["read_only"], true);
            let steps = readiness["steps"].as_array().unwrap();
            for (node, status) in expected {
                let step = steps
                    .iter()
                    .find(|step| step["path"] == serde_json::json!([node]))
                    .unwrap();
                assert_eq!(
                    step["status"], *status,
                    "incorrect terminal state for {node}"
                );
            }
            assert!(
                steps
                    .iter()
                    .all(|step| step["status"] != "ready" && step["status"] != "running")
            );
            assert_eq!(run.saved_checkpoint(), checkpoint.as_ref());
            assert!(
                run.control().is_none(),
                "inspection must not install resumable control"
            );
            assert_eq!(
                thread.architect_event_sequence(),
                sequence,
                "inspection must not emit execution events"
            );
            readiness
        })
    }

    async fn reload_run_for_readiness(
        connection: &Rc<NativeAgentConnection>,
        thread: &Entity<Thread>,
        acp_thread: &Entity<AcpThread>,
        keep_checkpoint: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Thread>, Entity<AcpThread>) {
        let mut saved = thread.read_with(cx, |thread, cx| thread.to_db(cx)).await;
        let snapshot = saved
            .persistent_architect
            .as_mut()
            .unwrap()
            .current
            .as_mut()
            .unwrap();
        assert!(matches!(
            snapshot.outcome,
            Some(ArchitectRunOutcome::Completed)
        ));
        assert!(
            snapshot.checkpoint.is_some(),
            "a terminal checkpoint must survive serialization"
        );
        if !keep_checkpoint {
            snapshot.checkpoint = None;
        }
        let id = thread.read_with(cx, |thread, _| thread.id().clone());
        let restored_id = acp::SessionId::new(format!("{id}-readiness-{keep_checkpoint}"));
        let (project, work_dirs) = acp_thread.read_with(cx, |thread, _| {
            (
                thread.project().clone(),
                thread.work_dirs().cloned().unwrap_or_default(),
            )
        });
        let database = cx
            .update(|cx| crate::ThreadsDatabase::connect(cx))
            .await
            .unwrap();
        database
            .save_thread(restored_id.clone(), saved, work_dirs.clone())
            .await
            .unwrap();
        let restored_acp = cx
            .update(|cx| {
                connection
                    .clone()
                    .load_session(restored_id.clone(), project, work_dirs, None, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        let restored = cx.update(|cx| connection.thread(&restored_id, cx).unwrap());
        (restored, restored_acp)
    }

    fn checkpoint_value_count(value: &serde_json::Value) -> usize {
        1 + match value {
            serde_json::Value::Array(values) => values.iter().map(checkpoint_value_count).sum(),
            serde_json::Value::Object(values) => values.values().map(checkpoint_value_count).sum(),
            _ => 0,
        }
    }

    #[gpui::test(iterations = 3)]
    async fn native_pipeline_does_not_enter_a_join_with_only_closed_routes(
        cx: &mut TestAppContext,
    ) {
        let (connection, thread, acp_thread, fake) = native_session(cx).await;
        let mut graph = graph();
        for edge in &mut graph.edges {
            edge.max_repeats = Some(0);
        }
        let mut start = ArchitectNode::new("start", "Start");
        start.locked = true;
        graph.add_node(start);
        graph.connect("start", "a");
        graph.connect("start", "b");
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        for expected in ["start", "a", "b"] {
            cx.run_until_parked();
            assert_running(&thread, expected, cx);
            finish_pending_step(&fake);
        }
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert_eq!(run.outcome, Some(RunOutcome::Completed));
            assert_eq!(run.history().len(), 3);
            assert!(!run.history().iter().any(|step| step.path == path("join")));
            assert!(
                thread
                    .architect_graph()
                    .unwrap()
                    .node_at(&path("join"))
                    .unwrap()
                    .result
                    .is_none()
            );
        });
        let expected = [
            ("start", "completed"),
            ("a", "completed"),
            ("b", "completed"),
            ("join", "skipped"),
        ];
        let before = assert_finished_readiness(&thread, &expected, cx);
        let (restored, restored_acp) =
            reload_run_for_readiness(&connection, &thread, &acp_thread, true, cx).await;
        assert_eq!(assert_finished_readiness(&restored, &expected, cx), before);
        cx.update(|cx| {
            assert!(matches!(
                resume_architect_run(restored.clone(), restored_acp.clone(), cx),
                Err(ArchitectRunStartError::NotResumable)
            ));
        });
        let (legacy, _legacy_acp) =
            reload_run_for_readiness(&connection, &thread, &acp_thread, false, cx).await;
        assert_finished_readiness(
            &legacy,
            &[
                ("start", "completed"),
                ("a", "completed"),
                ("b", "completed"),
                ("join", "unknown"),
            ],
            cx,
        );
    }

    #[gpui::test]
    async fn runtime_impact_includes_frozen_pinned_consumers_missing_from_draft_preview(
        cx: &mut TestAppContext,
    ) {
        let (_connection, thread, acp_thread, fake) = native_session(cx).await;
        let mut graph = graph();
        graph.node_at_mut(&path("a")).unwrap().pinned = true;
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        cx.run_until_parked();
        assert_running(&thread, "a", cx);
        finish_pending_step(&fake);
        cx.run_until_parked();
        assert_running(&thread, "b", cx);
        cx.update(|cx| pause_architect_run(&thread, cx));
        finish_pending_step(&fake);
        cx.run_until_parked();
        cx.update(|cx| stop_architect_run(&thread, None, cx));
        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(
                |graph| graph.node_at_mut(&path("a")).unwrap().pinned = false,
                cx,
            );
        });
        let draft = thread.read_with(cx, |thread, _| thread.architect_graph().unwrap().clone());
        let mut proposed = draft.clone();
        proposed.node_at_mut(&path("a")).unwrap().responsibility = "Changed ownership".into();
        let draft_preview = architect::preview_graph_replacement(&draft, &proposed).unwrap();
        assert!(!draft_preview.invalidated_steps.contains(&path("b")));
        let runtime = thread.read_with(cx, |thread, _| {
            preview_architect_graph_update(
                thread,
                &draft_preview.graph,
                &draft_preview.invalidated_steps,
            )
        });
        assert_eq!(runtime["can_apply"], true);
        assert!(
            runtime["invalidated_paths"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(["b"]))
        );
        assert!(
            !runtime["retained_results"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(["b"]))
        );
        cx.update(|cx| {
            apply_architect_graph_update(
                &thread,
                draft_preview.graph,
                &draft_preview.invalidated_steps,
                cx,
            )
            .unwrap()
        });
        thread.read_with(cx, |thread, _| {
            let consumer = thread
                .architect_graph()
                .unwrap()
                .node_at(&path("b"))
                .unwrap();
            assert!(consumer.result.is_none());
            assert!(!consumer.locked);
            let run = thread.architect_run().unwrap();
            assert_eq!(
                run.history().len(),
                2,
                "invalidating results must not erase completed visit history"
            );
            assert!(run.history().iter().all(|step| step.summary.is_some()));
            let state = run.control().unwrap().borrow();
            assert_eq!(
                serde_json::json!(state.invalidated),
                runtime["invalidated_paths"]
            );
            assert_eq!(state.archived_checkpoints.len(), 1);
            RunState::from_checkpoint(state.checkpoint()).unwrap();
        });
    }

    #[gpui::test]
    async fn archive_budget_is_preflighted_and_oversized_apply_is_atomic(cx: &mut TestAppContext) {
        let (_connection, thread, acp_thread, fake) = native_session(cx).await;
        cx.update(|cx| {
            let graph = graph();
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        cx.run_until_parked();
        cx.update(|cx| pause_architect_run(&thread, cx));
        finish_pending_step(&fake);
        cx.run_until_parked();
        cx.update(|cx| stop_architect_run(&thread, None, cx));
        let mut first = thread.read_with(cx, |thread, _| thread.architect_graph().unwrap().clone());
        first.add_node(ArchitectNode::new("first", "First addition"));
        cx.update(|cx| apply_architect_graph_update(&thread, first, &[], cx).unwrap());
        let (before_graph, before_checkpoint, sequence) = thread.read_with(cx, |thread, _| {
            (
                thread.architect_graph().unwrap().clone(),
                thread
                    .architect_run()
                    .unwrap()
                    .snapshot()
                    .checkpoint
                    .unwrap(),
                thread.architect_event_sequence(),
            )
        });
        assert_eq!(
            before_checkpoint["state"]["archived_checkpoints"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let budget = checkpoint_value_count(&before_checkpoint);
        validate_checkpoint_value_with_budget(&before_checkpoint, budget).unwrap();
        RunState::from_checkpoint(before_checkpoint.clone()).unwrap();
        let mut proposed = before_graph.clone();
        proposed.add_node(ArchitectNode::new("second", "Second addition"));
        let preview = thread.read_with(cx, |thread, _| {
            preview_architect_graph_update_with_budget(thread, &proposed, &[], budget)
        });
        assert_eq!(preview["can_rebase"], true);
        assert_eq!(preview["can_apply"], false);
        assert_eq!(preview["checkpoint_fits"], false);
        assert!(
            preview["checkpoint_error"]
                .as_str()
                .unwrap()
                .contains("preserved history")
        );
        assert!(
            preview["invalidated_paths"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(["second"]))
        );
        cx.update(|cx| {
            let error = apply_architect_graph_update_with_budget(
                &thread,
                proposed.clone(),
                &[],
                budget,
                cx,
            )
            .unwrap_err();
            assert!(error.to_string().contains("no history was truncated"));
        });
        let expected = thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph(), Some(&before_graph));
            assert_eq!(thread.architect_event_sequence(), sequence);
            let run = thread.architect_run().unwrap();
            assert_eq!(run.snapshot().checkpoint, Some(before_checkpoint));
            assert_eq!(run.history().len(), 1);
            assert!(run.history()[0].summary.is_some());
            let state = run.control().unwrap().borrow();
            let prepared =
                prepare_graph_update(&state.graph, &proposed, &[], Some(&state)).unwrap();
            prepared.checkpoint.unwrap().checkpoint()
        });
        let sufficient_budget = checkpoint_value_count(&expected);
        cx.update(|cx| {
            apply_architect_graph_update_with_budget(&thread, proposed, &[], sufficient_budget, cx)
                .unwrap()
        });
        thread.read_with(cx, |thread, _| {
            let actual = thread
                .architect_run()
                .unwrap()
                .snapshot()
                .checkpoint
                .unwrap();
            assert_eq!(
                actual, expected,
                "commit must install the exact state that was preflighted"
            );
            assert_eq!(
                actual["state"]["archived_checkpoints"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            RunState::from_checkpoint(actual).unwrap();
            assert!(
                thread
                    .architect_graph()
                    .unwrap()
                    .node_at(&path("a"))
                    .unwrap()
                    .result
                    .is_some()
            );
        });
    }

    #[gpui::test(iterations = 3)]
    async fn native_pipeline_checkpoints_the_other_root_before_waiting(cx: &mut TestAppContext) {
        let (connection, thread, acp_thread, fake) = native_session(cx).await;
        let snapshots = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            cx.observe(&thread, {
                let snapshots = snapshots.clone();
                move |thread, cx| {
                    if let Some(checkpoint) = thread
                        .read(cx)
                        .architect_run()
                        .and_then(|run| run.snapshot().checkpoint)
                    {
                        RunState::from_checkpoint(checkpoint.clone())
                            .expect("every notified checkpoint must be coherent");
                        snapshots.borrow_mut().push(checkpoint);
                    }
                }
            })
        });
        cx.update(|cx| {
            let graph = graph();
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        cx.run_until_parked();
        assert_running(&thread, "a", cx);
        cx.update(|cx| pause_architect_run(&thread, cx));
        let sequence = thread.read_with(cx, |thread, _| thread.architect_event_sequence());
        finish_pending_step(&fake);
        cx.run_until_parked();
        let saved = thread
            .read_with(cx, |thread, cx| {
                assert!(thread.architect_event_sequence() > sequence);
                assert!(thread.architect_run().unwrap().running_steps().is_empty());
                thread.to_db(cx)
            })
            .await;
        let snapshot = saved.persistent_architect.unwrap().current.unwrap();
        let restored = RunState::from_checkpoint(snapshot.checkpoint.unwrap()).unwrap();
        assert_eq!(restored.lanes[0].next, Decision::Run(path("b")));
        assert!(restored.graph.node_at(&path("a")).unwrap().result.is_some());
        assert!(
            restored
                .graph
                .node_at(&path("join"))
                .unwrap()
                .result
                .is_none()
        );
        assert!(matches!(
            snapshot.history[0].outcome,
            Some(ArchitectRunOutcome::Completed)
        ));
        assert!(snapshots.borrow().iter().any(|checkpoint| {
            checkpoint["state"]["paused"] == true
                && checkpoint["state"]["lanes"][0]["next"] == serde_json::json!({ "Run": ["b"] })
        }));
        cx.update(|cx| resume_architect_run(thread.clone(), acp_thread.clone(), cx).unwrap());
        cx.run_until_parked();
        assert_running(&thread, "b", cx);
        finish_pending_step(&fake);
        cx.run_until_parked();
        assert_running(&thread, "join", cx);
        finish_pending_step(&fake);
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert_eq!(run.outcome, Some(RunOutcome::Completed));
            assert_eq!(
                run.history()
                    .iter()
                    .map(|step| step.path.clone())
                    .collect::<Vec<_>>(),
                vec![path("a"), path("b"), path("join")]
            );
        });
        let expected = [
            ("a", "completed"),
            ("b", "completed"),
            ("join", "completed"),
        ];
        let before = assert_finished_readiness(&thread, &expected, cx);
        let (restored, restored_acp) =
            reload_run_for_readiness(&connection, &thread, &acp_thread, true, cx).await;
        assert_eq!(assert_finished_readiness(&restored, &expected, cx), before);
        cx.update(|cx| {
            assert!(matches!(
                resume_architect_run(restored.clone(), restored_acp.clone(), cx),
                Err(ArchitectRunStartError::NotResumable)
            ));
        });
        let (legacy, _legacy_acp) =
            reload_run_for_readiness(&connection, &thread, &acp_thread, false, cx).await;
        assert_finished_readiness(&legacy, &expected, cx);
    }

    #[gpui::test(iterations = 3)]
    async fn native_pipeline_runs_partial_joins_only_after_all_prerequisites(
        cx: &mut TestAppContext,
    ) {
        let (_connection, thread, acp_thread, fake) = native_session(cx).await;
        let mut graph = ArchitectGraph::default();
        for id in ["a", "b", "e", "c", "f", "d"] {
            let mut node = ArchitectNode::new(id, id);
            node.locked = true;
            graph.add_node(node);
        }
        for (from, to) in [
            ("a", "b"),
            ("a", "c"),
            ("a", "d"),
            ("b", "e"),
            ("c", "e"),
            ("e", "f"),
            ("d", "f"),
        ] {
            graph.connect(from, to);
        }
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        for expected in ["a", "b", "c", "e", "d", "f"] {
            cx.run_until_parked();
            assert_running(&thread, expected, cx);
            thread.read_with(cx, |thread, _| {
                let checkpoint = thread
                    .architect_run()
                    .unwrap()
                    .snapshot()
                    .checkpoint
                    .unwrap();
                RunState::from_checkpoint(checkpoint).unwrap();
            });
            finish_pending_step(&fake);
        }
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert_eq!(run.outcome, Some(RunOutcome::Completed));
            assert_eq!(run.history().len(), 6);
        });
    }

    #[gpui::test]
    async fn native_pipeline_persists_failure_instead_of_completed_summary(
        cx: &mut TestAppContext,
    ) {
        let (_connection, thread, acp_thread, _fake) = native_session(cx).await;
        let mut graph = graph();
        graph.node_at_mut(&path("a")).unwrap().model = Some(architect::StepModel {
            provider: "missing-provider".into(),
            model: "missing-model".into(),
        });
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        cx.run_until_parked();
        let saved = thread.read_with(cx, |thread, cx| thread.to_db(cx)).await;
        let snapshot = saved.persistent_architect.unwrap().current.unwrap();
        assert!(matches!(
            snapshot.history[0].outcome,
            Some(ArchitectRunOutcome::Failed { .. })
        ));
        let state = RunState::from_checkpoint(snapshot.checkpoint.unwrap()).unwrap();
        assert!(state.halted);
        assert!(state.lanes[0].interrupted);
        assert!(state.graph.node_at(&path("a")).unwrap().result.is_none());
    }

    #[gpui::test(iterations = 3)]
    async fn unlocked_core_insert_rebases_and_requires_review_before_resume(
        cx: &mut TestAppContext,
    ) {
        let (_connection, thread, acp_thread, fake) = native_session(cx).await;
        cx.update(|cx| {
            let graph = graph();
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph, cx).unwrap();
        });
        cx.run_until_parked();
        assert_running(&thread, "a", cx);
        cx.update(|cx| pause_architect_run(&thread, cx));
        finish_pending_step(&fake);
        cx.run_until_parked();
        cx.update(|cx| stop_architect_run(&thread, None, cx));
        let original = thread.read_with(cx, |thread, _| thread.architect_graph().unwrap().clone());
        let original_result = original.node_at(&path("a")).unwrap().result.clone();
        assert!(original_result.is_some());
        let core = architect::preview_graph_edits(
            &original,
            &[architect::GraphEdit::InsertNode {
                parent: NodePath::default(),
                node: ArchitectNode::new("inserted", "Inserted step"),
            }],
        )
        .unwrap();
        assert!(core.is_valid);
        assert!(!core.ready_to_run);
        assert!(!core.graph.node_at(&path("inserted")).unwrap().locked);
        let preview = thread.read_with(cx, |thread, _| {
            preview_architect_graph_update(thread, &core.graph, &core.invalidated_steps)
        });
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["can_rebase"], true);
        assert_eq!(preview["ready_to_run"], false);
        assert_eq!(preview["requires_review"], true);
        cx.update(|cx| {
            apply_architect_graph_update(&thread, core.graph.clone(), &core.invalidated_steps, cx)
                .unwrap();
        });
        let checkpoint = thread.read_with(cx, |thread, _| {
            let graph = thread.architect_graph().unwrap();
            assert_eq!(graph.node_at(&path("a")).unwrap().result, original_result);
            assert!(!graph.node_at(&path("inserted")).unwrap().locked);
            let run = thread.architect_run().unwrap();
            assert_eq!(run.history().len(), 1);
            run.snapshot().checkpoint.unwrap()
        });
        cx.update(|cx| {
            assert!(matches!(
                resume_architect_run(thread.clone(), acp_thread.clone(), cx),
                Err(ArchitectRunStartError::Refused(RunRefusal::NotReady(_)))
            ));
        });
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.architect_run().unwrap().snapshot().checkpoint,
                Some(checkpoint.clone())
            );
            assert!(!thread.architect_run().unwrap().is_running());
        });
        let saved = thread.read_with(cx, |thread, cx| thread.to_db(cx)).await;
        let persisted = saved
            .persistent_architect
            .unwrap()
            .current
            .unwrap()
            .checkpoint
            .unwrap();
        let mut restored =
            RunState::from_checkpoint(persisted).expect("unlocked is awaiting review, not corrupt");
        let unlocked = restored.graph.clone();
        assert!(restored.halted);
        assert!(matches!(
            restored.prepare_resume(&unlocked),
            Err(ArchitectRunStartError::Refused(RunRefusal::NotReady(_)))
        ));
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.update_architect_graph(
                    |graph| {
                        graph.lock_all();
                    },
                    cx,
                );
            });
            resume_architect_run(thread.clone(), acp_thread.clone(), cx).unwrap();
            let owner = thread.read(cx);
            let state = owner.architect_run().unwrap().control().unwrap().borrow();
            assert_eq!(&state.graph, owner.architect_graph().unwrap());
            assert!(state.graph.node_at(&path("inserted")).unwrap().locked);
        });
        for expected in ["b", "join", "inserted"] {
            cx.run_until_parked();
            assert_running(&thread, expected, cx);
            finish_pending_step(&fake);
        }
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert_eq!(run.outcome, Some(RunOutcome::Completed));
            assert_eq!(
                run.history()
                    .iter()
                    .filter(|step| step.path == path("a"))
                    .count(),
                1
            );
            assert_eq!(
                thread
                    .architect_graph()
                    .unwrap()
                    .node_at(&path("a"))
                    .unwrap()
                    .result,
                original_result
            );
        });
    }

    #[gpui::test]
    async fn nested_canvas_move_keeps_the_exact_lane_checkpoint(cx: &mut TestAppContext) {
        let (_connection, thread, acp_thread, _fake) = native_session(cx).await;
        let mut graph = ArchitectGraph::default();
        let mut container = ArchitectNode::new("container", "Container");
        container.locked = true;
        container.subplan = Some(Box::new(self::graph()));
        graph.add_node(container);
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(graph.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), graph.clone(), cx).unwrap();
            stop_architect_run(&thread, None, cx);
        });
        let before = thread.read_with(cx, |thread, _| {
            thread
                .architect_run()
                .unwrap()
                .snapshot()
                .checkpoint
                .unwrap()
        });
        let core = architect::preview_graph_edits(
            &graph,
            &[architect::GraphEdit::MoveNode {
                path: path("container").child("b".into()),
                position: architect::NodePosition { x: 200.0, y: 100.0 },
            }],
        )
        .unwrap();
        assert!(core.invalidated_steps.is_empty());
        let preview = thread.read_with(cx, |thread, _| {
            preview_architect_graph_update(thread, &core.graph, &[])
        });
        assert_eq!(preview["can_rebase"], true);
        assert_eq!(preview["can_apply"], true);
        cx.update(|cx| apply_architect_graph_update(&thread, core.graph, &[], cx).unwrap());
        thread.read_with(cx, |thread, _| {
            let after = thread
                .architect_run()
                .unwrap()
                .snapshot()
                .checkpoint
                .unwrap();
            assert_eq!(after["state"]["lanes"], before["state"]["lanes"]);
            RunState::from_checkpoint(after).unwrap();
        });
    }

    #[gpui::test]
    async fn graph_preview_refuses_unsupported_rebase_without_mutating_history(
        cx: &mut TestAppContext,
    ) {
        let (_connection, thread, acp_thread, _fake) = native_session(cx).await;
        let original = graph();
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(original.clone()), cx)
            });
            start_architect_run(thread.clone(), acp_thread.clone(), original.clone(), cx).unwrap();
            stop_architect_run(&thread, None, cx);
        });
        let before = thread.read_with(cx, |thread, _| {
            thread
                .architect_run()
                .unwrap()
                .snapshot()
                .checkpoint
                .unwrap()
        });
        let sequence = thread.read_with(cx, |thread, _| thread.architect_event_sequence());
        let mut proposed = original.clone();
        proposed.node_at_mut(&path("a")).unwrap().subplan = Some(Box::new(graph()));
        let preview = thread.read_with(cx, |thread, _| {
            preview_architect_graph_update(thread, &proposed, &[])
        });
        assert_eq!(preview["can_rebase"], false);
        assert_eq!(preview["can_apply"], false);
        assert_eq!(preview["lost_checkpoint"], true);
        assert_eq!(preview["history_preserved"], true);
        assert!(
            preview["invalidated_paths"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(["a"]))
        );
        cx.update(|cx| assert!(apply_architect_graph_update(&thread, proposed, &[], cx).is_err()));
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph(), Some(&original));
            assert_eq!(
                thread.architect_run().unwrap().snapshot().checkpoint,
                Some(before)
            );
            assert_eq!(thread.architect_event_sequence(), sequence);
        });
    }

    fn graph() -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for id in ["a", "join", "b"] {
            let mut node = ArchitectNode::new(id, id);
            node.locked = true;
            graph.add_node(node);
        }
        graph.connect("a", "join");
        graph.connect("b", "join");
        graph
    }

    fn path(node: &str) -> NodePath {
        NodePath::root(NodeId(node.into()))
    }

    #[test]
    fn checkpoint_roundtrip_retains_completed_work_and_interrupts_active_turns() {
        let mut graph = graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path("b")));
        graph.node_at_mut(&path("a")).unwrap().result = Some(architect::StepResult {
            summary: "A finished".into(),
            attempt: 1,
        });
        let mut state = RunState::new(graph, run);
        state.steps = 2;
        state.attempts = vec![(path("a"), 1), (path("b"), 1)];
        let mut checkpoint = state.checkpoint();
        checkpoint["in_flight"] = serde_json::json!([0]);
        let mut restored = RunState::from_checkpoint(checkpoint).unwrap();
        assert!(restored.halted);
        assert!(!restored.paused);
        assert!(restored.in_flight.is_empty());
        assert!(restored.lanes[0].interrupted);
        assert_eq!(restored.steps, 2);
        assert_eq!(restored.attempts, state.attempts);
        assert_eq!(restored.graph, state.graph);
        assert_eq!(restored.lanes[0].next, Decision::Run(path("b")));
        restored.prepare_resume(&state.graph).unwrap();
        assert_eq!(restored.lanes[0].run.attempt(&NodeId("b".into())), 2);
        assert_eq!(
            restored.lanes[0].run.history(),
            &[path("a"), path("b"), path("b")]
        );
        let next = restored.lanes[0].run.finish_step(&restored.graph);
        assert_eq!(next, Decision::Run(path("join")));
    }

    #[test]
    fn checkpoint_retains_the_question_conversation_without_serializing_entities() {
        let mut graph = graph();
        graph.edges[0].condition = EdgeCondition::LlmEvaluated {
            question: "Continue?".into(),
        };
        let mut run = PlanRun::start(&graph).unwrap();
        let next = run.finish_step(&graph);
        assert!(matches!(next, Decision::Ask(_)));
        let mut state = RunState::new(graph, run);
        state.lanes[0].last_step_session_id = Some(acp::SessionId::new("saved-step-conversation"));
        state.steps = 1;
        state.attempts = vec![(path("a"), 1)];
        let mut checkpoint = state.checkpoint();
        checkpoint["in_flight"] = serde_json::json!([0]);
        let mut restored = RunState::from_checkpoint(checkpoint).unwrap();
        assert_eq!(restored.lanes[0].next, next);
        assert!(!restored.lanes[0].interrupted);
        assert_eq!(
            restored.lanes[0].last_step_session_id,
            state.lanes[0].last_step_session_id
        );
        assert!(restored.lanes[0].last_step_thread.is_none());
        restored.prepare_resume(&state.graph).unwrap();
        assert_eq!(restored.lanes[0].next, next);
        assert_eq!(restored.lanes[0].run.history(), &[path("a")]);
    }

    #[test]
    fn checkpoint_validates_aggregate_attempts_and_numeric_counters() {
        let graph = graph();
        let state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["steps"] = serde_json::json!(MAX_RUN_STEPS);
        checkpoint["state"]["attempts"] = serde_json::json!(
            (0..9)
                .map(|index| (vec![format!("step-{index}")], MAX_NODE_VISITS))
                .collect::<Vec<_>>()
        );
        let error = RunState::from_checkpoint(checkpoint)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("whole-run"), "{error}");
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["steps"] = serde_json::json!(1);
        assert!(
            RunState::from_checkpoint(checkpoint).is_err(),
            "counters must agree exactly"
        );
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["lanes"][0]["run"]["stack"][0]["edge_uses"]["a->join"] =
            serde_json::json!(usize::MAX);
        assert!(RunState::from_checkpoint(checkpoint).is_err());
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["steps"] = serde_json::json!(-1);
        assert!(RunState::from_checkpoint(checkpoint).is_err());
    }

    #[test]
    fn checkpoint_graph_budget_covers_all_nested_siblings() {
        let graph = graph();
        let mut state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        for parent in ["a", "b"] {
            let mut nested = ArchitectGraph::default();
            for index in 0..=MAX_CHECKPOINT_GRAPH_NODES / 2 {
                let mut node = ArchitectNode::new(format!("nested-{index}"), "Nested");
                node.locked = true;
                nested.add_node(node);
            }
            state.graph.node_at_mut(&path(parent)).unwrap().subplan = Some(Box::new(nested));
        }
        let error = RunState::from_checkpoint(state.checkpoint())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("total node budget"), "{error}");
    }

    #[test]
    fn checkpoint_graph_budget_rejects_excessive_depth_and_duplicate_edges() {
        let graph = graph();
        let mut state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        let mut nested = graph.clone();
        for _ in 0..MAX_PLAN_DEPTH {
            let mut wrapper = ArchitectGraph::default();
            let mut node = ArchitectNode::new("container", "Container");
            node.locked = true;
            node.subplan = Some(Box::new(nested));
            wrapper.add_node(node);
            nested = wrapper;
        }
        state.graph = nested;
        assert!(
            RunState::from_checkpoint(state.checkpoint())
                .err()
                .unwrap()
                .to_string()
                .contains("nesting limit")
        );
        state.graph = graph;
        state.graph.edges.push(state.graph.edges[0].clone());
        assert!(
            RunState::from_checkpoint(state.checkpoint())
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate edge")
        );
    }

    #[test]
    fn affected_locks_include_frozen_or_proposed_locks_and_removed_paths() {
        let mut before = graph();
        before.node_at_mut(&path("a")).unwrap().locked = false;
        before.node_at_mut(&path("join")).unwrap().locked = false;
        let mut untouched = ArchitectNode::new("untouched", "Untouched");
        untouched.locked = true;
        before.add_node(untouched);
        let mut proposed = before.clone();
        proposed.node_at_mut(&path("a")).unwrap().title = "Changed title".into();
        proposed.node_at_mut(&path("join")).unwrap().locked = true;
        proposed.remove_node(&NodeId("b".into()));
        let core = architect::preview_graph_replacement(&before, &proposed).unwrap();
        let prepared = prepare_graph_update(&before, &proposed, &[], None).unwrap();
        assert_eq!(prepared.changed_steps, core.changed_steps);
        assert_eq!(prepared.affected_locks, vec![path("b"), path("join")]);
        assert!(
            !prepared.affected_locks.contains(&path("a")),
            "already-unlocked paths are not reopened locks"
        );
        assert!(!prepared.affected_locks.contains(&path("untouched")));
        assert!(prepared.graph.node_at(&path("b")).is_none());
        assert!(!prepared.graph.node_at(&path("join")).unwrap().locked);
        assert!(before.node_at(&path("b")).unwrap().locked);
        assert!(proposed.node_at(&path("join")).unwrap().locked);
    }

    #[test]
    fn affected_locks_include_invalidated_composite_parents_not_untouched_siblings() {
        let mut before = ArchitectGraph::default();
        let mut container = ArchitectNode::new("container", "Container");
        container.locked = true;
        container.subplan = Some(Box::new(graph()));
        before.add_node(container);
        let parent = path("container");
        let changed = parent.child("a".into());
        let mut proposed = before.clone();
        proposed.node_at_mut(&changed).unwrap().title = "Revised nested step".into();
        let prepared = prepare_graph_update(&before, &proposed, &[], None).unwrap();
        assert_eq!(
            prepared.affected_locks,
            vec![parent.clone(), changed, parent.child("join".into())]
        );
        assert!(
            prepared
                .graph
                .node_at(&parent.child("b".into()))
                .unwrap()
                .locked
        );
    }

    #[test]
    fn restore_compatibility_ignores_only_results_positions_and_locks_recursively() {
        let mut graph = graph();
        let mut container = ArchitectNode::new("container", "Container");
        container.locked = true;
        container.subplan = Some(Box::new(self::graph()));
        graph.add_node(container);
        let state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        let checkpoint = state.checkpoint();
        let mut root = graph.clone();
        for path in graph_paths(&root) {
            let node = root.node_at_mut(&path).unwrap();
            node.result = Some(architect::StepResult {
                summary: "Authoritative result".into(),
                attempt: 2,
            });
            node.position = Some(architect::Position { x: 200.0, y: 100.0 });
            node.locked = false;
        }
        let original_root = root.clone();
        assert!(
            state.restore_is_compatible(&root),
            "compatibility does not require execution readiness"
        );
        assert_eq!(root, original_root);
        assert_eq!(state.checkpoint(), checkpoint);
        let changes: [fn(&mut ArchitectNode); 8] = [
            |node| node.title = "Different title".into(),
            |node| node.responsibility = "Different ownership".into(),
            |node| node.intent = "Different goal".into(),
            |node| node.rules = vec!["Different constraint".into()],
            |node| node.capture = "Different handoff".into(),
            |node| node.pinned = !node.pinned,
            |node| {
                node.model = Some(architect::StepModel {
                    provider: "fake".into(),
                    model: "different".into(),
                })
            },
            |node| node.chat = Some(acp::SessionId::new("different-conversation")),
        ];
        for change in changes {
            let mut changed = root.clone();
            change(
                changed
                    .node_at_mut(&path("container").child("b".into()))
                    .unwrap(),
            );
            assert!(!state.restore_is_compatible(&changed));
        }
        let mut changed = root.clone();
        changed.node_at_mut(&path("container")).unwrap().id = NodeId("renamed".into());
        assert!(!state.restore_is_compatible(&changed));
        let mut changed = root.clone();
        changed.edges.reverse();
        assert!(
            !state.restore_is_compatible(&changed),
            "edge order affects routing"
        );
        let mut changed = root.clone();
        changed.nodes.reverse();
        assert!(
            !state.restore_is_compatible(&changed),
            "node order affects scheduling"
        );
        let mut changed = root.clone();
        changed.edges[0].max_repeats = Some(0);
        assert!(!state.restore_is_compatible(&changed));
        let mut changed = root.clone();
        let subplan = changed
            .node_at_mut(&path("container"))
            .unwrap()
            .subplan
            .take();
        changed.node_at_mut(&path("b")).unwrap().subplan = subplan;
        assert!(
            !state.restore_is_compatible(&changed),
            "identical leaf IDs under different parents are not the same steps"
        );
        assert_eq!(state.checkpoint(), checkpoint);
        assert_eq!(root, original_root);
    }

    #[test]
    fn runtime_replacement_uses_core_impact_for_every_execution_payload() {
        let mut before = graph();
        before.node_at_mut(&path("a")).unwrap().pinned = true;
        for node in &mut before.nodes {
            node.result = Some(architect::StepResult {
                summary: "Completed work".into(),
                attempt: 1,
            });
        }
        let changes: [fn(&mut ArchitectNode); 8] = [
            |node| node.title = "New title".into(),
            |node| node.responsibility = "New responsibility".into(),
            |node| node.intent = "New goal".into(),
            |node| node.rules = vec!["New constraint".into()],
            |node| node.capture = "New handoff requirement".into(),
            |node| node.pinned = false,
            |node| {
                node.model = Some(architect::StepModel {
                    provider: "fake".into(),
                    model: "other".into(),
                })
            },
            |node| node.chat = Some(acp::SessionId::new("reviewed-conversation")),
        ];
        for change in changes {
            let mut proposed = before.clone();
            change(proposed.node_at_mut(&path("a")).unwrap());
            let core = architect::preview_graph_replacement(&before, &proposed).unwrap();
            let prepared = prepare_graph_update(&before, &proposed, &[], None).unwrap();
            assert_eq!(prepared.invalidated, core.invalidated_steps);
            assert!(
                prepared.invalidated.contains(&path("b")),
                "pinned consumers are prerequisites even without an edge"
            );
            for affected in &prepared.invalidated {
                let node = prepared.graph.node_at(affected).unwrap();
                assert!(node.result.is_none());
                assert!(!node.locked);
            }
            assert!(
                before
                    .nodes
                    .iter()
                    .all(|node| node.result.is_some() && node.locked)
            );
        }
    }

    #[test]
    fn explicit_invalidation_keys_are_unioned_with_core_impact() {
        let before = graph();
        let mut proposed = before.clone();
        proposed.node_at_mut(&path("a")).unwrap().title = "Changed title".into();
        let core = architect::preview_graph_replacement(&before, &proposed).unwrap();
        assert!(!core.invalidated_steps.contains(&path("b")));
        let prepared =
            prepare_graph_update(&before, &proposed, &[path("b"), path("b")], None).unwrap();
        let mut expected: std::collections::BTreeSet<_> =
            core.invalidated_steps.into_iter().collect();
        expected.insert(path("b"));
        assert_eq!(
            prepared.invalidated,
            expected.into_iter().collect::<Vec<_>>()
        );
        assert!(!prepared.graph.node_at(&path("b")).unwrap().locked);
    }

    #[test]
    fn pending_nested_brief_edit_retains_completed_siblings_and_lane_positions() {
        let mut nested = graph();
        nested.node_at_mut(&path("a")).unwrap().result = Some(architect::StepResult {
            summary: "Retained sibling".into(),
            attempt: 1,
        });
        let mut original = ArchitectGraph::default();
        let mut container = ArchitectNode::new("container", "Container");
        container.locked = true;
        container.subplan = Some(Box::new(nested));
        original.add_node(container);
        let parent = path("container");
        let completed = parent.child("a".into());
        let pending = parent.child("b".into());
        let join = parent.child("join".into());
        let mut run = PlanRun::start(&original).unwrap();
        assert_eq!(run.finish_step(&original), Decision::Run(pending.clone()));
        let mut state = RunState::new(original.clone(), run);
        state.steps = 1;
        state.attempts = vec![(completed.clone(), 1)];
        let before = serde_json::to_value(&state.lanes).unwrap();
        let mut proposed = original.clone();
        proposed.node_at_mut(&parent).unwrap().locked = false;
        let step = proposed.node_at_mut(&pending).unwrap();
        step.intent = "The reviewed replacement brief".into();
        step.locked = false;
        proposed.node_at_mut(&join).unwrap().locked = false;
        let prepared = prepare_graph_update(
            &original,
            &proposed,
            &[parent, pending.clone(), join],
            Some(&state),
        )
        .unwrap();
        assert!(prepared.rebase_error.is_none());
        assert!(
            prepared.run.is_none(),
            "unchanged nested routing keeps the existing frames"
        );
        assert!(!prepared.invalidated.contains(&completed));
        assert_eq!(
            prepared.graph.node_at(&completed).unwrap().result,
            original.node_at(&completed).unwrap().result
        );
        state.graph = prepared.graph;
        let mut restored = RunState::from_checkpoint(state.checkpoint()).unwrap();
        assert_eq!(serde_json::to_value(&restored.lanes).unwrap(), before);
        let unlocked = restored.graph.clone();
        assert!(restored.prepare_resume(&unlocked).is_err());
        let mut reviewed = unlocked;
        reviewed.lock_all();
        restored.prepare_resume(&reviewed).unwrap();
        assert_eq!(restored.lanes[0].next, Decision::Run(pending));
        assert_eq!(restored.graph, reviewed);
    }

    #[test]
    fn relocking_does_not_authorize_unapplied_execution_changes() {
        let mut graph = graph();
        graph.node_at_mut(&path("b")).unwrap().locked = false;
        let run = PlanRun::rebase_remaining(&graph, &[]).unwrap();
        let mut state = RunState::new(graph.clone(), run);
        let before = state.checkpoint();
        let mut reviewed = graph;
        reviewed.lock_all();
        reviewed.node_at_mut(&path("b")).unwrap().intent = "An unapplied execution change".into();
        assert!(matches!(
            state.prepare_resume(&reviewed),
            Err(ArchitectRunStartError::ReviewMismatch)
        ));
        assert_eq!(state.checkpoint(), before);
    }

    #[test]
    fn unlocked_checkpoint_still_rejects_structural_damage() {
        let mut graph = graph();
        graph.node_at_mut(&path("b")).unwrap().locked = false;
        let run = PlanRun::rebase_remaining(&graph, &[]).unwrap();
        let state = RunState::new(graph, run);
        assert!(RunState::from_checkpoint(state.checkpoint()).is_ok());
        let mut corrupt = state.checkpoint();
        corrupt["state"]["graph"]["edges"][0]["to"] = serde_json::json!("missing");
        assert!(RunState::from_checkpoint(corrupt).is_err());
    }

    #[test]
    fn prepared_rebase_retains_unrelated_results_and_does_not_mutate_the_checkpoint() {
        let mut graph = graph();
        graph.node_at_mut(&path("a")).unwrap().result = Some(architect::StepResult {
            summary: "Retain A".into(),
            attempt: 1,
        });
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path("b")));
        let mut state = RunState::new(graph.clone(), run);
        state.steps = 1;
        state.attempts = vec![(path("a"), 1)];
        let before = state.checkpoint();
        let mut proposed = graph.clone();
        let mut added = ArchitectNode::new("new", "New prerequisite");
        added.locked = true;
        proposed.add_node(added);
        proposed.connect("new", "join");
        let prepared = prepare_graph_update(&graph, &proposed, &[], Some(&state)).unwrap();
        assert!(prepared.rebase_error.is_none());
        assert_eq!(
            prepared.invalidated,
            architect::preview_graph_replacement(&graph, &proposed)
                .unwrap()
                .invalidated_steps
        );
        assert_eq!(
            prepared.graph.node_at(&path("a")).unwrap().result,
            graph.node_at(&path("a")).unwrap().result
        );
        assert!(prepared.run.is_some());
        assert_eq!(state.checkpoint(), before);
    }

    #[test]
    fn checkpoint_rejects_unknown_versions_missing_steps_and_invalid_lane_trees() {
        let graph = graph();
        let state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        let mut checkpoint = state.checkpoint();
        checkpoint["version"] = serde_json::json!(999);
        assert!(RunState::from_checkpoint(checkpoint).is_err());
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["lanes"][0]["next"] = serde_json::json!({ "Run": ["missing"] });
        assert!(RunState::from_checkpoint(checkpoint).is_err());
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["lanes"][0]["children"] = serde_json::json!([0]);
        assert!(RunState::from_checkpoint(checkpoint).is_err());
        let mut checkpoint = state.checkpoint();
        checkpoint["state"]["steps"] = serde_json::json!(MAX_RUN_STEPS + 1);
        assert!(RunState::from_checkpoint(checkpoint).is_err());
    }

    #[test]
    fn checkpoint_roundtrip_preserves_fork_children() {
        let mut graph = graph();
        let mut start = ArchitectNode::new("start", "Start");
        start.locked = true;
        graph.add_node(start);
        graph.connect("start", "a");
        graph.connect("start", "b");
        let mut run = PlanRun::start(&graph).unwrap();
        assert!(matches!(run.finish_step(&graph), Decision::Fork { .. }));
        let mut state = RunState::new(graph, run);
        let lanes = state.lanes[0].run.fork_lanes(&state.graph);
        for lane in lanes {
            state.lanes.push(Lane::new(lane, &state.graph));
        }
        state.lanes[0].children = (1..state.lanes.len()).collect();
        let restored = RunState::from_checkpoint(state.checkpoint()).unwrap();
        assert_eq!(restored.lanes[0].children, vec![1, 2]);
        assert_eq!(restored.lanes[1].next, Decision::Run(path("a")));
        assert_eq!(restored.lanes[2].next, Decision::Run(path("b")));
    }

    #[test]
    fn a_blocked_rebase_keeps_its_original_checkpoint_and_history() {
        let graph = graph();
        let mut state = RunState::new(graph.clone(), PlanRun::start(&graph).unwrap());
        state.rebase_error = Some("Review the changed conditional route".into());
        let mut restored = RunState::from_checkpoint(state.checkpoint()).unwrap();
        assert!(restored.prepare_resume(&graph).is_err());
        assert_eq!(restored.graph, graph);
        assert_eq!(restored.lanes[0].run.history(), &[path("a")]);
    }
}

fn last_assistant_text(thread: &AcpThread, cx: &App) -> String {
    thread
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            AgentThreadEntry::AssistantMessage(message) => Some(
                message
                    .chunks
                    .iter()
                    .filter_map(|chunk| match chunk {
                        AssistantMessageChunk::Message { block, .. } => Some(block.to_markdown(cx)),
                        AssistantMessageChunk::Thought { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}
