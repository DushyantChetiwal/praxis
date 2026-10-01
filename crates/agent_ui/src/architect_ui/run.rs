use std::cell::Cell;
use std::collections::HashMap;

use agent_client_protocol::schema::v1 as acp;
use architect::{NodeId, NodePath};
use gpui::{App, Context, Entity, SharedString};

use crate::conversation_view::ThreadView;

use super::ArchitectPane;

/// Resets the pane's transient start state on every exit from `ArchitectPane::run`.
struct RunStartingGuard<'a> {
    run_starting: &'a Cell<bool>,
}

impl<'a> RunStartingGuard<'a> {
    fn new(run_starting: &'a Cell<bool>) -> Self {
        run_starting.set(true);
        Self { run_starting }
    }
}

impl Drop for RunStartingGuard<'_> {
    fn drop(&mut self) {
        self.run_starting.set(false);
    }
}

impl ArchitectPane {
    /// Starts the plan owned by the root conversation.
    ///
    /// Execution policy lives in `agent::start_architect_run`; this pane only
    /// supplies the owning conversation and presents start failures.
    pub(super) fn run(&mut self, cx: &mut Context<Self>) {
        self.start_run(None, cx);
    }

    /// Starts a run at one step, keeping what the steps before it reported.
    pub(super) fn run_from(&mut self, path: NodePath, cx: &mut Context<Self>) {
        self.start_run(Some(path), cx);
    }

    fn start_run(&mut self, from: Option<NodePath>, cx: &mut Context<Self>) {
        if self.is_running(cx) {
            return;
        }
        let run_starting = RunStartingGuard::new(&self.run_starting);

        // A run always covers the whole plan, even when started from inside a
        // nested one: what is on screen is a viewpoint, not a scope. Started
        // from a step, it goes on from there to the end of the whole plan.
        let Some(graph) = self.root_graph(cx).cloned() else {
            return;
        };
        let Some(acp_thread) = self.plan_acp_thread(cx) else {
            self.report(
                "Architect needs the conversation that owns this plan to be open in the agent \
                 panel."
                    .to_string(),
                cx,
            );
            return;
        };

        let thread = self.thread.clone();
        let started = match &from {
            Some(path) => {
                agent::start_architect_run_from(thread, acp_thread, graph, path.clone(), cx)
            }
            None => agent::start_architect_run(thread, acp_thread, graph, cx),
        };
        if let Err(error) = started {
            drop(run_starting);
            self.record_activity(None, format!("Run could not start: {error}"), cx);
            self.report(error.to_string(), cx);
            return;
        }

        drop(run_starting);
        match from {
            Some(path) => {
                let title = self.step_title(&path, cx);
                self.record_activity(Some(path), format!("Started the run at {title}"), cx);
            }
            None => self.record_activity(None, "Started the plan run", cx),
        }
        cx.notify();
    }

    /// Lets the steps under way finish, and starts no more until resumed.
    pub(super) fn pause_run(&mut self, cx: &mut Context<Self>) {
        agent::pause_architect_run(&self.thread, cx);
        self.record_activity(None, "Paused the plan run", cx);
        cx.notify();
    }

    /// Carries on a paused run, or picks up a stopped or failed one from the
    /// steps it was on.
    pub(super) fn resume_run(&mut self, cx: &mut Context<Self>) {
        let Some(acp_thread) = self.plan_acp_thread(cx) else {
            self.report(
                "Architect needs the conversation that owns this plan to be open in the agent \
                 panel."
                    .to_string(),
                cx,
            );
            return;
        };
        if let Err(error) = agent::resume_architect_run(self.thread.clone(), acp_thread, cx) {
            self.record_activity(None, format!("Run could not resume: {error}"), cx);
            self.report(error.to_string(), cx);
            return;
        }
        self.record_activity(None, "Resumed the plan run", cx);
        cx.notify();
    }

    pub(super) fn is_paused(&self, cx: &App) -> bool {
        self.thread
            .read(cx)
            .architect_run()
            .is_some_and(agent::ArchitectRun::is_paused)
    }

    // Recovery remains available even when the result banner has been dismissed.
    pub(super) fn can_resume(&self, cx: &App) -> bool {
        !self.run_starting.get()
            && self
                .thread
                .read(cx)
                .architect_run()
                .is_some_and(agent::ArchitectRun::can_resume)
    }

    /// The title of a step, for showing progress without walking the plan.
    pub(super) fn step_title(&self, path: &NodePath, cx: &Context<Self>) -> SharedString {
        self.root_graph(cx)
            .and_then(|root| root.node_at(path))
            .map(|node| SharedString::from(node.title.clone()))
            .or_else(|| path.leaf().map(|id| SharedString::from(id.0.clone())))
            .unwrap_or_else(|| SharedString::from("Step"))
    }

    /// Stops a run between steps, and stops the turn it is waiting on.
    pub(super) fn stop_run(&mut self, cx: &mut Context<Self>) {
        agent::stop_architect_run(&self.thread, None, cx);
        self.record_activity(None, "Requested that the plan run stop", cx);
        self.run_starting.set(false);
        cx.notify();
    }

    pub(super) fn is_running(&self, cx: &App) -> bool {
        self.run_starting.get()
            || self
                .thread
                .read(cx)
                .architect_run()
                .is_some_and(agent::ArchitectRun::is_running)
    }

    /// The thread the step being run works in, while it has one of its own
    /// rather than sharing the plan's conversation.
    pub(super) fn run_step_session(&self, cx: &App) -> Option<acp::SessionId> {
        if !self.is_running(cx) {
            return None;
        }
        let step_thread = self.thread.read(cx).architect_run()?.step_thread()?;
        let session_id = step_thread.read(cx).session_id().clone();
        let plan_session_id = self
            .plan_acp_thread(cx)
            .map(|thread| thread.read(cx).session_id().clone());
        (plan_session_id.as_ref() != Some(&session_id)).then_some(session_id)
    }

    /// The view of the running step's thread, once the plan's conversation
    /// has loaded it.
    pub(super) fn run_step_view(&self, cx: &App) -> Option<Entity<ThreadView>> {
        let session_id = self.run_step_session(cx)?;
        self.plan_conversation_view(cx)?
            .read(cx)
            .thread_view(&session_id)
    }

    /// How many things the running step is waiting for the user to allow.
    pub(super) fn run_step_pending_permissions(&self, cx: &App) -> usize {
        let Some(session_id) = self.run_step_session(cx) else {
            return 0;
        };
        self.plan_conversation_view(cx).map_or(0, |view| {
            view.read(cx).pending_permission_count(&session_id, cx)
        })
    }

    /// How often the latest run went from one step to another on the level
    /// being shown, so a loop can say how much of its repeat limit was used.
    pub(super) fn connection_uses(&self, cx: &App) -> HashMap<(NodeId, NodeId), usize> {
        self.thread
            .read(cx)
            .architect_run()
            .map(|run| {
                count_connection_uses(run.history().iter().map(|step| &step.path), &self.focus)
            })
            .unwrap_or_default()
    }

    /// The steps the run is carrying out, of which a fork has several. Only
    /// the leaves matter for highlighting, since the canvas shows one level at
    /// a time. Between steps, the step the run is deciding a way out of stays
    /// highlighted.
    pub(super) fn running_nodes(&self, cx: &App) -> Vec<NodeId> {
        let Some(run) = self.thread.read(cx).architect_run() else {
            return Vec::new();
        };
        let steps = run.running_steps();
        if steps.is_empty() {
            let current = run.current.as_ref().and_then(NodePath::leaf);
            return current.cloned().into_iter().collect();
        }
        steps
            .iter()
            .filter_map(|step| step.path.leaf().cloned())
            .collect()
    }
}

/// "Step 4 · Write the tests" for one running step, "Running 3 steps" for
/// several, whether a pause is still waiting on steps, and `None` once the run
/// has ended.
pub(crate) fn running_summary(run: &agent::ArchitectRun) -> Option<SharedString> {
    if !run.is_running() {
        return None;
    }
    if run.is_paused() {
        return Some(match run.running_steps().len() {
            0 => "Paused".into(),
            1 => "Pausing once the running step finishes".into(),
            count => format!("Pausing once {count} running steps finish").into(),
        });
    }
    match run.running_steps() {
        [] | [_] => Some(format!("Step {} · {}", run.step_number, run.current_title).into()),
        steps => Some(format!("Running {} steps", steps.len()).into()),
    }
}

/// Counts the moves between steps of the plan at `focus`, given every step a
/// run took in order. A step with a plan of its own is never run itself, so
/// its children's steps stand for it. Only the latest pass through the plan
/// counts, because a run's repeat limits start over each time it enters one.
fn count_connection_uses<'a>(
    steps: impl IntoIterator<Item = &'a NodePath>,
    focus: &NodePath,
) -> HashMap<(NodeId, NodeId), usize> {
    let mut uses = HashMap::default();
    let mut previous: Option<(&NodeId, bool)> = None;
    for path in steps {
        let Some((step, inside)) = path
            .as_slice()
            .strip_prefix(focus.as_slice())
            .and_then(<[NodeId]>::split_first)
        else {
            // The run left this plan, so the next visit is a new pass.
            uses.clear();
            previous = None;
            continue;
        };
        let nested = !inside.is_empty();
        if nested && previous == Some((step, true)) {
            continue;
        }
        if let Some((from, _)) = previous {
            *uses.entry((from.clone(), step.clone())).or_default() += 1;
        }
        previous = Some((step, nested));
    }
    uses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(ids: &[&str]) -> NodePath {
        NodePath(ids.iter().map(|id| NodeId((*id).to_string())).collect())
    }

    fn uses(steps: &[NodePath], focus: &NodePath, from: &str, to: &str) -> usize {
        count_connection_uses(steps, focus)
            .get(&(NodeId(from.to_string()), NodeId(to.to_string())))
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn connection_uses_count_loops_and_nested_steps_once_per_visit() {
        let steps = [
            path(&["draft"]),
            path(&["review", "read"]),
            path(&["review", "judge"]),
            path(&["draft"]),
            path(&["review", "read"]),
            path(&["review", "judge"]),
            path(&["ship"]),
        ];
        let root = NodePath::default();
        assert_eq!(uses(&steps, &root, "draft", "review"), 2);
        assert_eq!(uses(&steps, &root, "review", "draft"), 1);
        assert_eq!(uses(&steps, &root, "review", "ship"), 1);
        assert_eq!(uses(&steps, &root, "review", "review"), 0);
    }

    #[test]
    fn connection_uses_inside_a_nested_plan_count_only_its_latest_pass() {
        let steps = [
            path(&["review", "read"]),
            path(&["review", "judge"]),
            path(&["review", "read"]),
            path(&["review", "judge"]),
            path(&["draft"]),
            path(&["review", "read"]),
            path(&["review", "judge"]),
        ];
        let review = path(&["review"]);
        assert_eq!(uses(&steps, &review, "judge", "read"), 0);
        assert_eq!(uses(&steps, &review, "read", "judge"), 1);
        assert_eq!(uses(&steps[..4], &review, "judge", "read"), 1);
    }

    fn return_while_starting(run_starting: &Cell<bool>) -> Option<()> {
        let _guard = RunStartingGuard::new(run_starting);
        assert!(run_starting.get());
        None
    }

    #[test]
    fn run_starting_resets_after_early_return() {
        let run_starting = Cell::new(false);

        assert_eq!(return_while_starting(&run_starting), None);
        assert!(!run_starting.get());
    }
}
