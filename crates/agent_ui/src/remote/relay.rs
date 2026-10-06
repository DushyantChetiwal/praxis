use std::collections::{BTreeMap, HashMap};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, ensure};
use async_channel::{Receiver, Sender};
use chrono::{DateTime, Utc};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{AsyncApp, Task};
use gpui_tokio::Tokio;
use nostr_sdk::prelude::*;
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::{Value, json};

use super::crypto::{self, Key};
use super::{Watch, channel, handle_paired, snapshot, status};

// Application-specific ephemeral events: never public notes or stored DMs.
const PACKET_KIND: u16 = 21761;
const RELAYS: [&str; 2] = ["wss://relay.damus.io", "wss://relay.primal.net"];
const MAX_PACKET_BYTES: usize = 96_000;
const MAX_RECEIPTS: usize = 4096;
const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_AGE: i64 = 300;

struct Peer {
    id: String,
    key: Key,
    keys: Keys,
    phone_public: PublicKey,
    last_seen: Mutex<Option<Instant>>,
}

type Peers = Arc<RwLock<BTreeMap<String, Arc<Peer>>>>;
type Packet = (Arc<Peer>, String);

pub(super) struct Relay {
    peers: Peers,
    changed: Sender<()>,
    stop: Option<Sender<()>>,
    task: Option<Task<()>>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.take();
        // Let the service close the SDK sockets rather than orphaning its workers.
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

impl Relay {
    pub(super) fn start(channel: String, device: String, cx: &mut AsyncApp) -> Self {
        let peers = Arc::new(RwLock::new(BTreeMap::new()));
        let (changed, changes) = async_channel::bounded(1);
        let (stop, stopped) = async_channel::bounded(1);
        let task = cx.update(|cx| cx.spawn({
            let peers = peers.clone();
            async move |cx| {
                loop {
                    let result = serve(channel.clone(), device.clone(), peers.clone(), changes.clone(), stopped.clone(), cx).await;
                    for peer in peers.read().values() {
                        *peer.last_seen.lock() = None;
                    }
                    if stopped.is_closed() { break; }
                    if let Err(error) = result {
                        log::warn!("Praxis Remote live transport stopped; retrying in 30 seconds with GitHub fallback available: {error:#}");
                    }
                    if until_stopped(&stopped, cx.background_executor().timer(Duration::from_secs(30))).await.is_none() { break; }
                }
            }
        }));
        Self {
            peers,
            changed,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub(super) fn all_connected(&self) -> bool {
        let peers = self.peers.read();
        !peers.is_empty()
            && peers.values().all(|peer| {
                peer.last_seen
                    .lock()
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(45))
            })
    }

    pub(super) fn set_phones(
        &self,
        channel: &str,
        phones: impl IntoIterator<Item = (String, Key)>,
    ) {
        let mut peers = self.peers.write();
        let mut next = BTreeMap::new();
        for (id, key) in phones {
            if let Some(peer) = peers.get(&id).filter(|peer| peer.key == key) {
                next.insert(id, peer.clone());
                continue;
            }
            let make = || -> Result<Peer> {
                let desktop = crypto::relay_secret(&key, channel, &id, "desktop")?;
                let phone = crypto::relay_secret(&key, channel, &id, "phone")?;
                Ok(Peer {
                    id: id.clone(),
                    key,
                    keys: Keys::new(SecretKey::from_slice(&desktop)?),
                    phone_public: Keys::new(SecretKey::from_slice(&phone)?).public_key(),
                    last_seen: Mutex::new(None),
                })
            };
            match make() {
                Ok(peer) => {
                    next.insert(id, Arc::new(peer));
                }
                Err(error) => log::warn!("Praxis Remote could not prepare a live peer: {error:#}"),
            }
        }
        *peers = next;
        match self.changed.try_send(()) {
            Ok(()) | Err(async_channel::TrySendError::Full(())) => {}
            Err(error) => log::warn!("Praxis Remote live transport is unavailable: {error}"),
        }
    }
}

fn current(peers: &Peers, peer: &Arc<Peer>) -> bool {
    peers
        .read()
        .get(&peer.id)
        .is_some_and(|active| Arc::ptr_eq(active, peer))
}

async fn until_stopped<T>(
    stopped: &Receiver<()>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    let work = work.fuse();
    futures::pin_mut!(work);
    futures::select_biased! {
        _ = stopped.recv().fuse() => None,
        result = work => Some(result),
    }
}

async fn network(
    channel: String,
    peers: Peers,
    changes: Receiver<()>,
    incoming: Sender<Packet>,
    outgoing: Receiver<Packet>,
    stopped: Receiver<()>,
    executor: gpui::BackgroundExecutor,
) -> Result<()> {
    let client = Client::default();
    let result = until_stopped(&stopped, async {
    for relay in RELAYS {
        client.add_relay(relay).await?;
    }
    let mut notifications = client.notifications();
    client.connect().await;
    let receive = async {
        let mut subscribe_pending = true;
        loop {
            let changed = if subscribe_pending {
                futures::future::ready(Ok(())).left_future()
            } else {
                changes.recv().right_future()
            };
            futures::select_biased! {
                changed = changed.fuse() => {
                    if changed.is_err() { break; }
                    subscribe_pending = false;
                    let filters: Vec<_> = peers.read().values().map(|peer| Filter::new()
                        .author(peer.phone_public).pubkey(peer.keys.public_key())
                        .kind(Kind::from(PACKET_KIND)).limit(0)).collect();
                    if filters.is_empty() {
                        if let Err(error) = client.unsubscribe_all().await {
                            log::warn!("Praxis Remote could not remove a live subscription: {error:#}");
                        }
                    } else if let Err(error) = client.subscribe(filters).with_id(SubscriptionId::new("praxis-live")).await {
                        log::warn!("Praxis Remote could not subscribe to live updates: {error:#}");
                    }
                },
                notification = notifications.next().fuse() => {
                    let Some(notification) = notification else { break; };
                    let ClientNotification::Event { event, .. } = notification else { continue; };
                    if event.kind != Kind::from(PACKET_KIND) || event.content.len() > MAX_PACKET_BYTES || event.verify().is_err() { continue; }
                    let peer = peers.read().values().find(|peer| peer.phone_public == event.pubkey).cloned();
                    let Some(peer) = peer else { continue; };
                    if !event.tags.iter().any(|tag| tag.as_slice() == ["p", &peer.keys.public_key().to_hex()]) { continue; }
                    let age = Utc::now().timestamp() - event.created_at.as_secs() as i64;
                    if !(-120..=REQUEST_AGE).contains(&age) { continue; }
                    let aad = crypto::relay_aad(&channel, &peer.id, "request");
                    let Ok(plain) = crypto::open(&peer.key, &aad, &event.content) else { continue; };
                    let Ok(plain) = String::from_utf8(plain) else { continue; };
                    if incoming.send((peer, plain)).await.is_err() { break; }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let transmit = async {
        while let Ok((peer, plain)) = outgoing.recv().await {
            if !current(&peers, &peer) {
                continue;
            }
            let aad = crypto::relay_aad(&channel, &peer.id, "desktop");
            let blob = crypto::seal(&peer.key, &aad, plain.as_bytes())?;
            ensure!(
                blob.len() <= MAX_PACKET_BYTES,
                "live packet exceeds the transport limit"
            );
            let event = EventBuilder::new(Kind::from(PACKET_KIND), blob)
                .tags([Tag::public_key(peer.phone_public)])
                .finalize(&peer.keys)?;
            if let Err(error) = client
                .send_event(&event)
                .ack_policy(AckPolicy::none())
                .ok_timeout(Duration::from_secs(2))
                .await
            {
                log::debug!("Praxis Remote live packet was not acknowledged by a relay: {error:#}");
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let receive = pin!(receive);
    let transmit = pin!(transmit);
    let result = futures::future::select(receive, transmit).await;
    let result = match result {
        futures::future::Either::Left((result, _))
        | futures::future::Either::Right((result, _)) => result,
    };
    result
    }).await.unwrap_or(Ok(()));
    let shutdown = client.shutdown().fuse();
    let timeout = executor.timer(Duration::from_secs(5)).fuse();
    futures::pin_mut!(shutdown, timeout);
    futures::select_biased! {
        _ = shutdown => result,
        _ = timeout => Err(anyhow!("Timed out closing the live relay sockets")),
    }
}

#[derive(Deserialize)]
struct Request {
    id: String,
    op: String,
    #[serde(default)]
    args: Value,
    sent_at: String,
    #[serde(default)]
    epoch: Option<String>,
}

struct Receipt {
    fingerprint: Vec<u8>,
    expires: i64,
    answer: Option<String>,
}

#[derive(Default)]
struct Receipts {
    entries: HashMap<(String, String), Receipt>,
    bytes: usize,
}

impl Receipts {
    fn begin(
        &mut self,
        phone: &str,
        request: &Request,
        plain: &str,
        epoch: &str,
        now: i64,
    ) -> Result<Option<String>> {
        ensure!(
            request.epoch.as_deref() == Some(epoch),
            "Praxis restarted. This request was not executed by the new session. Refresh and check the conversation before resending."
        );
        let sent = DateTime::parse_from_rfc3339(&request.sent_at)?.timestamp();
        ensure!(
            (-120..=REQUEST_AGE).contains(&(now - sent)),
            "This live request expired. Refresh before sending a new request."
        );
        self.entries.retain(|_, receipt| receipt.expires >= now);
        self.bytes = self
            .entries
            .values()
            .filter_map(|receipt| receipt.answer.as_ref())
            .map(String::len)
            .sum();
        let fingerprint = ring::digest::digest(&ring::digest::SHA256, plain.as_bytes())
            .as_ref()
            .to_vec();
        let id = (phone.to_string(), request.id.clone());
        if let Some(receipt) = self.entries.get(&id) {
            ensure!(
                receipt.fingerprint == fingerprint,
                "A request ID was reused with different content."
            );
            return receipt.answer.clone().map(Some).context("This request was already received but its result is unavailable. Check the conversation before resending.");
        }
        ensure!(
            self.entries.len() < MAX_RECEIPTS,
            "Too many recent live requests. Retry after older requests expire."
        );
        self.entries.insert(
            id,
            Receipt {
                fingerprint,
                expires: sent + REQUEST_AGE,
                answer: None,
            },
        );
        Ok(None)
    }

    fn finish(&mut self, phone: &str, id: &str, answer: String) {
        if self.bytes.saturating_add(answer.len()) > MAX_RECEIPT_BYTES {
            return;
        }
        if let Some(receipt) = self.entries.get_mut(&(phone.into(), id.into())) {
            self.bytes += answer.len();
            receipt.answer = Some(answer);
        }
    }
}

async fn serve(
    channel_id: String,
    device: String,
    peers: Peers,
    changes: Receiver<()>,
    stopped: Receiver<()>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let epoch = crypto::random_id()?;
    let (incoming, requests) = async_channel::bounded(32);
    let (outgoing, responses) = async_channel::bounded(32);
    let (network_stop, network_stopped) = async_channel::bounded(1);
    let network = Tokio::spawn_result(
        cx,
        network(
            channel_id,
            peers.clone(),
            changes,
            incoming,
            responses,
            network_stopped,
            cx.background_executor().clone(),
        ),
    );
    let result = until_stopped(&stopped, async {
    let mut receipts = Receipts::default();
    let mut watches: HashMap<String, Watch> = HashMap::new();
    let mut published: HashMap<String, (String, Instant)> = HashMap::new();
    let mut sequence = 0u64;
    let mut next_snapshot = Instant::now();
    loop {
        if stopped.is_closed() {
            break;
        }
        if Instant::now() >= next_snapshot {
            next_snapshot = Instant::now() + Duration::from_secs(1);
            let now = Utc::now();
            watches.retain(|phone, watch| watch.until > now && peers.read().contains_key(phone));
            published.retain(|phone, _| watches.contains_key(phone));
            for (phone, watch) in &watches {
                let Some(peer) = peers.read().get(phone).cloned() else {
                    continue;
                };
                if peer
                    .last_seen
                    .lock()
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(45))
                {
                    continue;
                }
                let value = cx.update(|cx| snapshot(&status(&device, cx), Some(watch), cx));
                let fingerprint = value.to_string();
                let unchanged = published.get(phone).is_some_and(|(previous, at)| {
                    previous == &fingerprint && at.elapsed() < Duration::from_secs(15)
                });
                if unchanged {
                    continue;
                }
                let snapshot: Value = serde_json::from_str(&channel::fit_snapshot(value, now))?;
                sequence = sequence.saturating_add(1);
                let packet = json!({"type": "snapshot", "epoch": epoch, "sequence": sequence, "snapshot": snapshot}).to_string();
                match outgoing.try_send((peer, packet)) {
                    Ok(()) => {
                        published.insert(phone.clone(), (fingerprint, Instant::now()));
                    }
                    Err(async_channel::TrySendError::Full(_)) => {}
                    Err(error) => return Err(anyhow!("live connection stopped: {error}")),
                }
            }
        }
        let timer = cx.background_executor().timer(Duration::from_millis(250));
        let packet = futures::select_biased! {
            _ = stopped.recv().fuse() => break,
            packet = requests.recv().fuse() => packet.ok(),
            _ = timer.fuse() => continue,
        };
        let Some((peer, plain)) = packet else {
            break;
        };
        if !current(&peers, &peer) {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Request>(&plain) else {
            continue;
        };
        if request.id.is_empty()
            || request.id.len() > 64
            || !request
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            continue;
        }
        let answer = if request.op == "hello" {
            *peer.last_seen.lock() = Some(Instant::now());
            channel::envelope(&request.id, Ok(json!({"epoch": epoch})))
        } else {
            match receipts.begin(&peer.id, &request, &plain, &epoch, Utc::now().timestamp()) {
                Ok(Some(answer)) => answer,
                Err(error) => channel::envelope(&request.id, Err(error)),
                Ok(None) => {
                    let result = if request.op == "watch" {
                        let seconds = request
                            .args
                            .get("seconds")
                            .and_then(Value::as_i64)
                            .unwrap_or(300)
                            .clamp(1, 900);
                        let watch = Watch {
                            window: request.args.get("window").and_then(Value::as_u64),
                            session_id: request
                                .args
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            include_details: request
                                .args
                                .get("include_details")
                                .and_then(Value::as_bool)
                                != Some(false),
                            until: Utc::now() + chrono::Duration::seconds(seconds),
                        };
                        let until = watch.until.to_rfc3339();
                        watches.insert(peer.id.clone(), watch);
                        published.remove(&peer.id);
                        Ok(json!({"until": until}))
                    } else if matches!(request.op.as_str(), "unpair" | "batch") {
                        Err(anyhow!("Use the GitHub channel for this operation"))
                    } else {
                        cx.update(|cx| {
                            handle_paired(&request.op, &request.args, &device, &peer.id, cx)
                        })
                        .await
                    };
                    let answer = channel::envelope(&request.id, result);
                    receipts.finish(&peer.id, &request.id, answer.clone());
                    answer
                }
            }
        };
        let mut answer: Value = serde_json::from_str(&answer)?;
        answer["type"] = json!("response");
        answer["epoch"] = json!(epoch);
        if outgoing.send((peer, answer.to_string())).await.is_err() {
            break;
        }
    }
    Ok::<_, anyhow::Error>(())
    }).await;
    drop(network_stop);
    drop(outgoing);
    drop(requests);
    network.await?;
    result.unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn relay_shutdown_interrupts_queued_network_work(_cx: &mut gpui::TestAppContext) {
        let (stop, stopped) = async_channel::bounded::<()>(1);
        let (sender, receiver) = async_channel::bounded(1);
        sender.try_send("first").expect("fill the bounded queue");
        let waiting = until_stopped(&stopped, sender.send("second"));
        futures::pin_mut!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        drop(stop);
        assert!(
            waiting.await.is_none(),
            "shutdown must not wait for queue capacity"
        );
        assert_eq!(receiver.try_recv().expect("retained first item"), "first");
        assert!(receiver.is_empty());
        assert_eq!(until_stopped(&stopped, async { 42 }).await, None);
        let (_stop, stopped) = async_channel::bounded::<()>(1);
        assert_eq!(until_stopped(&stopped, async { 42 }).await, Some(42));
    }

    fn request(epoch: &str) -> Request {
        Request {
            id: "request-1".into(),
            op: "prompt".into(),
            args: json!({}),
            sent_at: "2026-10-06T00:00:00Z".into(),
            epoch: Some(epoch.into()),
        }
    }

    #[test]
    fn live_receipts_never_execute_duplicates_or_old_desktop_requests() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T00:00:00Z")
            .unwrap()
            .timestamp();
        let mut receipts = Receipts::default();
        let request = request("epoch");
        assert_eq!(
            receipts
                .begin("phone", &request, "one", "epoch", now)
                .unwrap(),
            None
        );
        assert!(
            receipts
                .begin("phone", &request, "one", "epoch", now)
                .is_err()
        );
        receipts.finish("phone", &request.id, "answer".into());
        assert_eq!(
            receipts
                .begin("phone", &request, "one", "epoch", now)
                .unwrap(),
            Some("answer".into())
        );
        assert!(
            receipts
                .begin("phone", &request, "different", "epoch", now)
                .is_err()
        );
        assert!(
            receipts
                .begin("phone", &request, "one", "new-epoch", now)
                .is_err()
        );
        assert!(
            receipts
                .begin("phone", &request, "one", "epoch", now + 301)
                .is_err()
        );
        assert_eq!(
            receipts
                .begin("other", &request, "one", "epoch", now)
                .unwrap(),
            None
        );
    }

    #[test]
    fn relay_identities_and_encryption_are_bound_to_phone_and_direction() {
        let key = [7; 32];
        let desktop = crypto::relay_secret(&key, "channel", "phone", "desktop").unwrap();
        assert_ne!(desktop, key);
        assert_ne!(
            desktop,
            crypto::relay_secret(&key, "channel", "phone", "phone").unwrap()
        );
        assert_ne!(
            desktop,
            crypto::relay_secret(&key, "channel", "other", "desktop").unwrap()
        );
        let aad = crypto::relay_aad("channel", "phone", "request");
        let sealed = crypto::seal(&key, &aad, b"private").unwrap();
        assert!(
            crypto::open(
                &key,
                &crypto::relay_aad("channel", "phone", "desktop"),
                &sealed
            )
            .is_err()
        );
        assert_eq!(crypto::open(&key, &aad, &sealed).unwrap(), b"private");
        assert!(SecretKey::from_slice(&desktop).is_ok());
    }
}
