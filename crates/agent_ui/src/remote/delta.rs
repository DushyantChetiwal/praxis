use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{Watch, channel};

const MAX_DELTA_BYTES: usize = 16_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
struct PeerBaseline {
    sequence: u64,
    fingerprint: String,
    snapshot: Value,
    sent_at: Instant,
}

#[derive(Default)]
pub(super) struct PeerDeltaTracker {
    baselines: HashMap<String, PeerBaseline>,
}

impl PeerDeltaTracker {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn retain_phones(&mut self, watches: &HashMap<String, Watch>) {
        self.baselines
            .retain(|phone, _| watches.contains_key(phone));
    }

    pub(super) fn remove_phone(&mut self, phone: &str) {
        self.baselines.remove(phone);
    }

    pub(super) fn next_packet(
        &mut self,
        phone: &str,
        epoch: &str,
        raw_snapshot: Value,
        deltas: bool,
        now: DateTime<Utc>,
        now_instant: Instant,
        sequence: &mut u64,
    ) -> Result<Option<String>> {
        let fingerprint = raw_snapshot.to_string();
        let previous = self.baselines.get(phone);
        let unchanged = previous.is_some_and(|baseline| {
            baseline.fingerprint == fingerprint
                && now_instant.duration_since(baseline.sent_at) < HEARTBEAT_INTERVAL
        });
        if unchanged {
            return Ok(None);
        }

        let fitted: Value = serde_json::from_str(&channel::fit_snapshot(raw_snapshot, now))?;

        if !deltas {
            return self.record_and_emit_snapshot(
                phone,
                epoch,
                fingerprint,
                fitted,
                now_instant,
                sequence,
            );
        }

        let Some(baseline) = previous else {
            return self.record_and_emit_snapshot(
                phone,
                epoch,
                fingerprint,
                fitted,
                now_instant,
                sequence,
            );
        };

        if let Some(delta_body) = compute_delta(&baseline.snapshot, &fitted, now) {
            let next_sequence = sequence.saturating_add(1);
            let packet = json!({
                "type": "delta",
                "epoch": epoch,
                "sequence": next_sequence,
                "base_sequence": baseline.sequence,
                "delta": delta_body,
            })
            .to_string();
            if packet.len() <= MAX_DELTA_BYTES {
                *sequence = next_sequence;
                self.baselines.insert(
                    phone.to_string(),
                    PeerBaseline {
                        sequence: next_sequence,
                        fingerprint,
                        snapshot: fitted,
                        sent_at: now_instant,
                    },
                );
                return Ok(Some(packet));
            }
        }

        self.record_and_emit_snapshot(phone, epoch, fingerprint, fitted, now_instant, sequence)
    }

    fn record_and_emit_snapshot(
        &mut self,
        phone: &str,
        epoch: &str,
        fingerprint: String,
        fitted: Value,
        now_instant: Instant,
        sequence: &mut u64,
    ) -> Result<Option<String>> {
        let next_sequence = sequence.saturating_add(1);
        let packet = json!({
            "type": "snapshot",
            "epoch": epoch,
            "sequence": next_sequence,
            "snapshot": fitted,
        })
        .to_string();
        *sequence = next_sequence;
        self.baselines.insert(
            phone.to_string(),
            PeerBaseline {
                sequence: next_sequence,
                fingerprint,
                snapshot: fitted,
                sent_at: now_instant,
            },
        );
        Ok(Some(packet))
    }
}

pub(super) fn compute_delta(
    previous: &Value,
    current: &Value,
    now: DateTime<Utc>,
) -> Option<Value> {
    let prev_watch = previous.get("watch")?;
    let next_watch = current.get("watch")?;
    if prev_watch.get("window") != next_watch.get("window")
        || prev_watch.get("session_id") != next_watch.get("session_id")
    {
        return None;
    }

    if previous.get("thread_error") != current.get("thread_error") {
        return None;
    }

    let prev_thread = previous.get("thread").filter(|value| !value.is_null())?;
    let next_thread = current.get("thread").filter(|value| !value.is_null())?;

    let session_id = next_thread.get("session_id")?.as_str()?;
    if prev_thread.get("session_id").and_then(Value::as_str) != Some(session_id) {
        return None;
    }

    let prev_total = prev_thread.get("total").and_then(Value::as_u64)?;
    let next_total = next_thread.get("total").and_then(Value::as_u64)?;
    if next_total < prev_total {
        return None;
    }

    let prev_entries = prev_thread.get("entries").and_then(Value::as_array)?;
    let next_entries = next_thread.get("entries").and_then(Value::as_array)?;

    let mut prev_by_index: HashMap<u64, &Value> = HashMap::with_capacity(prev_entries.len());
    for entry in prev_entries {
        if let Some(index) = entry.get("index").and_then(Value::as_u64) {
            prev_by_index.insert(index, entry);
        }
    }

    let mut upserts = Vec::new();
    for entry in next_entries {
        let Some(index) = entry.get("index").and_then(Value::as_u64) else {
            return None;
        };
        if prev_by_index.get(&index).copied() != Some(entry) {
            upserts.push(entry.clone());
        }
    }

    let status = if current.get("status") != previous.get("status") {
        current.get("status").cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let step_threads = if next_thread.get("step_threads") != prev_thread.get("step_threads") {
        next_thread
            .get("step_threads")
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    Some(json!({
        "updated_at": now.to_rfc3339(),
        "watch": next_watch,
        "status": status,
        "session_id": session_id,
        "title": next_thread.get("title").cloned().unwrap_or(Value::Null),
        "thread_status": next_thread
            .get("status")
            .cloned()
            .unwrap_or(Value::Null),
        "total": next_total,
        "next_before": next_thread
            .get("next_before")
            .cloned()
            .unwrap_or(Value::Null),
        "has_more": next_thread
            .get("has_more")
            .cloned()
            .unwrap_or(Value::Bool(false)),
        "upserts": upserts,
        "step_threads": step_threads,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot(
        session_id: &str,
        thread_status: &str,
        total: u64,
        entries: Vec<Value>,
        device_status_generating: bool,
    ) -> Value {
        json!({
            "watch": {
                "window": 1,
                "session_id": session_id,
                "until": "2026-11-19T12:00:00Z"
            },
            "status": {
                "device": "Laptop",
                "windows": [{
                    "id": 1,
                    "generating": device_status_generating
                }]
            },
            "thread": {
                "session_id": session_id,
                "before_index": null,
                "next_before": 0,
                "has_more": false,
                "title": "Test thread",
                "status": thread_status,
                "total": total,
                "entries": entries,
                "step_threads": []
            },
            "thread_error": null
        })
    }

    #[test]
    fn initial_baseline_emits_snapshot_then_compact_delta() {
        let mut tracker = PeerDeltaTracker::new();
        let mut sequence = 0u64;
        let now = Utc::now();
        let t0 = Instant::now();

        let first = sample_snapshot(
            "s1",
            "generating",
            2,
            vec![
                json!({"index": 0, "kind": "user", "text": "hello"}),
                json!({"index": 1, "kind": "assistant", "text": "hi"}),
            ],
            true,
        );

        let packet1: Value = serde_json::from_str(
            &tracker
                .next_packet("phone1", "e1", first, true, now, t0, &mut sequence)
                .expect("first packet")
                .expect("should emit"),
        )
        .expect("valid json");
        assert_eq!(packet1["type"], "snapshot");
        assert_eq!(packet1["sequence"], 1);
        assert_eq!(packet1["epoch"], "e1");

        let second = sample_snapshot(
            "s1",
            "generating",
            2,
            vec![
                json!({"index": 0, "kind": "user", "text": "hello"}),
                json!({"index": 1, "kind": "assistant", "text": "hi there!"}),
            ],
            true,
        );

        let packet2: Value = serde_json::from_str(
            &tracker
                .next_packet(
                    "phone1",
                    "e1",
                    second,
                    true,
                    now,
                    t0 + Duration::from_millis(80),
                    &mut sequence,
                )
                .expect("second packet")
                .expect("should emit"),
        )
        .expect("valid json");
        assert_eq!(packet2["type"], "delta");
        assert_eq!(packet2["sequence"], 2);
        assert_eq!(packet2["base_sequence"], 1);
        assert_eq!(packet2["delta"]["status"], Value::Null);
        let upserts = packet2["delta"]["upserts"]
            .as_array()
            .expect("upserts array");
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0]["index"], 1);
        assert_eq!(upserts[0]["text"], "hi there!");
    }

    #[test]
    fn unchanged_state_suppressed_within_15s_and_heartbeats_after() {
        let mut tracker = PeerDeltaTracker::new();
        let mut sequence = 0u64;
        let now = Utc::now();
        let t0 = Instant::now();

        let snap = sample_snapshot(
            "s1",
            "idle",
            1,
            vec![json!({"index": 0, "kind": "user", "text": "hello"})],
            false,
        );

        assert!(
            tracker
                .next_packet("phone1", "e1", snap.clone(), true, now, t0, &mut sequence,)
                .expect("ok")
                .is_some()
        );

        assert!(
            tracker
                .next_packet(
                    "phone1",
                    "e1",
                    snap.clone(),
                    true,
                    now,
                    t0 + Duration::from_secs(5),
                    &mut sequence,
                )
                .expect("ok")
                .is_none()
        );

        let heartbeat: Value = serde_json::from_str(
            &tracker
                .next_packet(
                    "phone1",
                    "e1",
                    snap,
                    true,
                    now,
                    t0 + Duration::from_secs(16),
                    &mut sequence,
                )
                .expect("ok")
                .expect("heartbeat emitted"),
        )
        .expect("valid json");
        assert_eq!(heartbeat["type"], "delta");
        assert_eq!(heartbeat["base_sequence"], 1);
        assert_eq!(heartbeat["sequence"], 2);
        assert_eq!(heartbeat["delta"]["upserts"], json!([]));
    }

    #[test]
    fn session_change_or_total_shrink_falls_back_to_snapshot() {
        let mut tracker = PeerDeltaTracker::new();
        let mut sequence = 0u64;
        let now = Utc::now();
        let t0 = Instant::now();

        let first = sample_snapshot(
            "s1",
            "idle",
            5,
            vec![json!({"index": 4, "kind": "assistant", "text": "done"})],
            false,
        );
        tracker
            .next_packet("phone1", "e1", first, true, now, t0, &mut sequence)
            .expect("ok");

        let shrunk = sample_snapshot(
            "s1",
            "idle",
            2,
            vec![json!({"index": 1, "kind": "assistant", "text": "reset"})],
            false,
        );
        let packet_shrunk: Value = serde_json::from_str(
            &tracker
                .next_packet(
                    "phone1",
                    "e1",
                    shrunk,
                    true,
                    now,
                    t0 + Duration::from_millis(100),
                    &mut sequence,
                )
                .expect("ok")
                .expect("emitted"),
        )
        .expect("json");
        assert_eq!(packet_shrunk["type"], "snapshot");
        assert_eq!(packet_shrunk["sequence"], 2);

        let switched = sample_snapshot(
            "s2",
            "idle",
            2,
            vec![json!({"index": 1, "kind": "assistant", "text": "other"})],
            false,
        );
        let packet_switched: Value = serde_json::from_str(
            &tracker
                .next_packet(
                    "phone1",
                    "e1",
                    switched,
                    true,
                    now,
                    t0 + Duration::from_millis(200),
                    &mut sequence,
                )
                .expect("ok")
                .expect("emitted"),
        )
        .expect("json");
        assert_eq!(packet_switched["type"], "snapshot");
        assert_eq!(packet_switched["sequence"], 3);
    }

    #[test]
    fn oversized_delta_falls_back_to_snapshot() {
        let mut tracker = PeerDeltaTracker::new();
        let mut sequence = 0u64;
        let now = Utc::now();
        let t0 = Instant::now();

        let first = sample_snapshot(
            "s1",
            "generating",
            1,
            vec![json!({"index": 0, "kind": "user", "text": "hi"})],
            true,
        );
        tracker
            .next_packet("phone1", "e1", first, true, now, t0, &mut sequence)
            .expect("ok");

        let big_text = "x".repeat(17_000);
        let second = sample_snapshot(
            "s1",
            "generating",
            2,
            vec![
                json!({"index": 0, "kind": "user", "text": "hi"}),
                json!({"index": 1, "kind": "assistant", "text": big_text}),
            ],
            true,
        );
        let packet: Value = serde_json::from_str(
            &tracker
                .next_packet(
                    "phone1",
                    "e1",
                    second,
                    true,
                    now,
                    t0 + Duration::from_millis(100),
                    &mut sequence,
                )
                .expect("ok")
                .expect("emitted"),
        )
        .expect("json");
        assert_eq!(packet["type"], "snapshot");
        assert_eq!(packet["sequence"], 2);
    }
}
