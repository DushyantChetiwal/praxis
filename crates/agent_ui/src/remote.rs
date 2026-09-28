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
//! remotely but never written: changes go through the agent.

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
/// What each transcript entry costs besides its text, once it is JSON.
const ENTRY_OVERHEAD: usize = 64;
const ENTRY_LIMIT: usize = 6_000;
const PERMISSION_DETAIL_LIMIT: usize = 4_000;
const FILE_LIMIT: usize = 20_000;
/// What a file's content may take once escaped as JSON, which leaves room
/// for the rest of the answer in the 46,000 bytes an answer may have.
const FILE_JSON_BUDGET: usize = 40_000;
const MAX_FILE_BYTES: u64 = 2_000_000;
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
            return match with_workspace(window, cx, |workspace, _, cx| {
                let roots = project_roots(workspace, cx);
                let (root_name, relative) = split_project_path(&roots, &path)?;
                if is_private(workspace, root_name, relative, cx) {
                    bail!("{path} is private, so Praxis will not show it");
                }
                Ok(roots)
            }) {
                Ok(roots) => cx
                    .background_executor()
                    .spawn(async move { read_file(&roots, &path) }),
                Err(error) => Task::ready(Err(error)),
            };
        }
        op => Err(anyhow!("Praxis does not know the request {op:?}")),
    };
    Task::ready(result)
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
    let thread = panel.read(cx).active_native_agent_thread(cx)?;
    let thread = thread.read(cx);
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
        "steps": graph.nodes.len(),
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

/// The most recent entries of a conversation that fit the budget, newest
/// last, in the order the phone shows them.
fn thread(workspace: &Entity<Workspace>, args: &Value, cx: &App) -> Result<Value> {
    let conversation_view = conversation_view(workspace, cx)?;
    let view = match args.get("session_id").and_then(Value::as_str) {
        Some(session_id) => conversation_view
            .read(cx)
            .thread_view(&acp::SessionId::new(session_id.to_string()))
            .context("that conversation is not open in the Agent panel")?,
        None => conversation_view
            .read(cx)
            .root_thread_view()
            .context("the conversation has not loaded yet")?,
    };
    let thread = view.read(cx).thread.read(cx);
    let total = thread.entries().len();
    let mut budget = TRANSCRIPT_BUDGET;
    let mut entries = Vec::new();
    for (index, entry) in thread.entries().iter().enumerate().rev() {
        let (role, text, status) = describe_entry(entry, cx);
        let text = truncate(text.trim(), ENTRY_LIMIT);
        if text.is_empty() {
            continue;
        }
        // Counted as JSON, since that is what has to fit in one comment.
        let cost = json_len(&text) + ENTRY_OVERHEAD;
        if cost > budget && !entries.is_empty() {
            break;
        }
        budget = budget.saturating_sub(cost);
        entries.push(json!({
            "index": index,
            "role": role,
            "text": text,
            "status": status,
        }));
    }
    entries.reverse();
    Ok(json!({
        "session_id": thread.session_id().0.as_ref(),
        "title": thread.title().map(|title| title.to_string()),
        "status": status_name(thread.status()),
        "total": total,
        "entries": entries,
    }))
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
        bail!("{display} is too large to show ({} bytes)", metadata.len());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {display}"))?;
    if bytes.contains(&0) {
        bail!("{display} is not a text file");
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

#[cfg(test)]
mod tests {
    use super::*;

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
