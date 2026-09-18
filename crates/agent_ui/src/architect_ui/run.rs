use std::cell::Cell;

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use architect::{Decision, NodeId, NodePath, PlanRun, RunOutcome, RunRefusal};
use gpui::{App, AsyncApp, Context, Entity, SharedString, Window};

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
    /// Drives the plan step by step, deciding at every branch which way to go.
    ///
    /// The alternative, compiling the whole plan into one spec (see
    /// `architect::compile_spec`), leaves the control flow to the model: it is
    /// shown the loops and conditions and trusted to honour them. A model that
    /// decides it has done enough will quietly leave a retry loop early, and
    /// nothing catches that. Here the position in the graph is held outside the
    /// model. It is told about one step at a time, and every condition is put to
    /// it as a question on its own whose answer is read back and acted on here.
    ///
    /// The graph is copied when the run starts, so a run carries out the plan as
    /// it was when the user pressed Run. A step's own chat can still rewrite
    /// routing while the run is in flight, and having the ground shift underneath
    /// a half-finished run would make it impossible to say afterwards what was
    /// actually carried out.
    pub(super) fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running(cx) {
            return;
        }
        let run_starting = RunStartingGuard::new(&self.run_starting);

        // A run always covers the whole plan, even when started from inside a
        // nested one: what is on screen is a viewpoint, not a scope.
        let Some(mut graph) = self.root_graph(cx).cloned() else {
            return;
        };

        let mut plan_run = match PlanRun::start(&graph) {
            Ok(plan_run) => plan_run,
            Err(refusal) => {
                self.report(
                    match &refusal {
                        RunRefusal::NothingToRun => "There is no plan to run yet.".to_string(),
                        RunRefusal::NotReady(_) => {
                            format!("This plan is not ready to run: {refusal}")
                        }
                    },
                    cx,
                );
                return;
            }
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

        // A run must not inherit summaries from an earlier attempt. Otherwise
        // a skipped branch, or a step that forgets complete_step, can hand stale
        // work to its successor. Clear both the immutable run snapshot and the
        // live graph that complete_step writes into.
        graph.clear_results();
        // Deliberation is over: the steps about to run have to be able to
        // change the project.
        self.thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| graph.clear_results(), cx);
            thread.set_session_mode(agent::SessionMode::Build, cx);
        });

        let first_step = plan_run.current();
        // The run reports to the thread, not to this view. A run outlives the
        // canvas — that is the point of it living on the thread — so anything it
        // records through the canvas is silently lost the moment the canvas is
        // closed. That left the step counter frozen, `complete_step` with nowhere
        // to write, and the run never marked finished, so the panel offered to
        // stop a run that had ended.
        //
        // Weak, because the thread owns the task: a strong handle here would be a
        // cycle that keeps the thread alive forever.
        let thread = self.thread.downgrade();
        let task = cx.spawn({
            let first_step = first_step.clone();
            // The canvas is deliberately unused in here: see `thread` above.
            async move |_this, cx| {
                let mut decision = Decision::Run(first_step);
                let outcome = loop {
                    match decision {
                        Decision::Run(node) => {
                            let step_number = plan_run.steps_taken();
                            let attempt = node.leaf().map(|id| plan_run.attempt(id)).unwrap_or(1);
                            // Read from the run's own copy of the plan rather than
                            // the live one, so progress is named as the run sees it.
                            let title: SharedString = graph
                                .node_at(&node)
                                .map(|step| SharedString::from(step.title.clone()))
                                .or_else(|| node.leaf().map(|id| SharedString::from(id.0.clone())))
                                .unwrap_or_else(|| SharedString::from("Step"));
                            // Telling the thread which step is running is what lets
                            // `complete_step` write its summary onto the right one.
                            thread
                                .update(cx, |thread, cx| {
                                    thread.note_architect_run_position(
                                        node.clone(),
                                        title,
                                        step_number,
                                        attempt,
                                        cx,
                                    );
                                })
                                .ok();

                            let prompt =
                                architect::step_prompt(&graph, &node, step_number, attempt);
                            let sent = send_and_wait(&acp_thread, prompt, cx).await;
                            thread
                                .update(cx, |thread, _cx| thread.clear_architect_step_visit())
                                .ok();
                            if let Err(error) = sent {
                                log::error!("Architect: step \"{node}\" could not run: {error}");
                                break RunOutcome::Cancelled;
                            }

                            // What the step reported is all the steps after it will
                            // be told, so it is recorded before the run moves on. A
                            // summary written by `complete_step` is preferred: it
                            // was authored as a handover. Falling back to the last
                            // thing said keeps a run going when the model forgets to
                            // call the tool, at the cost of a vaguer handover.
                            let reported = thread
                                .read_with(cx, |thread, _cx| {
                                    thread
                                        .architect_graph()
                                        .and_then(|graph| graph.node_at(&node))
                                        .and_then(|step| step.result.as_ref())
                                        .map(|result| result.summary.trim().to_string())
                                        .filter(|summary| !summary.is_empty())
                                })
                                .ok()
                                .flatten();
                            let summary = match reported {
                                Some(summary) => summary,
                                None => {
                                    log::warn!(
                                        "Architect: {node} ended without calling complete_step; \
                                         using its closing message as the summary"
                                    );
                                    let scraped = acp_thread.read_with(cx, |thread, cx| {
                                        last_assistant_text(thread, cx)
                                    });
                                    let summary = scraped.trim().to_string();
                                    let path = node.clone();
                                    let recorded = summary.clone();
                                    thread
                                        .update(cx, |thread, cx| {
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
                                        })
                                        .ok();
                                    summary
                                }
                            };
                            // Closes the step off in the run's own record, so the
                            // timeline stops it counting up and can show what it
                            // handed on.
                            let reported: SharedString = summary.clone().into();
                            thread
                                .update(cx, |thread, cx| {
                                    thread.finish_architect_run_step(Some(reported), cx);
                                })
                                .ok();

                            if let Some(step) = graph.node_at_mut(&node) {
                                step.result = Some(architect::StepResult { summary, attempt });
                            }

                            decision = plan_run.finish_step(&graph);
                        }
                        Decision::Ask(branch) => {
                            let prompt = architect::branch_prompt(&graph, &branch);
                            if let Err(error) = send_and_wait(&acp_thread, prompt, cx).await {
                                log::error!("Architect: a branch could not be decided: {error}");
                                break RunOutcome::Cancelled;
                            }

                            let reply = acp_thread
                                .read_with(cx, |thread, cx| last_assistant_text(thread, cx));

                            // An unreadable answer is treated as "no". Taking the
                            // branch anyway is how a retry loop becomes endless, and
                            // not taking it fails towards a run that stops early
                            // and visibly rather than one that never stops.
                            let taken = architect::parse_verdict(&reply).unwrap_or_else(|| {
                                log::warn!(
                                    "Architect: no YES or NO in the reply deciding {}; treating it as \
                                     no",
                                    branch.edge.0
                                );
                                false
                            });
                            decision = plan_run.answer(&graph, taken);
                        }
                        Decision::Done(outcome) => break outcome,
                    }
                };

                // A run that ended badly did so because of something the model kept
                // doing, so it is told; a run that simply finished has nothing to
                // add that another turn would be worth paying for.
                if !outcome.is_success()
                    && outcome != RunOutcome::Cancelled
                    && let Err(error) =
                        send_and_wait(&acp_thread, outcome.describe(&graph), cx).await
                {
                    log::error!("Architect: could not report how the run ended: {error}");
                }

                thread
                    .update(cx, |thread, cx| {
                        thread.finish_architect_run(outcome, cx);
                    })
                    .ok();
            }
        });

        let first_title = self.step_title(&first_step, cx);
        self.thread.update(cx, |thread, cx| {
            thread.start_architect_run(first_step, first_title, task, cx);
        });
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
        // Dropping the task ends the run at its next await point, but the turn
        // already in flight belongs to the thread and has to be told separately.
        if let Some(acp_thread) = self.plan_acp_thread(cx) {
            acp_thread
                .update(cx, |thread, cx| thread.cancel(cx))
                .detach();
        }
        self.thread
            .update(cx, |thread, cx| thread.stop_architect_run(cx));
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

/// Sends a prompt to a conversation and waits for the whole turn to finish.
///
/// This is the same path a typed message takes, so a run is subject to the tool
/// permissions, cancellation and rendering that any other turn is.
async fn send_and_wait(
    thread: &Entity<AcpThread>,
    prompt: String,
    cx: &mut AsyncApp,
) -> Result<()> {
    let send = thread.update(cx, |thread, cx| {
        thread.send(
            vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
            cx,
        )
    });
    send.await?;
    Ok(())
}

/// The text of the most recent thing the agent said.
///
/// Reasoning is left out: a model thinking through both answers before settling
/// on one would otherwise have its thinking read as the verdict.
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
