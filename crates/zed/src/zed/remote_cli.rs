//! `praxis --remote`: setting up and hosting Praxis Remote from a terminal,
//! including on display-less machines such as Linux or Windows VMs over SSH.
//! Signing in prints GitHub's device code instead of opening a browser,
//! pairing asks in the terminal instead of a window, and `start` / `serve`
//! run workspaces on headless windows without a display server.

use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_ui::remote::{self, CliStatus};
use anyhow::{Context as _, Result, bail};
use cli::{CliRequest, CliResponse, CliResponseSink, RemoteAction};
use futures::channel::mpsc;
use futures::future::{self, Either};
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::AsyncApp;
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenMode, OpenOptions, WorkspaceMatching};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const SERVE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// GitHub's device codes expire after 15 minutes.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A pairing the phone started also expires, after 5 minutes, so this only
/// bounds how long nobody starts one.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) async fn run(
    action: RemoteAction,
    phone: Option<String>,
    paths: Vec<String>,
    stop_on_exit: bool,
    app_state: Arc<AppState>,
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> i32 {
    let result = match action {
        RemoteAction::Status => show_status(responses, cx),
        RemoteAction::SignIn => sign_in(responses, cx).await,
        RemoteAction::Pair => pair(responses, requests, cx).await,
        RemoteAction::Start => start(&paths, app_state, responses, cx).await,
        RemoteAction::Serve => {
            serve(&paths, stop_on_exit, app_state, responses, requests, cx).await
        }
        RemoteAction::Stop => stop(responses, cx),
        RemoteAction::Unpair => unpair(phone, responses, cx),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            responses
                .send(CliResponse::Stderr {
                    message: format!("{error:#}"),
                })
                .log_err();
            1
        }
    }
}

fn say(responses: &dyn CliResponseSink, message: impl Into<String>) {
    responses
        .send(CliResponse::Stdout {
            message: message.into(),
        })
        .log_err();
}

fn current(cx: &mut AsyncApp) -> Result<CliStatus> {
    cx.update(|cx| remote::cli_status(cx))
        .context("Praxis Remote is not running in this Praxis. Restart Praxis and try again.")
}

fn show_status(responses: &dyn CliResponseSink, cx: &mut AsyncApp) -> Result<()> {
    let status = current(cx)?;
    say(responses, remote::cli_describe(&status));
    Ok(())
}

async fn sign_in(responses: &dyn CliResponseSink, cx: &mut AsyncApp) -> Result<()> {
    let status = connect(responses, cx).await?;
    let login = status.login.as_deref().unwrap_or("your account");
    say(
        responses,
        format!("Signed in to GitHub as {login}. Run `praxis --remote pair` to pair a phone."),
    );
    Ok(())
}

/// Opens any project folders passed to `--remote start` or `--remote serve`
/// that are not already open in a workspace.
async fn open_requested_paths(
    paths: &[String],
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for raw in paths {
        let path = PathBuf::from(raw);
        let already_open = cx.update(|cx| {
            cx.windows().into_iter().any(|window| {
                window
                    .downcast::<MultiWorkspace>()
                    .and_then(|window| window.read(cx).ok())
                    .is_some_and(|mw| {
                        mw.workspaces().any(|workspace| {
                            workspace
                                .read(cx)
                                .project()
                                .read(cx)
                                .visibility_for_paths(std::slice::from_ref(raw), false, cx)
                                .is_some()
                        })
                    })
            })
        });
        if already_open {
            continue;
        }
        let metadata = app_state
            .fs
            .metadata(&path)
            .await?
            .with_context(|| format!("{} does not exist", path.display()))?;
        if !metadata.is_dir {
            bail!("{} is not a directory", path.display());
        }
        cx.update(|cx| {
            workspace::open_paths(
                std::slice::from_ref(&path),
                app_state.clone(),
                OpenOptions {
                    open_mode: OpenMode::NewWindow,
                    workspace_matching: WorkspaceMatching::None,
                    add_dirs_to_sidebar: false,
                    ..OpenOptions::default()
                },
                cx,
            )
        })
        .await?;
    }
    Ok(())
}

async fn start(
    paths: &[String],
    app_state: Arc<AppState>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) -> Result<()> {
    open_requested_paths(paths, app_state, cx).await?;
    let status = connect(responses, cx).await?;
    say(responses, remote::cli_describe(&status));
    if status.phones.is_empty() {
        say(
            responses,
            "Praxis Remote is running in the background. Run `praxis --remote pair` to pair a phone, or `praxis --remote stop` to stop it.",
        );
    } else {
        say(
            responses,
            "Praxis Remote is running in the background. Run `praxis --remote stop` to stop it.",
        );
    }
    Ok(())
}

async fn serve(
    paths: &[String],
    stop_on_exit: bool,
    app_state: Arc<AppState>,
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> Result<()> {
    open_requested_paths(paths, app_state, cx).await?;
    let status = connect(responses, cx).await?;
    let device = status
        .device
        .clone()
        .unwrap_or_else(|| "this computer".to_string());
    let mut questions = cx
        .update(remote::cli_pairing_questions)
        .context("Praxis Remote is not running in this Praxis. Restart Praxis and try again.")?;

    say(responses, remote::cli_describe(&status));
    say(
        responses,
        format!(
            "Serving {device} on Praxis Remote. Open Praxis Remote on your phone to connect, \
             or tap Start pairing to add a phone. Press Ctrl-C to stop."
        ),
    );

    let mut last_summary = status.summary;
    loop {
        let mut heartbeat = cx
            .background_executor()
            .timer(SERVE_HEARTBEAT_INTERVAL)
            .fuse();
        select_biased! {
            request = requests.next().fuse() => match request {
                Some(other) => {
                    log::warn!("Ignoring an unexpected CLI request while serving: {other:?}");
                }
                None => {
                    if stop_on_exit {
                        schedule_quit(cx);
                    }
                    return Ok(());
                }
            },
            question = questions.next().fuse() => match question {
                Some(question) => {
                    if !answer_pairing_question(
                        question,
                        &device,
                        stop_on_exit,
                        responses,
                        requests,
                        cx,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
                None => bail!("Praxis Remote stopped."),
            },
            _ = heartbeat => {
                if responses.send(CliResponse::Ping).is_err() {
                    if stop_on_exit {
                        schedule_quit(cx);
                    }
                    return Ok(());
                }
                let status = current(cx)?;
                if status.failed {
                    if stop_on_exit {
                        schedule_quit(cx);
                    }
                    bail!("{}", status.summary);
                }
                if status.off {
                    if stop_on_exit {
                        schedule_quit(cx);
                    }
                    bail!("Praxis Remote was turned off.");
                }
                if status.summary != last_summary {
                    say(responses, status.summary.clone());
                    last_summary = status.summary;
                }
            }
        }
    }
}

fn stop(responses: &dyn CliResponseSink, cx: &mut AsyncApp) -> Result<()> {
    say(responses, "Stopping Praxis.");
    schedule_quit(cx);
    Ok(())
}

fn schedule_quit(cx: &mut AsyncApp) {
    cx.spawn(async move |cx| {
        // Give the CLI IPC channel a moment to flush Exit before tearing down.
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
        cx.update(|cx| cx.quit());
    })
    .detach();
}

/// Signs in if needed, then waits until Praxis Remote is connected.
async fn connect(responses: &dyn CliResponseSink, cx: &mut AsyncApp) -> Result<CliStatus> {
    let status = current(cx)?;
    if status.connected {
        return Ok(status);
    }
    cx.update(remote::cli_sign_in);
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    let mut shown_code = None;
    let mut shown_summary = None;
    loop {
        let status = current(cx)?;
        if status.connected {
            return Ok(status);
        }
        if status.failed {
            bail!("{}", status.summary);
        }
        if status.off {
            bail!("Praxis Remote was turned off before it connected.");
        }
        if let Some((code, uri)) = &status.sign_in {
            if shown_code.as_ref() != Some(code) {
                say(responses, remote::cli_sign_in_instructions(code, uri));
                shown_code = Some(code.clone());
            }
        } else if shown_summary.as_ref() != Some(&status.summary) {
            say(responses, status.summary.clone());
            shown_summary = Some(status.summary.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "Praxis Remote did not connect within 15 minutes. Run `praxis --remote status` to see why."
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

async fn pair(
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let status = connect(responses, cx).await?;
    let device = status.device.unwrap_or_else(|| "this computer".to_string());
    // Held until this returns; dropping it sends approval back to windows.
    let mut questions = cx
        .update(remote::cli_pairing_questions)
        .context("Praxis Remote is not running in this Praxis. Restart Praxis and try again.")?;
    say(
        responses,
        format!(
            "Open Praxis Remote on your phone, choose {device}, and tap Start pairing. \
             Waiting up to 10 minutes; press Ctrl-C to stop."
        ),
    );

    let timeout = pin!(cx.background_executor().timer(PAIR_TIMEOUT));
    let question = match future::select(questions.next(), timeout).await {
        Either::Left((Some(question), _)) => question,
        Either::Left((None, _)) => bail!("Praxis Remote stopped while waiting for a phone."),
        Either::Right(_) => bail!("No phone asked to pair within 10 minutes."),
    };
    answer_pairing_question(question, &device, false, responses, requests, cx).await?;
    Ok(())
}

/// Asks the CLI whether to allow `question`. Returns `Ok(false)` when the CLI
/// disconnected while answering.
async fn answer_pairing_question(
    question: remote::PairingQuestion,
    device: &str,
    stop_on_exit: bool,
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> Result<bool> {
    if responses
        .send(CliResponse::PromptConfirm {
            message: remote::cli_pairing_prompt(&question.phone, &question.device, &question.code),
        })
        .is_err()
    {
        question.answer.send(false).ok();
        if stop_on_exit {
            schedule_quit(cx);
        }
        return Ok(false);
    }
    let allowed = loop {
        match requests.next().await {
            Some(CliRequest::Confirm { allowed }) => break allowed,
            Some(other) => {
                log::warn!("Ignoring an unexpected CLI request while pairing: {other:?}")
            }
            None => {
                question.answer.send(false).ok();
                if stop_on_exit {
                    schedule_quit(cx);
                }
                return Ok(false);
            }
        }
    };
    let phone = question.phone;
    if question.answer.send(allowed).is_err() {
        bail!(
            "The pairing with \u{201c}{phone}\u{201d} expired before it was answered. Try again."
        );
    }
    if allowed {
        say(
            responses,
            format!(
                "Paired \u{201c}{phone}\u{201d}. It can now see and control Praxis on {device}."
            ),
        );
    } else {
        say(responses, format!("Denied \u{201c}{phone}\u{201d}."));
    }
    Ok(true)
}

fn unpair(phone: Option<String>, responses: &dyn CliResponseSink, cx: &mut AsyncApp) -> Result<()> {
    let phone = phone.context(
        "Name the phone to unpair with `--phone NAME`. `praxis --remote status` lists the paired phones.",
    )?;
    let message = cx.update(|cx| remote::cli_unpair(&phone, cx))?;
    say(responses, message);
    Ok(())
}
