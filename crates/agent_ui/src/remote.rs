//! Praxis Remote: following and steering the agent from an Android phone.
//!
//! Nothing listens on the network, and there is nothing to set up beyond
//! signing in. Praxis signs in to the user's GitHub account with the device
//! flow and keeps a secret gist there as the channel to their phones:
//!
//! - The gist's `praxis-remote.json` names this computer and says when it was
//!   last seen. Its `state.json` holds a snapshot of what Praxis is doing
//!   (windows, the watched conversation, what is waiting for approval),
//!   encrypted separately for each paired phone. It is refreshed every
//!   minute, and every few seconds while a phone is watching.
//! - A phone sends an encrypted request as a comment on the gist. Praxis
//!   carries it out and answers by editing that comment, which the phone then
//!   deletes.
//! - A phone pairs through a comment too, agreeing a key with this computer,
//!   and is only accepted once the user allows it here after comparing a code
//!   shown on both screens.
//!
//! `docs/src/ai/praxis-remote-protocol.md` specifies the wire format, which
//! the Android app in `remote-android/` follows as well. Files can be read
//! and downloaded remotely but never written: changes go through the agent.

mod channel;
mod crypto;
mod github;
mod modal;
mod store;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use acp_thread::{
    AgentThreadEntry, PermissionOptions, SelectedPermissionOutcome, ThreadStatus, ToolCallStatus,
};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, Task, TaskExt as _, WeakEntity,
};
use http_client::HttpClient;
use serde_json::{Value, json};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::{MultiWorkspace, Workspace};

use crate::automation::{architect_pane, with_workspace, workspace_windows};
use crate::conversation_view::{ConversationView, ThreadView};
use crate::thread_metadata_store::ThreadMetadataStore;
use crate::{AgentPanel, NewThread};

pub(crate) use modal::PraxisRemoteModal;
use store::PhoneInfo;

const TRANSCRIPT_BUDGET: usize = 36_000;
const ENTRY_LIMIT: usize = 6_000;
const HISTORY_PAGE_ENTRIES: usize = 100;
const PERMISSION_DETAIL_LIMIT: usize = 4_000;
const FILE_LIMIT: usize = 20_000;
/// What a file's content may take once escaped as JSON, which leaves room
/// for the rest of the answer in the 46,000 bytes an answer may have.
const FILE_JSON_BUDGET: usize = 40_000;
const MAX_FILE_BYTES: u64 = 2_000_000;
/// A download travels in pieces that each fit in one answer once base64
/// encoded.
const DOWNLOAD_CHUNK_BYTES: u64 = 32 * 1024;
/// Every piece is a comment that the phone writes and Praxis rewrites, and
/// GitHub limits how many comments an account may write in an hour.
const MAX_DOWNLOAD_BYTES: u64 = 5 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 500;
/// What a directory listing's entries may take once they are JSON.
const DIR_JSON_BUDGET: usize = 40_000;
const MAX_THREADS: usize = 30;

pub fn init(cx: &mut App) {
    let remote = cx.new(|_| PraxisRemote::new());
    cx.set_global(GlobalPraxisRemote(remote.clone()));
    remote.update(cx, |remote, cx| remote.start(false, cx));

    cx.background_spawn(async {
        if store::legacy_config_path().is_file() {
            log::warn!(
                "Praxis Remote no longer uses {}; sign in from \"Praxis Remote…\" and pair \
                 your phone again",
                store::legacy_config_path().display()
            );
        }
    })
    .detach();
}

/// What Praxis Remote is doing, as its modal shows it.
#[derive(Clone, Debug, PartialEq)]
enum RemoteStatus {
    Off,
    /// Waiting for the user to enter `user_code` at `verification_uri`.
    SigningIn {
        user_code: String,
        verification_uri: String,
    },
    Connecting,
    Connected,
    /// GitHub cannot be reached for now; Praxis keeps trying.
    Offline(String),
    /// Praxis Remote stopped. `sign_in` says whether signing in again is the
    /// way out, as opposed to retrying.
    Failed {
        reason: String,
        sign_in: bool,
    },
}

/// Praxis Remote's state for the whole app, and whatever it is running:
/// signing in, the channel, or turning off.
pub(crate) struct PraxisRemote {
    status: RemoteStatus,
    login: Option<String>,
    device: Option<String>,
    phones: Vec<PhoneInfo>,
    commands: Option<mpsc::UnboundedSender<channel::Command>>,
    _task: Option<Task<()>>,
}

struct GlobalPraxisRemote(Entity<PraxisRemote>);

impl Global for GlobalPraxisRemote {}

impl PraxisRemote {
    fn new() -> Self {
        Self {
            status: RemoteStatus::Off,
            login: None,
            device: None,
            phones: Vec::new(),
            commands: None,
            _task: None,
        }
    }

    fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalPraxisRemote>()
            .map(|global| global.0.clone())
    }

    /// Runs the channel, after signing in first if `sign_in` is set. Without
    /// it, the channel stops at once unless Praxis Remote was set up before.
    fn start(&mut self, sign_in: bool, cx: &mut Context<Self>) {
        let (sender, commands) = mpsc::unbounded();
        self.commands = Some(sender.clone());
        if sign_in {
            self.status = RemoteStatus::Connecting;
        }
        cx.notify();
        let http = cx.http_client();
        self._task = Some(cx.spawn(async move |this, cx| {
            if sign_in {
                if let Err(error) = sign_in_with_github(&this, &http, cx).await {
                    log::warn!("Praxis Remote could not sign in: {error:#}");
                    this.update(cx, |this, cx| {
                        this.status = RemoteStatus::Failed {
                            reason: format!("Could not sign in: {error:#}"),
                            sign_in: true,
                        };
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            }
            channel::run(this, http, sender, commands, cx).await;
        }));
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        self.start(true, cx);
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.start(false, cx);
    }

    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self._task = None;
        self.commands = None;
        self.status = if self.login.is_some() {
            RemoteStatus::Failed {
                reason: "Praxis Remote is signed out. Sign in again to reconnect.".into(),
                sign_in: true,
            }
        } else {
            RemoteStatus::Off
        };
        cx.notify();
    }

    /// Stops the channel and forgets everything: the gist, the sign-in and
    /// every paired phone.
    fn turn_off(&mut self, cx: &mut Context<Self>) {
        self.commands = None;
        self.status = RemoteStatus::Off;
        self.login = None;
        self.device = None;
        self.phones.clear();
        cx.notify();
        let http = cx.http_client();
        // Replacing the task stops the channel first.
        self._task = Some(cx.spawn(async move |_, cx| {
            channel::forget_everything(http, cx).await;
        }));
    }

    fn unpair(&mut self, phone_id: String, cx: &mut Context<Self>) {
        self.phones.retain(|phone| phone.id != phone_id);
        cx.notify();
        let command = channel::Command::Unpair(phone_id.clone());
        let sent = self
            .commands
            .as_ref()
            .is_some_and(|commands| commands.unbounded_send(command).is_ok());
        if !sent {
            // The channel is not running, so the phone is removed from what
            // it would load next time.
            cx.spawn(async move |_, cx| channel::forget_phone(phone_id, cx).await)
                .detach_and_log_err(cx);
        }
    }
}

async fn sign_in_with_github(
    this: &WeakEntity<PraxisRemote>,
    http: &Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let code = github::start_device_flow(http).await?;
    this.update(cx, |this, cx| {
        this.status = RemoteStatus::SigningIn {
            user_code: code.user_code.clone(),
            verification_uri: code.verification_uri.clone(),
        };
        cx.notify();
    })?;
    let executor = cx.background_executor().clone();
    let tokens = github::await_device_token(http, &code, &executor).await?;
    this.update(cx, |this, cx| {
        this.status = RemoteStatus::Connecting;
        cx.notify();
    })?;
    let login = github::Api::new(http.clone(), tokens.clone())
        .login()
        .await?;
    channel::remember_sign_in(login, tokens, cx).await
}

async fn resolve_device_name() -> String {
    for variable in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(variable)
            && !name.trim().is_empty()
        {
            return name.trim().to_string();
        }
    }
    match util::command::new_command("hostname").output().await {
        Ok(output) if output.status.success() => {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
        Ok(output) => log::warn!("`hostname` failed with {}", output.status),
        Err(error) => log::warn!("could not run `hostname`: {error}"),
    }
    "Computer".to_string()
}

/// What the phone asked to watch, and until when.
#[derive(Clone, Debug, PartialEq)]
struct Watch {
    window: Option<u64>,
    session_id: Option<String>,
    until: DateTime<Utc>,
}

impl Watch {
    fn to_json(&self) -> Value {
        json!({
            "window": self.window,
            "session_id": self.session_id,
            "until": self.until.to_rfc3339(),
        })
    }
}

/// What a phone sees without asking: every window, which `status` has
/// already described, and the conversation it is watching.
fn snapshot(status: &Value, watch: Option<&Watch>, cx: &mut App) -> Value {
    let (thread, thread_error) = match watch {
        Some(watch) => {
            let args = json!({ "session_id": watch.session_id });
            match with_workspace(watch.window, cx, |workspace, _, cx| {
                thread(workspace, &args, cx)
            }) {
                Ok(thread) => (thread, Value::Null),
                Err(error) => (Value::Null, json!(format!("{error:#}"))),
            }
        }
        None => (Value::Null, Value::Null),
    };
    json!({
        "watch": watch.map(Watch::to_json),
        "status": status,
        "thread": thread,
        "thread_error": thread_error,
    })
}

fn handle(op: &str, args: &Value, device: &str, cx: &mut App) -> Task<Result<Value>> {
    let window = args.get("window").and_then(Value::as_u64);
    let result = match op {
        "status" => Ok(status(device, cx)),
        "threads" => with_workspace(window, cx, |workspace, _, cx| threads(workspace, cx)),
        "thread" => with_workspace(window, cx, |workspace, _, cx| thread(workspace, args, cx)),
        "prompt" => with_workspace(window, cx, |workspace, window, cx| {
            let text = required(args, "text")?.to_string();
            if text.trim().is_empty() {
                bail!("the message is empty");
            }
            let view = root_thread_view(workspace, cx)?;
            let queued = view.update(cx, |view, cx| view.send_text(text, window, cx));
            Ok(json!({ "queued": queued }))
        }),
        "stop" => with_workspace(window, cx, |workspace, _, cx| {
            let view = root_thread_view(workspace, cx)?;
            view.update(cx, |view, cx| view.cancel_generation(cx));
            Ok(json!({ "stopped": true }))
        }),
        "new_thread" => with_workspace(window, cx, |workspace, window, cx| {
            let panel = agent_panel(workspace, cx)?;
            window.defer(cx, move |window, cx| {
                panel.update(cx, |panel, cx| panel.new_thread(&NewThread, window, cx));
            });
            Ok(json!({ "started": true }))
        }),
        "open_thread" => with_workspace(window, cx, |workspace, window, cx| {
            open_thread(workspace, required(args, "session_id")?, window, cx)
        }),
        "permission" => with_workspace(window, cx, |workspace, _, cx| {
            answer_permission(workspace, args, cx)
        }),
        "mode" => with_workspace(window, cx, |workspace, _, cx| {
            set_mode(workspace, required(args, "mode")?, cx)
        }),
        "architect" => with_workspace(window, cx, |workspace, window, cx| {
            architect(workspace, required(args, "op")?, window, cx)
        }),
        "list_dir" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return match with_workspace(window, cx, |workspace, _, cx| {
                Ok(project_roots(workspace, cx))
            }) {
                Ok(roots) => cx
                    .background_executor()
                    .spawn(async move { list_dir(&roots, &path) }),
                Err(error) => Task::ready(Err(error)),
            };
        }
        "read_file" => {
            let path = match required(args, "path") {
                Ok(path) => path.to_string(),
                Err(error) => return Task::ready(Err(error)),
            };
            return match readable_roots(window, &path, cx) {
                Ok(roots) => cx
                    .background_executor()
                    .spawn(async move { read_file(&roots, &path) }),
                Err(error) => Task::ready(Err(error)),
            };
        }
        "download" => {
            let path = match required(args, "path") {
                Ok(path) => path.to_string(),
                Err(error) => return Task::ready(Err(error)),
            };
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
            return match readable_roots(window, &path, cx) {
                Ok(roots) => cx
                    .background_executor()
                    .spawn(async move { download_chunk(&roots, &path, offset) }),
                Err(error) => Task::ready(Err(error)),
            };
        }
        op => Err(anyhow!("Praxis does not know the request {op:?}")),
    };
    Task::ready(result)
}

/// The window's projects, once the path is known not to be private.
fn readable_roots(window: Option<u64>, path: &str, cx: &mut App) -> Result<Vec<(String, PathBuf)>> {
    with_workspace(window, cx, |workspace, _, cx| {
        let roots = project_roots(workspace, cx);
        let (root_name, relative) = split_project_path(&roots, path)?;
        if is_private(workspace, root_name, relative, cx) {
            bail!("{path} is private, so Praxis will not share it");
        }
        Ok(roots)
    })
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("expected \"{key}\""))
}

fn agent_panel(workspace: &Entity<Workspace>, cx: &App) -> Result<Entity<AgentPanel>> {
    workspace
        .read(cx)
        .panel::<AgentPanel>(cx)
        .context("the Agent panel is not loaded in that window")
}

fn conversation_view(workspace: &Entity<Workspace>, cx: &App) -> Result<Entity<ConversationView>> {
    agent_panel(workspace, cx)?
        .read(cx)
        .active_conversation_view()
        .cloned()
        .context("the Agent panel is not showing a conversation")
}

fn root_thread_view(workspace: &Entity<Workspace>, cx: &App) -> Result<Entity<ThreadView>> {
    conversation_view(workspace, cx)?
        .read(cx)
        .root_thread_view()
        .context("the conversation has not loaded yet")
}

fn status(device: &str, cx: &App) -> Value {
    let active = cx.active_window().map(|window| window.window_id());
    let windows: Vec<Value> = workspace_windows(cx)
        .into_iter()
        .filter_map(|window| {
            // A window that closed since it was listed is simply left out.
            let workspace = window
                .downcast::<MultiWorkspace>()?
                .read(cx)
                .ok()?
                .workspace()
                .clone();
            Some(window_status(
                window.window_id().as_u64(),
                Some(window.window_id()) == active,
                &workspace,
                cx,
            ))
        })
        .collect();
    json!({ "device": device, "windows": windows })
}

fn window_status(id: u64, active: bool, workspace: &Entity<Workspace>, cx: &App) -> Value {
    let projects: Vec<String> = project_roots(workspace, cx)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let panel = workspace.read(cx).panel::<AgentPanel>(cx);
    let thread = panel.as_ref().and_then(|panel| thread_summary(panel, cx));
    let architect = panel
        .as_ref()
        .and_then(|panel| architect_summary(panel, cx));
    json!({
        "window": id,
        "projects": projects,
        "active": active,
        "thread": thread,
        "architect": architect,
    })
}

fn project_roots(workspace: &Entity<Workspace>, cx: &App) -> Vec<(String, PathBuf)> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| {
            let worktree = worktree.read(cx);
            (
                worktree.root_name_str().to_string(),
                worktree.abs_path().to_path_buf(),
            )
        })
        .collect()
}

fn thread_summary(panel: &Entity<AgentPanel>, cx: &App) -> Option<Value> {
    let conversation_view = panel.read(cx).active_conversation_view()?;
    let root = conversation_view.read(cx).root_thread_view()?;
    let view = root.read(cx);
    let thread = view.thread.read(cx);
    let mode = thread
        .connection()
        .session_modes(thread.session_id(), cx)
        .map(|modes| {
            let available: Vec<Value> = modes
                .all_modes()
                .into_iter()
                .map(|mode| json!({ "id": mode.id.0.as_ref(), "name": mode.name }))
                .collect();
            json!({ "current": modes.current_mode().0.as_ref(), "available": available })
        });
    Some(json!({
        "session_id": thread.session_id().0.as_ref(),
        "title": thread.title().map(|title| title.to_string()),
        "status": status_name(thread.status()),
        "entries": thread.entries().len(),
        "queued": view.message_queue.len(),
        "mode": mode,
        "pending": pending_permissions(conversation_view.read(cx), cx),
    }))
}

fn status_name(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Generating => "generating",
    }
}

fn architect_summary(panel: &Entity<AgentPanel>, cx: &App) -> Option<Value> {
    // Canvas focus and the selected step chat are not the owner of the run.
    let conversation = panel.read(cx).active_conversation_view()?;
    let owner = conversation.read(cx).as_native_thread(cx)?;
    architect_thread_summary(owner.read(cx))
}

fn architect_thread_summary(thread: &agent::Thread) -> Option<Value> {
    let graph = thread.architect_graph()?;
    let run = thread.architect_run();
    let running = run.is_some_and(agent::ArchitectRun::is_running);
    // Branches of a plan run side by side, so there can be several at once.
    let running_steps: Vec<Value> = run
        .filter(|run| run.is_running())
        .map(|run| {
            run.running_steps()
                .iter()
                .map(|step| json!({ "title": step.title.to_string(), "path": step.path }))
                .collect()
        })
        .unwrap_or_default();
    Some(json!({
        "steps": graph.step_count_deeply(),
        "running": running,
        "paused": run.is_some_and(agent::ArchitectRun::is_paused),
        "can_resume": run.is_some_and(agent::ArchitectRun::can_resume),
        "running_steps": running_steps,
        "current_step": run
            .filter(|run| run.is_running())
            .map(|run| run.current_title.to_string()),
        "step_number": run.map_or(0, |run| run.step_number),
        "outcome": run
            .and_then(|run| run.outcome.as_ref())
            .map(|outcome| outcome.describe(graph)),
    }))
}

/// Every option a permission prompt offers, flattened, with the outcome
/// choosing it produces.
fn permission_choices(
    options: &PermissionOptions,
) -> Vec<(acp::PermissionOption, SelectedPermissionOutcome)> {
    let mut choices: Vec<(acp::PermissionOption, SelectedPermissionOutcome)> = Vec::new();
    let mut push = |option: &acp::PermissionOption, outcome: SelectedPermissionOutcome| {
        if !choices
            .iter()
            .any(|(existing, _)| existing.option_id == option.option_id)
        {
            choices.push((option.clone(), outcome));
        }
    };
    match options {
        PermissionOptions::Flat(options) => {
            for option in options {
                push(
                    option,
                    SelectedPermissionOutcome::new(option.option_id.clone(), option.kind),
                );
            }
        }
        PermissionOptions::Dropdown(dropdown)
        | PermissionOptions::DropdownWithPatterns {
            choices: dropdown, ..
        } => {
            for choice in dropdown {
                push(&choice.allow, choice.build_outcome(true));
                push(&choice.deny, choice.build_outcome(false));
            }
        }
    }
    choices
}

fn pending_permissions(view: &ConversationView, cx: &App) -> Vec<Value> {
    let Some(conversation) = view.conversation() else {
        return Vec::new();
    };
    conversation
        .read(cx)
        .pending_tool_calls()
        .into_iter()
        .filter_map(|(thread, tool_call_id)| {
            let thread = thread.read(cx);
            let (_, call) = thread.tool_call(&tool_call_id)?;
            let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status else {
                return None;
            };
            let options: Vec<Value> = permission_choices(options)
                .into_iter()
                .map(|(option, _)| {
                    json!({
                        "id": option.option_id.0.as_ref(),
                        "name": option.name,
                        "kind": option.kind,
                    })
                })
                .collect();
            Some(json!({
                "session_id": thread.session_id().0.as_ref(),
                "tool_call_id": tool_call_id.0.as_ref(),
                "title": call.label.read(cx).source().to_string(),
                "detail": truncate(&call.to_markdown(cx), PERMISSION_DETAIL_LIMIT),
                "options": options,
            }))
        })
        .collect()
}

fn answer_permission(workspace: &Entity<Workspace>, args: &Value, cx: &mut App) -> Result<Value> {
    let session_id = required(args, "session_id")?;
    let tool_call_id = required(args, "tool_call_id")?;
    let option_id = required(args, "option_id")?;
    let conversation = conversation_view(workspace, cx)?
        .read(cx)
        .conversation()
        .cloned()
        .context("the conversation is not connected")?;

    let (session, tool_call, outcome) = {
        let pending = conversation.read(cx).pending_tool_calls();
        let (thread, tool_call) = pending
            .into_iter()
            .find(|(thread, id)| {
                id.0.as_ref() == tool_call_id
                    && thread.read(cx).session_id().0.as_ref() == session_id
            })
            .context("that request is no longer waiting for an answer")?;
        let thread = thread.read(cx);
        let (_, call) = thread
            .tool_call(&tool_call)
            .context("that request is no longer waiting for an answer")?;
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &call.status else {
            bail!("that request is no longer waiting for an answer");
        };
        let (_, outcome) = permission_choices(options)
            .into_iter()
            .find(|(option, _)| option.option_id.0.as_ref() == option_id)
            .with_context(|| format!("that request has no option {option_id:?}"))?;
        (thread.session_id().clone(), tool_call, outcome)
    };
    conversation.update(cx, |conversation, cx| {
        conversation.authorize_tool_call(session, tool_call, outcome, cx);
    });
    Ok(json!({ "answered": true }))
}

fn set_mode(workspace: &Entity<Workspace>, mode: &str, cx: &mut App) -> Result<Value> {
    let view = root_thread_view(workspace, cx)?;
    let thread = view.read(cx).thread.read(cx);
    let modes = thread
        .connection()
        .session_modes(thread.session_id(), cx)
        .context("this agent has no modes")?;
    let id = modes
        .all_modes()
        .into_iter()
        .find(|candidate| candidate.id.0.as_ref() == mode)
        .with_context(|| format!("this agent has no {mode:?} mode"))?
        .id;
    modes.set_mode(id, cx).detach_and_log_err(cx);
    Ok(json!({ "mode": mode }))
}

fn architect(
    workspace: &Entity<Workspace>,
    op: &str,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Result<Value> {
    if !matches!(op, "run" | "pause" | "resume" | "stop") {
        bail!("Praxis Remote can only \"run\", \"pause\", \"resume\", or \"stop\" a plan");
    }
    // Pausing and stopping through the canvas records it in the plan's
    // activity. Starting and resuming go through the runner directly, since
    // the canvas only shows why a run could not start, and the phone needs to
    // be told.
    if matches!(op, "pause" | "stop")
        && let Some(pane) = architect_pane(workspace.read(cx), cx)
    {
        pane.update(cx, |pane, cx| {
            pane.automation_command(op, &Value::Null, window, cx)
        })?;
        return Ok(json!({ "op": op, "done": true }));
    }
    let panel = agent_panel(workspace, cx)?;
    let thread = panel
        .read(cx)
        .active_native_agent_thread(cx)
        .context("the conversation has no plan")?;
    let acp_thread = panel
        .read(cx)
        .active_agent_thread(cx)
        .context("the conversation has not loaded yet")?;
    if op == "run" {
        let graph = thread
            .read(cx)
            .architect_graph()
            .cloned()
            .context("the conversation has no plan")?;
        agent::start_architect_run(thread, acp_thread, graph, cx)
            .map_err(|error| anyhow!("{error}"))?;
    } else if op == "resume" {
        agent::resume_architect_run(thread, acp_thread, cx).map_err(|error| anyhow!("{error}"))?;
    } else if op == "pause" {
        agent::pause_architect_run(&thread, cx);
    } else {
        agent::stop_architect_run(&thread, Some(&acp_thread), cx);
    }
    Ok(json!({ "op": op, "done": true }))
}

fn threads(workspace: &Entity<Workspace>, cx: &App) -> Result<Value> {
    let store = ThreadMetadataStore::try_global(cx).context("thread history is not available")?;
    let roots: Vec<PathBuf> = project_roots(workspace, cx)
        .into_iter()
        .map(|(_, path)| path)
        .collect();
    let active = root_thread_view(workspace, cx)
        .ok()
        .map(|view| view.read(cx).session_id.clone());
    let mut entries: Vec<_> = store
        .read(cx)
        .entries()
        .filter(|metadata| {
            !metadata.archived
                && metadata.agent_id == *agent::ZED_AGENT_ID
                && metadata.session_id.is_some()
                && metadata
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|path| roots.contains(path))
        })
        .collect();
    entries.sort_by_key(|metadata| std::cmp::Reverse(metadata.updated_at));
    let threads: Vec<Value> = entries
        .into_iter()
        .take(MAX_THREADS)
        .filter_map(|metadata| {
            let session_id = metadata.session_id.as_ref()?;
            Some(json!({
                "session_id": session_id.0.as_ref(),
                "title": metadata.display_title().to_string(),
                "updated_at": metadata.updated_at.to_rfc3339(),
                "active": active.as_ref() == Some(session_id),
            }))
        })
        .collect();
    Ok(json!({ "threads": threads }))
}

fn open_thread(
    workspace: &Entity<Workspace>,
    session_id: &str,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Result<Value> {
    let panel = agent_panel(workspace, cx)?;
    let session_id = acp::SessionId::new(session_id.to_string());
    let title = ThreadMetadataStore::try_global(cx)
        .and_then(|store| {
            store
                .read(cx)
                .entry_by_session(&session_id)
                .map(|metadata| metadata.display_title())
        })
        .context("Praxis has no conversation with that id")?;
    window.defer(cx, move |window, cx| {
        panel.update(cx, |panel, cx| {
            panel.open_thread(session_id, None, Some(title), window, cx);
        });
    });
    Ok(json!({ "opened": true }))
}

/// A bounded page of conversation entries, newest last. Without a cursor,
/// follows the live root and its active step conversations.
fn thread(workspace: &Entity<Workspace>, args: &Value, cx: &App) -> Result<Value> {
    let before = history_before_index(args)?;
    let conversation_view = conversation_view(workspace, cx)?;
    let conversation = conversation_view.read(cx);
    let owner = conversation.as_native_thread(cx);
    let thread = match args.get("session_id").and_then(Value::as_str) {
        Some(session_id) => {
            let session_id = acp::SessionId::new(session_id.to_string());
            conversation
                .thread_view(&session_id)
                .map(|view| view.read(cx).thread.clone())
                .or_else(|| live_step_thread(owner.as_ref()?, &session_id, cx))
                .context("that conversation is not open or running in this window")?
        }
        None => conversation
            .root_thread_view()
            .map(|view| view.read(cx).thread.clone())
            .context("the conversation has not loaded yet")?,
    };
    if let Some(before) = before {
        Ok(thread_page_snapshot(
            thread.read(cx),
            None,
            TRANSCRIPT_BUDGET,
            Some(before),
            cx,
        ))
    } else {
        Ok(watched_thread_snapshot(&thread, owner.as_ref(), cx))
    }
}

fn history_before_index(args: &Value) -> Result<Option<usize>> {
    args.get("before_index")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .context("before_index must be a non-negative integer")
        })
        .transpose()
}

fn live_step_thread(
    owner: &Entity<agent::Thread>,
    session_id: &acp::SessionId,
    cx: &App,
) -> Option<Entity<acp_thread::AcpThread>> {
    let run = owner
        .read(cx)
        .architect_run()
        .filter(|run| run.is_running())?;
    run.running_steps()
        .iter()
        .filter_map(|step| step.step_thread())
        .chain(run.step_thread())
        .find(|thread| thread.read(cx).session_id() == session_id)
}

fn watched_thread_snapshot(
    thread: &Entity<acp_thread::AcpThread>,
    owner: Option<&Entity<agent::Thread>>,
    cx: &App,
) -> Value {
    let thread = thread.read(cx);
    let mut steps = Vec::new();
    if let Some(owner) = owner {
        let owner = owner.read(cx);
        // A pinned child chat must not acquire an unrelated root's live steps.
        if owner.id() == thread.session_id()
            && let Some(run) = owner.architect_run().filter(|run| run.is_running())
        {
            for step in run.running_steps() {
                if let Some(step_thread) = step.step_thread()
                    && step_thread.read(cx).session_id() != thread.session_id()
                    && !steps.iter().any(|(_, existing)| existing == &step_thread)
                {
                    steps.push((step.title.to_string(), step_thread));
                }
            }
            // A branch decision can still be streaming after its step has finished.
            if let Some(step_thread) = run.step_thread()
                && step_thread.read(cx).status() == ThreadStatus::Generating
                && step_thread.read(cx).session_id() != thread.session_id()
                && !steps.iter().any(|(_, existing)| existing == &step_thread)
            {
                steps.push((run.current_title.to_string(), step_thread));
            }
        }
    }
    // Reserve the step_threads field and array separators, then share one budget.
    let budget = TRANSCRIPT_BUDGET.saturating_sub(32 + steps.len()) / (steps.len() + 1);
    let mut snapshot = thread_snapshot(thread, None, budget, cx);
    let step_threads: Vec<Value> = steps
        .into_iter()
        .filter_map(|(title, thread)| {
            let snapshot = thread_snapshot(thread.read(cx), Some(&title), budget, cx);
            (snapshot.to_string().len() <= budget).then_some(snapshot)
        })
        .collect();
    snapshot["step_threads"] = json!(step_threads);
    snapshot
}

fn thread_snapshot(
    thread: &acp_thread::AcpThread,
    title: Option<&str>,
    budget: usize,
    cx: &App,
) -> Value {
    thread_page_snapshot(thread, title, budget, None, cx)
}

fn thread_page_snapshot(
    thread: &acp_thread::AcpThread,
    title: Option<&str>,
    budget: usize,
    before: Option<usize>,
    cx: &App,
) -> Value {
    let end = before
        .unwrap_or(thread.entries().len())
        .min(thread.entries().len());
    let mut snapshot = json!({
        "session_id": thread.session_id().0.as_ref(),
        "before_index": before,
        "next_before": end,
        "has_more": false,
        "title": title.map(|title| truncate(title, 256))
            .or_else(|| thread.title().map(|title| truncate(&title, 256))),
        "status": status_name(thread.status()),
        "total": thread.entries().len(),
        "entries": [],
    });
    let budget = budget.saturating_sub(snapshot.to_string().len());
    let (entries, next_before) = transcript_page(thread.entries(), end, budget, cx);
    snapshot["entries"] = json!(entries);
    snapshot["next_before"] = json!(next_before);
    snapshot["has_more"] = json!(next_before > 0);
    snapshot
}

#[cfg(test)]
fn transcript_entries(source: &[AgentThreadEntry], budget: usize, cx: &App) -> Vec<Value> {
    transcript_page(source, source.len(), budget, cx).0
}

fn transcript_page(
    source: &[AgentThreadEntry],
    before: usize,
    mut budget: usize,
    cx: &App,
) -> (Vec<Value>, usize) {
    let end = before.min(source.len());
    let mut next_before = end;
    let mut entries = Vec::new();
    for (index, entry) in source
        .iter()
        .take(end)
        .enumerate()
        .rev()
        .take(HISTORY_PAGE_ENTRIES)
    {
        let (role, text, status) = describe_entry(entry, cx);
        let mut value = json!({ "index": index, "role": role, "text": "", "status": status });
        let limit = budget.min(ENTRY_LIMIT);
        if let AgentThreadEntry::AssistantMessage(message) = entry
            && message
                .chunks
                .iter()
                .any(|chunk| matches!(chunk, acp_thread::AssistantMessageChunk::Thought { .. }))
        {
            let mut parts = Vec::new();
            let mut has_content = false;
            let mut remaining = limit.saturating_sub(value.to_string().len() + 16);
            for (part_index, chunk) in message.chunks.iter().enumerate().rev() {
                let (role, block) = match chunk {
                    acp_thread::AssistantMessageChunk::Message { block, .. } => {
                        ("assistant", block)
                    }
                    acp_thread::AssistantMessageChunk::Thought { block, .. } => {
                        ("reasoning", block)
                    }
                };
                let text = block.to_markdown(cx);
                if text.trim().is_empty() {
                    continue;
                }
                has_content = true;
                let mut part = json!({ "index": part_index, "role": role, "text": "" });
                // Assistant text also appears in the legacy fallback below.
                let text_budget = remaining.saturating_sub(part.to_string().len() + 8) / 2;
                let text = truncate_json_text(text.trim(), text_budget);
                if text.is_empty() {
                    break;
                }
                part["text"] = json!(text);
                remaining = remaining.saturating_sub(part.to_string().len() + json_len(&text) + 8);
                parts.push(part);
            }
            parts.reverse();
            if parts.is_empty() {
                if has_content {
                    break;
                }
                next_before = index;
                continue;
            }
            value["text"] = json!(
                parts
                    .iter()
                    .filter(|part| part["role"] == "assistant")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            );
            value["parts"] = json!(parts);
        } else {
            if text.trim().is_empty() {
                next_before = index;
                continue;
            }
            let text =
                truncate_json_text(text.trim(), limit.saturating_sub(value.to_string().len()));
            if text.is_empty() {
                break;
            }
            value["text"] = json!(text);
        }
        let cost = value.to_string().len() + 1;
        if cost > budget {
            break;
        }
        budget -= cost;
        entries.push(value);
        next_before = index;
    }
    entries.reverse();
    (entries, next_before)
}

fn truncate_json_text(text: &str, budget: usize) -> String {
    let mut limit = text.len().min(budget);
    let mut text = truncate(text, limit);
    while json_len(&text) > budget && limit > 0 {
        limit /= 2;
        text = truncate(&text, limit);
    }
    if json_len(&text) > budget || limit == 0 {
        String::new()
    } else {
        text
    }
}

fn describe_entry(
    entry: &AgentThreadEntry,
    cx: &App,
) -> (&'static str, String, Option<&'static str>) {
    match entry {
        AgentThreadEntry::UserMessage(message) => ("user", message.content.to_markdown(cx), None),
        AgentThreadEntry::AssistantMessage(message) => {
            let text = message
                .chunks
                .iter()
                .filter_map(|chunk| match chunk {
                    acp_thread::AssistantMessageChunk::Message { block, .. } => {
                        Some(block.to_markdown(cx))
                    }
                    acp_thread::AssistantMessageChunk::Thought { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            ("assistant", text, None)
        }
        AgentThreadEntry::ToolCall(call) => {
            let status = match call.status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::WaitingForConfirmation { .. } => "waiting",
                ToolCallStatus::InProgress => "running",
                ToolCallStatus::Completed => "completed",
                ToolCallStatus::Failed => "failed",
                ToolCallStatus::Rejected => "rejected",
                ToolCallStatus::Canceled => "canceled",
            };
            ("tool", call.to_markdown(cx), Some(status))
        }
        entry => ("notice", entry.to_markdown(cx), None),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// How many bytes `text` takes as a JSON string.
fn json_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len(), |json| json.len())
}

/// Splits a path the phone names, such as `project/src/main.rs`, into the
/// project it is in and the path inside that project, refusing anything that
/// would climb out of the project or into its Git internals.
fn split_project_path<'a>(
    roots: &'a [(String, PathBuf)],
    path: &'a str,
) -> Result<(&'a str, &'a str)> {
    let path = path.trim().trim_matches('/');
    let (root_name, relative) = match path.split_once('/') {
        Some((first, rest)) if roots.iter().any(|(name, _)| name == first) => (first, rest),
        _ if roots.iter().any(|(name, _)| name == path) => (path, ""),
        _ => match roots {
            [(name, _)] => (name.as_str(), path),
            _ => bail!("{path:?} does not start with the name of one of this window's projects"),
        },
    };
    let plain = Path::new(relative)
        .components()
        .all(|component| match component {
            Component::Normal(name) => name != ".git",
            Component::CurDir => true,
            _ => false,
        });
    if !plain {
        bail!("{relative:?} is not a plain path inside the project");
    }
    Ok((root_name, relative))
}

/// Resolves a path the phone names to where it is on disk, and the name to
/// show for it.
fn resolve_project_path(roots: &[(String, PathBuf)], path: &str) -> Result<(String, PathBuf)> {
    let (root_name, relative) = split_project_path(roots, path)?;
    let (_, root) = roots
        .iter()
        .find(|(name, _)| name == root_name)
        .context("no such project")?;
    let display = if relative.is_empty() {
        root_name.to_string()
    } else {
        format!("{root_name}/{relative}")
    };
    Ok((display, root.join(relative)))
}

/// Whether the project's `private_files` setting covers a path, in which
/// case the agent would not read it either.
fn is_private(workspace: &Entity<Workspace>, root_name: &str, relative: &str, cx: &App) -> bool {
    // A path Praxis cannot represent is refused rather than guessed about.
    let Ok(relative) = RelPath::from_unix_str(relative) else {
        return true;
    };
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .any(|worktree| {
            let worktree = worktree.read(cx);
            worktree.root_name_str() == root_name
                && worktree
                    .as_local()
                    .is_some_and(|local| local.is_path_private(relative))
        })
}

/// Refuses a path that leaves its project through a link.
fn ensure_inside(roots: &[(String, PathBuf)], path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    let inside = roots.iter().any(|(_, root)| {
        root.canonicalize()
            .is_ok_and(|root| canonical.starts_with(root))
    });
    if !inside {
        bail!("that path leads outside the project");
    }
    Ok(canonical)
}

fn list_dir(roots: &[(String, PathBuf)], path: &str) -> Result<Value> {
    if path.trim().trim_matches('/').is_empty() {
        let entries: Vec<Value> = roots
            .iter()
            .map(|(name, _)| json!({ "name": name, "path": name, "dir": true }))
            .collect();
        return Ok(json!({ "path": "", "entries": entries }));
    }
    let (display, path) = resolve_project_path(roots, path)?;
    let path = ensure_inside(roots, &path)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path).with_context(|| format!("listing {display}"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let dir = entry.file_type()?.is_dir();
        entries.push((dir, name));
    }
    entries.sort_by(|(a_dir, a_name), (b_dir, b_name)| {
        b_dir
            .cmp(a_dir)
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
    });
    let total = entries.len();
    let mut budget = DIR_JSON_BUDGET;
    let mut listed = Vec::new();
    for (dir, name) in entries {
        let entry = json!({ "path": format!("{display}/{name}"), "name": name, "dir": dir });
        let cost = entry.to_string().len() + 1;
        if listed.len() == MAX_DIR_ENTRIES || cost > budget {
            break;
        }
        budget -= cost;
        listed.push(entry);
    }
    let truncated = listed.len() < total;
    Ok(json!({ "path": display, "entries": listed, "truncated": truncated }))
}

fn read_file(roots: &[(String, PathBuf)], path: &str) -> Result<Value> {
    let (display, path) = resolve_project_path(roots, path)?;
    let path = ensure_inside(roots, &path)?;
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        bail!("{display} is not a file");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!(
            "{display} is too large to show ({} bytes); download it instead",
            metadata.len()
        );
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {display}"))?;
    if bytes.contains(&0) {
        bail!("{display} is not a text file; download it instead");
    }
    let text = String::from_utf8_lossy(&bytes);
    // Escaping makes some text longer (quotes, backslashes, control
    // characters), and the answer has to fit in one comment.
    let mut limit = FILE_LIMIT;
    let mut content = truncate(&text, limit);
    loop {
        let escaped = json_len(&content);
        if escaped <= FILE_JSON_BUDGET {
            break;
        }
        // In proportion, which is exact for text that escapes evenly.
        limit = (limit * FILE_JSON_BUDGET / escaped).min(limit.saturating_sub(1));
        content = truncate(&text, limit);
    }
    Ok(json!({
        "path": display,
        "truncated": text.len() > limit,
        "size": metadata.len(),
        "content": content,
    }))
}

/// One piece of a file, starting at `offset`. The phone asks for the pieces in
/// order and checks that `version` stays the same, so a file that changes
/// while it downloads is noticed instead of saved half old and half new.
fn download_chunk(roots: &[(String, PathBuf)], path: &str, offset: u64) -> Result<Value> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let (display, path) = resolve_project_path(roots, path)?;
    let path = ensure_inside(roots, &path)?;
    let mut file = std::fs::File::open(&path).with_context(|| format!("opening {display}"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("{display} is not a file");
    }
    let size = metadata.len();
    if size > MAX_DOWNLOAD_BYTES {
        bail!(
            "{display} is too large to download ({:.1} MB; the limit is {} MB)",
            size as f64 / (1024.0 * 1024.0),
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        );
    }
    if offset > size {
        bail!("{display} changed while it was downloading; try again");
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.by_ref()
        .take(DOWNLOAD_CHUNK_BYTES)
        .read_to_end(&mut data)
        .with_context(|| format!("reading {display}"))?;
    Ok(json!({
        "size": size,
        "offset": offset,
        "version": format!("{size}:{modified}"),
        "data": BASE64.encode(&data),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_entry(parts: &[(bool, &str)], cx: &mut App) -> AgentThreadEntry {
        let registry = Arc::new(language::LanguageRegistry::new(
            cx.background_executor().clone(),
        ));
        AgentThreadEntry::AssistantMessage(acp_thread::AssistantMessage {
            chunks: parts
                .iter()
                .map(|(thinking, text)| {
                    let block = acp_thread::MessageContent::new(
                        (*text).into(),
                        &registry,
                        util::paths::PathStyle::local(),
                        cx,
                    );
                    if *thinking {
                        acp_thread::AssistantMessageChunk::Thought { id: None, block }
                    } else {
                        acp_thread::AssistantMessageChunk::Message { id: None, block }
                    }
                })
                .collect(),
            indented: false,
            is_subagent_output: false,
        })
    }

    #[test]
    fn history_cursor_requires_a_non_negative_integer() {
        assert_eq!(history_before_index(&json!({})).expect("no cursor"), None);
        assert_eq!(
            history_before_index(&json!({ "before_index": 0 })).expect("zero"),
            Some(0)
        );
        assert_eq!(
            history_before_index(&json!({ "before_index": 42 })).expect("cursor"),
            Some(42)
        );
        for value in [json!(-1), json!(1.5), json!("42"), json!(true)] {
            assert!(history_before_index(&json!({ "before_index": value })).is_err());
        }
    }

    #[gpui::test]
    fn history_pages_are_exclusive_bounded_and_keep_original_indices(cx: &mut App) {
        let source: Vec<_> = (0..137)
            .map(|index| assistant_entry(&[(index % 2 == 0, "Provider content")], cx))
            .collect();
        for budget in [512, TRANSCRIPT_BUDGET] {
            let mut before = source.len() + 20;
            let mut received = Vec::new();
            while before > 0 {
                let (page, next) = transcript_page(&source, before, budget - 2, cx);
                assert!(next < before, "each nonterminal page makes progress");
                assert!(page.len() <= HISTORY_PAGE_ENTRIES);
                assert!(serde_json::to_string(&page).expect("page JSON").len() <= budget);
                for entry in page {
                    let index = entry["index"].as_u64().expect("entry index") as usize;
                    assert!(index < before);
                    assert!(index >= next);
                    received.push(index);
                }
                before = next;
            }
            received.sort_unstable();
            assert_eq!(received, (0..source.len()).collect::<Vec<_>>());
        }
        let (empty, next) = transcript_page(&source, 0, TRANSCRIPT_BUDGET, cx);
        assert!(empty.is_empty());
        assert_eq!(next, 0);
        let (empty, next) = transcript_page(&source, 5, 0, cx);
        assert!(empty.is_empty());
        assert_eq!(next, 5, "a budget-rejected entry must not be skipped");
    }

    #[gpui::test]
    fn history_cursor_advances_over_bounded_blank_pages(cx: &mut App) {
        let source: Vec<_> = (0..205)
            .map(|_| assistant_entry(&[(true, "  ")], cx))
            .collect();
        let mut before = source.len();
        for expected in [105, 5, 0] {
            let (page, next) = transcript_page(&source, before, TRANSCRIPT_BUDGET, cx);
            assert!(page.is_empty());
            assert_eq!(next, expected);
            before = next;
        }
    }

    #[gpui::test]
    fn provider_thoughts_keep_their_type_order_and_legacy_entry_indices(cx: &mut App) {
        let source = vec![
            assistant_entry(&[(false, "An ordinary answer")], cx),
            assistant_entry(
                &[
                    (true, "Provider thought"),
                    (false, "Answer"),
                    (true, "More thought"),
                ],
                cx,
            ),
            assistant_entry(&[(true, "Still thinking")], cx),
            assistant_entry(&[(true, "  ")], cx),
        ];
        let entries = transcript_entries(&source, TRANSCRIPT_BUDGET, cx);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["index"], 0);
        assert!(entries[0].get("parts").is_none(), "never invent reasoning");
        assert_eq!(entries[1]["index"], 1);
        assert_eq!(entries[1]["text"], "Answer");
        assert_eq!(
            entries[1]["parts"],
            json!([
                { "index": 0, "role": "reasoning", "text": "Provider thought" },
                { "index": 1, "role": "assistant", "text": "Answer" },
                { "index": 2, "role": "reasoning", "text": "More thought" },
            ])
        );
        assert_eq!(entries[2]["index"], 2);
        assert_eq!(entries[2]["text"], "");
        assert_eq!(entries[2]["parts"][0]["text"], "Still thinking");
    }

    #[gpui::test]
    fn reasoning_and_legacy_text_share_the_escaped_transcript_budget(cx: &mut App) {
        let text = "\"\\\n\t🦀".repeat(4_000);
        let source: Vec<_> = (0..20)
            .map(|_| assistant_entry(&[(true, &text), (false, &text)], cx))
            .collect();
        for budget in [256, 1_024, TRANSCRIPT_BUDGET] {
            let entries = transcript_entries(&source, budget - 2, cx);
            assert!(!entries.is_empty());
            assert!(
                serde_json::to_string(&entries)
                    .expect("entries serialize")
                    .len()
                    <= budget
            );
            assert!(entries.len() < source.len());
            assert_eq!(entries.last().expect("newest entry")["index"], 19);
        }
    }

    #[gpui::test]
    async fn root_snapshot_includes_nested_totals_and_live_step_thinking(
        cx: &mut gpui::TestAppContext,
    ) {
        use acp_thread::AgentConnection as _;
        use architect::{ArchitectGraph, ArchitectNode, NodePath};
        use std::rc::Rc;
        use util::path_list::PathList;

        crate::conversation_view::tests::init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });
        let filesystem = project::FakeFs::new(cx.executor());
        filesystem.insert_tree("/", json!({ "project": {} })).await;
        let project = project::Project::test(filesystem.clone(), [Path::new("/project")], cx).await;
        let connection = cx.update(|cx| {
            let store = agent::ThreadStore::global(cx);
            Rc::new(agent::NativeAgentConnection(agent::NativeAgent::new(
                store,
                agent::Templates::new(),
                filesystem,
                cx,
            )))
        });
        let session = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project, PathList::new(&[Path::new("/project")]), cx)
            })
            .await
            .expect("root session");
        let session_id = session.read_with(cx, |thread, _| thread.session_id().clone());
        let owner = cx
            .update(|cx| connection.thread(&session_id, cx))
            .expect("root owner");
        let mut inner = ArchitectGraph::default();
        inner.add_node(ArchitectNode::new("leaf", "Leaf"));
        let mut child = ArchitectNode::new("child", "Child");
        child.subplan = Some(Box::new(inner));
        let mut nested = ArchitectGraph::default();
        nested.add_node(child);
        nested.add_node(ArchitectNode::new("sibling", "Sibling"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        graph.add_node(ArchitectNode::new("other", "Other"));
        let path = NodePath(vec!["parent".into(), "child".into(), "leaf".into()]);
        owner.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(graph), cx);
            thread.start_architect_run(path.clone(), "Leaf".into(), Task::ready(()), cx);
        });
        let mut steps = Vec::new();
        for (index, (path, title)) in [(path, "Leaf"), (NodePath::root("other".into()), "Other")]
            .into_iter()
            .enumerate()
        {
            let step = cx
                .update(|cx| {
                    connection.create_architect_step_thread(
                        &session_id,
                        path.clone(),
                        title.into(),
                        cx,
                    )
                })
                .expect("step session");
            step.update(cx, |thread, cx| {
                thread.push_assistant_content_block(
                    format!("Thinking in {title}").into(),
                    true,
                    cx,
                );
            });
            let visit = owner.update(cx, |thread, cx| {
                let visit =
                    thread.note_architect_run_position(path, title.into(), index + 1, 1, cx);
                thread.set_architect_run_step_thread(visit, &step, cx);
                visit
            });
            steps.push((visit, step));
        }
        cx.read(|cx| {
            let summary = architect_thread_summary(owner.read(cx)).expect("plan summary");
            assert_eq!(
                summary["steps"], 5,
                "all depths, not just the two root nodes"
            );
            assert_eq!(
                summary["running_steps"]
                    .as_array()
                    .expect("running steps")
                    .len(),
                2
            );
            let snapshot = watched_thread_snapshot(&session, Some(&owner), cx);
            assert_eq!(snapshot["session_id"], session_id.0.as_ref());
            assert_eq!(
                snapshot["step_threads"]
                    .as_array()
                    .expect("live threads")
                    .len(),
                2
            );
            let thinking = &snapshot["step_threads"][0]["entries"][0]["parts"][0];
            assert_eq!(thinking["role"], "reasoning");
            assert_eq!(thinking["text"], "Thinking in Leaf");
            assert!(snapshot.to_string().len() <= TRANSCRIPT_BUDGET);
            let pinned = watched_thread_snapshot(&steps[0].1, Some(&owner), cx);
            assert_eq!(pinned["step_threads"], json!([]));
            let step_session = steps[0].1.read(cx).session_id();
            assert_eq!(
                live_step_thread(&owner, step_session, cx),
                Some(steps[0].1.clone())
            );
            assert!(live_step_thread(&owner, &acp::SessionId::new("unrelated"), cx).is_none());
            let page =
                thread_page_snapshot(steps[0].1.read(cx), None, TRANSCRIPT_BUDGET, Some(1), cx);
            assert_eq!(page["before_index"], 1);
            assert_eq!(page["next_before"], 0);
            assert_eq!(page["has_more"], false);
            assert_eq!(page["entries"][0]["parts"][0]["role"], "reasoning");
            assert!(page.get("step_threads").is_none());
            assert!(page.to_string().len() <= TRANSCRIPT_BUDGET);
        });
        steps[0].1.update(cx, |thread, cx| {
            thread.push_assistant_content_block(" — more provider text".into(), true, cx);
        });
        cx.read(|cx| {
            let snapshot = watched_thread_snapshot(&session, Some(&owner), cx);
            assert_eq!(
                snapshot["step_threads"][0]["entries"][0]["parts"][0]["text"],
                "Thinking in Leaf — more provider text"
            );
        });
        for (visit, _) in steps {
            owner.update(cx, |thread, cx| {
                thread.finish_architect_run_step(visit, None, cx)
            });
        }
        cx.read(|cx| {
            assert_eq!(
                watched_thread_snapshot(&session, Some(&owner), cx)["step_threads"],
                json!([])
            );
        });
    }

    #[test]
    fn a_file_downloads_in_pieces_that_reassemble_exactly() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(&root).expect("create");
        let bytes: Vec<u8> = (0..DOWNLOAD_CHUNK_BYTES * 2 + 123)
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(root.join("image.bin"), &bytes).expect("write");
        let roots = vec![("app".to_string(), root)];

        let mut received = Vec::new();
        let mut version = None;
        loop {
            let chunk =
                download_chunk(&roots, "app/image.bin", received.len() as u64).expect("a piece");
            assert_eq!(chunk["size"], bytes.len() as u64);
            assert_eq!(chunk["offset"], received.len() as u64);
            let this_version = chunk["version"].as_str().expect("version").to_string();
            assert_eq!(
                *version.get_or_insert_with(|| this_version.clone()),
                this_version
            );
            let data = BASE64
                .decode(chunk["data"].as_str().expect("data"))
                .expect("base64");
            assert!(data.len() as u64 <= DOWNLOAD_CHUNK_BYTES);
            if data.is_empty() {
                break;
            }
            received.extend(data);
        }
        assert_eq!(received, bytes);
    }

    #[test]
    fn a_full_download_piece_fits_in_one_answer() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(&root).expect("create");
        std::fs::write(
            root.join("large.bin"),
            vec![0xff; MAX_DOWNLOAD_BYTES as usize],
        )
        .expect("write");
        let roots = vec![("app".to_string(), root)];

        let offset = MAX_DOWNLOAD_BYTES - DOWNLOAD_CHUNK_BYTES;
        let chunk = download_chunk(&roots, "app/large.bin", offset).expect("a piece");
        let envelope = json!({
            "id": "m".repeat(64),
            "ok": true,
            "result": chunk,
        });
        assert!(envelope.to_string().len() <= channel::MAX_ANSWER_LEN);
    }

    #[test]
    fn downloads_stay_inside_the_project_and_under_the_size_limit() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(&root).expect("create");
        std::fs::write(temp.path().join("secret.txt"), "hidden").expect("write");
        std::fs::write(
            root.join("huge.bin"),
            vec![0; MAX_DOWNLOAD_BYTES as usize + 1],
        )
        .expect("write");
        std::fs::write(root.join("small.txt"), "small").expect("write");
        let roots = vec![("app".to_string(), root)];

        assert!(download_chunk(&roots, "app/../secret.txt", 0).is_err());
        assert!(download_chunk(&roots, "app", 0).is_err(), "not a file");
        let too_large = download_chunk(&roots, "app/huge.bin", 0).expect_err("too large");
        assert!(too_large.to_string().contains("too large to download"));
        assert!(
            download_chunk(&roots, "app/small.txt", 6).is_err(),
            "an offset past the end means the file changed"
        );
    }

    #[test]
    fn paths_name_a_project_and_cannot_climb_out_of_it() {
        let roots = vec![
            ("app".to_string(), PathBuf::from("/work/app")),
            ("docs".to_string(), PathBuf::from("/work/docs")),
        ];
        let (display, path) = resolve_project_path(&roots, "app/src/main.rs").expect("resolves");
        assert_eq!(display, "app/src/main.rs");
        assert_eq!(path, Path::new("/work/app").join("src/main.rs"));
        assert_eq!(
            resolve_project_path(&roots, "docs").expect("a root").0,
            "docs"
        );
        assert!(resolve_project_path(&roots, "app/../secrets").is_err());
        assert!(resolve_project_path(&roots, "elsewhere/file").is_err());
        assert!(
            resolve_project_path(&roots, "app/.git/config").is_err(),
            "Git's own files can hold credentials"
        );

        let single = vec![("app".to_string(), PathBuf::from("/work/app"))];
        assert_eq!(
            resolve_project_path(&single, "src/lib.rs")
                .expect("resolves")
                .0,
            "app/src/lib.rs"
        );
    }

    #[test]
    fn files_are_read_but_never_from_outside_the_project() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(root.join("src")).expect("create");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("write");
        std::fs::write(temp.path().join("secret.txt"), "hidden").expect("write");
        let roots = vec![("app".to_string(), root)];

        let file = read_file(&roots, "app/src/main.rs").expect("reads");
        assert_eq!(file["content"], "fn main() {}\n");
        assert_eq!(file["truncated"], false);

        let listing = list_dir(&roots, "app").expect("lists");
        assert_eq!(listing["entries"][0]["name"], "src");
        assert_eq!(listing["entries"][0]["dir"], true);

        assert!(read_file(&roots, "app/../secret.txt").is_err());
        assert_eq!(
            list_dir(&roots, "").expect("roots")["entries"][0]["name"],
            "app"
        );
    }

    #[test]
    fn truncation_keeps_whole_characters() {
        assert_eq!(truncate("héllo", 2), "h…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn a_file_answer_fits_in_one_comment_however_it_escapes() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(&root).expect("create");
        std::fs::write(root.join("quotes.txt"), "\"".repeat(FILE_LIMIT)).expect("write");
        std::fs::write(root.join("controls.txt"), "\u{1}".repeat(FILE_LIMIT)).expect("write");
        let roots = vec![("app".to_string(), root)];

        for name in ["app/quotes.txt", "app/controls.txt"] {
            let file = read_file(&roots, name).expect("reads");
            let content = file["content"].as_str().expect("content");
            assert!(json_len(content) <= FILE_JSON_BUDGET, "{name}");
            assert_eq!(file["truncated"], true, "{name}");
        }
    }

    #[test]
    fn a_long_directory_listing_is_cut_to_fit_in_one_comment() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let root = temp.path().join("app");
        std::fs::create_dir_all(&root).expect("create");
        for index in 0..400 {
            let name = format!("{index:03}-{}", "n".repeat(100));
            std::fs::write(root.join(name), "").expect("write");
        }
        let roots = vec![("app".to_string(), root)];

        let listing = list_dir(&roots, "app").expect("lists");
        assert_eq!(listing["truncated"], true);
        assert!(listing["entries"].to_string().len() <= DIR_JSON_BUDGET + 2);
        let first = format!("000-{}", "n".repeat(100));
        assert_eq!(listing["entries"][0]["name"], first);
    }
}
