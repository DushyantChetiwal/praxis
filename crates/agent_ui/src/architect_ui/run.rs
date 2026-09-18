use std::cell::Cell;

use architect::{NodeId, NodePath};
use gpui::{Context, SharedString, Window};

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
    pub(super) fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

        // Everything the run says belongs in the conversation that owns the
        // plan, not in whichever step's chat happens to be on screen.
        self.show_plan_chat(window, cx);
        if let Err(error) = agent::start_architect_run(self.thread.clone(), acp_thread, graph, cx) {
            self.report(error.to_string(), cx);
            return;
        }

        drop(run_starting);
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
        self.run_starting.set(false);
        cx.notify();
    }

    pub(super) fn is_running(&self, cx: &Context<Self>) -> bool {
        self.run_starting.get()
            || self
                .thread
                .read(cx)
                .architect_run()
                .is_some_and(agent::ArchitectRun::is_running)
    }

    /// The step the run is carrying out, if one is. Only the leaf matters for
    /// highlighting, since the canvas shows one level at a time.
    pub(super) fn running_node<'a>(&self, cx: &'a Context<Self>) -> Option<&'a NodeId> {
        self.thread
            .read(cx)
            .architect_run()?
            .current
            .as_ref()?
            .leaf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
