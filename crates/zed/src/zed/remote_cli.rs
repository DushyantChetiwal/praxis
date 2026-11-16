//! `praxis --remote`: setting up Praxis Remote from a terminal, for machines
//! where nobody sits at a Praxis window, such as a VM reached over SSH.
//! Signing in prints GitHub's device code instead of opening a browser, and
//! pairing asks in the terminal instead of a window.

use std::pin::pin;
use std::time::{Duration, Instant};

use agent_ui::remote::{self, CliStatus};
use anyhow::{Context as _, Result, bail};
use cli::{CliRequest, CliResponse, CliResponseSink, RemoteAction};
use futures::StreamExt as _;
use futures::channel::mpsc;
use futures::future::{self, Either};
use gpui::AsyncApp;
use util::ResultExt as _;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// GitHub's device codes expire after 15 minutes.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A pairing the phone started also expires, after 5 minutes, so this only
/// bounds how long nobody starts one.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) async fn run(
    action: RemoteAction,
    phone: Option<String>,
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> i32 {
    let result = match action {
        RemoteAction::Status => show_status(responses, cx),
        RemoteAction::SignIn => sign_in(responses, cx).await,
        RemoteAction::Pair => pair(responses, requests, cx).await,
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
        format!(
            "Signed in to GitHub as {login}. Run `praxis --remote pair` to pair a phone."
        ),
    );
    Ok(())
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
    let device = status
        .device
        .unwrap_or_else(|| "this computer".to_string());
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

    responses.send(CliResponse::PromptConfirm {
        message: remote::cli_pairing_prompt(&question.phone, &question.device, &question.code),
    })?;
    let allowed = loop {
        match requests.next().await {
            Some(CliRequest::Confirm { allowed }) => break allowed,
            Some(other) => log::warn!("Ignoring an unexpected CLI request while pairing: {other:?}"),
            // The terminal went away without answering.
            None => break false,
        }
    };
    let phone = question.phone;
    if question.answer.send(allowed).is_err() {
        bail!("The pairing with \u{201c}{phone}\u{201d} expired before it was answered. Try again.");
    }
    if allowed {
        say(
            responses,
            format!("Paired \u{201c}{phone}\u{201d}. It can now see and control Praxis on {device}."),
        );
    } else {
        say(responses, format!("Denied \u{201c}{phone}\u{201d}."));
    }
    Ok(())
}

fn unpair(
    phone: Option<String>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) -> Result<()> {
    let phone = phone.context(
        "Name the phone to unpair with `--phone NAME`. `praxis --remote status` lists the paired phones.",
    )?;
    let message = cx.update(|cx| remote::cli_unpair(&phone, cx))?;
    say(responses, message);
    Ok(())
}
