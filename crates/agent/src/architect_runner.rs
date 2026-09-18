use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema::v1 as acp;
use architect::{Decision, PlanRun, RunOutcome, RunRefusal};
use gpui::{App, AsyncApp, Entity, SharedString};

use crate::{ArchitectRun, SessionMode, Thread};

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
        }
    }
}

impl std::error::Error for ArchitectRunStartError {}

impl From<RunRefusal> for ArchitectRunStartError {
    fn from(refusal: RunRefusal) -> Self {
        Self::Refused(refusal)
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
    mut graph: architect::ArchitectGraph,
    cx: &mut App,
) -> Result<(), ArchitectRunStartError> {
    if thread
        .read(cx)
        .architect_run()
        .is_some_and(ArchitectRun::is_running)
    {
        return Err(ArchitectRunStartError::AlreadyRunning);
    }

    let mut plan_run = PlanRun::start(&graph)?;
    let first_step = plan_run.current();
    let first_title = step_title(&graph, &first_step);

    // A run must not inherit summaries from an earlier attempt. Clear both the
    // immutable run snapshot and the live graph that complete_step writes into.
    graph.clear_results();
    thread.update(cx, |thread, cx| {
        thread.update_architect_graph(|graph| graph.clear_results(), cx);
        thread.set_session_mode(SessionMode::Build, cx);
    });

    // Weak, because the thread owns the task. Keeping a strong entity here
    // would form a cycle that keeps the thread alive forever.
    let weak_thread = thread.downgrade();
    let first_step_for_run = first_step.clone();
    let task = cx.spawn(async move |cx| {
        let mut decision = Decision::Run(first_step_for_run);
        let outcome = loop {
            match decision {
                Decision::Run(node) => {
                    let step_number = plan_run.steps_taken();
                    let attempt = node.leaf().map(|id| plan_run.attempt(id)).unwrap_or(1);
                    let title = step_title(&graph, &node);
                    weak_thread
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

                    let prompt = architect::step_prompt(&graph, &node, step_number, attempt);
                    let sent = send_and_wait(&acp_thread, prompt, cx).await;
                    weak_thread
                        .update(cx, |thread, _cx| thread.clear_architect_step_visit())
                        .ok();
                    if let Err(error) = sent {
                        log::error!("Architect: step \"{node}\" could not run: {error}");
                        break RunOutcome::Cancelled;
                    }

                    let reported = weak_thread
                        .read_with(cx, |thread, _cx| {
                            thread
                                .architect_graph()
                                .and_then(|graph| graph.node_at(&node))
                                .and_then(|step| step.result.as_ref())
                                .filter(|result| result.attempt == attempt)
                                .map(|result| result.summary.trim().to_string())
                                .filter(|summary| !summary.is_empty())
                        })
                        .ok()
                        .flatten();
                    let summary = match reported {
                        Some(summary) => summary,
                        None => {
                            log::warn!(
                                "Architect: {node} ended without calling complete_step; using its \
                                 closing message as the summary"
                            );
                            let summary = acp_thread
                                .read_with(cx, |thread, cx| last_assistant_text(thread, cx))
                                .trim()
                                .to_string();
                            let path = node.clone();
                            let recorded = summary.clone();
                            weak_thread
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

                    let reported: SharedString = summary.clone().into();
                    weak_thread
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

                    let reply =
                        acp_thread.read_with(cx, |thread, cx| last_assistant_text(thread, cx));
                    let taken = architect::parse_verdict(&reply).unwrap_or_else(|| {
                        log::warn!(
                            "Architect: no YES or NO in the reply deciding {}; treating it as no",
                            branch.edge.0
                        );
                        false
                    });
                    decision = plan_run.answer(&graph, taken);
                }
                Decision::Done(outcome) => break outcome,
            }
        };

        if !outcome.is_success()
            && outcome != RunOutcome::Cancelled
            && let Err(error) = send_and_wait(&acp_thread, outcome.describe(&graph), cx).await
        {
            log::error!("Architect: could not report how the run ended: {error}");
        }

        weak_thread
            .update(cx, |thread, cx| {
                thread.finish_architect_run(outcome, cx);
            })
            .ok();
    });

    thread.update(cx, |thread, cx| {
        thread.start_architect_run(first_step, first_title, task, cx);
    });
    Ok(())
}

/// Cancels both the in-flight conversation turn and the task owned by the
/// thread. Either entity may already be gone while a window is closing.
pub fn stop_architect_run(
    thread: &Entity<Thread>,
    acp_thread: Option<&Entity<AcpThread>>,
    cx: &mut App,
) {
    if let Some(acp_thread) = acp_thread {
        acp_thread
            .update(cx, |thread, cx| thread.cancel(cx))
            .detach();
    }
    thread.update(cx, |thread, cx| thread.stop_architect_run(cx));
}

fn step_title(graph: &architect::ArchitectGraph, path: &architect::NodePath) -> SharedString {
    graph
        .node_at(path)
        .map(|node| SharedString::from(node.title.clone()))
        .or_else(|| path.leaf().map(|id| SharedString::from(id.0.clone())))
        .unwrap_or_else(|| SharedString::from("Step"))
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
