use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolCapability, ToolInput};

/// The id of the checklist this tool keeps. One per conversation: each update
/// replaces it, so the card above the message box always shows the latest.
pub const AGENT_PLAN_ID: &str = "praxis-agent-plan";

const MAX_PLAN_STEPS: usize = 50;
pub(super) const MAX_STEP_LENGTH: usize = 300;

/// Keep a short checklist of the steps in your current task. The user sees it
/// above the message box, on the desktop and on their phone.
///
/// Use it when a task takes several distinct steps or more than a few turns:
/// list the steps before you start, then update it as each one starts and
/// finishes, so the user can follow along without reading every message. Skip
/// it for quick, single-step requests.
///
/// - Send the whole checklist every time; it replaces the previous one. Send
///   an empty list to clear it.
/// - Keep at most one step `in_progress`, and mark a step `completed` as soon
///   as it is done rather than all at the end.
/// - Add, reword or drop steps as you learn more. Mark a step you abandon
///   `cancelled` instead of silently removing it.
/// - Write each step as a short, user-facing action, such as "Add the retry
///   setting" or "Run the agent tests".
///
/// When `run_steps` is available and several pending steps are independent of
/// each other, carry them out at the same time with it; it keeps this checklist
/// up to date for you.
///
/// The checklist and the Architect canvas are two views of one plan: the
/// checklist lists the plan's steps in the order they run. Steps you list here
/// appear on the canvas, and steps drafted or changed there appear here, so
/// always send the steps as the checklist currently shows them, reworded or
/// extended as needed. Steps the user has settled on the canvas stay even if
/// you leave them out. Use `draft_plan` when steps need branches, rules, file
/// assignments or a model of their own, or the user wants to run the plan.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct UpdatePlanToolInput {
    /// The complete checklist, in order. It replaces the previous one.
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct PlanStep {
    /// What this step does, as a short action.
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl PlanStepStatus {
    fn to_acp(self) -> acp::PlanEntryStatus {
        match self {
            Self::Pending => acp::PlanEntryStatus::Pending,
            Self::InProgress => acp::PlanEntryStatus::InProgress,
            Self::Completed => acp::PlanEntryStatus::Completed,
            Self::Cancelled => acp::PlanEntryStatus::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UpdatePlanToolOutput {
    Updated { completed: usize, total: usize },
    Error { error: String },
}

impl From<UpdatePlanToolOutput> for LanguageModelToolResultContent {
    fn from(output: UpdatePlanToolOutput) -> Self {
        match output {
            UpdatePlanToolOutput::Updated { total: 0, .. } => "Checklist cleared.".into(),
            UpdatePlanToolOutput::Updated { completed, total } => {
                format!("Checklist updated: {completed} of {total} steps done.").into()
            }
            UpdatePlanToolOutput::Error { error } => error.into(),
        }
    }
}

/// Shows the agent's own checklist for the task it is working on.
///
/// The checklist lives in the conversation, not the project, and is rendered
/// by the plan card that external agents' plans already use.
pub struct UpdatePlanTool {
    thread: WeakEntity<Thread>,
}

impl UpdatePlanTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

fn validate(steps: &[PlanStep]) -> Result<(), String> {
    if steps.len() > MAX_PLAN_STEPS {
        return Err(format!(
            "A checklist can have at most {MAX_PLAN_STEPS} steps; merge related steps."
        ));
    }
    if steps.iter().any(|step| step.step.trim().is_empty()) {
        return Err("Every step needs a description.".into());
    }
    if let Some(step) = steps
        .iter()
        .find(|step| step.step.chars().count() > MAX_STEP_LENGTH)
    {
        return Err(format!(
            "Keep each step under {MAX_STEP_LENGTH} characters; this one is too long: {:?}",
            step.step.chars().take(60).collect::<String>()
        ));
    }
    let in_progress = steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(format!(
            "{in_progress} steps are in_progress; keep at most one and mark the rest pending."
        ));
    }
    Ok(())
}

pub(crate) fn plan_entries(steps: &[PlanStep]) -> Vec<acp::PlanEntry> {
    steps
        .iter()
        .map(|step| {
            acp::PlanEntry::new(
                step.step.trim().to_string(),
                acp::PlanEntryPriority::Medium,
                step.status.to_acp(),
            )
        })
        .collect()
}

fn status_name(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "pending",
        PlanStepStatus::InProgress => "in_progress",
        PlanStepStatus::Completed => "completed",
        PlanStepStatus::Cancelled => "cancelled",
    }
}

/// The checklist as a compaction summary carries it, so the agent still has
/// it after the history that built it is gone.
pub(crate) fn checklist_for_summary(steps: &[PlanStep]) -> Option<String> {
    if steps.is_empty() {
        return None;
    }
    let lines = steps
        .iter()
        .map(|step| format!("- [{}] {}", status_name(step.status), step.step.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "## Checklist\n\nThe checklist you keep with `update_plan`, as it stood when the \
         conversation was compacted. The user still sees it, so keep updating it:\n\n{lines}"
    ))
}

fn is_open(step: &PlanStep) -> bool {
    matches!(
        step.status,
        PlanStepStatus::Pending | PlanStepStatus::InProgress
    )
}

/// Asks for the checklist to be brought up to date at the start of a turn,
/// when it still has steps to do.
pub(crate) fn checklist_review_note(steps: &[PlanStep]) -> Option<String> {
    if !steps.iter().any(is_open) {
        return None;
    }
    let lines = steps
        .iter()
        .map(|step| format!("- [{}] {}", status_name(step.status), step.step.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Checklist review: your checklist currently reads as below. If it no longer \
         matches your progress, update it with `update_plan` now, and keep it current \
         until this turn ends.\n{lines}"
    ))
}

/// Points an agent that keeps waiting at work it could do instead.
pub(crate) fn repeated_wait_note(waits: u32, steps: &[PlanStep]) -> String {
    let open = steps
        .iter()
        .filter(|step| is_open(step))
        .map(|step| format!("- {}", step.step.trim()))
        .collect::<Vec<_>>();
    let resume = "If nothing else can be done, end your turn instead of waiting again: \
                  Praxis resumes this conversation when a background task finishes.";
    if open.is_empty() {
        return format!("You have waited {waits} times in a row. {resume}");
    }
    format!(
        "You have waited {waits} times in a row. Work on a checklist step that does \
         not depend on what you are waiting for, or run independent steps side by side \
         with `run_steps` if it is available:\n{}\n{resume}",
        open.join("\n")
    )
}

fn summarize(steps: &[PlanStep]) -> UpdatePlanToolOutput {
    UpdatePlanToolOutput::Updated {
        completed: steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count(),
        total: steps.len(),
    }
}

impl AgentTool for UpdatePlanTool {
    type Input = UpdatePlanToolInput;
    type Output = UpdatePlanToolOutput;

    const NAME: &'static str = "update_plan";

    fn capability() -> ToolCapability {
        ToolCapability::ConversationMutation
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        let Ok(input) = input else {
            return "Update checklist".into();
        };
        if input.steps.is_empty() {
            return "Clear checklist".into();
        }
        let completed = input
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count();
        let total = input.steps.len();
        format!("Update checklist ({completed} of {total} done)").into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| UpdatePlanToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;
            validate(&input.steps).map_err(|error| UpdatePlanToolOutput::Error { error })?;
            // The plan may keep steps the checklist left out, so what is shown
            // and reported is the checklist as the plan now has it.
            let checklist = self
                .thread
                .update(cx, |thread, cx| {
                    thread.set_agent_checklist(&input.steps, cx)?;
                    Ok::<_, String>(thread.agent_checklist())
                })
                .map_err(|error| UpdatePlanToolOutput::Error {
                    error: format!("The conversation this checklist belongs to is gone: {error}"),
                })?
                .map_err(|error| UpdatePlanToolOutput::Error { error })?;
            event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&checklist));
            Ok(summarize(&checklist))
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        _output: Self::Output,
        _event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        // Reopening a conversation shows the checklist as the plan has it now,
        // not as each update in the history left it.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThreadEvent;
    use futures::StreamExt as _;
    use serde_json::json;

    fn step(text: &str, status: PlanStepStatus) -> PlanStep {
        PlanStep {
            step: text.into(),
            status,
        }
    }

    #[test]
    fn a_checklist_the_model_might_send_deserializes() {
        let input: UpdatePlanToolInput = serde_json::from_value(json!({
            "steps": [
                { "step": "Read the config loader", "status": "completed" },
                { "step": "Add the retry setting", "status": "in_progress" },
                { "step": "Run the tests", "status": "pending" },
                { "step": "Benchmark", "status": "cancelled" }
            ]
        }))
        .expect("checklist should deserialize");
        assert_eq!(input.steps.len(), 4);
        assert_eq!(input.steps[1].status, PlanStepStatus::InProgress);
        assert!(validate(&input.steps).is_ok());
    }

    #[test]
    fn more_than_one_step_in_progress_is_refused() {
        let steps = [
            step("First", PlanStepStatus::InProgress),
            step("Second", PlanStepStatus::InProgress),
        ];
        let error = validate(&steps).expect_err("two steps in progress");
        assert!(error.contains("keep at most one"), "{error}");
    }

    #[test]
    fn blank_and_oversized_steps_are_refused() {
        assert!(validate(&[step("  ", PlanStepStatus::Pending)]).is_err());
        let long = "x".repeat(MAX_STEP_LENGTH + 1);
        assert!(validate(&[step(&long, PlanStepStatus::Pending)]).is_err());
        let too_many: Vec<_> = (0..=MAX_PLAN_STEPS)
            .map(|index| step(&format!("Step {index}"), PlanStepStatus::Pending))
            .collect();
        assert!(validate(&too_many).is_err());
        assert!(validate(&[]).is_ok(), "an empty checklist clears it");
    }

    #[test]
    fn entries_keep_order_and_status() {
        let entries = plan_entries(&[
            step(" Read ", PlanStepStatus::Completed),
            step("Write", PlanStepStatus::InProgress),
            step("Drop", PlanStepStatus::Cancelled),
        ]);
        assert_eq!(entries[0].content, "Read");
        assert_eq!(entries[0].status, acp::PlanEntryStatus::Completed);
        assert_eq!(entries[1].status, acp::PlanEntryStatus::InProgress);
        assert_eq!(entries[2].status, acp::PlanEntryStatus::Cancelled);
    }

    fn checklist() -> UpdatePlanToolInput {
        UpdatePlanToolInput {
            steps: vec![
                step("Read the loader", PlanStepStatus::Completed),
                step("Add the setting", PlanStepStatus::InProgress),
            ],
        }
    }

    async fn next_plan(events: &mut crate::ToolCallEventStreamReceiver) -> acp::PlanItems {
        match events.next().await {
            Some(Ok(ThreadEvent::Plan(plan))) => plan,
            other => panic!("expected a checklist update, got {other:?}"),
        }
    }

    #[gpui::test]
    async fn running_shows_the_checklist(cx: &mut gpui::TestAppContext) {
        let (stream, mut events) = ToolCallEventStream::test();
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let tool = Arc::new(UpdatePlanTool::new(thread.downgrade()));
        let task = cx.update(|cx| tool.run(ToolInput::resolved(checklist()), stream, cx));
        let output = task.await.expect("a valid checklist");
        assert!(matches!(
            output,
            UpdatePlanToolOutput::Updated {
                completed: 1,
                total: 2,
            }
        ));

        let plan = next_plan(&mut events).await;
        assert_eq!(plan.plan_id, acp::PlanId::from(AGENT_PLAN_ID));
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].content, "Add the setting");
        assert_eq!(plan.entries[1].status, acp::PlanEntryStatus::InProgress);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.agent_checklist(), checklist().steps);
            // The checklist is the plan: its steps are on the canvas.
            let graph = thread.architect_graph().expect("the checklist made a plan");
            let titles: Vec<&str> = graph.nodes.iter().map(|node| node.title.as_str()).collect();
            assert_eq!(titles, ["Read the loader", "Add the setting"]);
            assert!(graph.is_single_line());
        });
    }

    /// A step changed on the canvas is changed in the checklist, and a step
    /// the user settled there stays when the agent leaves it out.
    #[gpui::test]
    async fn the_checklist_follows_the_plan(cx: &mut gpui::TestAppContext) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        let tool = Arc::new(UpdatePlanTool::new(thread.downgrade()));
        let (stream, _events) = ToolCallEventStream::test();
        cx.update(|cx| {
            tool.clone()
                .run(ToolInput::resolved(checklist()), stream, cx)
        })
        .await
        .expect("a valid checklist");

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(
                |graph| {
                    let node = &mut graph.nodes[1];
                    node.title = "Add the retry setting".into();
                    node.locked = true;
                },
                cx,
            );
        });
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.agent_checklist(),
                vec![
                    step("Read the loader", PlanStepStatus::Completed),
                    step("Add the retry setting", PlanStepStatus::InProgress),
                ],
                "a renamed step keeps its progress"
            );
        });

        let (stream, _events) = ToolCallEventStream::test();
        let input = UpdatePlanToolInput {
            steps: vec![
                step("Read the loader", PlanStepStatus::Completed),
                step("Write the docs", PlanStepStatus::Pending),
            ],
        };
        let output = cx
            .update(|cx| tool.run(ToolInput::resolved(input), stream, cx))
            .await
            .expect("a valid checklist");
        assert!(matches!(
            output,
            UpdatePlanToolOutput::Updated {
                completed: 1,
                total: 3,
            }
        ));
        thread.read_with(cx, |thread, _| {
            let steps: Vec<String> = thread
                .agent_checklist()
                .into_iter()
                .map(|step| step.step)
                .collect();
            assert_eq!(
                steps,
                ["Read the loader", "Write the docs", "Add the retry setting"]
            );
        });
    }

    #[test]
    fn notes_point_at_the_steps_still_to_do() {
        let mut steps = checklist().steps;
        steps.push(step("Write the docs", PlanStepStatus::Pending));
        let review = checklist_review_note(&steps).expect("open steps");
        assert!(
            review.contains("- [in_progress] Add the setting"),
            "{review}"
        );
        let done = [step("Read the loader", PlanStepStatus::Completed)];
        assert_eq!(checklist_review_note(&done), None);
        assert_eq!(checklist_review_note(&[]), None);

        let nudge = repeated_wait_note(2, &steps);
        assert!(nudge.contains("waited 2 times"), "{nudge}");
        assert!(nudge.contains("- Write the docs"), "{nudge}");
        assert!(!nudge.contains("Read the loader"), "{nudge}");
        assert!(nudge.contains("end your turn"), "{nudge}");
        assert!(repeated_wait_note(5, &done).contains("end your turn"));
    }

    #[test]
    fn the_checklist_reads_well_in_a_compaction_summary() {
        assert_eq!(checklist_for_summary(&[]), None);
        let summary = checklist_for_summary(&checklist().steps).expect("a checklist");
        assert!(summary.starts_with("## Checklist"), "{summary}");
        assert!(
            summary.ends_with("- [completed] Read the loader\n- [in_progress] Add the setting")
        );
    }

    /// Reopening a conversation shows the checklist as the plan has it now,
    /// so replaying an old update shows nothing of its own.
    #[gpui::test]
    async fn replay_leaves_the_checklist_to_the_plan(cx: &mut gpui::TestAppContext) {
        let (stream, mut events) = ToolCallEventStream::test();
        let accepted = UpdatePlanToolOutput::Updated {
            completed: 1,
            total: 2,
        };
        let tool = UpdatePlanTool::new(WeakEntity::new_invalid());
        cx.update(|cx| tool.replay(checklist(), accepted, stream, cx))
            .expect("replay");
        assert!(!matches!(events.try_recv(), Ok(Ok(ThreadEvent::Plan(_)))));
    }

    #[test]
    fn saved_output_replays_and_reads_well_to_the_model() {
        let saved = json!({ "completed": 1, "total": 3 });
        let output: UpdatePlanToolOutput = serde_json::from_value(saved).expect("saved output");
        let LanguageModelToolResultContent::Text(text) = output.into() else {
            panic!("expected text");
        };
        assert_eq!(text.as_ref(), "Checklist updated: 1 of 3 steps done.");
        let saved = json!({ "error": "bad" });
        let error: UpdatePlanToolOutput = serde_json::from_value(saved).expect("saved error");
        assert!(matches!(error, UpdatePlanToolOutput::Error { .. }));
    }
}
