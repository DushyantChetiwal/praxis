use super::terminal_tool::{
    COMMAND_OUTPUT_LIMIT, TerminalOutputSelection, process_content, select_terminal_output_lines,
};
use crate::{AgentTool, TerminalHandle, ToolCallEventStream, ToolCapability, ToolInput};
use acp_thread::ProcessOutput;
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Result, anyhow};
use futures::{FutureExt as _, channel::mpsc, future::Shared};
use gpui::{App, AsyncApp, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    hash::{Hash as _, Hasher as _},
    rc::{Rc, Weak},
    sync::Arc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

const MAX_ACTIVE_TASKS: usize = 8;
const MAX_RETAINED_TASKS: usize = 32;
pub(super) const DEFAULT_YIELD_MS: u64 = 10_000;
pub(super) const MAX_YIELD_MS: u64 = 30_000;
const DEFAULT_WAIT_MS: u64 = 30_000;
/// Long enough to wait out a build or CI job in one call. Each expiry costs a
/// model round trip and grows the context.
const MAX_WAIT_MS: u64 = 30 * 60 * 1000;
const TRUNCATION_NOTE: &str = "Output was truncated. Head/tail selections apply to the bounded terminal capture, not the complete command history.";
/// Status checks that see the same output this far apart flag the command as
/// possibly waiting for input or hung.
const STALLED_OUTPUT_NOTE_AFTER: Duration = Duration::from_secs(10 * 60);
const NOTICE_COMMAND_CHARS: usize = 120;

/// A terminal command an agent thread started, as listed for people rather
/// than for the model.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTaskSummary {
    pub id: String,
    pub command: String,
    pub running: bool,
    /// Whether the command outlived its tool call and kept running on its own.
    pub in_background: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
enum StopReason {
    Cancelled,
    Stopped,
    TimedOut,
}

#[derive(Default)]
pub(crate) struct TerminalTaskRegistry {
    tasks: RefCell<VecDeque<TerminalTask>>,
    reservations: RefCell<Vec<Weak<TerminalLaunchCancellation>>>,
    children: RefCell<Vec<Weak<TerminalTaskRegistry>>>,
    /// Told whenever a task exits, so the thread can report or wake on it.
    completion_signal: RefCell<Option<mpsc::UnboundedSender<()>>>,
}

struct TerminalLaunchCancellation {
    cancelled: Cell<bool>,
    sender: RefCell<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

pub(super) struct TerminalTaskReservation {
    registry: Weak<TerminalTaskRegistry>,
    cancellation: Rc<TerminalLaunchCancellation>,
}

impl TerminalTaskReservation {
    pub(super) fn check(&self) -> Result<()> {
        if self.registry.upgrade().is_none() || self.cancellation.cancelled.get() {
            return Err(anyhow!("Terminal launch cancelled"));
        }
        Ok(())
    }

    pub(super) fn cancelled(&self) -> impl std::future::Future<Output = ()> + use<> {
        let mut receiver = self.cancellation.receiver.clone();
        async move {
            loop {
                if *receiver.borrow() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

impl TerminalTaskRegistry {
    pub(super) fn reserve(self: &Rc<Self>) -> Result<TerminalTaskReservation> {
        let active = self
            .tasks
            .borrow()
            .iter()
            .filter(|task| task.state.result.borrow().is_none())
            .count();
        let mut reservations = self.reservations.borrow_mut();
        reservations.retain(|reservation| reservation.strong_count() > 0);
        if active + reservations.len() >= MAX_ACTIVE_TASKS {
            return Err(anyhow!(
                "At most {MAX_ACTIVE_TASKS} terminal tasks may run in this thread. Wait for or stop an existing task."
            ));
        }
        let (sender, receiver) = watch::channel(false);
        let cancellation = Rc::new(TerminalLaunchCancellation {
            cancelled: Cell::new(false),
            sender: RefCell::new(sender),
            receiver,
        });
        reservations.push(Rc::downgrade(&cancellation));
        Ok(TerminalTaskReservation {
            registry: Rc::downgrade(self),
            cancellation,
        })
    }

    /// A stream that receives an item whenever one of this thread's tasks
    /// exits. A later call replaces the earlier stream.
    pub(crate) fn completion_signals(&self) -> mpsc::UnboundedReceiver<()> {
        let (sender, receiver) = mpsc::unbounded();
        self.completion_signal.replace(Some(sender));
        receiver
    }

    /// Whether a command that kept running on its own has exited without the
    /// model having seen its result.
    pub(crate) fn has_unreported_completions(&self) -> bool {
        self.tasks.borrow().iter().any(TerminalTask::is_unreported)
    }

    /// One line for each background command that exited since the model last
    /// heard about it, marking each as reported.
    pub(crate) fn take_finished_notices(&self) -> Vec<String> {
        self.tasks
            .borrow()
            .iter()
            .filter(|task| task.is_unreported())
            .map(|task| {
                task.state.reported.set(true);
                task.finished_notice()
            })
            .collect()
    }

    /// Starts a command that has already outlived its tool call.
    #[cfg(test)]
    pub(crate) fn start_in_background_for_test(
        self: &Rc<Self>,
        terminal: Rc<dyn TerminalHandle>,
        cx: &mut AsyncApp,
    ) -> String {
        let (stream, _events) = ToolCallEventStream::test();
        let reservation = self.reserve().expect("a free task slot");
        let task = self
            .start(
                reservation,
                terminal,
                "cargo test".into(),
                TerminalOutputSelection::default(),
                None,
                None,
                &stream,
                cx,
            )
            .expect("the task starts");
        task.background(cx);
        task.id
    }

    pub(crate) fn register_child(&self, child: &Rc<Self>) {
        let mut children = self.children.borrow_mut();
        children.retain(|child| child.strong_count() > 0);
        children.push(Rc::downgrade(child));
    }

    pub(crate) fn cancel_all(&self) {
        self.reservations.borrow_mut().retain(|reservation| {
            let Some(reservation) = reservation.upgrade() else {
                return false;
            };
            reservation.cancelled.set(true);
            reservation.sender.borrow_mut().send(true).log_err();
            true
        });
        for task in self.tasks.borrow().iter() {
            task.request_stop(StopReason::Cancelled);
        }
        // A child's terminal lifetime is independent of its model turn. These
        // weak links neither retain threads nor expose their tasks to the parent.
        self.children.borrow_mut().retain(|child| {
            let Some(child) = child.upgrade() else {
                return false;
            };
            child.cancel_all();
            true
        });
    }

    pub(crate) fn summaries(&self, now: Instant) -> Vec<TerminalTaskSummary> {
        self.tasks
            .borrow()
            .iter()
            .map(|task| TerminalTaskSummary {
                id: task.id.clone(),
                command: task.state.command.clone(),
                running: !task.is_complete(),
                in_background: task.state.backgrounded.get(),
                elapsed: now.saturating_duration_since(task.state.started_at),
            })
            .collect()
    }

    /// Commands that outlived their tool call and are still running.
    pub(crate) fn running_in_background(&self) -> usize {
        self.tasks
            .borrow()
            .iter()
            .filter(|task| task.state.backgrounded.get() && !task.is_complete())
            .count()
    }

    /// Kills every running command right away. When Praxis quits, the tasks
    /// that would act on a stop request never run.
    pub(crate) fn kill_all_now(&self, cx: &mut App) {
        for task in self.tasks.borrow().iter() {
            if let Some(terminal) = task.state.terminal.borrow().as_ref() {
                terminal.kill_now(cx);
            }
        }
        for child in self.children.borrow().iter() {
            if let Some(child) = child.upgrade() {
                child.kill_all_now(cx);
            }
        }
    }

    /// Stops one command for a person, who the model is then told stopped it.
    pub(crate) fn stop_task(&self, task_id: &str) -> Result<(), String> {
        self.get(task_id)?.request_stop(StopReason::Cancelled);
        Ok(())
    }

    /// The command's output for a person: the live capture while it runs, or
    /// the final result once it exits. Reading a live capture needs an async
    /// context that is not inside an update.
    pub(crate) fn task_output(&self, task_id: &str, cx: &AsyncApp) -> Result<String, String> {
        let task = self.get(task_id)?;
        if let Some(result) = task.state.result.borrow().as_ref() {
            return result
                .as_ref()
                .map(|result| result.content.clone())
                .map_err(Clone::clone);
        }
        let terminal = task.state.terminal.borrow();
        let terminal = terminal
            .as_ref()
            .ok_or_else(|| "Terminal task has no output handle".to_string())?;
        terminal
            .current_output(cx)
            .map(|output| output.output)
            .map_err(|error| error.to_string())
    }

    /// Shows one command's terminal in the Terminal panel, for a person.
    /// Needs an async context that is not inside an update.
    pub(crate) fn reveal_task(&self, task_id: &str, cx: &AsyncApp) -> Result<(), String> {
        let task = self.get(task_id)?;
        let terminal = task.state.terminal.borrow();
        let terminal = terminal
            .as_ref()
            .ok_or_else(|| "Terminal task has no terminal".to_string())?;
        terminal
            .show_in_terminal_panel(cx)
            .map_err(|error| error.to_string())
    }

    fn get(&self, task_id: &str) -> Result<TerminalTask, String> {
        self.tasks.borrow().iter().find(|task| task.id == task_id).cloned()
            .ok_or_else(|| format!("Unknown terminal task {task_id}. Tasks belong only to this thread; old completed tasks may have been evicted."))
    }

    pub(super) fn start(
        &self,
        reservation: TerminalTaskReservation,
        terminal: Rc<dyn TerminalHandle>,
        command: String,
        selection: TerminalOutputSelection,
        note: Option<String>,
        timeout_ms: Option<u64>,
        event_stream: &ToolCallEventStream,
        cx: &mut AsyncApp,
    ) -> Result<TerminalTask> {
        let guard = TerminalCleanup::new(terminal.clone(), cx);
        reservation.check()?;
        let (stop_tx, mut stop_rx) = watch::channel(None);
        let started_at = cx.background_executor().now();
        let state = Rc::new(TerminalTaskState {
            terminal: RefCell::new(Some(terminal.clone())),
            result: RefCell::new(None),
            command: command.clone(),
            started_at,
            output_progress: Cell::new(OutputProgress {
                fingerprint: output_fingerprint(""),
                changed_at: started_at,
            }),
            selection,
            note,
            stop_tx: RefCell::new(stop_tx),
            backgrounded: Cell::new(false),
            reported: Cell::new(false),
        });
        let completion_signal = self.completion_signal.borrow().clone();
        // Construct the timer before spawning: yielding never resets the hard deadline.
        let deadline =
            timeout_ms.map(|ms| cx.background_executor().timer(Duration::from_millis(ms)));
        let turn_cancelled = event_stream.cancelled_by_user();
        let cancelled = {
            let state = state.clone();
            async move {
                turn_cancelled.await;
                if state.backgrounded.get() {
                    futures::future::pending::<()>().await;
                }
            }
        };
        let completion = cx
            .spawn({
                let state = state.clone();
                async move |cx| {
                    let mut guard = guard;
                    let result = async {
                        let exit = guard.terminal.wait_for_exit(cx)?;
                        let deadline = async move {
                            match deadline {
                                Some(timer) => timer.await,
                                None => futures::future::pending().await,
                            }
                        };
                        let stop = async move {
                            loop {
                                if let Some(reason) = *stop_rx.borrow() {
                                    return reason;
                                }
                                if stop_rx.changed().await.is_err() {
                                    return StopReason::Cancelled;
                                }
                            }
                        };
                        let (status, reason) = futures::select_biased! {
                            status = exit.clone().fuse() => (status, None),
                            _ = deadline.fuse() => {
                                guard.terminal.kill(cx)?;
                                (exit.await, Some(StopReason::TimedOut))
                            },
                            _ = cancelled.fuse() => {
                                guard.terminal.kill(cx)?;
                                (exit.await, Some(StopReason::Cancelled))
                            },
                            reason = stop.fuse() => {
                                guard.terminal.kill(cx)?;
                                (exit.await, Some(reason))
                            },
                        };
                        guard.exited = true;
                        let user_stopped = matches!(reason, Some(StopReason::Cancelled))
                            || (reason.is_none() && guard.terminal.was_stopped_by_user(cx)?);
                        let mut output =
                            bounded_output(guard.terminal.current_output(cx)?, selection);
                        output.exit_status = Some(status.clone());
                        let truncated = output.truncated;
                        let content = process_content(
                            output,
                            &command,
                            matches!(reason, Some(StopReason::TimedOut)),
                            user_stopped,
                            TerminalOutputSelection::default(),
                        );
                        let content = if matches!(reason, Some(StopReason::Stopped)) {
                            format!("Command stopped by terminal_stop.\n\n{content}")
                        } else {
                            content
                        };
                        let content = if truncated {
                            format!("{content}\n\n{TRUNCATION_NOTE}")
                        } else {
                            content
                        };
                        let content = state.with_note(content);
                        Ok::<_, anyhow::Error>(CompletedTerminalTask { content, status })
                    }
                    .await
                    .map_err(|error| error.to_string());
                    *state.result.borrow_mut() = Some(result);
                    state.terminal.borrow_mut().take();
                    if let Some(signal) = completion_signal {
                        // The receiver is gone only when its thread is.
                        signal.unbounded_send(()).ok();
                    }
                    // Dropping the native handle releases its PTY, even while the bounded
                    // completed result is retained in the registry.
                }
            })
            .shared();
        let task = TerminalTask {
            id: uuid::Uuid::new_v4().to_string(),
            state,
            completion,
        };
        let mut tasks = self.tasks.borrow_mut();
        while tasks.len() >= MAX_RETAINED_TASKS {
            let Some(index) = tasks
                .iter()
                .position(|task| task.state.result.borrow().is_some())
            else {
                break;
            };
            tasks.remove(index);
        }
        tasks.push_back(task.clone());
        drop(reservation);
        Ok(task)
    }
}

impl Drop for TerminalTaskRegistry {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

// Kill outside entity updates, including when a thread is dropped while a tool
// still holds a completion future. Native handle release alone also kills, but
// explicit cleanup makes the lifetime contract apply to every environment.
pub(super) struct TerminalCleanup {
    terminal: Rc<dyn TerminalHandle>,
    cx: AsyncApp,
    pub(super) exited: bool,
}

impl TerminalCleanup {
    fn new(terminal: Rc<dyn TerminalHandle>, cx: &AsyncApp) -> Self {
        Self {
            terminal,
            cx: cx.clone(),
            exited: false,
        }
    }

    pub(super) fn into_terminal(mut self) -> Rc<dyn TerminalHandle> {
        self.exited = true;
        self.terminal.clone()
    }
}

impl std::ops::Deref for TerminalCleanup {
    type Target = dyn TerminalHandle;

    fn deref(&self) -> &Self::Target {
        self.terminal.as_ref()
    }
}

// Dropping an in-flight creation cancels environment/sandbox preparation. If
// creation already returned a handle but its consumer hasn't been polled, take
// ownership of that handle before dropping the task so even non-native handles
// are killed. Do not detach and await creation: that could spawn after Stop.
pub(super) struct TerminalCreation {
    task: Task<Result<Rc<dyn TerminalHandle>>>,
    cx: AsyncApp,
    finished: bool,
}

impl TerminalCreation {
    pub(super) fn new(task: Task<Result<Rc<dyn TerminalHandle>>>, cx: &AsyncApp) -> Self {
        Self {
            task,
            cx: cx.clone(),
            finished: false,
        }
    }
}

impl std::future::Future for TerminalCreation {
    type Output = Result<TerminalCleanup>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let result = std::task::ready!(std::pin::Pin::new(&mut self.task).poll(cx));
        self.finished = true;
        std::task::Poll::Ready(result.map(|terminal| TerminalCleanup::new(terminal, &self.cx)))
    }
}

impl Drop for TerminalCreation {
    fn drop(&mut self) {
        if !self.finished {
            match (&mut self.task).now_or_never() {
                Some(Ok(terminal)) => drop(TerminalCleanup::new(terminal, &self.cx)),
                Some(Err(error)) => log::debug!("Cancelled terminal creation failed: {error:#}"),
                None => {}
            }
        }
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        if !self.exited {
            let terminal = self.terminal.clone();
            self.cx
                .spawn(async move |cx| {
                    terminal.kill(cx).log_err();
                })
                .detach();
        }
    }
}

struct CompletedTerminalTask {
    content: String,
    status: acp::TerminalExitStatus,
}

struct TerminalTaskState {
    terminal: RefCell<Option<Rc<dyn TerminalHandle>>>,
    result: RefCell<Option<Result<CompletedTerminalTask, String>>>,
    command: String,
    started_at: Instant,
    output_progress: Cell<OutputProgress>,
    selection: TerminalOutputSelection,
    note: Option<String>,
    stop_tx: RefCell<watch::Sender<Option<StopReason>>>,
    backgrounded: Cell<bool>,
    /// Whether the model has seen the final result.
    reported: Cell<bool>,
}

#[derive(Clone, Copy)]
struct OutputProgress {
    fingerprint: u64,
    changed_at: Instant,
}

fn output_fingerprint(output: &str) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    output.hash(&mut hasher);
    hasher.finish()
}

impl TerminalTaskState {
    fn with_note(&self, content: String) -> String {
        match &self.note {
            Some(note) => format!("{note}\n\n{content}"),
            None => content,
        }
    }

    /// How long the output has gone unchanged across status checks. Compares
    /// content rather than length, which stops growing once the capture is full.
    fn unchanged_output_for(&self, output: &str, now: Instant) -> Duration {
        let fingerprint = output_fingerprint(output);
        let mut progress = self.output_progress.get();
        if progress.fingerprint != fingerprint {
            progress = OutputProgress {
                fingerprint,
                changed_at: now,
            };
            self.output_progress.set(progress);
        }
        now.saturating_duration_since(progress.changed_at)
    }
}

#[derive(Clone)]
pub(super) struct TerminalTask {
    id: String,
    state: Rc<TerminalTaskState>,
    completion: Shared<Task<()>>,
}

impl TerminalTask {
    pub(super) fn background(&self, cx: &AsyncApp) {
        if !self.state.backgrounded.replace(true)
            && !self.is_complete()
            && let Some(terminal) = self.state.terminal.borrow().as_ref()
        {
            terminal.show_in_terminal_panel(cx).log_err();
        }
    }

    pub(super) fn cancel(&self) {
        self.request_stop(StopReason::Cancelled);
    }

    pub(super) fn is_complete(&self) -> bool {
        self.state.result.borrow().is_some()
    }

    fn is_unreported(&self) -> bool {
        self.state.backgrounded.get() && self.is_complete() && !self.state.reported.get()
    }

    fn finished_notice(&self) -> String {
        let mut command: String = self
            .state
            .command
            .chars()
            .take(NOTICE_COMMAND_CHARS)
            .collect();
        if command.len() < self.state.command.len() {
            command.push('…');
        }
        let outcome = match self.state.result.borrow().as_ref() {
            Some(Ok(result)) => match result.status.exit_code {
                Some(code) => format!("exited with code {code}"),
                None => "exited without an exit code".to_string(),
            },
            Some(Err(_)) => "ended, but its exit could not be confirmed".to_string(),
            None => "is still running".to_string(),
        };
        format!(
            "Background task {} `{command}` {outcome}. Read its output with terminal_status.",
            self.id
        )
    }

    fn request_stop(&self, reason: StopReason) {
        if self.state.result.borrow().is_none() {
            self.state.stop_tx.borrow_mut().send(Some(reason)).log_err();
        }
    }

    pub(super) async fn wait(&self, wait_ms: u64, cx: &AsyncApp) {
        futures::select_biased! {
            _ = self.completion.clone().fuse() => {},
            _ = cx.background_executor().timer(Duration::from_millis(wait_ms)).fuse() => {},
        }
    }

    pub(super) fn response(&self, foreground: bool, cx: &AsyncApp) -> Result<String, String> {
        if let Some(result) = self.state.result.borrow().as_ref() {
            self.state.reported.set(true);
            let result = result.as_ref().map_err(|error| format!("task_id: {}\nstill_running: unknown\nExit could not be confirmed; cleanup requested.\nmonitor_error: {error}", self.id))?;
            if foreground {
                return Ok(result.content.clone());
            }
            return Ok(format!(
                "task_id: {}\nstill_running: false\nexit_status: {:?}\n\n{}",
                self.id, result.status, result.content
            ));
        }
        let terminal = self.state.terminal.borrow();
        let terminal = terminal
            .as_ref()
            .ok_or_else(|| "Terminal task has no output handle".to_string())?;
        let output = terminal
            .current_output(cx)
            .map_err(|error| error.to_string())?;
        let unchanged_for = self
            .state
            .unchanged_output_for(&output.output, cx.background_executor().now());
        let stalled = if unchanged_for >= STALLED_OUTPUT_NOTE_AFTER {
            format!(
                "No new output for {} minutes. If the command is waiting for input or hung, stop it with terminal_stop and rerun it non-interactively.\n",
                unchanged_for.as_secs() / 60
            )
        } else {
            String::new()
        };
        let output = bounded_output(output, self.state.selection);
        let truncated = if output.truncated {
            TRUNCATION_NOTE
        } else {
            ""
        };
        Ok(self.state.with_note(format!("task_id: {}\nstill_running: true\nCommand is still running; no final exit status yet.\n{stalled}{truncated}\n\n```\n{}\n```\nContinue with work that does not depend on this task. If nothing remains, end your turn instead of waiting: Praxis resumes this conversation when the task finishes. Do not shell sleep or busy-poll terminal_status.", self.id, output.output)))
    }
}

fn bounded_output(mut output: ProcessOutput, selection: TerminalOutputSelection) -> ProcessOutput {
    output.output = select_terminal_output_lines(&output.output, selection);
    if output.output.len() > COMMAND_OUTPUT_LIMIT as usize {
        // Keep the end, where commands report their results.
        let mut start = output.output.len() - COMMAND_OUTPUT_LIMIT as usize;
        while !output.output.is_char_boundary(start) {
            start += 1;
        }
        output.output.drain(..start);
        output.truncated = true;
    }
    output
}

/// Read the current bounded output and exit status of a terminal task in this thread.
/// This does not wait. Continue independent work while it runs, rather than
/// busy-polling or running shell sleep.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminalStatusToolInput {
    /// Task id returned by terminal in this thread (not a shell command or PID).
    pub task_id: String,
}

/// Wait for a terminal task in this thread, waking immediately on exit.
/// Prefer other work first: tool results tell you when background tasks finish.
/// For a long task with nothing else to do, end your turn instead; Praxis resumes
/// the conversation when a background task finishes. Use this tool for short
/// waits whose result you need right away. Never use shell sleep or busy-poll
/// status. Wait expiry returns still_running without killing the command; the
/// launch command's hard timeout_ms continues to apply.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminalWaitToolInput {
    /// Task id returned by terminal in this thread.
    pub task_id: String,
    /// Wait duration in milliseconds, from 1 through 1800000 (30 minutes)
    /// inclusive. Defaults to 30000 when omitted or null. Exit wakes the wait
    /// early. This is NOT a process runtime limit.
    #[serde(default)]
    #[schemars(range(min = 1, max = 1800000))]
    pub timeout_ms: Option<u64>,
}

/// Stop an existing terminal task owned by this thread and report its output/status.
/// Cannot execute commands, restart tasks, or stop arbitrary system processes.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminalStopToolInput {
    /// Task id returned by terminal in this thread.
    pub task_id: String,
}

fn terminal_wait_title(remaining: Duration) -> String {
    let seconds = remaining.as_millis().div_ceil(1000);
    format!("Waiting · {}:{:02} remaining", seconds / 60, seconds % 60)
}

async fn wait_with_countdown(
    task: &TerminalTask,
    wait_ms: u64,
    event_stream: &ToolCallEventStream,
    cx: &AsyncApp,
) -> Result<(), String> {
    let executor = cx.background_executor().clone();
    let deadline = executor.now() + Duration::from_millis(wait_ms);
    let completion = task.wait(wait_ms, cx).fuse();
    futures::pin_mut!(completion);
    event_stream
        .update_fields(|update| update.title(terminal_wait_title(Duration::from_millis(wait_ms))));
    loop {
        futures::select_biased! {
            _ = event_stream.cancelled_by_user().fuse() => return Err("Terminal wait cancelled".into()),
            _ = completion => break,
            _ = executor.timer(Duration::from_secs(1)).fuse() => {
                event_stream.update_fields(|update| update.title(terminal_wait_title(deadline.saturating_duration_since(executor.now()))));
            },
        }
    }
    event_stream.update_fields(|update| update.title("Finished waiting"));
    Ok(())
}

macro_rules! terminal_management_tool {
    ($tool:ident, $input:ty, $name:literal, $title:literal, $capability:ident, $restricted:literal, $action:expr) => {
        pub struct $tool {
            registry: Weak<TerminalTaskRegistry>,
        }
        impl $tool {
            pub(crate) fn new(registry: &Rc<TerminalTaskRegistry>) -> Self {
                Self {
                    registry: Rc::downgrade(registry),
                }
            }
        }
        impl AgentTool for $tool {
            type Input = $input;
            type Output = String;
            const NAME: &'static str = $name;
            fn capability() -> ToolCapability {
                ToolCapability::$capability
            }
            fn kind() -> acp::ToolKind {
                if Self::NAME == "terminal_wait" {
                    acp::ToolKind::Other
                } else {
                    acp::ToolKind::Execute
                }
            }
            fn allow_in_restricted_mode() -> bool {
                $restricted
            }
            fn initial_title(
                &self,
                input: Result<Self::Input, serde_json::Value>,
                _cx: &mut App,
            ) -> SharedString {
                if Self::NAME == "terminal_wait" {
                    return $title.into();
                }
                match input {
                    Ok(input) => format!("{} {}", $title, input.task_id).into(),
                    Err(_) => $title.into(),
                }
            }
            fn run(
                self: Arc<Self>,
                input: ToolInput<Self::Input>,
                event_stream: ToolCallEventStream,
                cx: &mut App,
            ) -> Task<Result<String, String>> {
                cx.spawn(async move |cx| {
                    let input = input.recv().await.map_err(|error| error.to_string())?;
                    let task = self
                        .registry
                        .upgrade()
                        .ok_or_else(|| "Terminal thread has been released".to_string())?
                        .get(&input.task_id)?;
                    let wait_ms: u64 = ($action)(&task, &input)?;
                    if wait_ms > 0 && Self::NAME == "terminal_wait" {
                        wait_with_countdown(&task, wait_ms, &event_stream, cx).await?;
                    } else if wait_ms > 0 {
                        futures::select_biased! {
                            _ = event_stream.cancelled_by_user().fuse() => {
                                // Turn replacement cancels this wait, not its job.
                                // Explicit Stop is handled by the thread registry.
                                return Err("Terminal management cancelled".to_string());
                            },
                            _ = task.wait(wait_ms, cx).fuse() => {},
                        }
                    }
                    task.response(false, cx)
                })
            }
        }
    };
}

terminal_management_tool!(
    TerminalStatusTool,
    TerminalStatusToolInput,
    "terminal_status",
    "Terminal status",
    ReadOnly,
    true,
    |_task: &TerminalTask, _input: &TerminalStatusToolInput| Ok::<_, String>(0)
);
terminal_management_tool!(
    TerminalWaitTool,
    TerminalWaitToolInput,
    "terminal_wait",
    "Waiting",
    ReadOnly,
    true,
    |_task: &TerminalTask, input: &TerminalWaitToolInput| {
        let timeout = input.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
        if !(1..=MAX_WAIT_MS).contains(&timeout) {
            return Err(format!("timeout_ms must be between 1 and {MAX_WAIT_MS}"));
        }
        Ok(timeout)
    }
);
terminal_management_tool!(
    TerminalStopTool,
    TerminalStopToolInput,
    "terminal_stop",
    "Stopping terminal task",
    ArbitraryExecution,
    false,
    |task: &TerminalTask, _input: &TerminalStopToolInput| {
        task.request_stop(StopReason::Stopped);
        Ok::<_, String>(DEFAULT_YIELD_MS)
    }
);

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use crate::tests::FakeTerminalHandle;
    use gpui::TestAppContext;

    fn start(
        registry: &Rc<TerminalTaskRegistry>,
        terminal: Rc<FakeTerminalHandle>,
        timeout_ms: Option<u64>,
        cx: &mut TestAppContext,
    ) -> TerminalTask {
        let (stream, _events) = ToolCallEventStream::test();
        registry
            .start(
                registry.reserve().unwrap(),
                terminal,
                "test command".into(),
                TerminalOutputSelection::default(),
                None,
                timeout_ms,
                &stream,
                &mut cx.to_async(),
            )
            .unwrap()
    }

    #[gpui::test]
    async fn terminal_tasks_background_exposes_the_original_process_once(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        task.background(&cx.to_async());
        task.background(&cx.to_async());
        assert_eq!(terminal.panel_exposures(), 1);
        assert!(!terminal.was_killed());
        terminal.signal_exit();
        task.completion.clone().await;
        task.background(&cx.to_async());
        assert_eq!(terminal.panel_exposures(), 1);
    }

    #[gpui::test]
    async fn terminal_tasks_report_background_completions_once(cx: &mut TestAppContext) {
        use futures::StreamExt as _;

        let registry = Rc::new(TerminalTaskRegistry::default());
        let mut signals = registry.completion_signals();
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        task.background(&cx.to_async());
        assert!(!registry.has_unreported_completions());
        terminal.signal_exit();
        task.completion.clone().await;
        assert!(signals.next().await.is_some());
        assert!(registry.has_unreported_completions());
        let notices = registry.take_finished_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&task.id), "{notices:?}");
        assert!(notices[0].contains("`test command`"), "{notices:?}");
        assert!(registry.take_finished_notices().is_empty());

        // Reading the result directly also counts as reporting it.
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        task.background(&cx.to_async());
        terminal.signal_exit();
        task.completion.clone().await;
        assert!(registry.has_unreported_completions());
        assert!(task.response(false, &cx.to_async()).is_ok());
        assert!(!registry.has_unreported_completions());

        // A command that finished inside its own tool call was never backgrounded.
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        terminal.signal_exit();
        task.completion.clone().await;
        assert!(!registry.has_unreported_completions());
    }

    fn advance(cx: &mut TestAppContext, milliseconds: u64) {
        // advance_clock polls while advancing, which can start a timer at the
        // wrong test instant. Advance the raw clock only after parking tasks.
        cx.dispatcher
            .scheduler()
            .clock()
            .advance(Duration::from_millis(milliseconds));
        cx.run_until_parked();
    }

    #[test]
    fn terminal_tasks_legacy_inputs_and_capabilities() {
        for value in [
            serde_json::json!({"command": "echo hello", "cd": "."}),
            serde_json::json!({"command": "echo hello", "cd": ".", "yield_ms": null}),
        ] {
            let input: crate::TerminalToolInput = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(input.yield_ms.unwrap_or(DEFAULT_YIELD_MS), 10_000);
            let input: crate::SandboxedTerminalToolInput = serde_json::from_value(value).unwrap();
            assert!(input.yield_ms.is_none());
        }
        let mode = crate::SessionMode::Plan;
        assert!(TerminalStatusTool::capability().is_allowed_in(mode));
        assert!(TerminalWaitTool::capability().is_allowed_in(mode));
        assert!(!TerminalStopTool::capability().is_allowed_in(mode));
        assert!(!TerminalStopTool::allow_in_restricted_mode());
    }

    #[gpui::test]
    async fn terminal_tasks_fast_exit_keeps_final_format(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(|cx| FakeTerminalHandle::new_with_immediate_exit(cx, 0)));
        let task = start(&registry, terminal.clone(), None, cx);
        task.completion.clone().await;
        assert_eq!(
            task.response(true, &cx.to_async()).unwrap(),
            "```\ncommand output\n```"
        );
        let response = task.response(false, &cx.to_async()).unwrap();
        assert!(response.contains("still_running: false"));
        assert!(response.contains("exit_code: Some(0)"));
        assert!(
            task.state.terminal.borrow().is_none(),
            "completed tasks must release their PTY handle"
        );
        assert!(!terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_yield_preserves_hard_deadline(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), Some(1000), cx);
        let mut foreground = cx.spawn({
            let task = task.clone();
            async move |cx| {
                task.wait(10, &cx).await;
                task.response(true, &cx)
            }
        });
        cx.run_until_parked();
        assert!((&mut foreground).now_or_never().is_none());
        advance(cx, 10);
        let response = foreground.await.unwrap();
        assert!(response.contains("still_running: true"));
        assert!(response.contains(&task.id));
        assert!(!response.contains("executed successfully"));
        assert!(!terminal.was_killed());
        advance(cx, 989);
        assert!(!terminal.was_killed());
        advance(cx, 1);
        task.completion.clone().await;
        assert!(terminal.was_killed());
        let response = task.response(false, &cx.to_async()).unwrap();
        assert!(response.contains("timed out"));
        assert!(response.contains("still_running: false"));
        assert!(response.contains("partial output"));
    }

    #[gpui::test]
    async fn terminal_tasks_status_is_scoped_and_bounded(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let other = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(|cx| {
            FakeTerminalHandle::new_never_exits(cx)
                .with_output(ProcessOutput::new("日".repeat(30_000), false))
        }));
        let task = start(&registry, terminal.clone(), None, cx);
        let (stream, _events) = ToolCallEventStream::test();
        let response = cx
            .update(|cx| {
                Arc::new(TerminalStatusTool::new(&registry)).run(
                    ToolInput::resolved(TerminalStatusToolInput {
                        task_id: task.id.clone(),
                    }),
                    stream,
                    cx,
                )
            })
            .await
            .unwrap();
        assert!(response.contains("still_running: true"));
        assert!(response.contains("truncated"));
        assert!(response.len() < COMMAND_OUTPUT_LIMIT as usize + 1024);
        let (stream, _events) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                Arc::new(TerminalStatusTool::new(&other)).run(
                    ToolInput::resolved(TerminalStatusToolInput {
                        task_id: task.id.clone(),
                    }),
                    stream,
                    cx,
                )
            })
            .await;
        assert!(result.unwrap_err().contains("Unknown terminal task"));
        assert!(!terminal.was_killed());
        terminal.signal_exit();
        task.completion.clone().await;
        assert!(
            task.response(false, &cx.to_async()).unwrap().len()
                < COMMAND_OUTPUT_LIMIT as usize + 1024
        );
    }

    #[gpui::test]
    async fn terminal_tasks_wait_countdown_is_compact_and_wakes_early(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        let tool = Arc::new(TerminalWaitTool::new(&registry));
        assert_eq!(TerminalWaitTool::kind(), acp::ToolKind::Other);
        let title = cx.update(|cx| {
            tool.initial_title(
                Ok(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: None,
                }),
                cx,
            )
        });
        assert_eq!(title.as_ref(), "Waiting");
        assert!(!title.contains(&task.id));
        let (stream, mut events) = ToolCallEventStream::test();
        let wait = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: None,
                }),
                stream,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            events
                .expect_update_fields()
                .await
                .title
                .value()
                .map(String::as_str),
            Some("Waiting · 0:30 remaining")
        );
        advance(cx, 1000);
        assert_eq!(
            events
                .expect_update_fields()
                .await
                .title
                .value()
                .map(String::as_str),
            Some("Waiting · 0:29 remaining")
        );
        terminal.signal_exit();
        cx.run_until_parked();
        assert_eq!(
            events
                .expect_update_fields()
                .await
                .title
                .value()
                .map(String::as_str),
            Some("Finished waiting")
        );
        assert!(wait.await.expect("wait").contains("still_running: false"));
        assert!(!terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_wait_accepts_long_waits_and_ends_on_cancel(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        let (stream, _events, mut cancellation_tx) = ToolCallEventStream::test_with_cancellation();
        let mut wait = cx.update(|cx| {
            Arc::new(TerminalWaitTool::new(&registry)).run(
                ToolInput::resolved(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: Some(MAX_WAIT_MS),
                }),
                stream,
                cx,
            )
        });
        cx.run_until_parked();
        advance(cx, 10 * 60 * 1000);
        assert!((&mut wait).now_or_never().is_none());

        ToolCallEventStream::signal_cancellation_with_sender(&mut cancellation_tx);
        cx.run_until_parked();
        let result = (&mut wait)
            .now_or_never()
            .expect("cancelling the turn must end the wait immediately");
        assert!(result.is_err());
        assert!(!terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_flag_output_that_stops_changing(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        task.background(&cx.to_async());
        let summaries = registry.summaries(cx.executor().now());
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].command, "test command");
        assert!(summaries[0].running && summaries[0].in_background);
        assert_eq!(registry.running_in_background(), 1);

        let response = task.response(false, &cx.to_async()).unwrap();
        assert!(!response.contains("No new output"));
        advance(cx, 5 * 60 * 1000);
        let response = task.response(false, &cx.to_async()).unwrap();
        assert!(!response.contains("No new output"));
        advance(cx, 6 * 60 * 1000);
        let response = task.response(false, &cx.to_async()).unwrap();
        assert!(
            response.contains("No new output for 11 minutes"),
            "a command whose output stopped changing should be flagged: {response}"
        );

        registry.stop_task(&task.id).unwrap();
        task.completion.clone().await;
        assert!(terminal.was_killed());
        assert_eq!(registry.running_in_background(), 0);
        assert!(!registry.summaries(cx.executor().now())[0].running);
        assert!(registry.stop_task("missing").is_err());
    }

    #[gpui::test]
    async fn terminal_tasks_reveal_shows_known_running_tasks(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);

        assert!(
            registry
                .reveal_task("missing", &cx.to_async())
                .unwrap_err()
                .contains("Unknown terminal task")
        );
        assert_eq!(terminal.panel_exposures(), 0);

        registry.reveal_task(&task.id, &cx.to_async()).unwrap();
        assert_eq!(terminal.panel_exposures(), 1);
        assert!(!terminal.was_killed());

        // An exited command has released its terminal, so there is nothing to show.
        terminal.signal_exit();
        task.completion.clone().await;
        assert!(registry.reveal_task(&task.id, &cx.to_async()).is_err());
        assert_eq!(terminal.panel_exposures(), 1);
    }

    #[gpui::test]
    async fn terminal_tasks_wait_wakes_on_exit(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        let (stream, _events) = ToolCallEventStream::test();
        let mut wait = cx.update(|cx| {
            Arc::new(TerminalWaitTool::new(&registry)).run(
                ToolInput::resolved(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: Some(60_000),
                }),
                stream,
                cx,
            )
        });
        cx.run_until_parked();
        assert!((&mut wait).now_or_never().is_none());
        terminal.signal_exit();
        cx.run_until_parked();
        let response = (&mut wait)
            .now_or_never()
            .expect("exit must wake the waiter without advancing the clock")
            .unwrap();
        assert!(response.contains("still_running: false"));
        assert!(!terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_wait_expiry_does_not_kill(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        for timeout in [Some(0), Some(MAX_WAIT_MS + 1)] {
            let (stream, _events) = ToolCallEventStream::test();
            let result = cx
                .update(|cx| {
                    Arc::new(TerminalWaitTool::new(&registry)).run(
                        ToolInput::resolved(TerminalWaitToolInput {
                            task_id: task.id.clone(),
                            timeout_ms: timeout,
                        }),
                        stream,
                        cx,
                    )
                })
                .await;
            assert!(result.unwrap_err().contains("between 1 and 1800000"));
        }
        let (stream, _events) = ToolCallEventStream::test();
        let mut wait = cx.update(|cx| {
            Arc::new(TerminalWaitTool::new(&registry)).run(
                ToolInput::resolved(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: None,
                }),
                stream,
                cx,
            )
        });
        cx.run_until_parked();
        advance(cx, 29_999);
        assert!((&mut wait).now_or_never().is_none());
        advance(cx, 1);
        assert!(wait.await.unwrap().contains("still_running: true"));
        assert!(!terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_stop_cancel_and_drop_cleanup(cx: &mut TestAppContext) {
        for action in ["stop", "cancel", "drop", "drop_without_waiter"] {
            let registry = Rc::new(TerminalTaskRegistry::default());
            let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
            let task = start(&registry, terminal.clone(), None, cx);
            cx.run_until_parked();
            match action {
                "stop" => {
                    let (stream, _events) = ToolCallEventStream::test();
                    let response = cx
                        .update(|cx| {
                            Arc::new(TerminalStopTool::new(&registry)).run(
                                ToolInput::resolved(TerminalStopToolInput {
                                    task_id: task.id.clone(),
                                }),
                                stream,
                                cx,
                            )
                        })
                        .await
                        .unwrap();
                    assert!(response.contains("terminal_stop"));
                    assert!(response.contains("still_running: false"));
                }
                "cancel" => registry.cancel_all(),
                "drop" => drop(registry),
                _ => {
                    drop(task);
                    drop(registry);
                    cx.run_until_parked();
                    assert!(terminal.was_killed());
                    continue;
                }
            }
            task.completion.clone().await;
            cx.run_until_parked();
            assert!(terminal.was_killed(), "{action}");
            assert!(
                task.response(false, &cx.to_async())
                    .unwrap()
                    .contains("still_running: false")
            );
        }
    }

    #[gpui::test]
    async fn terminal_tasks_limits_and_completed_eviction(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let reservations = (0..MAX_ACTIVE_TASKS)
            .map(|_| registry.reserve().unwrap())
            .collect::<Vec<_>>();
        assert!(registry.reserve().is_err());
        drop(reservations);
        let mut active = Vec::new();
        for _ in 0..MAX_ACTIVE_TASKS {
            let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
            active.push(start(&registry, terminal, None, cx));
        }
        assert!(registry.reserve().is_err());
        registry.cancel_all();
        for task in &active {
            task.completion.clone().await;
        }
        let first_id = active[0].id.clone();
        drop(active);
        for _ in 0..MAX_RETAINED_TASKS {
            let terminal =
                Rc::new(cx.update(|cx| FakeTerminalHandle::new_with_immediate_exit(cx, 0)));
            start(&registry, terminal, None, cx).completion.await;
        }
        assert_eq!(registry.tasks.borrow().len(), MAX_RETAINED_TASKS);
        assert!(registry.get(&first_id).is_err());
    }

    #[gpui::test]
    async fn terminal_tasks_both_launch_variants_validate_and_yield(cx: &mut TestAppContext) {
        use settings::Settings as _;
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), [], cx).await;
        cx.update(|cx| {
            let mut settings = agent_settings::AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Allow;
            settings.tool_permissions.tools.remove("terminal");
            settings.sandbox_permissions.allow_unsandboxed = true;
            agent_settings::AgentSettings::override_global(settings, cx);
        });
        for sandboxed in [false, true] {
            let registry = Rc::new(TerminalTaskRegistry::default());
            let environment = Rc::new(cx.update(|cx| {
                crate::tests::FakeThreadEnvironment::default()
                    .with_terminal(FakeTerminalHandle::new_never_exits(cx))
            }));
            let tool = if sandboxed {
                crate::SandboxedTerminalTool::new(project.clone(), environment.clone())
                    .with_task_registry(&registry)
                    .erase()
            } else {
                crate::TerminalTool::new(project.clone(), environment.clone())
                    .with_task_registry(&registry)
                    .erase()
            };
            let (stream, _events) = ToolCallEventStream::test();
            let result = cx
                .update(|cx| {
                    tool.clone().run(
                        ToolInput::resolved(serde_json::json!({
                            "command": "echo work", "cd": ".", "yield_ms": 30001,
                        })),
                        stream,
                        cx,
                    )
                })
                .await;
            assert!(result.is_err());
            assert_eq!(environment.terminal_creation_count(), 0);
            let (stream, _events, mut cancellation) = ToolCallEventStream::test_with_cancellation();
            cancellation.send(true).unwrap();
            let cancelled = cx
                .update(|cx| {
                    tool.clone().run(
                        ToolInput::resolved(serde_json::json!({
                            "command": "echo work", "cd": ".", "yield_ms": 0,
                        })),
                        stream,
                        cx,
                    )
                })
                .await;
            assert!(cancelled.is_err());
            assert_eq!(environment.terminal_creation_count(), 0);
            let (stream, _events) = ToolCallEventStream::test();
            let response = cx
                .update(|cx| {
                    tool.run(
                        ToolInput::resolved(serde_json::json!({
                            "command": "echo work", "cd": ".", "yield_ms": 0,
                            "head_lines": 1, "tail_lines": 1,
                        })),
                        stream,
                        cx,
                    )
                })
                .await
                .unwrap_or_else(|error| panic!("{}", error.raw_output));
            assert!(
                response
                    .raw_output
                    .as_str()
                    .unwrap()
                    .contains("still_running: true")
            );
            assert_eq!(environment.terminal_creation_count(), 1);
            assert_eq!(environment.terminal_output_limits(), vec![Some(100 * 1024)]);
            assert_eq!(registry.tasks.borrow().len(), 1);
            registry.cancel_all();
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    async fn terminal_tasks_background_ignores_turn_cancel_but_not_stop(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let (stream, _events, mut cancellation) = ToolCallEventStream::test_with_cancellation();
        let task = registry
            .start(
                registry.reserve().unwrap(),
                terminal.clone(),
                "test".into(),
                TerminalOutputSelection::default(),
                None,
                None,
                &stream,
                &mut cx.to_async(),
            )
            .unwrap();
        drop(stream);
        cx.run_until_parked();
        assert!(!terminal.was_killed());
        task.background(&cx.to_async());
        cancellation.send(true).unwrap();
        cx.run_until_parked();
        assert!(!terminal.was_killed());
        registry.cancel_all();
        task.completion.clone().await;
        assert!(terminal.was_killed());
        assert!(
            task.response(false, &cx.to_async())
                .unwrap()
                .contains("user stopped")
        );
    }

    #[gpui::test]
    async fn terminal_tasks_interrupted_wait_does_not_stop_job(cx: &mut TestAppContext) {
        let registry = Rc::new(TerminalTaskRegistry::default());
        let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let task = start(&registry, terminal.clone(), None, cx);
        task.background(&cx.to_async());
        let (stream, _events, mut cancellation) = ToolCallEventStream::test_with_cancellation();
        let wait = cx.update(|cx| {
            Arc::new(TerminalWaitTool::new(&registry)).run(
                ToolInput::resolved(TerminalWaitToolInput {
                    task_id: task.id.clone(),
                    timeout_ms: None,
                }),
                stream,
                cx,
            )
        });
        cx.run_until_parked();
        cancellation.send(true).unwrap();
        assert!(wait.await.is_err());
        cx.run_until_parked();
        assert!(!terminal.was_killed());
        registry.cancel_all();
        task.completion.await;
        assert!(terminal.was_killed());
    }

    #[gpui::test]
    async fn terminal_tasks_cancel_invalidates_reservations_and_ready_creation(
        cx: &mut TestAppContext,
    ) {
        for returned in [false, true] {
            let registry = Rc::new(TerminalTaskRegistry::default());
            let reservation = registry.reserve().unwrap();
            let terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
            let creation = TerminalCreation::new(
                Task::ready(Ok(terminal.clone() as Rc<dyn TerminalHandle>)),
                &cx.to_async(),
            );
            if returned {
                let guard = creation.await.unwrap();
                registry.cancel_all();
                assert!(reservation.check().is_err());
                drop(guard);
            } else {
                registry.cancel_all();
                assert!(reservation.check().is_err());
                // Cancellation won before the consumer read the ready handle.
                drop(creation);
            }
            cx.run_until_parked();
            assert!(terminal.was_killed());
            reservation.cancelled().await;
            drop(reservation);
            assert!(
                registry.reserve().unwrap().check().is_ok(),
                "later launches must remain usable"
            );
        }
    }

    #[gpui::test]
    async fn terminal_tasks_child_cancellation_is_recursive_and_weak(cx: &mut TestAppContext) {
        let parent = Rc::new(TerminalTaskRegistry::default());
        let child = Rc::new(TerminalTaskRegistry::default());
        let grandchild = Rc::new(TerminalTaskRegistry::default());
        parent.register_child(&child);
        child.register_child(&grandchild);
        let stale = Rc::new(TerminalTaskRegistry::default());
        parent.register_child(&stale);
        drop(stale);
        let child_terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let grandchild_terminal = Rc::new(cx.update(FakeTerminalHandle::new_never_exits));
        let child_task = start(&child, child_terminal.clone(), None, cx);
        let grandchild_task = start(&grandchild, grandchild_terminal.clone(), None, cx);
        child_task.background(&cx.to_async());
        grandchild_task.background(&cx.to_async());
        parent.cancel_all();
        child_task.completion.await;
        grandchild_task.completion.await;
        assert!(child_terminal.was_killed());
        assert!(grandchild_terminal.was_killed());
        assert_eq!(parent.children.borrow().len(), 1);
        assert_eq!(Rc::strong_count(&child), 1);
    }

    #[test]
    fn terminal_tasks_output_selection_precedes_cap() {
        let output =
            ProcessOutput::new(format!("first\n{}\nlast", "middle\n".repeat(10_000)), false);
        let output = bounded_output(
            output,
            TerminalOutputSelection {
                head_lines: Some(1),
                tail_lines: Some(1),
            },
        );
        assert_eq!(output.output, "first\n\nlast");
        assert!(!output.truncated);
    }
}
