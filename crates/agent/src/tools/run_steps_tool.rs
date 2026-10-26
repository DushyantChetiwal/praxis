use acp_thread::{
    ParallelStep as StepProgress, ParallelStepStatus, SubagentSessionInfo, parallel_steps_meta,
};
use agent_client_protocol::schema::v2 as acp_v2;
use anyhow::Result;
use collections::HashMap;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::{LanguageModelToolResultContent, LanguageModelToolUseInput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use util::ResultExt as _;

use super::update_plan_tool::{MAX_STEP_LENGTH, plan_entries};
use crate::{
    AGENT_PLAN_ID, AgentMessage, AgentMessageContent, AgentTool, PlanStep, PlanStepStatus, Thread,
    ThreadEnvironment, ToolCallEventStream, ToolCapability, ToolInput, UpdatePlanTool,
    UpdatePlanToolInput,
};

const MIN_PARALLEL_STEPS: usize = 2;
const MAX_PARALLEL_STEPS: usize = 6;

/// Carry out several independent steps of your checklist at the same time, one
/// agent per step, and wait until all of them finish.
///
/// Use it when two or more pending steps do not depend on each other, such as
/// changes to separate parts of the codebase or independent investigations.
/// Keep steps that build on each other in order and do those yourself.
///
/// - Name each step as it appears in your checklist. The checklist marks the
///   steps in progress while they run and completed when they finish, so you
///   do not need to call `update_plan` for them. A step that is not in the
///   checklist yet is added.
/// - The agents do not see this conversation. Give each one everything it
///   needs in `instructions`: the goal, the relevant files, and constraints.
/// - You do not have to work out upfront which files each step touches. The
///   first step to change a file keeps it; another step that tries to change
///   the same file is refused and reports what it could not do, so you can
///   finish that part yourself afterwards.
/// - You receive each agent's final summary, the files it changed, and its
///   session_id, which `spawn_agent` can use to follow up with that agent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct RunStepsToolInput {
    /// The independent steps to run at the same time, from 2 to 6 of them.
    pub steps: Vec<ParallelStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ParallelStep {
    /// The checklist step this carries out, worded as it is in the checklist.
    pub step: String,
    /// Everything the agent carrying out this step needs to know, because it
    /// does not see this conversation.
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StepOutcome {
    pub step: String,
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<acp_v2::SessionId>,
    pub output: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunStepsToolOutput {
    Finished {
        steps: Vec<StepOutcome>,
        checklist: Vec<PlanStep>,
    },
    Error {
        error: String,
    },
}

impl From<RunStepsToolOutput> for LanguageModelToolResultContent {
    fn from(output: RunStepsToolOutput) -> Self {
        match output {
            RunStepsToolOutput::Finished { steps, .. } => {
                let summary = serde_json::json!({ "steps": steps });
                serde_json::to_string(&summary)
                    .unwrap_or_else(|error| format!("Failed to serialize run_steps: {error}"))
                    .into()
            }
            RunStepsToolOutput::Error { error } => error.into(),
        }
    }
}

#[derive(Debug)]
struct ClaimedFile {
    step: String,
    path: PathBuf,
}

/// Which step first changed each file while steps run side by side.
#[derive(Clone, Debug, Default)]
pub struct FileClaims(Rc<RefCell<HashMap<String, ClaimedFile>>>);

impl FileClaims {
    pub fn claim_for(&self, step: &str) -> FileClaim {
        FileClaim {
            claims: self.clone(),
            step: step.to_string(),
        }
    }

    pub fn files_for(&self, step: &str) -> Vec<String> {
        let mut files: Vec<String> = self
            .0
            .borrow()
            .values()
            .filter(|file| file.step == step)
            .map(|file| file.path.to_string_lossy().into_owned())
            .collect();
        files.sort();
        files
    }
}

/// One step's share of the [`FileClaims`] for a set of parallel steps.
#[derive(Clone, Debug)]
pub struct FileClaim {
    claims: FileClaims,
    step: String,
}

impl FileClaim {
    pub fn claim(&self, path: &Path) -> Result<(), String> {
        // Compared without case, so the same file cannot slip through on a
        // case-insensitive filesystem under two spellings.
        let key = path.to_string_lossy().to_lowercase();
        let mut claims = self.claims.0.borrow_mut();
        match claims.get(&key) {
            Some(owner) if owner.step != self.step => Err(format!(
                "`{}` is being changed by the parallel step \"{}\", so this step cannot change \
                 it. Leave that change, finish the rest of this step, and say in your summary \
                 what you could not change.",
                path.display(),
                owner.step
            )),
            Some(_) => Ok(()),
            None => {
                claims.insert(
                    key,
                    ClaimedFile {
                        step: self.step.clone(),
                        path: path.to_path_buf(),
                    },
                );
                Ok(())
            }
        }
    }
}

/// The checklist as the agent last left it: the newest accepted `update_plan`
/// or `run_steps` call, or nothing.
pub(crate) fn latest_checklist<'a>(
    messages: impl IntoIterator<Item = &'a AgentMessage>,
) -> Vec<PlanStep> {
    for message in messages {
        for content in message.content.iter().rev() {
            let AgentMessageContent::ToolUse(tool_use) = content else {
                continue;
            };
            let Some(result) = message.tool_results.get(&tool_use.id) else {
                continue;
            };
            if result.is_error {
                continue;
            }
            let checklist = match tool_use.name.as_ref() {
                UpdatePlanTool::NAME => {
                    let input = match &tool_use.input {
                        LanguageModelToolUseInput::Json(value) => {
                            serde_json::from_value::<UpdatePlanToolInput>(value.clone()).ok()
                        }
                        LanguageModelToolUseInput::Text(text) => serde_json::from_str(text).ok(),
                    };
                    input.map(|input| input.steps)
                }
                RunStepsTool::NAME => result
                    .output
                    .clone()
                    .and_then(|output| serde_json::from_value(output).ok())
                    .and_then(|output| match output {
                        RunStepsToolOutput::Finished { checklist, .. } => Some(checklist),
                        RunStepsToolOutput::Error { .. } => None,
                    }),
                _ => None,
            };
            if let Some(checklist) = checklist {
                return checklist;
            }
        }
    }
    Vec::new()
}

fn validate(steps: &[ParallelStep]) -> Result<(), String> {
    if steps.len() < MIN_PARALLEL_STEPS || steps.len() > MAX_PARALLEL_STEPS {
        return Err(format!(
            "Run from {MIN_PARALLEL_STEPS} to {MAX_PARALLEL_STEPS} steps at a time; do a single \
             step yourself."
        ));
    }
    for (index, step) in steps.iter().enumerate() {
        let name = step.step.trim();
        if name.is_empty() || step.instructions.trim().is_empty() {
            return Err("Every step needs a name and instructions.".into());
        }
        if name.chars().count() > MAX_STEP_LENGTH {
            return Err(format!(
                "Keep each step name under {MAX_STEP_LENGTH} characters."
            ));
        }
        let repeated = steps
            .iter()
            .skip(index + 1)
            .any(|other| other.step.trim().eq_ignore_ascii_case(name));
        if repeated {
            return Err(format!("The step {name:?} is listed more than once."));
        }
    }
    Ok(())
}

fn set_status(checklist: &mut Vec<PlanStep>, step: &str, status: PlanStepStatus) {
    match checklist.iter_mut().find(|entry| entry.step.trim() == step) {
        Some(entry) => entry.status = status,
        None => checklist.push(PlanStep {
            step: step.to_string(),
            status,
        }),
    }
}

fn step_message(step: &str, instructions: &str, all_steps: &[String]) -> String {
    let others = all_steps
        .iter()
        .filter(|other| other.as_str() != step)
        .map(|other| format!("- {other}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are carrying out one step of a larger task. Other agents are carrying out the \
         other steps at the same time.\n\n\
         Your step: {step}\n\n\
         {instructions}\n\n\
         Steps running alongside yours:\n{others}\n\n\
         If another step has already changed a file you need, your edit is refused. Leave that \
         change, finish the rest of your step, and say what you could not do. Change files only \
         with the file editing tools, never with terminal commands, so overlapping edits are \
         caught. End with a short summary of what you did."
    )
}

/// Each step's progress, as the conversation shows it, once the steps have
/// finished.
fn finished_progress(steps: &[StepOutcome]) -> Vec<StepProgress> {
    steps
        .iter()
        .map(|outcome| StepProgress {
            step: outcome.step.clone(),
            status: if outcome.completed {
                ParallelStepStatus::Done
            } else {
                ParallelStepStatus::NotFinished
            },
            // Every step runs in a new agent, so its whole conversation is the
            // step's.
            session: outcome
                .session_id
                .clone()
                .map(|session_id| SubagentSessionInfo {
                    session_id,
                    message_start_index: 0,
                    message_end_index: None,
                }),
            error: (!outcome.completed).then(|| outcome.output.clone()),
        })
        .collect()
}

fn outcome_text(steps: &[StepOutcome]) -> String {
    steps
        .iter()
        .map(|outcome| {
            let status = if outcome.completed {
                "Done"
            } else {
                "Not finished"
            };
            format!("{status}: {}\n\n{}", outcome.step, outcome.output)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Runs independent checklist steps side by side, one subagent per step.
pub struct RunStepsTool {
    thread: WeakEntity<Thread>,
    environment: Rc<dyn ThreadEnvironment>,
}

impl RunStepsTool {
    pub fn new(thread: WeakEntity<Thread>, environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self {
            thread,
            environment,
        }
    }

    fn store_checklist(&self, checklist: &[PlanStep], cx: &mut gpui::AsyncApp) {
        self.thread
            .update(cx, |thread, _| {
                thread.set_agent_checklist(checklist.to_vec())
            })
            .log_err();
    }
}

impl AgentTool for RunStepsTool {
    type Input = RunStepsToolInput;
    type Output = RunStepsToolOutput;

    const NAME: &'static str = "run_steps";

    fn capability() -> ToolCapability {
        ToolCapability::ArbitraryExecution
    }

    fn kind() -> acp_v2::ToolKind {
        acp_v2::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Run {} steps in parallel", input.steps.len()).into(),
            Err(_) => "Run steps in parallel".into(),
        }
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
                .map_err(|error| RunStepsToolOutput::Error {
                    error: format!("Failed to receive tool input: {error}"),
                })?;
            validate(&input.steps).map_err(|error| RunStepsToolOutput::Error { error })?;
            let mut checklist = self
                .thread
                .read_with(cx, |thread, _| thread.agent_checklist().to_vec())
                .map_err(|error| RunStepsToolOutput::Error {
                    error: format!("The conversation these steps belong to is gone: {error}"),
                })?;
            let names: Vec<String> = input
                .steps
                .iter()
                .map(|step| step.step.trim().to_string())
                .collect();
            for name in &names {
                set_status(&mut checklist, name, PlanStepStatus::InProgress);
            }
            self.store_checklist(&checklist, cx);
            event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&checklist));

            // Each step's progress is reported on this call as it changes, so
            // the conversation can show every step's agent at work instead of
            // a single call that says nothing until all of them finish.
            let mut progress: Vec<StepProgress> = names
                .iter()
                .map(|name| StepProgress {
                    step: name.clone(),
                    status: ParallelStepStatus::Running,
                    session: None,
                    error: None,
                })
                .collect();
            let claims = FileClaims::default();
            let mut running = FuturesUnordered::new();
            for (index, (name, step)) in names.iter().zip(&input.steps).enumerate() {
                let message = step_message(name, &step.instructions, &names);
                let created = cx.update(|cx| {
                    let subagent = self.environment.create_subagent(name.clone(), None, cx)?;
                    subagent.set_file_claim(claims.claim_for(name), cx);
                    progress[index].session = Some(SubagentSessionInfo {
                        session_id: subagent.id(),
                        message_start_index: subagent.num_entries(cx),
                        message_end_index: None,
                    });
                    event_stream.subagent_spawned(subagent.id());
                    anyhow::Ok(subagent)
                });
                let started = created.map(|subagent| {
                    let send = subagent.send(message, cx);
                    (subagent, send)
                });
                running.push(async move {
                    match started {
                        Ok((subagent, send)) => (index, Some(subagent), send.await),
                        Err(error) => (index, None, Err(error)),
                    }
                });
            }
            event_stream.update_fields(|update| update.meta(parallel_steps_meta(&progress)));

            let mut results: Vec<Option<(Option<acp_v2::SessionId>, Result<String, String>)>> =
                vec![None; names.len()];
            while let Some((index, subagent, result)) = running.next().await {
                let name = &names[index];
                let result = result.map_err(|error| format!("{error:#}"));
                let step = &mut progress[index];
                if let (Some(subagent), Some(session)) = (&subagent, step.session.as_mut()) {
                    session.message_end_index =
                        cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));
                }
                let status = match &result {
                    Ok(_) => {
                        step.status = ParallelStepStatus::Done;
                        PlanStepStatus::Completed
                    }
                    Err(error) => {
                        step.status = ParallelStepStatus::NotFinished;
                        step.error = Some(error.clone());
                        PlanStepStatus::Pending
                    }
                };
                event_stream.update_fields(|update| update.meta(parallel_steps_meta(&progress)));
                set_status(&mut checklist, name, status);
                self.store_checklist(&checklist, cx);
                event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&checklist));
                results[index] = Some((subagent.map(|subagent| subagent.id()), result));
            }

            let outcomes: Vec<StepOutcome> = names
                .into_iter()
                .zip(results)
                .map(|(name, result)| {
                    let (session_id, result) =
                        result.unwrap_or_else(|| (None, Err("The step did not run.".into())));
                    let (completed, output) = match result {
                        Ok(output) => (true, output),
                        Err(error) => (false, error),
                    };
                    StepOutcome {
                        changed_files: claims.files_for(&name),
                        step: name,
                        completed,
                        session_id,
                        output,
                    }
                })
                .collect();
            let text = outcome_text(&outcomes);
            event_stream.update_fields(|update| update.content(vec![text.into()]));
            Ok(RunStepsToolOutput::Finished {
                steps: outcomes,
                checklist,
            })
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        if let RunStepsToolOutput::Finished { steps, checklist } = output {
            event_stream.update_plan(AGENT_PLAN_ID, plan_entries(&checklist));
            let text = outcome_text(&steps);
            let progress = finished_progress(&steps);
            event_stream.update_fields(|update| {
                update
                    .content(vec![text.into()])
                    .meta(parallel_steps_meta(&progress))
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SubagentHandle, ThreadEvent};
    use acp_thread::AgentModelId;
    use gpui::AsyncApp;
    use language_model::{LanguageModelToolResult, LanguageModelToolUse};
    use serde_json::json;

    fn parallel(step: &str) -> ParallelStep {
        ParallelStep {
            step: step.into(),
            instructions: format!("Instructions for {step}"),
        }
    }

    #[test]
    fn steps_need_names_instructions_and_company() {
        assert!(validate(&[parallel("Alone")]).is_err());
        assert!(validate(&[parallel("One"), parallel("Two")]).is_ok());
        let too_many: Vec<_> = (0..=MAX_PARALLEL_STEPS)
            .map(|index| parallel(&format!("Step {index}")))
            .collect();
        assert!(validate(&too_many).is_err());
        let error = validate(&[parallel("Same"), parallel(" same ")]).expect_err("repeated");
        assert!(error.contains("more than once"), "{error}");
        let mut blank = parallel("Blank");
        blank.instructions = "  ".into();
        assert!(validate(&[blank, parallel("Other")]).is_err());
    }

    #[test]
    fn the_first_step_to_change_a_file_keeps_it() {
        let claims = FileClaims::default();
        let first = claims.claim_for("First");
        let second = claims.claim_for("Second");
        assert!(first.claim(Path::new("/project/src/lib.rs")).is_ok());
        assert!(first.claim(Path::new("/project/src/lib.rs")).is_ok());
        let error = second
            .claim(Path::new("/project/SRC/lib.rs"))
            .expect_err("the file belongs to the first step");
        assert!(error.contains("\"First\""), "{error}");
        assert!(second.claim(Path::new("/project/src/main.rs")).is_ok());
        assert_eq!(claims.files_for("First"), vec!["/project/src/lib.rs"]);
        assert_eq!(claims.files_for("Second"), vec!["/project/src/main.rs"]);
    }

    fn tool_call(
        name: &str,
        input: serde_json::Value,
        output: Option<serde_json::Value>,
        is_error: bool,
    ) -> AgentMessage {
        let id: language_model::LanguageModelToolUseId = format!("{name}-call").into();
        let tool_use = LanguageModelToolUse {
            id: id.clone(),
            name: name.into(),
            raw_input: input.to_string(),
            input: LanguageModelToolUseInput::Json(input),
            is_input_complete: true,
            thought_signature: None,
        };
        let result = LanguageModelToolResult {
            tool_use_id: id.clone(),
            tool_name: name.into(),
            is_error,
            content: Vec::new(),
            output,
        };
        AgentMessage {
            content: vec![AgentMessageContent::ToolUse(tool_use)],
            tool_results: [(id, result)].into_iter().collect(),
            reasoning_details: None,
        }
    }

    #[test]
    fn the_checklist_comes_from_the_newest_accepted_call() {
        let older = tool_call(
            "update_plan",
            json!({"steps": [{"step": "Old", "status": "pending"}]}),
            Some(json!({"completed": 0, "total": 1})),
            false,
        );
        let refused = tool_call(
            "update_plan",
            json!({"steps": [{"step": "Refused", "status": "pending"}]}),
            None,
            true,
        );
        let ran = tool_call(
            "run_steps",
            json!({}),
            Some(json!({"steps": [], "checklist": [{"step": "Ran", "status": "completed"}]})),
            false,
        );
        assert!(latest_checklist([&refused]).is_empty());
        assert_eq!(latest_checklist([&refused, &older])[0].step, "Old");
        assert_eq!(latest_checklist([&ran, &older])[0].step, "Ran");
    }

    struct FakeStep {
        label: String,
        claim: RefCell<Option<FileClaim>>,
    }

    impl SubagentHandle for FakeStep {
        fn id(&self) -> acp_v2::SessionId {
            acp_v2::SessionId::new(format!("session-{}", self.label))
        }

        fn num_entries(&self, _cx: &App) -> usize {
            0
        }

        fn send(&self, _message: String, _cx: &AsyncApp) -> Task<Result<String>> {
            if self.label == "Gamma" {
                return Task::ready(Err(anyhow::anyhow!("step failed")));
            }
            let Some(claim) = self.claim.borrow().clone() else {
                return Task::ready(Err(anyhow::anyhow!("no file claim was set")));
            };
            let own_file = format!("/project/{}.rs", self.label);
            let own = claim.claim(Path::new(&own_file)).is_ok();
            let shared = claim.claim(Path::new("/project/shared.rs")).is_ok();
            Task::ready(Ok(format!("own: {own}, shared: {shared}")))
        }

        fn set_file_claim(&self, claim: FileClaim, _cx: &mut App) {
            self.claim.replace(Some(claim));
        }
    }

    struct FakeEnvironment;

    impl ThreadEnvironment for FakeEnvironment {
        fn create_terminal(
            &self,
            _command: String,
            _extra_env: Vec<(String, String)>,
            _cwd: Option<PathBuf>,
            _output_byte_limit: Option<u64>,
            _sandbox_wrap: Option<acp_thread::SandboxWrap>,
            _cx: &mut AsyncApp,
        ) -> Task<Result<Rc<dyn crate::TerminalHandle>>> {
            Task::ready(Err(anyhow::anyhow!("no terminals in this test")))
        }

        fn create_subagent(
            &self,
            label: String,
            _model: Option<AgentModelId>,
            _cx: &mut App,
        ) -> Result<Rc<dyn SubagentHandle>> {
            Ok(Rc::new(FakeStep {
                label,
                claim: RefCell::new(None),
            }))
        }
    }

    #[gpui::test]
    async fn steps_run_side_by_side_and_keep_the_checklist(cx: &mut gpui::TestAppContext) {
        let (_connection, _agent, thread, _session) =
            super::super::architect_run_tool::architect_tool_test_session(cx).await;
        #[allow(clippy::arc_with_non_send_sync)]
        let tool = Arc::new(RunStepsTool::new(
            thread.downgrade(),
            Rc::new(FakeEnvironment),
        ));
        let input = RunStepsToolInput {
            steps: vec![parallel("Alpha"), parallel("Beta"), parallel("Gamma")],
        };
        let (stream, mut events) = ToolCallEventStream::test();
        let output = cx
            .update(|cx| tool.run(ToolInput::resolved(input), stream, cx))
            .await
            .expect("the steps finish");
        let RunStepsToolOutput::Finished { steps, checklist } = output else {
            panic!("expected finished steps");
        };

        let mut plans = Vec::new();
        let mut progress_updates = Vec::new();
        let mut spawned = 0;
        while let Ok(Ok(event)) = events.try_recv() {
            match event {
                ThreadEvent::Plan(plan) => plans.push(plan),
                ThreadEvent::SubagentSpawned(_) => spawned += 1,
                ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::V2(update)) => {
                    let meta = update.meta.value().cloned();
                    let progress = acp_thread::parallel_steps_from_meta(&meta);
                    if !progress.is_empty() {
                        progress_updates.push(progress);
                    }
                }
                _ => {}
            }
        }
        assert_eq!(spawned, 3);
        // The checklist changes once when the steps start and again as each
        // one finishes, rather than all at once at the end.
        assert_eq!(plans.len(), 4);
        let statuses = |plan: &acp_v2::PlanItems| {
            plan.entries
                .iter()
                .map(|entry| entry.status.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            statuses(&plans[0]),
            vec![acp_v2::PlanEntryStatus::InProgress; 3]
        );
        assert_eq!(
            statuses(&plans[3]),
            vec![
                acp_v2::PlanEntryStatus::Completed,
                acp_v2::PlanEntryStatus::Completed,
                acp_v2::PlanEntryStatus::Pending,
            ]
        );

        // Every step's agent is reported as soon as the steps start, then
        // each step's progress as it finishes.
        assert_eq!(progress_updates.len(), 4);
        let progress_statuses = |progress: &[StepProgress]| {
            progress.iter().map(|step| step.status).collect::<Vec<_>>()
        };
        assert_eq!(
            progress_statuses(&progress_updates[0]),
            vec![ParallelStepStatus::Running; 3]
        );
        let started_sessions: Vec<_> = progress_updates[0]
            .iter()
            .map(|step| step.session.as_ref().map(|session| session.session_id.clone()))
            .collect();
        assert_eq!(
            started_sessions,
            ["Alpha", "Beta", "Gamma"]
                .map(|name| Some(acp_v2::SessionId::new(format!("session-{name}"))))
                .to_vec()
        );
        let last = &progress_updates[3];
        assert_eq!(
            progress_statuses(last),
            vec![
                ParallelStepStatus::Done,
                ParallelStepStatus::Done,
                ParallelStepStatus::NotFinished,
            ]
        );
        assert!(last[0].error.is_none());
        assert!(
            last[2]
                .error
                .as_ref()
                .is_some_and(|error| error.contains("step failed")),
            "a step that did not finish says why"
        );
        let checklist_statuses: Vec<_> = checklist.iter().map(|entry| entry.status).collect();
        assert_eq!(
            checklist_statuses,
            vec![
                PlanStepStatus::Completed,
                PlanStepStatus::Completed,
                PlanStepStatus::Pending,
            ]
        );

        assert!(steps[0].completed);
        assert_eq!(steps[0].output, "own: true, shared: true");
        assert_eq!(
            steps[0].changed_files,
            vec!["/project/Alpha.rs", "/project/shared.rs"]
        );
        assert_eq!(steps[1].output, "own: true, shared: false");
        assert_eq!(steps[1].changed_files, vec!["/project/Beta.rs"]);
        assert!(!steps[2].completed);
        assert!(steps[2].output.contains("step failed"));
        assert_eq!(
            steps[2].session_id,
            Some(acp_v2::SessionId::new("session-Gamma"))
        );
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.agent_checklist(), checklist.as_slice());
        });
    }
}
