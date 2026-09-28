//! Praxis Remote: controlling Praxis from a phone through a GitHub issue.
//!
//! It is off unless `remote/config.json` exists in the app's data directory:
//!
//! ```json
//! { "repository": "owner/praxis-remote", "token": "github_pat_…", "device_name": "Laptop" }
//! ```
//!
//! Only `repository` is required. Without a `token`, `PRAXIS_REMOTE_TOKEN` or
//! the GitHub CLI's login (`gh auth token`) is used. Without a `device_name`,
//! the computer's name is.
//!
//! Nothing listens on the network. Praxis keeps one open issue per computer in
//! that repository, titled `Praxis · <device>`:
//!
//! - The issue body carries a heartbeat and a snapshot of what Praxis is doing
//!   (windows, the active conversation, what is waiting for approval). It is
//!   refreshed every minute, and every few seconds while a phone is watching.
//!   A phone polls it with `If-None-Match`, which costs nothing when unchanged.
//! - A phone sends a request as a comment on the issue. Praxis polls the
//!   comments the same way, carries the request out and answers by editing
//!   that comment, which the phone then deletes.
//!
//! Only comments written by the account that owns the token are obeyed, and
//! requests older than a few minutes are refused rather than replayed. Anyone
//! who can write as that account can drive the agent, so the repository should
//! be private and the token scoped to Issues on that repository alone. Files
//! can be read remotely but never written: changes go through the agent.
//!
//! The Android app for the phone lives in `remote-android/` at the root of the
//! repository.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acp_thread::{
    AgentThreadEntry, PermissionOptions, SelectedPermissionOutcome, ThreadStatus, ToolCallStatus,
};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use gpui::{App, AsyncApp, Entity, Task, TaskExt as _};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use util::rel_path::RelPath;
use workspace::{MultiWorkspace, Workspace};

use crate::automation::{architect_pane, with_workspace, workspace_windows};
use crate::conversation_view::{ConversationView, ThreadView};
use crate::thread_metadata_store::ThreadMetadataStore;
use crate::{AgentPanel, NewThread};

const TITLE_PREFIX: &str = "Praxis · ";
const DEVICE_MARKER: &str = "<!-- praxis-device ";
const STATE_MARKER: &str = "<!-- praxis-state -->";
const REQUEST_MARKER: &str = "<!-- praxis-request -->";
const RESPONSE_MARKER: &str = "<!-- praxis-response ";

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
/// How often a changing snapshot is republished while a phone watches. Each
/// write counts against GitHub's limit on content changes, which the phone's
/// requests share, so this stays well under it.
const WATCHED_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
/// How soon after a request the snapshot may be republished, so the phone
/// sees the effect of what it asked for quickly.
const SETTLE_DELAY: Duration = Duration::from_secs(3);
const ERROR_BACKOFF: Duration = Duration::from_secs(60);
const DEFAULT_WATCH_SECONDS: i64 = 300;
const MAX_WATCH_SECONDS: i64 = 900;
/// Requests sent while Praxis was not running are refused, not replayed.
const MAX_REQUEST_AGE_SECONDS: i64 = 300;
/// GitHub refuses issue and comment bodies over 65,536 characters.
const MAX_BODY_LEN: usize = 64_000;
const MAX_RESPONSE_BYTES: u64 = 8_000_000;
const TRANSCRIPT_BUDGET: usize = 36_000;
const ENTRY_LIMIT: usize = 6_000;
const PERMISSION_DETAIL_LIMIT: usize = 4_000;
const FILE_LIMIT: usize = 40_000;
const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_DIR_ENTRIES: usize = 500;
const MAX_THREADS: usize = 30;

#[derive(Deserialize)]
struct Config {
    repository: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
}

pub fn init(cx: &mut App) {
    let config_path = paths::data_dir().join("remote").join("config.json");
    if !config_path.is_file() {
        return;
    }
    let http = cx.http_client();
    cx.spawn(async move |cx| {
        if let Err(error) = run(config_path, http, cx).await {
            log::error!("Praxis Remote stopped: {error:#}");
        }
    })
    .detach();
}

async fn run(config_path: PathBuf, http: Arc<dyn HttpClient>, cx: &mut AsyncApp) -> Result<()> {
    let Some(config) = cx
        .background_executor()
        .spawn(async move { read_config(&config_path) })
        .await?
    else {
        return Ok(());
    };
    validate_repository(&config.repository)?;
    // On macOS and Linux the login shell's environment, and with it `gh` on
    // the `PATH`, may only arrive a little after startup.
    let token = loop {
        match resolve_token(config.token.as_deref()).await {
            Ok(token) => break token,
            Err(error) => {
                log::warn!("Praxis Remote has no GitHub token yet, retrying: {error:#}");
                cx.background_executor().timer(ERROR_BACKOFF).await;
            }
        }
    };
    let device = resolve_device_name(config.device_name.as_deref()).await;
    let github = GitHub {
        http,
        token,
        repository: config.repository,
    };

    let mut remote = loop {
        match Remote::connect(&github, &device).await {
            Ok(remote) => break remote,
            Err(error) => {
                log::warn!("Praxis Remote could not reach GitHub, retrying: {error:#}");
                cx.background_executor().timer(ERROR_BACKOFF).await;
            }
        }
    };
    log::info!(
        "Praxis Remote enabled for {} through issue #{} in {}",
        remote.device,
        remote.issue,
        github.repository
    );

    loop {
        let result = match remote.answer_requests(&github, cx).await {
            Ok(()) => remote.publish(&github, cx).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                if remote.failing {
                    log::info!("Praxis Remote reached GitHub again");
                    remote.failing = false;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
            Err(error) => {
                if !remote.failing {
                    log::warn!("Praxis Remote could not reach GitHub: {error:#}");
                    remote.failing = true;
                }
                cx.background_executor().timer(ERROR_BACKOFF).await;
            }
        }
    }
}

fn read_config(path: &Path) -> Result<Option<Config>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let config = serde_json_lenient::from_str(&contents)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(config))
}

fn validate_repository(repository: &str) -> Result<()> {
    let valid_part = |part: &str| {
        !part.is_empty()
            && !part.chars().all(|c| c == '.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repository.split_once('/') {
        Some((owner, name)) if valid_part(owner) && valid_part(name) => Ok(()),
        _ => bail!("\"repository\" must look like \"owner/name\", not {repository:?}"),
    }
}

async fn resolve_token(configured: Option<&str>) -> Result<String> {
    if let Some(token) = configured.map(str::trim).filter(|token| !token.is_empty()) {
        return Ok(token.to_string());
    }
    if let Ok(token) = std::env::var("PRAXIS_REMOTE_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(token.trim().to_string());
    }
    let output = util::command::new_command("gh")
        .args(["auth", "token"])
        .output()
        .await
        .context("no token is configured and the GitHub CLI could not be run")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        bail!("no token is configured and the GitHub CLI is not signed in");
    }
    Ok(token)
}

async fn resolve_device_name(configured: Option<&str>) -> String {
    if let Some(name) = configured.map(str::trim).filter(|name| !name.is_empty()) {
        return name.to_string();
    }
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

struct GitHub {
    http: Arc<dyn HttpClient>,
    token: String,
    repository: String,
}

struct Reply {
    status: StatusCode,
    etag: Option<String>,
    body: Value,
}

impl GitHub {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        etag: Option<&str>,
    ) -> Result<Reply> {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(format!("https://api.github.com{path}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "Praxis")
            .header("Authorization", format!("Bearer {}", self.token))
            .follow_redirects(http_client::RedirectPolicy::NoFollow);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let body = match body {
            Some(body) => {
                request = request.header("Content-Type", "application/json");
                AsyncBody::from(serde_json::to_vec(&body)?)
            }
            None => AsyncBody::default(),
        };
        let mut response = self.http.send(request.body(body)?).await?;
        let status = response.status();
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut bytes = Vec::new();
        response
            .body_mut()
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut bytes)
            .await?;

        if status == StatusCode::NOT_MODIFIED {
            return Ok(Reply {
                status,
                etag,
                body: Value::Null,
            });
        }
        if !status.is_success() {
            let message = match serde_json::from_slice::<Value>(&bytes) {
                Ok(body) => body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Err(_) => truncate(&String::from_utf8_lossy(&bytes), 500),
            };
            return Err(GitHubError {
                status,
                message: format!(
                    "GitHub answered {} to {method} {path}: {message}",
                    status.as_u16()
                ),
            }
            .into());
        }
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .with_context(|| format!("GitHub's answer to {method} {path} was not JSON"))?
        };
        Ok(Reply { status, etag, body })
    }

    fn repo_path(&self, suffix: &str) -> String {
        format!("/repos/{}/{suffix}", self.repository)
    }
}

/// A call GitHub refused, keeping the status for the callers that care which.
#[derive(Debug)]
struct GitHubError {
    status: StatusCode,
    message: String,
}

impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GitHubError {}

fn github_status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<GitHubError>()
        .map(|error| error.status)
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

struct Remote {
    device: String,
    login: String,
    issue: u64,
    started_at: DateTime<Utc>,
    comments_etag: Option<String>,
    answered: HashSet<u64>,
    /// Answers GitHub refused to take, retried before anything new is read,
    /// since the request they answer has already been carried out.
    unsent: Vec<(u64, String)>,
    watch: Option<Watch>,
    /// The last snapshot written, without its timestamp, to tell whether
    /// anything changed since.
    published: Option<String>,
    published_at: Option<Instant>,
    /// Set by a request whose effect the phone should see soon.
    settle_at: Option<Instant>,
    /// Set by a new watch, which always deserves a fresh snapshot.
    publish_now: bool,
    failing: bool,
}

impl Remote {
    async fn connect(github: &GitHub, device: &str) -> Result<Self> {
        let user = github.call(Method::GET, "/user", None, None).await?;
        let login = user
            .body
            .get("login")
            .and_then(Value::as_str)
            .context("GitHub did not say who the token belongs to")?
            .to_string();
        let issue = find_or_create_issue(github, device, &login).await?;
        Ok(Self {
            device: device.to_string(),
            login,
            issue,
            started_at: Utc::now(),
            comments_etag: None,
            answered: HashSet::default(),
            unsent: Vec::new(),
            watch: None,
            published: None,
            published_at: None,
            settle_at: None,
            publish_now: true,
            failing: false,
        })
    }

    async fn answer_requests(&mut self, github: &GitHub, cx: &mut AsyncApp) -> Result<()> {
        while let Some((comment_id, body)) = self.unsent.first().cloned() {
            send_answer(github, comment_id, body).await?;
            self.unsent.remove(0);
        }

        // The newest comments across the repository, so that a new request is
        // always on the first page however many older comments pile up.
        let path = github.repo_path("issues/comments?sort=created&direction=desc&per_page=100");
        let reply = github
            .call(Method::GET, &path, None, self.comments_etag.as_deref())
            .await?;
        if reply.status == StatusCode::NOT_MODIFIED {
            return Ok(());
        }

        let issue_suffix = format!("/issues/{}", self.issue);
        let mut comments = reply.body.as_array().cloned().unwrap_or_default();
        comments.reverse();
        for comment in comments {
            let on_this_issue = comment
                .get("issue_url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.ends_with(&issue_suffix));
            if !on_this_issue {
                continue;
            }
            let Some(request) = PendingRequest::from_comment(&comment) else {
                continue;
            };
            if self.answered.contains(&request.comment_id) {
                continue;
            }
            if !request.author.eq_ignore_ascii_case(&self.login) {
                log::warn!(
                    "Praxis Remote ignored a request from {}, who does not own the token",
                    request.author
                );
                self.answered.insert(request.comment_id);
                continue;
            }
            self.answered.insert(request.comment_id);

            let answer = self.answer(&request, cx).await;
            let body = response_body(&request.id, answer);
            if let Err(error) = send_answer(github, request.comment_id, body.clone()).await {
                self.unsent.push((request.comment_id, body));
                return Err(error);
            }
        }
        // Only once every request on the page is answered, so that a failure
        // part way through is not hidden behind an unchanged page.
        self.comments_etag = reply.etag;
        Ok(())
    }

    async fn answer(&mut self, request: &PendingRequest, cx: &mut AsyncApp) -> Result<Value> {
        let parsed = request
            .parsed
            .as_ref()
            .map_err(|error| anyhow!("{error}"))?;
        let created_at = request
            .created_at
            .context("GitHub did not say when the request was sent")?;
        let age = Utc::now().signed_duration_since(created_at).num_seconds();
        if age > MAX_REQUEST_AGE_SECONDS {
            bail!(
                "this request was sent {} minutes ago, while Praxis was not running on {}; \
                 send it again",
                age / 60,
                self.device
            );
        }
        self.settle_at = Some(Instant::now() + SETTLE_DELAY);
        match parsed.op.as_str() {
            "batch" => {
                let requests = parsed
                    .args
                    .get("requests")
                    .and_then(Value::as_array)
                    .context("expected \"requests\"")?;
                let mut results = Vec::with_capacity(requests.len());
                for item in requests {
                    let op = item.get("op").and_then(Value::as_str).unwrap_or_default();
                    let args = item.get("args").cloned().unwrap_or(Value::Null);
                    let result = if op == "batch" {
                        Err(anyhow!("a batch cannot contain another batch"))
                    } else {
                        self.answer_op(op, &args, cx).await
                    };
                    results.push(match result {
                        Ok(result) => json!({ "ok": true, "result": result }),
                        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                    });
                }
                Ok(json!({ "results": results }))
            }
            op => self.answer_op(op, &parsed.args, cx).await,
        }
    }

    async fn answer_op(&mut self, op: &str, args: &Value, cx: &mut AsyncApp) -> Result<Value> {
        if op == "watch" {
            let seconds = args
                .get("seconds")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_WATCH_SECONDS)
                .clamp(1, MAX_WATCH_SECONDS);
            let watch = Watch {
                window: args.get("window").and_then(Value::as_u64),
                session_id: args
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                until: Utc::now() + chrono::Duration::seconds(seconds),
            };
            let until = watch.until.to_rfc3339();
            self.watch = Some(watch);
            self.publish_now = true;
            return Ok(json!({ "until": until }));
        }
        let device = self.device.clone();
        let task = cx.update(|cx| handle(op, args, &device, cx));
        task.await
    }

    async fn publish(&mut self, github: &GitHub, cx: &mut AsyncApp) -> Result<()> {
        let now = Utc::now();
        if self.watch.as_ref().is_some_and(|watch| watch.until <= now) {
            self.watch = None;
        }
        let since_published = self.published_at.map(|at| at.elapsed());
        let heartbeat_due = since_published.is_none_or(|elapsed| elapsed >= HEARTBEAT_INTERVAL);
        if self.watch.is_none() && !heartbeat_due && !self.publish_now {
            return Ok(());
        }

        let watch = self.watch.clone();
        let device = self.device.clone();
        let state = cx.update(|cx| snapshot(&device, watch.as_ref(), cx));
        let fingerprint = state.to_string();
        let changed = self.published.as_deref() != Some(fingerprint.as_str());
        let settled = self.settle_at.is_some_and(|at| Instant::now() >= at);
        let gap = if settled {
            SETTLE_DELAY
        } else {
            WATCHED_PUBLISH_INTERVAL
        };
        let due = self.publish_now
            || heartbeat_due
            || (changed && since_published.is_none_or(|elapsed| elapsed >= gap));
        if !due {
            return Ok(());
        }

        let body = issue_body(&self.device, self.started_at, now, state);
        let path = github.repo_path(&format!("issues/{}", self.issue));
        let patch = json!({ "body": body, "state": "open" });
        match github.call(Method::PATCH, &path, Some(patch), None).await {
            Ok(_) => {}
            Err(error) if is_gone(&error) => {
                self.recreate_issue(github).await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        self.published = Some(fingerprint);
        self.published_at = Some(Instant::now());
        self.publish_now = false;
        if settled {
            self.settle_at = None;
        }
        Ok(())
    }

    async fn recreate_issue(&mut self, github: &GitHub) -> Result<()> {
        log::info!("Praxis Remote's issue is gone; opening a new one");
        self.issue = find_or_create_issue(github, &self.device, &self.login).await?;
        self.comments_etag = None;
        self.publish_now = true;
        Ok(())
    }
}

fn is_gone(error: &anyhow::Error) -> bool {
    matches!(
        github_status(error),
        Some(StatusCode::NOT_FOUND | StatusCode::GONE)
    )
}

/// Answers a request by replacing the comment that carried it.
async fn send_answer(github: &GitHub, comment_id: u64, body: String) -> Result<()> {
    let path = github.repo_path(&format!("issues/comments/{comment_id}"));
    match github
        .call(Method::PATCH, &path, Some(json!({ "body": body })), None)
        .await
    {
        Ok(_) => Ok(()),
        // The phone gave up and removed its request.
        Err(error) if is_gone(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn find_or_create_issue(github: &GitHub, device: &str, login: &str) -> Result<u64> {
    let title = format!("{TITLE_PREFIX}{device}");
    for state in ["open", "closed"] {
        let path = github.repo_path(&format!(
            "issues?state={state}&creator={login}&per_page=100&sort=updated"
        ));
        let reply = github.call(Method::GET, &path, None, None).await?;
        let existing = reply.body.as_array().and_then(|issues| {
            issues.iter().find_map(|issue| {
                let is_pull_request = issue.get("pull_request").is_some();
                let matches = issue.get("title").and_then(Value::as_str) == Some(title.as_str());
                (matches && !is_pull_request)
                    .then(|| issue.get("number").and_then(Value::as_u64))
                    .flatten()
            })
        });
        if let Some(number) = existing {
            return Ok(number);
        }
    }
    let now = Utc::now();
    let body = issue_body(device, now, now, json!({}));
    let reply = github
        .call(
            Method::POST,
            &github.repo_path("issues"),
            Some(json!({ "title": title, "body": body })),
            None,
        )
        .await?;
    reply
        .body
        .get("number")
        .and_then(Value::as_u64)
        .context("GitHub did not say which issue it opened")
}

#[derive(Deserialize)]
struct RequestPayload {
    id: String,
    op: String,
    #[serde(default)]
    args: Value,
}

struct PendingRequest {
    comment_id: u64,
    author: String,
    created_at: Option<DateTime<Utc>>,
    /// The request's id, echoed in the answer so the phone can match it.
    id: String,
    parsed: Result<RequestPayload, String>,
}

impl PendingRequest {
    /// The request a comment carries, or `None` for any other comment,
    /// including one that has already been answered.
    fn from_comment(comment: &Value) -> Option<Self> {
        let body = comment.get("body").and_then(Value::as_str)?;
        let rest = body.trim_start().strip_prefix(REQUEST_MARKER)?;
        let comment_id = comment.get("id").and_then(Value::as_u64)?;
        let author = comment
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let created_at = comment
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        let parsed = parse_request(rest);
        let id = match &parsed {
            Ok(payload) => payload.id.clone(),
            Err(_) => String::from("unknown"),
        };
        Some(Self {
            comment_id,
            author,
            created_at,
            id,
            parsed,
        })
    }
}

fn parse_request(text: &str) -> Result<RequestPayload, String> {
    let json = json_object_in(text).ok_or("the request has no JSON in it")?;
    let payload: RequestPayload =
        serde_json::from_str(json).map_err(|error| format!("the request is not valid: {error}"))?;
    if !is_valid_request_id(&payload.id) {
        return Err("the request id may only use letters, digits, '-' and '_'".into());
    }
    Ok(payload)
}

fn is_valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// The text from the first `{` to the last `}`, which is how both ends pull
/// the JSON out of a fenced block without caring about the fence itself.
fn json_object_in(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start < end).then(|| &text[start..=end])
}

fn response_body(id: &str, answer: Result<Value>) -> String {
    let id = if is_valid_request_id(id) {
        id
    } else {
        "unknown"
    };
    let mut payload = match answer {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }),
        Err(error) => json!({ "id": id, "ok": false, "error": format!("{error:#}") }),
    }
    .to_string();
    if payload.len() > MAX_BODY_LEN {
        payload = json!({
            "id": id,
            "ok": false,
            "error": format!("the answer was too large to send ({} bytes)", payload.len()),
        })
        .to_string();
    }
    format!("{RESPONSE_MARKER}{id} -->\n```json\n{payload}\n```\n")
}

fn issue_body(
    device: &str,
    started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    mut snapshot: Value,
) -> String {
    let meta = json!({
        "version": 1,
        "device": device,
        "started_at": started_at.to_rfc3339(),
        "last_seen": now.to_rfc3339(),
    })
    .to_string()
    // Only ever inside JSON strings, so escaping it keeps the JSON the same
    // while stopping a device name from closing the comment around it.
    .replace('>', "\\u003e");
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("updated_at".into(), json!(now.to_rfc3339()));
    }
    let build = |state: &Value| {
        format!(
            "{DEVICE_MARKER}{meta} -->\n\
             Praxis on **{device}** is controlled from Praxis Remote through this issue. \
             Requests arrive as comments and are answered in place. Keep this repository \
             private, and do not edit this issue by hand.\n\n\
             {STATE_MARKER}\n```json\n{state}\n```\n"
        )
    };
    let body = build(&snapshot);
    if body.len() <= MAX_BODY_LEN {
        return body;
    }
    // A transcript is fitted to the budget before it gets here, so this only
    // guards against a snapshot that is unexpectedly large in other ways.
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("thread".into(), Value::Null);
        object.insert(
            "thread_error".into(),
            json!("the conversation was too large to show"),
        );
    }
    let body = build(&snapshot);
    if body.len() <= MAX_BODY_LEN {
        return body;
    }
    build(&json!({
        "updated_at": now.to_rfc3339(),
        "watch": null,
        "status": null,
        "thread": null,
        "thread_error": "what Praxis is doing was too large to show",
    }))
}

/// What the phone sees without asking: every window, and the conversation it
/// is watching.
fn snapshot(device: &str, watch: Option<&Watch>, cx: &mut App) -> Value {
    let status = status(device, cx);
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
        if text.len() > budget && !entries.is_empty() {
            break;
        }
        budget = budget.saturating_sub(text.len());
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
    let truncated = entries.len() > MAX_DIR_ENTRIES;
    let entries: Vec<Value> = entries
        .into_iter()
        .take(MAX_DIR_ENTRIES)
        .map(|(dir, name)| json!({ "path": format!("{display}/{name}"), "name": name, "dir": dir }))
        .collect();
    Ok(json!({ "path": display, "entries": entries, "truncated": truncated }))
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
    let content = truncate(&text, FILE_LIMIT);
    Ok(json!({
        "path": display,
        "truncated": content.len() < text.len(),
        "size": metadata.len(),
        "content": content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(body: &str, login: &str) -> Value {
        json!({
            "id": 7,
            "body": body,
            "user": { "login": login },
            "created_at": "2026-01-01T00:00:00Z",
        })
    }

    #[test]
    fn a_request_comment_is_read_and_anything_else_is_ignored() {
        let request = PendingRequest::from_comment(&comment(
            "<!-- praxis-request -->\n```json\n{\"id\":\"a1\",\"op\":\"status\",\"args\":{}}\n```",
            "someone",
        ))
        .expect("a request comment");
        assert_eq!(request.comment_id, 7);
        assert_eq!(request.author, "someone");
        assert_eq!(request.id, "a1");
        assert_eq!(request.parsed.as_ref().map(|p| p.op.as_str()), Ok("status"));

        let answered = response_body("a1", Ok(json!({ "done": true })));
        assert!(PendingRequest::from_comment(&comment(&answered, "someone")).is_none());
        assert!(PendingRequest::from_comment(&comment("Looks good!", "someone")).is_none());
    }

    #[test]
    fn a_malformed_request_is_answered_with_the_reason() {
        let request =
            PendingRequest::from_comment(&comment("<!-- praxis-request -->\nnot json", "someone"))
                .expect("still a request");
        assert!(request.parsed.is_err());
        assert_eq!(request.id, "unknown");

        let unsafe_id = PendingRequest::from_comment(&comment(
            "<!-- praxis-request -->\n{\"id\":\"x --> <b>\",\"op\":\"status\"}",
            "someone",
        ))
        .expect("still a request");
        assert!(
            unsafe_id.parsed.is_err(),
            "an id must not be able to end the marker"
        );
    }

    #[test]
    fn an_answer_carries_its_id_and_parses_back() {
        let body = response_body("r-9", Ok(json!({ "queued": false })));
        assert!(body.starts_with("<!-- praxis-response r-9 -->"));
        let payload: Value =
            serde_json::from_str(json_object_in(&body).expect("json")).expect("valid json");
        assert_eq!(payload["id"], "r-9");
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["result"]["queued"], false);

        let body = response_body("r-9", Err(anyhow!("no window")));
        let payload: Value =
            serde_json::from_str(json_object_in(&body).expect("json")).expect("valid json");
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"], "no window");
    }

    #[test]
    fn an_oversized_answer_is_replaced_by_an_error() {
        let body = response_body(
            "big",
            Ok(json!({ "content": "x".repeat(MAX_BODY_LEN * 2) })),
        );
        assert!(body.len() < MAX_BODY_LEN);
        let payload: Value =
            serde_json::from_str(json_object_in(&body).expect("json")).expect("valid json");
        assert_eq!(payload["ok"], false);
    }

    #[test]
    fn the_issue_body_carries_the_heartbeat_and_the_snapshot() {
        let now = Utc::now();
        let body = issue_body("Laptop", now, now, json!({ "status": { "windows": [] } }));
        let device_line = body.lines().next().expect("a first line");
        assert!(device_line.starts_with(DEVICE_MARKER));
        assert!(device_line.contains("\"device\":\"Laptop\""));
        let (_, state) = body.split_once(STATE_MARKER).expect("a state marker");
        let state: Value =
            serde_json::from_str(json_object_in(state).expect("json")).expect("valid json");
        assert_eq!(state["status"]["windows"], json!([]));
        assert!(state["updated_at"].is_string());

        let huge = json!({ "thread": { "text": "x".repeat(MAX_BODY_LEN * 2) } });
        let body = issue_body("Laptop", now, now, huge);
        assert!(body.len() <= MAX_BODY_LEN);
        assert!(body.contains("too large to show"));

        let huge_status = json!({ "status": { "text": "x".repeat(MAX_BODY_LEN * 2) } });
        let body = issue_body("Laptop", now, now, huge_status);
        assert!(body.len() <= MAX_BODY_LEN);
        assert!(body.contains("too large to show"));
    }

    #[test]
    fn a_device_name_cannot_close_the_heartbeat_comment() {
        let now = Utc::now();
        let body = issue_body("evil --> <b>", now, now, json!({}));
        let device_line = body.lines().next().expect("a first line");
        let meta = device_line
            .strip_prefix(DEVICE_MARKER)
            .and_then(|rest| rest.strip_suffix(" -->"))
            .expect("one comment on the line");
        assert!(!meta.contains("-->"));
        let meta: Value = serde_json::from_str(meta).expect("still valid json");
        assert_eq!(meta["device"], "evil --> <b>");
    }

    #[test]
    fn only_owner_slash_name_repositories_are_accepted() {
        assert!(validate_repository("someone/praxis-remote").is_ok());
        assert!(validate_repository("someone").is_err());
        assert!(validate_repository("someone/../x").is_err());
        assert!(validate_repository("a/b?c").is_err());
        assert!(validate_repository("someone/..").is_err());
        assert!(validate_repository("../..").is_err());
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
}
