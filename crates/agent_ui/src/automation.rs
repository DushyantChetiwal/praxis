//! A local control channel that lets an agent drive and inspect the running
//! app the way a person would, so changes can be checked in a real build
//! rather than only in tests.
//!
//! It is off unless an `automation` directory exists in the app's data
//! directory when the app starts, and it only ever reads and writes inside
//! that directory: nothing listens on the network. A client writes a JSON
//! request to `automation/requests/<name>.json` (writing elsewhere first and
//! renaming, so a half-written file is never read) and the reply appears as
//! `automation/responses/<name>.json`, either `{"ok":true,"result":...}` or
//! `{"ok":false,"error":"..."}`. Requests are handled in name order.
//!
//! Requests:
//! - `{"command":"state"}`: the window, its docks and tabs, and the canvas.
//! - `{"command":"action","name":"workspace::Save","data":null}`: dispatches
//!   an action to whatever has focus.
//! - `{"command":"keys","keys":"ctrl-z ctrl-d"}`: types keystrokes.
//! - `{"command":"architect","op":"select","args":{"node":"..."}}`: a canvas
//!   action; see `ArchitectPane::automation_command`. `open` opens the canvas
//!   for the Agent panel's thread.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Entity, Keystroke, Window};
use serde::Deserialize;
use serde_json::{Value, json};
use workspace::{MultiWorkspace, Workspace};

use crate::AgentPanel;
use crate::architect_ui::ArchitectPane;

const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Request {
    State,
    Action {
        name: String,
        #[serde(default)]
        data: Option<Value>,
    },
    Keys {
        keys: String,
    },
    Architect {
        op: String,
        #[serde(default)]
        args: Value,
    },
}

pub fn init(cx: &mut App) {
    let root = paths::data_dir().join("automation");
    if !root.is_dir() {
        return;
    }
    let requests = root.join("requests");
    let responses = root.join("responses");
    for folder in [&requests, &responses] {
        if let Err(error) = std::fs::create_dir_all(folder) {
            log::error!("Could not create {}: {error:#}", folder.display());
            return;
        }
    }
    log::info!("Automation channel enabled in {}", root.display());

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let folder = requests.clone();
            let task = cx
                .background_executor()
                .spawn(async move { take_requests(&folder) });
            let pending = match task.await {
                Ok(pending) => pending,
                Err(error) => {
                    log::error!("Could not read automation requests: {error:#}");
                    continue;
                }
            };
            for (name, contents) in pending {
                let result = match serde_json::from_str::<Request>(&contents) {
                    Ok(request) => cx.update(|cx| handle(request, cx)),
                    Err(error) => Err(anyhow!("could not parse the request: {error}")),
                };
                let reply = match result {
                    Ok(result) => json!({ "ok": true, "result": result }),
                    Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                };
                let folder = responses.clone();
                let written = cx
                    .background_executor()
                    .spawn(async move { write_reply(&folder, &name, &reply) })
                    .await;
                if let Err(error) = written {
                    log::error!("Could not write an automation reply: {error:#}");
                }
            }
        }
    })
    .detach();
}

/// Reads and removes every waiting request, oldest name first. A request is
/// removed before it is handled so that one which fails is never retried.
fn take_requests(folder: &Path) -> Result<Vec<(String, String)>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(folder)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    let mut pending = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = name.to_string();
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        pending.push((name, contents));
    }
    Ok(pending)
}

fn write_reply(folder: &Path, name: &str, reply: &Value) -> Result<()> {
    let partial = folder.join(format!("{name}.partial"));
    std::fs::write(&partial, serde_json::to_vec_pretty(reply)?)?;
    std::fs::rename(&partial, folder.join(name))?;
    Ok(())
}

fn handle(request: Request, cx: &mut App) -> Result<Value> {
    let window = cx
        .window_stack()
        .unwrap_or_else(|| cx.windows())
        .into_iter()
        .find(|window| window.downcast::<MultiWorkspace>().is_some())
        .context("no Praxis window is open")?;
    // Updating the window rather than the `MultiWorkspace` in it leaves the
    // root free for anything the request goes on to do.
    window.update(cx, |root, window, cx| {
        let multi_workspace = root
            .downcast::<MultiWorkspace>()
            .map_err(|_| anyhow!("the window no longer shows a workspace"))?;
        let workspace = multi_workspace.read(cx).workspace().clone();
        match request {
            Request::State => Ok(state(&workspace, window, cx)),
            Request::Action { name, data } => {
                let action = cx.build_action(&name, data)?;
                window.dispatch_action(action, cx);
                Ok(json!({ "dispatched": name }))
            }
            Request::Keys { keys } => {
                let mut handled = Vec::new();
                for key in keys.split_whitespace() {
                    let keystroke = Keystroke::parse(key)?;
                    handled.push(json!({
                        "key": key,
                        "handled": window.dispatch_keystroke(keystroke, cx),
                    }));
                }
                Ok(json!({ "keys": handled }))
            }
            Request::Architect { op, args } => architect(&op, &args, &workspace, window, cx),
        }
    })?
}

fn architect(
    op: &str,
    args: &Value,
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Result<Value> {
    if op == "open" {
        let panel = workspace
            .read(cx)
            .panel::<AgentPanel>(cx)
            .context("the Agent panel is not loaded")?;
        panel.update(cx, |panel, cx| {
            panel.defer_open_architect_workspace(window, cx);
        });
        return Ok(json!({ "deferred": true }));
    }
    let pane = architect_pane(workspace.read(cx), cx)
        .context("Architect is not open; send the \"open\" op first")?;
    pane.update(cx, |pane, cx| pane.automation_command(op, args, window, cx))
}

fn architect_pane(workspace: &Workspace, cx: &App) -> Option<Entity<ArchitectPane>> {
    workspace.item_of_type::<ArchitectPane>(cx).or_else(|| {
        workspace
            .panel::<AgentPanel>(cx)
            .and_then(|panel| panel.read(cx).retained_architect_pane())
    })
}

fn state(workspace: &Entity<Workspace>, window: &Window, cx: &mut App) -> Value {
    let (projects, tabs, docks, pane) = {
        let workspace = workspace.read(cx);
        let projects: Vec<String> = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).root_name_str().to_string())
            .collect();
        let active_pane = workspace.active_pane().read(cx);
        let active_item = active_pane.active_item().map(|item| item.item_id());
        let tabs: Vec<Value> = active_pane
            .items()
            .map(|item| {
                json!({
                    "title": item.tab_content_text(0, cx).to_string(),
                    "active": Some(item.item_id()) == active_item,
                })
            })
            .collect();
        let docks: Vec<Value> = workspace
            .all_docks()
            .iter()
            .map(|dock| {
                let dock = dock.read(cx);
                json!({
                    "position": format!("{:?}", dock.position()),
                    "open": dock.is_open(),
                    "panel": dock.active_panel().map(|panel| panel.persistent_name()),
                })
            })
            .collect();
        (projects, tabs, docks, architect_pane(workspace, cx))
    };
    let architect = pane.map(|pane| pane.update(cx, |pane, cx| pane.automation_state(cx)));
    let bounds = window.bounds();
    json!({
        "projects": projects,
        "active": window.is_window_active(),
        "scale_factor": window.scale_factor(),
        "bounds": {
            "x": f32::from(bounds.origin.x),
            "y": f32::from(bounds.origin.y),
            "width": f32::from(bounds.size.width),
            "height": f32::from(bounds.size.height),
        },
        "tabs": tabs,
        "docks": docks,
        "architect": architect,
    })
}
