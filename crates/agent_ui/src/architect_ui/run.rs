use std::cell::Cell;
use std::collections::HashMap;

use architect::{NodeId, NodePath};
use gpui::{App, Context, SharedString};

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
        if self.is_running(cx) {
            return;
        }
        let run_starting = RunStartingGuard::new(&self.run_starting);

        // A run always covers the whole plan, even when started from inside a
        // nested one: what is on screen is a viewpoint, not a scope.
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

        if let Err(error) = agent::start_architect_run(self.thread.clone(), acp_thread, graph, cx) {
            drop(run_starting);
            self.record_activity(None, format!("Run could not start: {error}"), cx);
            self.report(error.to_string(), cx);
            return;
        }

        drop(run_starting);
        self.record_activity(None, "Started the plan run", cx);
        cx.notify();
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
        let acp_thread = self.plan_acp_thread(cx);
        agent::stop_architect_run(&self.thread, acp_thread.as_ref(), cx);
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

    /// The step the run is carrying out, if one is. Only the leaf matters for
    /// highlighting, since the canvas shows one level at a time.
    pub(super) fn running_node<'a>(&self, cx: &'a App) -> Option<&'a NodeId> {
        self.thread
            .read(cx)
            .architect_run()?
            .current
            .as_ref()?
            .leaf()
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
