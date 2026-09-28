use std::cell::RefCell;
use std::rc::Rc;

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema::v1 as acp;
use architect::{
    ArchitectGraph, Branch, Decision, MAX_RUN_STEPS, NodePath, PlanRun, RunOutcome, RunRefusal,
};
use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, join_all};
use gpui::{App, AsyncApp, Context, Entity, SharedString, WeakEntity};

use crate::{ArchitectRun, ArchitectStepVisitId, NativeAgentConnection, SessionMode, Thread};

#[derive(Debug)]
pub enum ArchitectRunStartError {
    AlreadyRunning,
    Refused(RunRefusal),
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
pub(crate) struct RunState {
    /// The run's own copy of the plan, with what each step has reported, so
    /// edits made while the run is going cannot change its control flow.
    graph: ArchitectGraph,
    /// Lane 0 is the run itself. The rest run the branches of forks.
    lanes: Vec<Lane>,
    /// Steps started so far across every lane. It numbers them, and bounds the
    /// whole run however many ways it forks.
    steps: usize,
    in_flight: Vec<(usize, Entity<AcpThread>)>,
    /// Set once a lane fails, so the others stop instead of carrying on.
    halted: bool,
}

impl RunState {
    fn new(graph: ArchitectGraph, run: PlanRun) -> Self {
        let lane = Lane::new(run, &graph);
        Self {
            graph,
            lanes: vec![lane],
            steps: 0,
            in_flight: Vec::new(),
            halted: false,
        }
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

    /// Hands over every turn in flight, for stopping them.
    pub(crate) fn take_in_flight(&mut self) -> Vec<Entity<AcpThread>> {
        self.in_flight.drain(..).map(|(_, turn)| turn).collect()
    }
}

/// One line of work through the plan: the run itself, or a branch of a fork.
struct Lane {
    run: PlanRun,
    /// What the lane does next. It stays on a step until that step has been
    /// reported, so a lane that is interrupted knows which step it was on.
    next: Decision,
    /// The lanes running the branches of the fork this lane is waiting on.
    children: Vec<usize>,
    /// Where the lane's latest step ran. The question deciding its way out is
    /// put there, because that conversation saw the work it is about.
    last_step_thread: Option<Entity<AcpThread>>,
}

impl Lane {
    fn new(mut run: PlanRun, graph: &ArchitectGraph) -> Self {
        let next = run.decide(graph);
        Self {
            run,
            next,
            children: Vec::new(),
            last_step_thread: None,
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
    mut graph: ArchitectGraph,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    if thread
        .read(cx)
        .architect_run()
        .is_some_and(ArchitectRun::is_running)
    {
        return Err(ArchitectRunStartError::AlreadyRunning);
    }

    let plan_run = PlanRun::start(&graph)?;
    let first_step = plan_run.current();
    let first_title = step_title(&graph, &first_step);

    // A run must not inherit summaries from an earlier attempt. Clear both the
    // immutable run snapshot and the live graph that complete_step writes into.
    graph.clear_results();
    thread.update(cx, |thread, cx| {
        thread.update_architect_graph(|graph| graph.clear_results(), cx);
        thread.set_session_mode(SessionMode::Build, cx);
    });

    let state = Rc::new(RefCell::new(RunState::new(graph, plan_run)));
    let driver = Driver::new(&thread, acp_thread, state.clone(), cx);
    let task = cx.spawn(async move |cx| driver.run_to_end(cx).await);
    thread.update(cx, |thread, cx| {
        thread.start_architect_run(first_step, first_title, task, cx);
        thread.set_architect_run_control(state);
    });
    Ok(())
}

/// Cancels both the in-flight conversation turns and the task owned by the
/// thread. Either entity may already be gone while a window is closing.
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
        Some(control) => control.borrow_mut().take_in_flight(),
        None if run.is_some_and(ArchitectRun::is_running) => {
            let step_thread = run.and_then(ArchitectRun::step_thread);
            step_thread
                .or_else(|| acp_thread.cloned())
                .into_iter()
                .collect()
        }
        None => Vec::new(),
    };
    for working_thread in working {
        working_thread
            .update(cx, |thread, cx| thread.cancel(cx))
            .detach();
    }
    thread.update(cx, |thread, cx| thread.stop_architect_run(cx));
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

        if !outcome.is_success() && outcome != RunOutcome::Cancelled {
            let report = outcome.describe(&self.state.borrow().graph);
            self.begin_turn(0, &self.plan_thread);
            let sent = send_and_wait(&self.plan_thread, report, cx).await;
            self.end_turn(0);
            if let Err(error) = sent {
                log::error!("Architect: could not report how the run ended: {error}");
            }
        }

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
            let next = match next {
                Decision::Run(node) => self.run_step(lane, node, cx).await,
                Decision::Ask(branch) => self.decide_branch(lane, branch, cx).await,
                Decision::Fork { .. } => self.run_fork(lane, cx).await,
                Decision::Done(outcome) => return outcome,
            };
            match next {
                Ok(next) => self.state.borrow_mut().lanes[lane].next = next,
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
        let (step_number, attempt, title, mut prompt, alongside, note) = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            if state.steps >= MAX_RUN_STEPS {
                let steps = state.steps;
                return Err(RunOutcome::StepLimit { steps });
            }
            state.steps += 1;
            let step_number = state.steps;
            let run = &state.lanes[lane].run;
            let attempt = node.leaf().map(|id| run.attempt(id)).unwrap_or(1);
            let title = step_title(&state.graph, &node);
            let prompt = architect::step_prompt(&state.graph, &node, step_number, attempt);
            let alongside = if concurrent {
                state.running_alongside(lane)
            } else {
                Vec::new()
            };
            let note = architect::parallel_steps_prompt(&state.graph, &alongside);
            (step_number, attempt, title, prompt, alongside, note)
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
                    connection.create_architect_run_step_thread(plan_session_id, label, visit, cx)
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
                    Err(error) if alongside.is_empty() => {
                        log::warn!(
                            "Architect: running {node} in the plan's conversation, since a thread \
                             of its own could not be made: {error:#}"
                        );
                        self.plan_thread.clone()
                    }
                    Err(error) => {
                        // The plan's conversation can only take one step at a
                        // time, and others are running alongside this one.
                        let message =
                            format!("No thread could be made for step \"{node}\": {error:#}");
                        return Err(self.fail_step(visit, message, cx));
                    }
                }
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
        self.update_thread(cx, |thread, cx| {
            thread.set_architect_run_step_thread(visit, &step_thread, cx);
        })?;
        self.state.borrow_mut().lanes[lane].last_step_thread = Some(step_thread.clone());

        self.begin_turn(lane, &step_thread);
        let sent = send_and_wait(&step_thread, prompt, cx).await;
        let halted = self.end_turn(lane);
        self.update_thread(cx, |thread, _cx| {
            thread.clear_architect_step_visit(visit);
        })?;
        if let Err(error) = sent {
            if halted {
                self.finish_visit(visit, None, cx)?;
                return Err(RunOutcome::Cancelled);
            }
            let message = format!("Step \"{node}\" could not run: {error}");
            return Err(self.fail_step(visit, message, cx));
        }

        let reported = self.read_thread(cx, |thread, _| reported_summary(thread, &node, attempt))?;
        let summary = match reported {
            Some(summary) => summary,
            // Another lane failed and stopped this step before it reported,
            // so it is left to be run again.
            None if halted => {
                self.finish_visit(visit, None, cx)?;
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

        self.finish_visit(visit, Some(summary.clone().into()), cx)?;
        let mut guard = self.state.borrow_mut();
        let state = &mut *guard;
        if let Some(step) = state.graph.node_at_mut(&node) {
            step.result = Some(architect::StepResult { summary, attempt });
        }
        Ok(state.lanes[lane].run.finish_step(&state.graph))
    }

    /// Puts the question deciding a way out of the lane's last step to the
    /// conversation that carried that step out.
    async fn decide_branch(
        &self,
        lane: usize,
        branch: Branch,
        cx: &mut AsyncApp,
    ) -> Result<Decision, RunOutcome> {
        let (prompt, asked) = {
            let state = self.state.borrow();
            let asked = match &state.lanes[lane].last_step_thread {
                Some(thread) => thread.clone(),
                None => self.plan_thread.clone(),
            };
            (architect::branch_prompt(&state.graph, &branch), asked)
        };

        self.begin_turn(lane, &asked);
        let sent = send_and_wait(&asked, prompt, cx).await;
        if self.end_turn(lane) {
            return Err(RunOutcome::Cancelled);
        }
        if let Err(error) = sent {
            let message = format!("A branch could not be decided: {error}");
            log::error!("Architect: {message}");
            return Err(RunOutcome::Failed { message });
        }

        let reply = closing_message(&asked, cx);
        let taken = architect::parse_verdict(&reply).unwrap_or_else(|| {
            log::warn!(
                "Architect: no YES or NO in the reply deciding {}; treating it as no",
                branch.edge.0
            );
            false
        });
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
            state.take_in_flight()
        };
        for turn in turns {
            turn.update(cx, |thread, cx| thread.cancel(cx)).detach();
        }
    }

    fn begin_turn(&self, lane: usize, thread: &Entity<AcpThread>) {
        let mut state = self.state.borrow_mut();
        state.in_flight.push((lane, thread.clone()));
    }

    /// Notes that a lane's turn ended, and says whether the run was halted
    /// while it was in flight.
    fn end_turn(&self, lane: usize) -> bool {
        let mut state = self.state.borrow_mut();
        state.in_flight.retain(|(turn_lane, _)| *turn_lane != lane);
        state.halted
    }

    /// Closes a step that could not be carried out, recording why.
    fn fail_step(
        &self,
        visit: ArchitectStepVisitId,
        message: String,
        cx: &mut AsyncApp,
    ) -> RunOutcome {
        log::error!("Architect: {message}");
        if let Err(outcome) = self.finish_visit(visit, Some(message.clone().into()), cx) {
            return outcome;
        }
        RunOutcome::Failed { message }
    }

    fn finish_visit(
        &self,
        visit: ArchitectStepVisitId,
        summary: Option<SharedString>,
        cx: &mut AsyncApp,
    ) -> Result<(), RunOutcome> {
        self.update_thread(cx, |thread, cx| {
            thread.finish_architect_run_step(visit, summary, cx);
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
        if !outcome.is_success() && outcome != RunOutcome::Cancelled {
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

/// What a step reported through `complete_step` on this attempt, if anything.
fn reported_summary(thread: &Thread, node: &NodePath, attempt: usize) -> Option<String> {
    thread
        .architect_graph()
        .and_then(|graph| graph.node_at(node))
        .and_then(|step| step.result.as_ref())
        .filter(|result| result.attempt == attempt)
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
) -> anyhow::Result<()> {
    let send = thread.update(cx, |thread, cx| {
        thread.send(
            vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
            cx,
        )
    });
    send.await?;
    Ok(())
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
