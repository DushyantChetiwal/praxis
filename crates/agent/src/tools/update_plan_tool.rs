use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolCapability, ToolInput};

/// The id of the checklist this tool keeps. One per conversation: each update
/// replaces it, so the card above the message box always shows the latest.
pub const AGENT_PLAN_ID: &str = "praxis-agent-plan";

const MAX_PLAN_STEPS: usize = 50;
const MAX_STEP_LENGTH: usize = 300;

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
/// This checklist is separate from the Architect canvas. Use `draft_plan` only
/// when the user wants to review, reshape or run a plan as a flowchart.
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
pub struct UpdatePlanTool;

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

fn plan_entries(steps: &[PlanStep]) -> Vec<acp::PlanEntry> {
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
        cx.spawn(async move |_cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| UpdatePlanToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;
            validate(&input.steps).map_err(|error| UpdatePlanToolOutput::Error { error })?;
            event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&input.steps));
            Ok(summarize(&input.steps))
        })
    }

    fn replay(
        &self,
        input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        // Replaying every update in order leaves the card showing the latest
        // checklist, as it was when the conversation was saved.
        if matches!(output, UpdatePlanToolOutput::Updated { .. }) {
            event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&input.steps));
        }
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
        let task = cx.update(|cx| {
            Arc::new(UpdatePlanTool).run(ToolInput::resolved(checklist()), stream, cx)
        });
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
    }

    /// Reopening a conversation replays its tool calls, which has to bring the
    /// checklist back, but a refused update never showed one.
    #[gpui::test]
    async fn replay_restores_only_accepted_checklists(cx: &mut gpui::TestAppContext) {
        let (stream, mut events) = ToolCallEventStream::test();
        let accepted = UpdatePlanToolOutput::Updated {
            completed: 1,
            total: 2,
        };
        cx.update(|cx| UpdatePlanTool.replay(checklist(), accepted, stream, cx))
            .expect("replay");
        assert_eq!(next_plan(&mut events).await.entries.len(), 2);

        let (stream, mut events) = ToolCallEventStream::test();
        let refused = UpdatePlanToolOutput::Error {
            error: "refused".into(),
        };
        cx.update(|cx| UpdatePlanTool.replay(checklist(), refused, stream, cx))
            .expect("replay");
        assert!(
            !matches!(events.try_recv(), Ok(Ok(ThreadEvent::Plan(_)))),
            "a refused update shows nothing"
        );
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
