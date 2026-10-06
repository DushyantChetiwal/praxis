//! The channel to the phones: finding or creating the gist, answering the
//! requests posted on it, pairing new phones and publishing what Praxis is
//! doing, all as `docs/src/ai/praxis-remote-protocol.md` specifies.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::Either;
use gpui::{App, AppContext as _, AsyncApp, PromptLevel, Task, WeakEntity};
use http_client::{HttpClient, Method, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use util::ResultExt as _;

use super::crypto::{self, Key, PROTOCOL, PairingKey, PairingSecrets};
use super::github::{self, Api, SignedOut, Tokens};
use super::store::{self, LocalState, PhoneInfo, Secrets};
use super::{
    PraxisRemote, RemoteStatus, Watch, handle_paired, resolve_device_name, snapshot, status,
};
use crate::automation::workspace_windows;

const META_FILE: &str = "praxis-remote.json";
const STATE_FILE: &str = "state.json";
const STATE_VERSION: u32 = 1;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
/// How often a changing snapshot is republished while a phone watches. Each
/// write counts against GitHub's limit on content changes, which the phones'
/// requests share, so this stays well under it.
const WATCHED_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
/// How soon after a request the snapshot may be republished, so the phone
/// sees the effect of what it asked for quickly.
const SETTLE_DELAY: Duration = Duration::from_secs(3);
const ERROR_BACKOFF: Duration = Duration::from_secs(60);
const MAX_TRANSIENT_BACKOFF: Duration = Duration::from_secs(30);
/// Old comments are only noticed on a full read of the list, so one is made
/// this often even while the list looks unchanged.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);
/// A refresh that GitHub still refuses within this long means the sign-in
/// itself is gone.
const REFRESH_GRACE: Duration = Duration::from_secs(60);
const DEFAULT_WATCH_SECONDS: i64 = 300;
const MAX_WATCH_SECONDS: i64 = 900;
/// Bound how long an offline phone request remains actionable.
const MAX_REQUEST_AGE_SECONDS: i64 = 300;
const MAX_CLOCK_AHEAD_SECONDS: i64 = 120;
const PAIRING_TIMEOUT_SECONDS: i64 = 300;
const CLEANUP_AGE_SECONDS: i64 = 600;
/// Plain-text caps that keep every comment and file within GitHub's limits.
pub(super) const MAX_ANSWER_LEN: usize = 46_000;
const MAX_SNAPSHOT_LEN: usize = 64_000;
const MAX_PHONE_NAME_CHARS: usize = 60;
/// Every phone's key shares one keychain entry, and Windows caps an entry at
/// 2,560 bytes.
const MAX_PHONES: usize = 16;
const COMMENTS_PER_PAGE: usize = 100;
const GISTS_PER_PAGE: usize = 100;
const GIST_PAGES: usize = 3;
const REFUSED_SIGN_IN: &str = "GitHub no longer accepts this computer's sign-in; sign in again";

fn recovery_delay(error: &anyhow::Error, failures: u32) -> Duration {
    if let Some(delay) = github::retry_after(error) {
        return delay.max(POLL_INTERVAL);
    }
    if github::github_status(error) == Some(StatusCode::FORBIDDEN) {
        return ERROR_BACKOFF;
    }
    // A brief network failure should recover inside the phone's response
    // window, not force every pending request through a minute-long blackout.
    (POLL_INTERVAL * (1 << failures.saturating_sub(1).min(4))).min(MAX_TRANSIENT_BACKOFF)
}

/// What the rest of Praxis asks of a running channel.
#[derive(Debug)]
pub(super) enum Command {
    Unpair(String),
    PromptAnswered { comment_id: u64, allowed: bool },
}

/// Runs the channel until it stops for good, which it reports.
pub(super) async fn run(
    this: WeakEntity<PraxisRemote>,
    http: Arc<dyn HttpClient>,
    sender: mpsc::UnboundedSender<Command>,
    commands: mpsc::UnboundedReceiver<Command>,
    cx: &mut AsyncApp,
) {
    let Err(error) = run_channel(&this, http, sender, commands, cx).await else {
        return;
    };
    log::error!("Praxis Remote stopped: {error:#}");
    let sign_in = github::is_signed_out(&error);
    this.update(cx, |this, cx| {
        this.status = RemoteStatus::Failed {
            reason: format!("{error:#}"),
            sign_in,
        };
        cx.notify();
    })
    .log_err();
}

async fn run_channel(
    this: &WeakEntity<PraxisRemote>,
    http: Arc<dyn HttpClient>,
    sender: mpsc::UnboundedSender<Command>,
    commands: mpsc::UnboundedReceiver<Command>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some(state) = cx.background_spawn(async { store::load_state() }).await? else {
        this.update(cx, |this, cx| {
            this.status = RemoteStatus::Off;
            cx.notify();
        })?;
        return Ok(());
    };
    this.update(cx, |this, cx| {
        this.status = RemoteStatus::Connecting;
        this.login = Some(state.login.clone());
        cx.notify();
    })?;
    let secrets = store::load_secrets(cx).await?.unwrap_or_default();
    let Some(tokens) = secrets.tokens.clone() else {
        return Err(signed_out("Praxis Remote is signed out; sign in again"));
    };
    let device = resolve_device_name().await;
    let api = Api::new(http, tokens);
    let mut channel = Channel::new(this.clone(), api, state, secrets, device);
    channel.relay = Some(super::relay::Relay::start(
        channel.state.channel.clone(),
        channel.device.clone(),
        cx,
    ));
    channel.sync_relay();
    channel.sender = Some(sender);
    channel.commands = Some(commands);
    channel.report_phones(cx)?;
    channel.run(cx).await
}

fn signed_out(message: &str) -> anyhow::Error {
    SignedOut(message.to_string()).into()
}

fn is_unauthorized(error: &anyhow::Error) -> bool {
    github::github_status(error) == Some(StatusCode::UNAUTHORIZED)
}

/// Saves a new sign-in. Signing in again to the same account keeps the
/// channel and the paired phones; another account starts afresh.
pub(super) async fn remember_sign_in(
    login: String,
    tokens: Tokens,
    cx: &mut AsyncApp,
) -> Result<()> {
    let existing = cx.background_spawn(async { store::load_state() }).await?;
    let (state, mut secrets) = match existing {
        Some(state) if state.login.eq_ignore_ascii_case(&login) => {
            let secrets = store::load_secrets(cx).await?.unwrap_or_default();
            (state, secrets)
        }
        _ => {
            let state = LocalState {
                version: STATE_VERSION,
                login,
                channel: crypto::random_id()?,
                gist_id: None,
                phones: Vec::new(),
            };
            (state, Secrets::default())
        }
    };
    secrets.tokens = Some(tokens);
    store::save_secrets(&state.login, &secrets, cx).await?;
    cx.background_spawn(async move { store::save_state(&state) })
        .await
}

/// Deletes the gist, as far as GitHub allows, and everything saved locally.
pub(super) async fn forget_everything(http: Arc<dyn HttpClient>, cx: &mut AsyncApp) -> Result<()> {
    let _channel_lock = cx
        .background_spawn(async { store::acquire_channel_lock() })
        .await?;
    let state = cx
        .background_spawn(async { store::load_state() })
        .await
        .log_err()
        .flatten();
    let secrets = store::load_secrets(cx).await.log_err().flatten();
    let had_secrets = secrets.is_some();
    let tokens = secrets.and_then(|secrets| secrets.tokens);
    if let Some(gist_id) = state.and_then(|state| state.gist_id)
        && let Some(tokens) = tokens
        && let Err(error) = delete_gist(http, tokens, &gist_id).await
    {
        log::warn!("Praxis Remote could not delete its gist: {error:#}");
    }
    if had_secrets {
        store::delete_secrets(cx).await?;
    }
    cx.background_spawn(async { store::delete_state() }).await
}

async fn delete_gist(http: Arc<dyn HttpClient>, mut tokens: Tokens, gist_id: &str) -> Result<()> {
    if !is_gist_id(gist_id) {
        return Ok(());
    }
    if tokens.needs_refresh()
        && let Some(refresh_token) = tokens.refresh_token.clone()
    {
        tokens = github::refresh(&http, &refresh_token).await?;
    }
    let api = Api::new(http, tokens);
    let path = format!("/gists/{gist_id}");
    match api.call(Method::DELETE, &path, None, None).await {
        Ok(_) => Ok(()),
        Err(error) if github::is_gone(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes a phone from what the channel loads next time, for when it is not
/// running to do it itself.
pub(super) async fn forget_phone(phone_id: String, cx: &mut AsyncApp) -> Result<()> {
    let _channel_lock = cx
        .background_spawn(async { store::acquire_channel_lock() })
        .await?;
    let Some(mut state) = cx.background_spawn(async { store::load_state() }).await? else {
        return Ok(());
    };
    state.phones.retain(|phone| phone.id != phone_id);
    if let Some(mut secrets) = store::load_secrets(cx).await? {
        secrets.phone_keys.remove(&phone_id);
        store::save_secrets(&state.login, &secrets, cx).await?;
    }
    cx.background_spawn(async move { store::save_state(&state) })
        .await
}

struct Phone {
    info: PhoneInfo,
    key: Key,
}

struct Channel {
    this: WeakEntity<PraxisRemote>,
    api: Api,
    state: LocalState,
    secrets: Secrets,
    device: String,
    started_at: DateTime<Utc>,
    phones: BTreeMap<String, Phone>,
    relay: Option<super::relay::Relay>,
    /// Set when the phones changed and have not been saved yet.
    phones_dirty: bool,
    sender: Option<mpsc::UnboundedSender<Command>>,
    commands: Option<mpsc::UnboundedReceiver<Command>>,
    queued: Vec<Command>,
    /// The gist in use, once it has been found or created this session.
    gist_id: Option<String>,
    comments_etag: Option<String>,
    cleaned_at: Option<Instant>,
    handled: HashSet<u64>,
    warned: HashSet<u64>,
    seen: SeenRequests,
    /// Answers GitHub refused to take, retried before anything new is read,
    /// since the request they answer has already been carried out.
    unsent: Vec<(u64, String)>,
    /// Phones that asked to be unpaired, once their answer is on its way.
    unpair_after: BTreeSet<String>,
    pairings: BTreeMap<u64, Pairing>,
    /// Pairings whose status is written, in case a list read just after
    /// still shows them unfinished.
    finished_pairings: HashSet<u64>,
    /// The pairing the user is being asked about, one at a time.
    prompt: Option<u64>,
    _prompt_task: Option<Task<()>>,
    watches: HashMap<String, Watch>,
    /// The last snapshots written, without their timestamps, to tell whether
    /// anything changed since.
    published: Option<String>,
    published_at: Option<Instant>,
    /// Set by a request whose effect the phone should see soon.
    settle_at: Option<Instant>,
    /// Set by anything that always deserves a fresh snapshot.
    publish_now: bool,
    refreshed_at: Option<Instant>,
    reported: Option<RemoteStatus>,
    failing: bool,
}

impl Channel {
    fn new(
        this: WeakEntity<PraxisRemote>,
        api: Api,
        state: LocalState,
        secrets: Secrets,
        device: String,
    ) -> Self {
        let phones = load_phones(&state, &secrets);
        Self {
            this,
            api,
            state,
            secrets,
            device,
            started_at: Utc::now(),
            phones,
            relay: None,
            phones_dirty: false,
            sender: None,
            commands: None,
            queued: Vec::new(),
            gist_id: None,
            comments_etag: None,
            cleaned_at: None,
            handled: HashSet::default(),
            warned: HashSet::default(),
            seen: SeenRequests::default(),
            unsent: Vec::new(),
            unpair_after: BTreeSet::new(),
            pairings: BTreeMap::new(),
            finished_pairings: HashSet::default(),
            prompt: None,
            _prompt_task: None,
            watches: HashMap::default(),
            published: None,
            published_at: None,
            settle_at: None,
            publish_now: true,
            refreshed_at: None,
            reported: None,
            failing: false,
        }
    }

    async fn run(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let mut failures = 0u32;
        loop {
            self.apply_commands(cx)?;
            let mut result = self.step(cx).await;
            if result.as_ref().is_err_and(is_unauthorized) {
                result = self.refresh_tokens(true, cx).await;
                if result.is_ok() {
                    continue;
                }
            }
            match result {
                Ok(()) => {
                    failures = 0;
                    if self.failing {
                        log::info!("Praxis Remote reached GitHub again");
                        self.failing = false;
                    }
                    let status = if self.gist_id.is_some() {
                        RemoteStatus::Connected
                    } else {
                        RemoteStatus::Connecting
                    };
                    self.report_status(status, cx)?;
                    let interval = if self
                        .relay
                        .as_ref()
                        .is_some_and(|relay| relay.all_connected())
                    {
                        Duration::from_secs(30)
                    } else {
                        POLL_INTERVAL
                    };
                    self.wait(interval, cx).await;
                }
                Err(error) if github::is_signed_out(&error) => return Err(error),
                Err(error) => {
                    if !self.failing {
                        log::warn!("Praxis Remote could not reach GitHub: {error:#}");
                        self.failing = true;
                    }
                    failures = failures.saturating_add(1);
                    let delay = recovery_delay(&error, failures);
                    let status = RemoteStatus::Offline(format!("{error:#}"));
                    self.report_status(status, cx)?;
                    self.wait(delay, cx).await;
                }
            }
        }
    }

    async fn step(&mut self, cx: &mut AsyncApp) -> Result<()> {
        self.persist(cx).await?;
        if self.api.tokens.needs_refresh() {
            self.refresh_tokens(false, cx).await?;
        }
        if self.gist_id.is_none() {
            self.connect(cx).await?;
        }
        self.send_unsent().await?;
        self.read_comments(cx).await?;
        self.advance_pairings(cx).await?;
        self.publish(cx).await
    }

    /// Sleeps, waking early for a command.
    async fn wait(&mut self, duration: Duration, cx: &mut AsyncApp) {
        let timer = pin!(cx.background_executor().timer(duration));
        let Some(commands) = self.commands.as_mut() else {
            timer.await;
            return;
        };
        let next = futures::future::select(timer, commands.next());
        if let Either::Right((Some(command), _)) = next.await {
            self.queued.push(command);
        }
    }

    fn apply_commands(&mut self, cx: &mut AsyncApp) -> Result<()> {
        if let Some(commands) = self.commands.as_mut() {
            while let Ok(command) = commands.try_recv() {
                self.queued.push(command);
            }
        }
        for command in std::mem::take(&mut self.queued) {
            match command {
                Command::Unpair(phone_id) => self.remove_phone(&phone_id, cx)?,
                Command::PromptAnswered {
                    comment_id,
                    allowed,
                } => self.answer_prompt(comment_id, allowed, cx)?,
            }
        }
        Ok(())
    }

    fn answer_prompt(&mut self, comment_id: u64, allowed: bool, cx: &mut AsyncApp) -> Result<()> {
        if self.prompt == Some(comment_id) {
            self.prompt = None;
            self._prompt_task = None;
        }
        let Some(pairing) = self.pairings.get_mut(&comment_id) else {
            return Ok(());
        };
        let Some(secrets) = pairing.decide(allowed) else {
            return Ok(());
        };
        let info = PhoneInfo {
            id: pairing.phone_id.clone(),
            name: pairing.name.clone(),
            paired_at: Utc::now(),
        };
        log::info!("Praxis Remote paired {:?}", info.name);
        self.add_phone(info, secrets.key, cx)
    }

    fn sync_relay(&self) {
        if let Some(relay) = &self.relay {
            relay.set_phones(
                &self.state.channel,
                self.phones
                    .iter()
                    .map(|(id, phone)| (id.clone(), phone.key)),
            );
        }
    }

    fn add_phone(&mut self, info: PhoneInfo, key: Key, cx: &mut AsyncApp) -> Result<()> {
        self.phones.insert(info.id.clone(), Phone { info, key });
        self.sync_relay();
        self.phones_dirty = true;
        self.publish_now = true;
        self.report_phones(cx)
    }

    fn remove_phone(&mut self, phone_id: &str, cx: &mut AsyncApp) -> Result<()> {
        let Some(phone) = self.phones.remove(phone_id) else {
            return Ok(());
        };
        log::info!("Praxis Remote unpaired {:?}", phone.info.name);
        self.sync_relay();
        self.watches.remove(phone_id);
        cx.update(|cx| super::images::clear(phone_id, cx));
        self.phones_dirty = true;
        self.publish_now = true;
        self.report_phones(cx)
    }

    /// Saves the phones, keys first, so that a phone on the list always has
    /// its key.
    async fn persist(&mut self, cx: &mut AsyncApp) -> Result<()> {
        if !self.phones_dirty {
            return Ok(());
        }
        self.secrets.phone_keys = self
            .phones
            .iter()
            .map(|(id, phone)| (id.clone(), crypto::encode(&phone.key)))
            .collect();
        self.state.phones = self
            .phones
            .values()
            .map(|phone| phone.info.clone())
            .collect();
        store::save_secrets(&self.state.login, &self.secrets, cx).await?;
        self.save_state(cx).await?;
        self.phones_dirty = false;
        Ok(())
    }

    async fn save_state(&self, cx: &mut AsyncApp) -> Result<()> {
        let state = self.state.clone();
        cx.background_spawn(async move { store::save_state(&state) })
            .await
    }

    async fn refresh_tokens(&mut self, after_refusal: bool, cx: &mut AsyncApp) -> Result<()> {
        let refreshed_recently = self
            .refreshed_at
            .is_some_and(|at| at.elapsed() < REFRESH_GRACE);
        if after_refusal && refreshed_recently {
            return Err(signed_out(REFUSED_SIGN_IN));
        }
        let Some(refresh_token) = self.api.tokens.refresh_token.clone() else {
            return Err(signed_out(REFUSED_SIGN_IN));
        };
        let tokens = github::refresh(self.api.http(), &refresh_token).await?;
        self.refreshed_at = Some(Instant::now());
        self.api.tokens = tokens.clone();
        self.secrets.tokens = Some(tokens);
        store::save_secrets(&self.state.login, &self.secrets, cx).await
    }

    fn report_status(&mut self, status: RemoteStatus, cx: &mut AsyncApp) -> Result<()> {
        if self.reported.as_ref() == Some(&status) {
            return Ok(());
        }
        self.reported = Some(status.clone());
        self.this.update(cx, |this, cx| {
            this.status = status;
            cx.notify();
        })
    }

    fn report_phones(&self, cx: &mut AsyncApp) -> Result<()> {
        let phones: Vec<PhoneInfo> = self
            .phones
            .values()
            .map(|phone| phone.info.clone())
            .collect();
        let login = self.state.login.clone();
        let device = self.device.clone();
        self.this.update(cx, |this, cx| {
            this.phones = phones;
            this.login = Some(login);
            this.device = Some(device);
            cx.notify();
        })
    }

    async fn connect(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let known = self.state.gist_id.clone().filter(|id| is_gist_id(id));
        let meta = meta_json(
            &self.state.channel,
            &self.device,
            self.started_at,
            Utc::now(),
            self.phones.keys(),
        );
        let channel = &self.state.channel;
        let found = find_or_create_gist(&self.api, channel, known.as_deref(), &meta).await?;
        if known.as_deref() != Some(found.as_str()) {
            self.state.gist_id = Some(found.clone());
            self.save_state(cx).await?;
        }
        log::info!(
            "Praxis Remote is connected as @{} on {}",
            self.state.login,
            self.device
        );
        self.gist_id = Some(found);
        self.comments_etag = None;
        self.publish_now = true;
        Ok(())
    }

    fn lose_gist(&mut self) {
        log::info!("Praxis Remote's gist is gone; making a new one");
        self.gist_id = None;
        self.comments_etag = None;
        self.unsent.clear();
        self.publish_now = true;
    }

    /// Replaces a comment's text, or returns `false` if it is gone.
    async fn edit_comment(&self, comment_id: u64, body: &str) -> Result<bool> {
        let Some(gist_id) = &self.gist_id else {
            return Ok(false);
        };
        let path = format!("/gists/{gist_id}/comments/{comment_id}");
        let body = json!({ "body": body });
        match self.api.call(Method::PATCH, &path, Some(body), None).await {
            Ok(_) => Ok(true),
            Err(error) if github::is_gone(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn delete_comment(&self, comment_id: u64) -> Result<()> {
        let Some(gist_id) = &self.gist_id else {
            return Ok(());
        };
        let path = format!("/gists/{gist_id}/comments/{comment_id}");
        match self.api.call(Method::DELETE, &path, None, None).await {
            Ok(_) => Ok(()),
            Err(error) if github::is_gone(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn send_unsent(&mut self) -> Result<()> {
        while let Some((comment_id, body)) = self.unsent.first() {
            // A phone that gave up has removed its request, and that is fine.
            self.edit_comment(*comment_id, body).await?;
            self.unsent.remove(0);
        }
        Ok(())
    }

    async fn read_comments(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let Some(gist_id) = self.gist_id.clone() else {
            return Ok(());
        };
        let cleanup_due = self
            .cleaned_at
            .is_none_or(|at| at.elapsed() >= CLEANUP_INTERVAL);
        let etag = if cleanup_due {
            None
        } else {
            self.comments_etag.clone()
        };
        let path = format!("/gists/{gist_id}/comments?per_page={COMMENTS_PER_PAGE}");
        let reply = self.api.call(Method::GET, &path, None, etag.as_deref());
        let first = match reply.await {
            Ok(reply) => reply,
            Err(error) if github::is_gone(&error) => {
                self.lose_gist();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if first.status == StatusCode::NOT_MODIFIED {
            return Ok(());
        }
        let mut comments = comments_in(&first.body);
        // The list is oldest first, so a new request may be past the first
        // page. Cleaning up keeps it short enough that the pages in between
        // do not matter.
        if let Some(last_page) = &first.last_page {
            let reply = self.api.call(Method::GET, last_page, None, None).await?;
            let mut ids: HashSet<u64> = comments.iter().map(|comment| comment.id).collect();
            let more = comments_in(&reply.body);
            comments.extend(more.into_iter().filter(|comment| ids.insert(comment.id)));
        }
        for comment in comments {
            self.handle_comment(comment, cx).await?;
        }
        // Only once every comment is handled, so that a failure part way
        // through is not hidden behind an unchanged list. With more than one
        // page, the first says nothing about the last.
        self.comments_etag = match first.last_page {
            None => first.etag,
            Some(_) => None,
        };
        if cleanup_due {
            self.cleaned_at = Some(Instant::now());
        }
        Ok(())
    }

    async fn handle_comment(&mut self, comment: Comment, cx: &mut AsyncApp) -> Result<()> {
        if should_delete(&comment, &self.state.login, Utc::now()) {
            let stranger = !comment.author.eq_ignore_ascii_case(&self.state.login);
            if stranger && self.warned.insert(comment.id) {
                log::warn!(
                    "Praxis Remote is deleting a comment by {}, who is not signed in here",
                    comment.author
                );
            }
            // Leftovers are harmless, so a failure here waits for next time.
            if let Err(error) = self.delete_comment(comment.id).await {
                log::warn!("Praxis Remote could not delete a comment: {error:#}");
            }
            return Ok(());
        }
        let created_at = comment.created_at;
        match comment.kind {
            CommentKind::Request { phone_id, blob } => {
                self.answer_request(comment.id, &phone_id, &blob, created_at, cx)
                    .await
            }
            CommentKind::Pair(fields) => self.handle_pairing(comment.id, fields, created_at).await,
            CommentKind::Answer | CommentKind::Other => Ok(()),
        }
    }

    async fn answer_request(
        &mut self,
        comment_id: u64,
        phone_id: &str,
        blob: &str,
        created_at: Option<DateTime<Utc>>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if !crypto::is_id(phone_id) || !self.handled.insert(comment_id) {
            return Ok(());
        }
        let key = self.phones.get(phone_id).map(|phone| phone.key);
        let opened = open_request(
            key.as_ref(),
            &self.state.channel,
            phone_id,
            blob,
            created_at,
            Utc::now(),
            &mut self.seen,
            &self.device,
        );
        let channel = self.state.channel.clone();
        let body = match opened {
            Opened::Rejected(reason) => {
                log::warn!("Praxis Remote rejected a request: {reason}");
                rejected_comment(phone_id, &reason)
            }
            Opened::Refused { key, id, reason } => {
                let answer = Err(anyhow!(reason));
                response_comment(&key, &channel, phone_id, &id, answer)?
            }
            Opened::Accepted { key, request } => {
                self.settle_at = Some(Instant::now() + SETTLE_DELAY);
                let answer = self.answer(phone_id, &request, cx).await;
                response_comment(&key, &channel, phone_id, &request.id, answer)?
            }
        };
        let sent = self.edit_comment(comment_id, &body).await;
        if sent.is_err() {
            self.unsent.push((comment_id, body));
        }
        // The answer is sealed already, so a phone that asked to be unpaired
        // can still read it.
        for phone_id in std::mem::take(&mut self.unpair_after) {
            self.remove_phone(&phone_id, cx)?;
        }
        sent.map(|_| ())
    }

    async fn answer(
        &mut self,
        phone_id: &str,
        request: &RequestPayload,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let op = request.op.as_str();
        if op != "batch" {
            return self.answer_op(phone_id, op, &request.args, cx).await;
        }
        let requests = request
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
                self.answer_op(phone_id, op, &args, cx).await
            };
            results.push(match result {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
            });
        }
        Ok(json!({ "results": results }))
    }

    async fn answer_op(
        &mut self,
        phone_id: &str,
        op: &str,
        args: &Value,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        match op {
            "watch" => {
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
                    include_details: args.get("include_details").and_then(Value::as_bool)
                        != Some(false),
                    until: Utc::now() + chrono::Duration::seconds(seconds),
                };
                let until = watch.until.to_rfc3339();
                self.watches.insert(phone_id.to_string(), watch);
                self.publish_now = true;
                Ok(json!({ "until": until }))
            }
            "unpair" => {
                self.unpair_after.insert(phone_id.to_string());
                Ok(json!({ "unpaired": true }))
            }
            op => {
                let device = self.device.clone();
                let task = cx.update(|cx| handle_paired(op, args, &device, phone_id, cx));
                task.await
            }
        }
    }

    async fn handle_pairing(
        &mut self,
        comment_id: u64,
        fields: Map<String, Value>,
        created_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        if let Some(pairing) = self.pairings.get_mut(&comment_id) {
            pairing.update(fields, &self.state.channel);
            return Ok(());
        }
        if self.finished_pairings.contains(&comment_id) {
            return Ok(());
        }
        let (phone_id, name, commit) = match new_pairing(&fields, created_at, Utc::now()) {
            NewPairing::Ignore => return Ok(()),
            NewPairing::Refuse(status) => {
                return self.refuse_pairing(comment_id, fields, status).await;
            }
            NewPairing::Begin {
                phone_id,
                name,
                commit,
            } => (phone_id, name, commit),
        };
        if self.phones.len() >= MAX_PHONES && !self.phones.contains_key(&phone_id) {
            log::warn!("Praxis Remote refused {name:?}: too many phones");
            return self
                .refuse_pairing(comment_id, fields, PairStatus::Denied)
                .await;
        }
        let key = PairingKey::generate()?;
        let began = created_at.unwrap_or_else(Utc::now);
        let pairing = Pairing::begin(fields, phone_id, name, commit, began, key);
        let body = pairing.comment(None);
        if self.edit_comment(comment_id, &body).await? {
            self.pairings.insert(comment_id, pairing);
        }
        Ok(())
    }

    async fn refuse_pairing(
        &mut self,
        comment_id: u64,
        fields: Map<String, Value>,
        status: PairStatus,
    ) -> Result<()> {
        let body = pair_comment(with_status(fields, status));
        self.edit_comment(comment_id, &body).await?;
        self.finished_pairings.insert(comment_id);
        Ok(())
    }

    async fn advance_pairings(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let now = Utc::now();
        for pairing in self.pairings.values_mut() {
            pairing.expire_if_due(now);
        }
        let decided: Vec<(u64, PairStatus)> = self
            .pairings
            .iter()
            .filter_map(|(id, pairing)| match pairing.stage {
                Stage::Deciding(status) => Some((*id, status)),
                _ => None,
            })
            .collect();
        for (comment_id, status) in decided {
            let Some(pairing) = self.pairings.get(&comment_id) else {
                continue;
            };
            let phone_id = pairing.phone_id.clone();
            let body = pairing.comment(Some(status));
            let written = self.edit_comment(comment_id, &body).await?;
            self.pairings.remove(&comment_id);
            self.finished_pairings.insert(comment_id);
            // The phone gave up before it heard, so it never got the key.
            if !written && status == PairStatus::Approved {
                self.remove_phone(&phone_id, cx)?;
            }
        }
        if self.prompt.is_none() {
            let next = self
                .pairings
                .iter()
                .find(|(_, pairing)| pairing.waiting_to_ask())
                .map(|(id, _)| *id);
            if let Some(comment_id) = next {
                self.ask(comment_id, cx);
            }
        }
        Ok(())
    }

    /// Asks the user at the computer whether to allow a phone, with a native
    /// prompt on a Praxis window.
    fn ask(&mut self, comment_id: u64, cx: &mut AsyncApp) {
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let Some(pairing) = self.pairings.get_mut(&comment_id) else {
            return;
        };
        let Stage::AwaitingUser { secrets, asked } = &mut pairing.stage else {
            return;
        };
        let name = &pairing.name;
        let device = &self.device;
        let message = format!("Allow “{name}” to control Praxis on {device}?");
        let detail = pairing_detail(&secrets.code);
        let Some(answer) = cx.update(|cx| show_prompt(&message, &detail, cx)) else {
            // There is no window to ask in yet; this is tried again.
            return;
        };
        *asked = true;
        self.prompt = Some(comment_id);
        self._prompt_task = Some(cx.spawn(async move |_| {
            let allowed = answer.await.is_ok_and(|index| index == 0);
            let command = Command::PromptAnswered {
                comment_id,
                allowed,
            };
            sender.unbounded_send(command).log_err();
        }));
    }

    async fn publish(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let Some(gist_id) = self.gist_id.clone() else {
            return Ok(());
        };
        let now = Utc::now();
        self.watches.retain(|_, watch| watch.until > now);
        let since_published = self.published_at.map(|at| at.elapsed());
        let heartbeat_due = since_published.is_none_or(|elapsed| elapsed >= HEARTBEAT_INTERVAL);
        if self.watches.is_empty() && !heartbeat_due && !self.publish_now {
            return Ok(());
        }

        let snapshots = self.snapshots(cx);
        let fingerprint = serde_json::to_string(&snapshots)?;
        let changed = self.published.as_deref() != Some(fingerprint.as_str());
        let settled = self.settle_at.is_some_and(|at| Instant::now() >= at);
        let gap = if settled {
            SETTLE_DELAY
        } else {
            // Passive updates share GitHub's quota with command replies and
            // the phone's reads. Preserve headroom before the quota is empty.
            self.api.passive_publish_interval(WATCHED_PUBLISH_INTERVAL)
        };
        let due = self.publish_now
            || heartbeat_due
            || (changed && since_published.is_none_or(|elapsed| elapsed >= gap));
        if !due {
            return Ok(());
        }

        let meta = meta_json(
            &self.state.channel,
            &self.device,
            self.started_at,
            now,
            self.phones.keys(),
        );
        let state = state_json(&self.state.channel, &snapshots, &self.phones, now)?;
        let files = json!({
            META_FILE: { "content": meta },
            STATE_FILE: { "content": state },
        });
        let path = format!("/gists/{gist_id}");
        let body = json!({ "files": files });
        match self.api.call(Method::PATCH, &path, Some(body), None).await {
            Ok(_) => {}
            Err(error) if github::is_gone(&error) => {
                self.lose_gist();
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

    /// Each paired phone's snapshot, without a timestamp. Nothing is looked
    /// at while no phone is paired.
    fn snapshots(&self, cx: &mut AsyncApp) -> BTreeMap<String, Value> {
        if self.phones.is_empty() {
            return BTreeMap::new();
        }
        let device = self.device.clone();
        let watches: Vec<(String, Option<Watch>)> = self
            .phones
            .keys()
            .map(|id| (id.clone(), self.watches.get(id).cloned()))
            .collect();
        cx.update(|cx| {
            let status = status(&device, cx);
            watches
                .into_iter()
                .map(|(id, watch)| {
                    let snapshot = snapshot(&status, watch.as_ref(), cx);
                    (id, snapshot)
                })
                .collect()
        })
    }
}

fn load_phones(state: &LocalState, secrets: &Secrets) -> BTreeMap<String, Phone> {
    let mut phones = BTreeMap::new();
    for info in &state.phones {
        let name = &info.name;
        let Some(key) = secrets.phone_keys.get(&info.id) else {
            log::warn!("Praxis Remote dropped {name:?}, whose key is missing");
            continue;
        };
        match crypto::decode_key(key) {
            Ok(key) => {
                let phone = Phone {
                    info: info.clone(),
                    key,
                };
                phones.insert(info.id.clone(), phone);
            }
            Err(error) => log::warn!("Praxis Remote dropped {name:?}: {error:#}"),
        }
    }
    phones
}

fn is_gist_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The channel a gist carries, if it is a Praxis Remote gist of this version.
fn gist_channel(gist: &Value) -> Option<String> {
    let file = gist.get("files")?.get(META_FILE)?;
    let meta: Value = serde_json::from_str(file.get("content")?.as_str()?).ok()?;
    if meta.get("protocol").and_then(Value::as_str) != Some(PROTOCOL) {
        return None;
    }
    let channel = meta.get("channel").and_then(Value::as_str)?;
    crypto::is_id(channel).then(|| channel.to_string())
}

async fn is_our_gist(api: &Api, id: &str, channel: &str) -> Result<bool> {
    let path = format!("/gists/{id}");
    match api.call(Method::GET, &path, None, None).await {
        Ok(reply) => Ok(gist_channel(&reply.body).as_deref() == Some(channel)),
        Err(error) if github::is_gone(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn find_or_create_gist(
    api: &Api,
    channel: &str,
    known: Option<&str>,
    meta: &str,
) -> Result<String> {
    if let Some(id) = known
        && is_our_gist(api, id, channel).await?
    {
        return Ok(id.to_string());
    }
    for page in 1..=GIST_PAGES {
        let path = format!("/gists?per_page={GISTS_PER_PAGE}&page={page}");
        let reply = api.call(Method::GET, &path, None, None).await?;
        let gists = reply.body.as_array().cloned().unwrap_or_default();
        for gist in &gists {
            let Some(id) = gist.get("id").and_then(Value::as_str) else {
                continue;
            };
            let files = gist.get("files");
            let has_meta = files.is_some_and(|files| files.get(META_FILE).is_some());
            let candidate = has_meta && is_gist_id(id) && Some(id) != known;
            if candidate && is_our_gist(api, id, channel).await? {
                return Ok(id.to_string());
            }
        }
        if gists.len() < GISTS_PER_PAGE {
            break;
        }
    }
    let files = json!({
        META_FILE: { "content": meta },
        STATE_FILE: { "content": "{}" },
    });
    let body = json!({
        "description": "Praxis Remote: the end-to-end encrypted channel to your phones",
        "public": false,
        "files": files,
    });
    let reply = api.call(Method::POST, "/gists", Some(body), None).await?;
    let id = reply.body.get("id").and_then(Value::as_str);
    id.filter(|id| is_gist_id(id))
        .map(str::to_string)
        .context("GitHub did not say which gist it created")
}

fn show_prompt(message: &str, detail: &str, cx: &mut App) -> Option<oneshot::Receiver<usize>> {
    let windows = workspace_windows(cx);
    let window = cx
        .active_window()
        .filter(|active| windows.contains(active))
        .or_else(|| windows.first().copied())?;
    let answers = ["Allow", "Deny"];
    window
        .update(cx, |_, window, cx| {
            window.activate_window();
            window.prompt(PromptLevel::Warning, message, Some(detail), &answers, cx)
        })
        .log_err()
}

fn pairing_detail(code: &str) -> String {
    let code = crypto::display_code(code);
    format!(
        "Allow it only if the phone shows the code {code}.\n\n\
         The phone will be able to see your Praxis windows and conversations, browse folders \
         on this computer and open them in new windows, read files in open projects, choose \
         models, send or steer messages, answer questions and permission requests, and run \
         or stop plans. You can unpair it at any time from Praxis Remote."
    )
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `praxis-remote.json`, which lets an unpaired phone find and name this
/// computer.
fn meta_json<'a>(
    channel: &str,
    device: &str,
    started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    phones: impl IntoIterator<Item = &'a String>,
) -> String {
    let phones: Vec<&String> = phones.into_iter().collect();
    let meta = json!({
        "protocol": PROTOCOL,
        "channel": channel,
        "device": device,
        "started_at": rfc3339(started_at),
        "last_seen": rfc3339(now),
        "phones": phones,
        "nostr": 1,
    });
    format!("{meta:#}")
}

/// `state.json`: each phone's snapshot, sealed for that phone alone.
fn state_json(
    channel: &str,
    snapshots: &BTreeMap<String, Value>,
    phones: &BTreeMap<String, Phone>,
    now: DateTime<Utc>,
) -> Result<String> {
    let mut state = Map::new();
    for (phone_id, snapshot) in snapshots {
        let Some(phone) = phones.get(phone_id) else {
            continue;
        };
        let plain = fit_snapshot(snapshot.clone(), now);
        let aad = crypto::state_aad(channel, phone_id);
        let blob = crypto::seal(&phone.key, &aad, plain.as_bytes())?;
        state.insert(phone_id.clone(), Value::String(blob));
    }
    Ok(Value::Object(state).to_string())
}

/// A snapshot as text, with its timestamp, cut down if it is too large.
pub(super) fn fit_snapshot(mut snapshot: Value, now: DateTime<Utc>) -> String {
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("updated_at".into(), json!(rfc3339(now)));
    }
    let text = snapshot.to_string();
    if text.len() <= MAX_SNAPSHOT_LEN {
        return text;
    }
    // A transcript is fitted to its budget before it gets here, so this only
    // guards against a snapshot that is unexpectedly large in other ways.
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("thread".into(), Value::Null);
        let error = json!("the conversation was too large to show");
        object.insert("thread_error".into(), error);
    }
    let text = snapshot.to_string();
    if text.len() <= MAX_SNAPSHOT_LEN {
        return text;
    }
    json!({
        "updated_at": rfc3339(now),
        "watch": null,
        "status": null,
        "thread": null,
        "thread_error": "what Praxis is doing was too large to show",
    })
    .to_string()
}

#[derive(Debug)]
struct Comment {
    id: u64,
    author: String,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    kind: CommentKind,
}

#[derive(Debug, PartialEq)]
enum CommentKind {
    Request {
        phone_id: String,
        blob: String,
    },
    /// A response or a rejection, written by this computer.
    Answer,
    Pair(Map<String, Value>),
    Other,
}

fn comments_in(body: &Value) -> Vec<Comment> {
    let Some(comments) = body.as_array() else {
        return Vec::new();
    };
    comments.iter().filter_map(parse_comment).collect()
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(text).ok()?;
    Some(time.with_timezone(&Utc))
}

fn time_field(comment: &Value, key: &str) -> Option<DateTime<Utc>> {
    parse_time(comment.get(key)?.as_str()?)
}

fn parse_comment(comment: &Value) -> Option<Comment> {
    let id = comment.get("id").and_then(Value::as_u64)?;
    let author = comment
        .get("user")
        .and_then(|user| user.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let body = comment.get("body").and_then(Value::as_str);
    Some(Comment {
        id,
        author: author.to_string(),
        created_at: time_field(comment, "created_at"),
        updated_at: time_field(comment, "updated_at"),
        kind: classify(body.unwrap_or_default()),
    })
}

/// What a comment is, from its first line.
fn classify(body: &str) -> CommentKind {
    let body = body.replace("\r\n", "\n");
    let (first, rest) = body.split_once('\n').unwrap_or((body.as_str(), ""));
    let words: Vec<&str> = first.split_whitespace().collect();
    match words.as_slice() {
        [PROTOCOL, "request", phone_id] => CommentKind::Request {
            phone_id: phone_id.to_string(),
            blob: rest.trim().to_string(),
        },
        [PROTOCOL, "response", _, _] | [PROTOCOL, "rejected", _] => CommentKind::Answer,
        [PROTOCOL, "pair"] => {
            let fields = json_object_in(rest).and_then(|json| serde_json::from_str(json).ok());
            fields.map_or(CommentKind::Other, CommentKind::Pair)
        }
        _ => CommentKind::Other,
    }
}

/// The text from the first `{` to the last `}`.
fn json_object_in(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start < end).then(|| &text[start..=end])
}

fn age_in_seconds(time: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    now.signed_duration_since(time).num_seconds()
}

fn older_than(time: Option<DateTime<Utc>>, seconds: i64, now: DateTime<Utc>) -> bool {
    time.is_some_and(|time| age_in_seconds(time, now) > seconds)
}

/// Whether housekeeping removes a comment: anything by another account, and
/// this computer's own answers and finished pairings once they are old, in
/// case a phone could not remove them.
fn should_delete(comment: &Comment, login: &str, now: DateTime<Utc>) -> bool {
    if !comment.author.eq_ignore_ascii_case(login) {
        return true;
    }
    let finished = match &comment.kind {
        CommentKind::Answer => true,
        CommentKind::Pair(fields) => fields.contains_key("status"),
        CommentKind::Request { .. } | CommentKind::Other => false,
    };
    let changed_at = comment.updated_at.or(comment.created_at);
    finished && older_than(changed_at, CLEANUP_AGE_SECONDS, now)
}

#[derive(Debug, Deserialize)]
struct RequestPayload {
    id: String,
    op: String,
    #[serde(default)]
    args: Value,
    sent_at: String,
}

#[derive(Debug)]
enum Opened {
    /// The request cannot be used at all, and is answered in plain text.
    Rejected(String),
    /// The request was read but is refused, with an encrypted error.
    Refused {
        key: Key,
        id: String,
        reason: String,
    },
    Accepted {
        key: Key,
        request: RequestPayload,
    },
}

/// Request ids this computer has seen, per phone, for as long as a request
/// with that id could still be accepted.
#[derive(Default)]
struct SeenRequests {
    sent: HashMap<(String, String), DateTime<Utc>>,
}

impl SeenRequests {
    /// Records a request, returning `false` if it was seen before.
    fn insert(
        &mut self,
        phone_id: &str,
        id: &str,
        sent_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        let window = MAX_REQUEST_AGE_SECONDS + MAX_CLOCK_AHEAD_SECONDS;
        let fresh = |at: &DateTime<Utc>| age_in_seconds(*at, now) <= window;
        self.sent.retain(|_, at| fresh(at));
        let key = (phone_id.to_string(), id.to_string());
        self.sent.insert(key, sent_at).is_none()
    }
}

fn is_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Decrypts a request and decides whether to carry it out.
fn open_request(
    key: Option<&Key>,
    channel: &str,
    phone_id: &str,
    blob: &str,
    created_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    seen: &mut SeenRequests,
    device: &str,
) -> Opened {
    let Some(key) = key.copied() else {
        let reason = format!("this phone is not paired with {device}; pair it again");
        return Opened::Rejected(reason);
    };
    let aad = crypto::request_aad(channel, phone_id);
    let Ok(plain) = crypto::open(&key, &aad, blob) else {
        let reason = format!("{device} could not decrypt the request; pair again");
        return Opened::Rejected(reason);
    };
    let request = match serde_json::from_slice::<RequestPayload>(&plain) {
        Ok(request) if is_request_id(&request.id) => request,
        _ => {
            let reason = format!("{device} could not read the request");
            return Opened::Rejected(reason);
        }
    };
    let refuse = |reason: String| Opened::Refused {
        key,
        id: request.id.clone(),
        reason,
    };
    let Some(created_at) = created_at else {
        return refuse("GitHub did not say when the request was sent".into());
    };
    let Some(sent_at) = parse_time(&request.sent_at) else {
        return refuse("the request does not say when it was sent".into());
    };
    let age = age_in_seconds(sent_at, now).max(age_in_seconds(created_at, now));
    if age > MAX_REQUEST_AGE_SECONDS {
        let minutes = age / 60;
        let reason = format!(
            "this request was sent {minutes} minutes ago, while Praxis was not running on \
             {device}; send it again"
        );
        return refuse(reason);
    }
    let ahead = -age_in_seconds(sent_at, now);
    if ahead > MAX_CLOCK_AHEAD_SECONDS {
        let minutes = ahead / 60;
        let reason = format!(
            "this request is dated {minutes} minutes ahead of {device}'s clock; check the time \
             on both"
        );
        return refuse(reason);
    }
    if !seen.insert(phone_id, &request.id, sent_at, now) {
        return refuse("this request was already received once".into());
    }
    Opened::Accepted { key, request }
}

/// The answer's plain text: `{"id", "ok", "result" | "error"}`, replaced by
/// an error if it would not fit in one comment.
pub(super) fn envelope(id: &str, answer: Result<Value>) -> String {
    let payload = match answer {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }),
        Err(error) => json!({ "id": id, "ok": false, "error": format!("{error:#}") }),
    }
    .to_string();
    if payload.len() <= MAX_ANSWER_LEN {
        return payload;
    }
    let size = payload.len();
    let error = format!("the answer was too large to send ({size} bytes)");
    json!({ "id": id, "ok": false, "error": error }).to_string()
}

fn response_comment(
    key: &Key,
    channel: &str,
    phone_id: &str,
    request_id: &str,
    answer: Result<Value>,
) -> Result<String> {
    let plain = envelope(request_id, answer);
    let aad = crypto::response_aad(channel, phone_id, request_id);
    let blob = crypto::seal(key, &aad, plain.as_bytes())?;
    let comment = format!("{PROTOCOL} response {phone_id} {request_id}\n{blob}");
    Ok(comment)
}

fn rejected_comment(phone_id: &str, reason: &str) -> String {
    format!("{PROTOCOL} rejected {phone_id}\n{reason}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairStatus {
    Approved,
    Denied,
    Expired,
    Invalid,
}

impl PairStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Invalid => "invalid",
        }
    }
}

fn pair_comment(fields: Map<String, Value>) -> String {
    format!("{PROTOCOL} pair\n{}", Value::Object(fields))
}

fn with_status(mut fields: Map<String, Value>, status: PairStatus) -> Map<String, Value> {
    fields.insert("status".into(), json!(status.as_str()));
    fields
}

/// What to do about a pairing comment this computer is not following yet.
#[derive(Debug, PartialEq)]
enum NewPairing {
    Ignore,
    Begin {
        phone_id: String,
        name: String,
        commit: String,
    },
    Refuse(PairStatus),
}

fn is_commitment(text: &str) -> bool {
    crypto::decode(text).is_ok_and(|bytes| bytes.len() == 32)
}

fn new_pairing(
    fields: &Map<String, Value>,
    created_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> NewPairing {
    let too_old = older_than(created_at, PAIRING_TIMEOUT_SECONDS, now);
    if fields.contains_key("status") || created_at.is_none() || too_old {
        return NewPairing::Ignore;
    }
    if fields.contains_key("desktop_key") || fields.contains_key("phone_key") {
        // Begun by an earlier run of Praxis, whose key is gone.
        return NewPairing::Refuse(PairStatus::Expired);
    }
    let text = |key: &str| fields.get(key).and_then(Value::as_str);
    let (Some(phone_id), Some(commit)) = (text("phone_id"), text("commit")) else {
        return NewPairing::Refuse(PairStatus::Invalid);
    };
    if !crypto::is_id(phone_id) || !is_commitment(commit) {
        return NewPairing::Refuse(PairStatus::Invalid);
    }
    NewPairing::Begin {
        phone_id: phone_id.to_string(),
        name: phone_name(text("name").unwrap_or_default()),
        commit: commit.to_string(),
    }
}

/// A phone's name as the prompt and the modal show it.
fn phone_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PHONE_NAME_CHARS)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "Unnamed phone".to_string()
    } else {
        name.to_string()
    }
}

/// One phone's pairing, from the computer's side.
struct Pairing {
    /// Read once, at step 1; later changes to them are ignored.
    phone_id: String,
    name: String,
    commit: String,
    began: DateTime<Utc>,
    desktop_key: String,
    phone_key: Option<String>,
    /// The comment's fields as last seen, which every rewrite keeps.
    fields: Map<String, Value>,
    stage: Stage,
}

enum Stage {
    /// Step 2 is written; waiting for the phone's key.
    AwaitingPhoneKey(PairingKey),
    /// Step 3 checks out; waiting for the user at the computer.
    AwaitingUser {
        secrets: PairingSecrets,
        asked: bool,
    },
    /// Decided, with the status still to be written.
    Deciding(PairStatus),
}

impl Pairing {
    fn begin(
        fields: Map<String, Value>,
        phone_id: String,
        name: String,
        commit: String,
        began: DateTime<Utc>,
        key: PairingKey,
    ) -> Self {
        Self {
            phone_id,
            name,
            commit,
            began,
            desktop_key: crypto::encode(key.public_key()),
            phone_key: None,
            fields,
            stage: Stage::AwaitingPhoneKey(key),
        }
    }

    /// The whole comment for this step, keeping every field already there.
    fn comment(&self, status: Option<PairStatus>) -> String {
        let mut fields = self.fields.clone();
        fields.insert("phone_id".into(), json!(self.phone_id));
        fields.insert("commit".into(), json!(self.commit));
        fields.insert("desktop_key".into(), json!(self.desktop_key));
        if let Some(phone_key) = &self.phone_key {
            fields.insert("phone_key".into(), json!(phone_key));
        }
        match status {
            Some(status) => pair_comment(with_status(fields, status)),
            None => pair_comment(fields),
        }
    }

    /// Takes in the comment as it is now, moving on once the phone reveals
    /// the key it committed to.
    fn update(&mut self, fields: Map<String, Value>, channel: &str) {
        let phone_key = fields.get("phone_key").and_then(Value::as_str);
        let phone_key = phone_key.map(str::to_string);
        self.fields = fields;
        let Some(phone_key) = phone_key else {
            return;
        };
        if !matches!(self.stage, Stage::AwaitingPhoneKey(_)) {
            return;
        }
        let stage = std::mem::replace(&mut self.stage, Stage::Deciding(PairStatus::Invalid));
        let Stage::AwaitingPhoneKey(key) = stage else {
            return;
        };
        let Some(phone_public) = crypto::decode(&phone_key).ok() else {
            return;
        };
        let commit = crypto::decode(&self.commit).unwrap_or_default();
        if !crypto::matches_commitment(&phone_public, &commit) {
            return;
        }
        let agreed = key.agree_with_phone(channel, &self.phone_id, &phone_public);
        if let Some(secrets) = agreed.log_err() {
            self.phone_key = Some(phone_key);
            self.stage = Stage::AwaitingUser {
                secrets,
                asked: false,
            };
        }
    }

    fn expire_if_due(&mut self, now: DateTime<Utc>) {
        let unfinished = !matches!(self.stage, Stage::Deciding(_));
        if unfinished && older_than(Some(self.began), PAIRING_TIMEOUT_SECONDS, now) {
            self.stage = Stage::Deciding(PairStatus::Expired);
        }
    }

    fn waiting_to_ask(&self) -> bool {
        matches!(self.stage, Stage::AwaitingUser { asked: false, .. })
    }

    /// Records the user's answer, returning the agreed secrets if they
    /// allowed the phone while it could still be allowed.
    fn decide(&mut self, allowed: bool) -> Option<PairingSecrets> {
        if !matches!(self.stage, Stage::AwaitingUser { .. }) {
            return None;
        }
        let status = if allowed {
            PairStatus::Approved
        } else {
            PairStatus::Denied
        };
        match std::mem::replace(&mut self.stage, Stage::Deciding(status)) {
            Stage::AwaitingUser { secrets, .. } if allowed => Some(secrets),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL: &str = "0123456789abcdef0123456789abcdef";
    const PHONE_ID: &str = "fedcba9876543210fedcba9876543210";
    const OTHER_PHONE_ID: &str = "00112233445566778899aabbccddeeff";
    const DEVICE: &str = "Laptop";

    fn fixed_key(byte: u8) -> Key {
        [byte; crypto::KEY_LEN]
    }

    fn request_blob(key: &Key, phone_id: &str, id: &str, sent_at: DateTime<Utc>) -> String {
        let sent_at = rfc3339(sent_at);
        let plain = json!({ "id": id, "op": "status", "args": {}, "sent_at": sent_at });
        let aad = crypto::request_aad(CHANNEL, phone_id);
        crypto::seal(key, &aad, plain.to_string().as_bytes()).expect("seals")
    }

    fn open_at(
        key: Option<&Key>,
        blob: &str,
        created_at: DateTime<Utc>,
        now: DateTime<Utc>,
        seen: &mut SeenRequests,
    ) -> Opened {
        let created = Some(created_at);
        open_request(key, CHANNEL, PHONE_ID, blob, created, now, seen, DEVICE)
    }

    fn rejection(opened: Opened) -> String {
        match opened {
            Opened::Rejected(reason) => reason,
            other => panic!("not rejected: {other:?}"),
        }
    }

    fn refusal(opened: Opened) -> String {
        match opened {
            Opened::Refused { id, reason, .. } => {
                assert_eq!(id, "r-1");
                reason
            }
            other => panic!("not refused: {other:?}"),
        }
    }

    fn words(comment: &str) -> Vec<&str> {
        let first = comment.lines().next().unwrap_or_default();
        first.split(' ').collect()
    }

    fn minutes(minutes: i64) -> chrono::Duration {
        chrono::Duration::minutes(minutes)
    }

    #[test]
    fn a_request_is_answered_in_place_for_its_phone_alone() {
        let key = fixed_key(7);
        let now = Utc::now();
        let blob = request_blob(&key, PHONE_ID, "r-1", now);
        let body = format!("{PROTOCOL} request {PHONE_ID}\r\n{blob}\r\n");
        let CommentKind::Request { phone_id, blob } = classify(&body) else {
            panic!("not a request");
        };
        assert_eq!(phone_id, PHONE_ID);

        let mut seen = SeenRequests::default();
        let opened = open_at(Some(&key), &blob, now, now, &mut seen);
        let Opened::Accepted { request, .. } = opened else {
            panic!("not accepted");
        };
        assert_eq!(request.id, "r-1");
        assert_eq!(request.op, "status");

        let answer = Ok(json!({ "queued": true }));
        let comment = response_comment(&key, CHANNEL, PHONE_ID, "r-1", answer).expect("sealed");
        assert_eq!(classify(&comment), CommentKind::Answer);
        assert_eq!(words(&comment), [PROTOCOL, "response", PHONE_ID, "r-1"]);
        let (_, sealed) = comment.split_once('\n').expect("two lines");
        let aad = crypto::response_aad(CHANNEL, PHONE_ID, "r-1");
        let plain = crypto::open(&key, &aad, sealed).expect("opens");
        let envelope: Value = serde_json::from_slice(&plain).expect("json");
        let expected = json!({ "id": "r-1", "ok": true, "result": { "queued": true } });
        assert_eq!(envelope, expected);

        let other = crypto::response_aad(CHANNEL, PHONE_ID, "r-2");
        assert!(crypto::open(&key, &other, sealed).is_err());
        assert!(crypto::open(&fixed_key(8), &aad, sealed).is_err());
    }

    #[test]
    fn unpaired_phones_and_undecryptable_requests_are_rejected_in_plain_text() {
        let now = Utc::now();
        let blob = request_blob(&fixed_key(7), PHONE_ID, "r-1", now);
        let mut seen = SeenRequests::default();
        let unpaired = open_at(None, &blob, now, now, &mut seen);
        assert!(rejection(unpaired).contains("not paired"));
        let wrong_key = open_at(Some(&fixed_key(8)), &blob, now, now, &mut seen);
        assert!(rejection(wrong_key).contains("decrypt"));
        let blob = request_blob(&fixed_key(7), OTHER_PHONE_ID, "r-1", now);
        let for_another_phone = open_at(Some(&fixed_key(7)), &blob, now, now, &mut seen);
        assert!(rejection(for_another_phone).contains("decrypt"));

        let rejected = rejected_comment(PHONE_ID, "this phone is not paired");
        assert_eq!(classify(&rejected), CommentKind::Answer);
        assert_eq!(words(&rejected), [PROTOCOL, "rejected", PHONE_ID]);
        assert_eq!(rejected.lines().nth(1), Some("this phone is not paired"));
    }

    #[test]
    fn stale_future_and_replayed_requests_are_refused() {
        let key = Some(fixed_key(7));
        let key = key.as_ref();
        let now = Utc::now();
        let mut seen = SeenRequests::default();

        let blob = request_blob(&fixed_key(7), PHONE_ID, "r-1", now);
        let late_comment = open_at(key, &blob, now - minutes(6), now, &mut seen);
        assert!(refusal(late_comment).contains("6 minutes ago"));
        let old = request_blob(&fixed_key(7), PHONE_ID, "r-1", now - minutes(6));
        let old = open_at(key, &old, now, now, &mut seen);
        assert!(refusal(old).contains("ago"));
        let ahead = request_blob(&fixed_key(7), PHONE_ID, "r-1", now + minutes(3));
        let ahead = open_at(key, &ahead, now, now, &mut seen);
        assert!(refusal(ahead).contains("clock"));

        let slightly_ahead = request_blob(&fixed_key(7), PHONE_ID, "r-1", now + minutes(1));
        let opened = open_at(key, &slightly_ahead, now, now, &mut seen);
        assert!(matches!(opened, Opened::Accepted { .. }));
        let replayed = open_at(key, &blob, now, now, &mut seen);
        assert!(refusal(replayed).contains("already"));
        let second = request_blob(&fixed_key(7), PHONE_ID, "r-2", now);
        let opened = open_at(key, &second, now, now, &mut seen);
        assert!(matches!(opened, Opened::Accepted { .. }));
    }

    #[test]
    fn replay_protection_forgets_requests_that_could_no_longer_be_accepted() {
        let now = Utc::now();
        let mut seen = SeenRequests::default();
        assert!(seen.insert(PHONE_ID, "a", now, now));
        assert!(!seen.insert(PHONE_ID, "a", now, now));
        assert!(seen.insert(OTHER_PHONE_ID, "a", now, now), "per phone");
        let later = now + minutes(10);
        assert!(seen.insert(PHONE_ID, "b", later, later));
        assert_eq!(seen.sent.len(), 1);
    }

    /// Plays the phone's side of a pairing.
    struct TestPhone {
        key: PairingKey,
        public: Vec<u8>,
    }

    impl TestPhone {
        fn new() -> Self {
            let key = PairingKey::generate().expect("a key");
            let public = key.public_key().to_vec();
            Self { key, public }
        }

        fn step_one(&self) -> Map<String, Value> {
            let commit = crypto::encode(&crypto::commitment(&self.public));
            let fields = json!({ "phone_id": PHONE_ID, "name": "Pixel 8", "commit": commit });
            fields.as_object().cloned().expect("an object")
        }

        fn public_key(&self) -> Value {
            json!(crypto::encode(&self.public))
        }
    }

    fn pair_fields(comment: &str) -> Map<String, Value> {
        match classify(comment) {
            CommentKind::Pair(fields) => fields,
            other => panic!("not a pairing comment: {other:?}"),
        }
    }

    fn begin(fields: &Map<String, Value>, began: DateTime<Utc>) -> Pairing {
        let NewPairing::Begin {
            phone_id,
            name,
            commit,
        } = new_pairing(fields, Some(began), began)
        else {
            panic!("not a new pairing");
        };
        let key = PairingKey::generate().expect("a key");
        Pairing::begin(fields.clone(), phone_id, name, commit, began, key)
    }

    fn decided(pairing: &Pairing) -> Option<PairStatus> {
        match pairing.stage {
            Stage::Deciding(status) => Some(status),
            _ => None,
        }
    }

    /// The pairing once the phone has revealed `phone_key` at step 3.
    fn reveal(pairing: &mut Pairing, phone_key: Value) {
        let mut fields = pair_fields(&pairing.comment(None));
        fields.insert("phone_key".into(), phone_key);
        pairing.update(fields, CHANNEL);
    }

    #[test]
    fn a_pairing_goes_from_the_phones_commitment_to_approval() {
        let phone = TestPhone::new();
        let now = Utc::now();
        // Step 1, as Android writes it, escaping every slash in the JSON.
        let json = Value::Object(phone.step_one()).to_string();
        let json = json.replace('/', "\\/");
        let step_one = format!("{PROTOCOL} pair\n{json}");
        let mut pairing = begin(&pair_fields(&step_one), now);
        assert_eq!(pairing.name, "Pixel 8");

        // Step 2 keeps the phone's fields and adds this computer's key.
        let fields = pair_fields(&pairing.comment(None));
        assert_eq!(fields.get("commit"), phone.step_one().get("commit"));
        assert_eq!(fields.get("name"), Some(&json!("Pixel 8")));
        assert_eq!(fields.get("status"), None);
        let desktop_key = fields.get("desktop_key").and_then(Value::as_str);
        let desktop_public = crypto::decode(desktop_key.expect("a key")).expect("base64");

        // Step 3.
        let phone_key = phone.public_key();
        reveal(&mut pairing, phone_key.clone());
        assert!(pairing.waiting_to_ask());
        let Stage::AwaitingUser { secrets, .. } = &pairing.stage else {
            panic!("not waiting for the user");
        };
        let TestPhone { key, .. } = phone;
        let on_phone = key.agree_with_desktop(CHANNEL, PHONE_ID, &desktop_public);
        let on_phone = on_phone.expect("agrees");
        assert_eq!(secrets.code, on_phone.code, "both screens show one code");

        // Step 4.
        let agreed = pairing.decide(true).expect("allowed");
        assert_eq!(agreed.key, on_phone.key);
        assert!(pairing.decide(true).is_none(), "an answer counts once");
        let fields = pair_fields(&pairing.comment(Some(PairStatus::Approved)));
        assert_eq!(fields.get("status"), Some(&json!("approved")));
        assert_eq!(fields.get("phone_key"), Some(&phone_key));
        assert_eq!(fields.get("phone_id"), Some(&json!(PHONE_ID)));
        assert_eq!(fields.get("name"), Some(&json!("Pixel 8")));
    }

    #[test]
    fn a_denied_pairing_agrees_no_key() {
        let phone = TestPhone::new();
        let mut pairing = begin(&phone.step_one(), Utc::now());
        reveal(&mut pairing, phone.public_key());
        assert!(pairing.decide(false).is_none());
        assert_eq!(decided(&pairing), Some(PairStatus::Denied));
    }

    #[test]
    fn a_key_that_does_not_match_the_commitment_is_invalid() {
        let phone = TestPhone::new();
        let impostor = TestPhone::new();
        let mut pairing = begin(&phone.step_one(), Utc::now());
        reveal(&mut pairing, impostor.public_key());
        assert_eq!(decided(&pairing), Some(PairStatus::Invalid));
        assert!(pairing.decide(true).is_none());
        let fields = pair_fields(&pairing.comment(Some(PairStatus::Invalid)));
        assert_eq!(fields.get("status"), Some(&json!("invalid")));
    }

    #[test]
    fn pairings_expire_and_are_never_resumed_or_begun_late() {
        let phone = TestPhone::new();
        let now = Utc::now();
        let mut pairing = begin(&phone.step_one(), now);
        pairing.expire_if_due(now + minutes(4));
        assert_eq!(decided(&pairing), None);
        pairing.expire_if_due(now + minutes(6));
        assert_eq!(decided(&pairing), Some(PairStatus::Expired));
        assert!(pairing.decide(true).is_none(), "too late to allow");

        let fields = phone.step_one();
        let expired = NewPairing::Refuse(PairStatus::Expired);
        let invalid = NewPairing::Refuse(PairStatus::Invalid);
        let late = now + minutes(6);
        assert_eq!(new_pairing(&fields, Some(now), late), NewPairing::Ignore);
        assert_eq!(new_pairing(&fields, None, now), NewPairing::Ignore);

        let mut resumed = fields.clone();
        resumed.insert("desktop_key".into(), json!("AAAA"));
        assert_eq!(new_pairing(&resumed, Some(now), now), expired);
        let mut finished = fields.clone();
        finished.insert("status".into(), json!("denied"));
        assert_eq!(new_pairing(&finished, Some(now), now), NewPairing::Ignore);
        let mut bad_id = fields.clone();
        bad_id.insert("phone_id".into(), json!("../gists"));
        assert_eq!(new_pairing(&bad_id, Some(now), now), invalid);
        let mut bad_commit = fields;
        bad_commit.insert("commit".into(), json!("c2hvcnQ="));
        assert_eq!(new_pairing(&bad_commit, Some(now), now), invalid);
    }

    #[test]
    fn phone_names_are_short_and_printable() {
        assert_eq!(phone_name(" Pixel\n8 "), "Pixel8");
        assert_eq!(phone_name(""), "Unnamed phone");
        let long = phone_name(&"x".repeat(100));
        assert_eq!(long.chars().count(), MAX_PHONE_NAME_CHARS);
    }

    fn phones(keys: &[(&str, Key)]) -> BTreeMap<String, Phone> {
        let mut phones = BTreeMap::new();
        for (id, key) in keys {
            let info = PhoneInfo {
                id: id.to_string(),
                name: "Phone".into(),
                paired_at: Utc::now(),
            };
            phones.insert(id.to_string(), Phone { info, key: *key });
        }
        phones
    }

    #[test]
    fn the_gist_names_the_computer_and_seals_a_snapshot_for_each_phone() {
        let now = Utc::now();
        let phones = phones(&[(PHONE_ID, fixed_key(1)), (OTHER_PHONE_ID, fixed_key(2))]);
        let meta = meta_json(CHANNEL, DEVICE, now, now, phones.keys());
        let meta: Value = serde_json::from_str(&meta).expect("json");
        assert_eq!(meta["protocol"], PROTOCOL);
        assert_eq!(meta["channel"], CHANNEL);
        assert_eq!(meta["device"], DEVICE);
        assert_eq!(meta["last_seen"], rfc3339(now));
        assert_eq!(meta["phones"], json!([OTHER_PHONE_ID, PHONE_ID]));
        let gist = json!({ "files": { META_FILE: { "content": meta.to_string() } } });
        assert_eq!(gist_channel(&gist).as_deref(), Some(CHANNEL));

        let mut snapshots = BTreeMap::new();
        let mine = json!({ "status": { "device": DEVICE } });
        snapshots.insert(PHONE_ID.to_string(), mine);
        snapshots.insert(OTHER_PHONE_ID.to_string(), json!({ "status": null }));
        let state = state_json(CHANNEL, &snapshots, &phones, now).expect("sealed");
        let state: Value = serde_json::from_str(&state).expect("json");
        let blob = state[PHONE_ID].as_str().expect("a blob");
        let aad = crypto::state_aad(CHANNEL, PHONE_ID);
        let plain = crypto::open(&fixed_key(1), &aad, blob).expect("opens");
        let snapshot: Value = serde_json::from_slice(&plain).expect("json");
        assert_eq!(snapshot["status"]["device"], DEVICE);
        assert_eq!(snapshot["updated_at"], rfc3339(now));
        assert!(crypto::open(&fixed_key(2), &aad, blob).is_err());
        let other_aad = crypto::state_aad(CHANNEL, OTHER_PHONE_ID);
        assert!(crypto::open(&fixed_key(1), &other_aad, blob).is_err());
        assert!(state[OTHER_PHONE_ID].is_string());
    }

    #[test]
    fn with_no_phones_there_is_no_snapshot() {
        let now = Utc::now();
        let none = BTreeMap::new();
        let state = state_json(CHANNEL, &BTreeMap::new(), &none, now).expect("empty");
        assert_eq!(state, "{}");
        let meta = meta_json(CHANNEL, DEVICE, now, now, none.keys());
        let meta: Value = serde_json::from_str(&meta).expect("json");
        assert_eq!(meta["phones"], json!([]));
    }

    #[test]
    fn an_oversized_answer_becomes_an_error_and_every_answer_fits_a_comment() {
        let content = "x".repeat(MAX_ANSWER_LEN);
        let huge = envelope("big", Ok(json!({ "content": content })));
        assert!(huge.len() <= MAX_ANSWER_LEN);
        let huge: Value = serde_json::from_str(&huge).expect("json");
        assert_eq!(huge["ok"], false);
        assert_eq!(huge["id"], "big");

        let largest = json!("x".repeat(MAX_ANSWER_LEN - 100));
        let request_id = "r".repeat(64);
        let plain = envelope(&request_id, Ok(largest.clone()));
        assert!(plain.len() <= MAX_ANSWER_LEN);
        assert!(plain.len() > MAX_ANSWER_LEN - 200, "not an error");
        let comment = response_comment(&fixed_key(1), CHANNEL, PHONE_ID, &request_id, Ok(largest));
        let comment = comment.expect("sealed");
        assert!(comment.chars().count() < 65_536, "GitHub's limit");
    }

    #[test]
    fn an_oversized_snapshot_drops_the_conversation_first() {
        let now = Utc::now();
        let small = fit_snapshot(json!({ "status": { "windows": [] } }), now);
        let small: Value = serde_json::from_str(&small).expect("json");
        assert_eq!(small["status"]["windows"], json!([]));

        let text = "x".repeat(MAX_SNAPSHOT_LEN);
        let fitted = fit_snapshot(json!({ "status": {}, "thread": { "text": text } }), now);
        assert!(fitted.len() <= MAX_SNAPSHOT_LEN);
        let fitted: Value = serde_json::from_str(&fitted).expect("json");
        assert_eq!(fitted["status"], json!({}));
        let error = fitted["thread_error"].as_str().unwrap_or_default();
        assert!(error.contains("too large"));

        let text = "x".repeat(MAX_SNAPSHOT_LEN);
        let fitted = fit_snapshot(json!({ "status": { "text": text } }), now);
        assert!(fitted.len() <= MAX_SNAPSHOT_LEN);
        assert!(fitted.contains("too large to show"));
    }

    fn comment(author: &str, body: &str, minutes_ago: i64, now: DateTime<Utc>) -> Comment {
        let updated_at = now - minutes(minutes_ago);
        let created_at = updated_at - minutes(1);
        let comment = json!({
            "id": 1,
            "body": body,
            "user": { "login": author },
            "created_at": rfc3339(created_at),
            "updated_at": rfc3339(updated_at),
        });
        parse_comment(&comment).expect("a comment")
    }

    #[test]
    fn housekeeping_removes_strangers_and_old_finished_comments() {
        let now = Utc::now();
        let delete = |author: &str, body: &str, minutes_ago: i64| {
            should_delete(&comment(author, body, minutes_ago, now), "me", now)
        };
        let answer = rejected_comment(PHONE_ID, "no");
        let finished = format!("{PROTOCOL} pair\n{{\"status\":\"denied\"}}");
        let unfinished = format!("{PROTOCOL} pair\n{{\"phone_id\":\"{PHONE_ID}\"}}");
        let request = format!("{PROTOCOL} request {PHONE_ID}\nAAAA");

        assert!(delete("stranger", "hello", 0));
        assert!(delete("stranger", &request, 0));
        assert!(!delete("Me", "a note to self", 60));
        assert!(!delete("me", &answer, 5));
        assert!(delete("me", &answer, 11));
        assert!(!delete("me", &finished, 5));
        assert!(delete("me", &finished, 11));
        assert!(!delete("me", &unfinished, 60));
        assert!(!delete("me", &request, 60));
    }

    #[test]
    fn transient_recovery_uses_a_short_bounded_backoff() {
        let error = anyhow!("temporary connection failure");
        assert_eq!(recovery_delay(&error, 1), Duration::from_secs(3));
        assert_eq!(recovery_delay(&error, 2), Duration::from_secs(6));
        assert_eq!(recovery_delay(&error, 3), Duration::from_secs(12));
        assert_eq!(recovery_delay(&error, 4), Duration::from_secs(24));
        assert_eq!(recovery_delay(&error, u32::MAX), MAX_TRANSIENT_BACKOFF);
    }

    #[test]
    fn comments_are_told_apart_by_their_first_line() {
        let response = format!("{PROTOCOL} response {PHONE_ID} r1\nAAAA");
        assert_eq!(classify(&response), CommentKind::Answer);
        assert_eq!(classify("Looks good!"), CommentKind::Other);
        let incomplete = format!("{PROTOCOL} request");
        assert_eq!(classify(&incomplete), CommentKind::Other);
        let malformed = format!("{PROTOCOL} pair\nnot json");
        assert_eq!(classify(&malformed), CommentKind::Other);
        assert_eq!(classify("praxis-remote/v1 pair\n{}"), CommentKind::Other);
        let escaped = format!("{PROTOCOL} pair\n{{\"commit\":\"a\\/b\"}}");
        let fields = pair_fields(&escaped);
        assert_eq!(fields.get("commit"), Some(&json!("a/b")));
    }

    #[test]
    fn only_plain_gist_ids_are_used_in_paths() {
        assert!(is_gist_id("aa5a315d61ae9438b18d"));
        assert!(!is_gist_id(""));
        assert!(!is_gist_id("../user"));
        assert!(!is_gist_id("abc?x=1"));
    }
}
