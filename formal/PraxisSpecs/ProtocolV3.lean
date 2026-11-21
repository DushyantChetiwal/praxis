/-!
Formal model of Praxis Remote Protocol v3 delta streaming across:
1. Desktop `PeerDeltaTracker` (`crates/agent_ui/src/remote/delta.rs`, `relay.rs`)
2. Android `RelayCursor` (`remote-android/.../data/NostrChannel.kt`)
3. Android `TranscriptHistory.applyDelta` (`remote-android/.../data/TranscriptHistory.kt`)

Each section proves a concrete counterexample against the buggy transition rule
and proves the corresponding safety invariant for the fixed transition rule.
-/

namespace PraxisSpecs.ProtocolV3

/-- A half-open entry index interval `[start, stop)`. -/
structure HistoryRange where
  start : Nat
  stop : Nat
  deriving DecidableEq, Repr

/-- Abstract transcript state tracking loaded entry indices and claimed covered ranges. -/
structure TranscriptState where
  total : Nat
  entries : List Nat
  ranges : List HistoryRange
  deriving DecidableEq, Repr

/-- Incoming Protocol v3 delta packet carrying `upserts` and a new tail window `[nextBefore, total)`. -/
structure ThreadDelta where
  total : Nat
  nextBefore : Nat
  upserts : List Nat
  deriving DecidableEq, Repr

/-- An index `i` is claimed covered if it falls inside any recorded `HistoryRange`. -/
def inRanges (ranges : List HistoryRange) (i : Nat) : Prop :=
  ∃ r ∈ ranges, r.start ≤ i ∧ i < r.stop

/-- Coverage soundness: every index claimed covered by `ranges` actually exists in `entries`. -/
def CoverageSound (s : TranscriptState) : Prop :=
  ∀ i, inRanges s.ranges i → i ∈ s.entries

def lastRangeEnd (ranges : List HistoryRange) : Nat :=
  match ranges.getLast? with
  | some r => r.stop
  | none => 0

/-- Models the pre-fix `TranscriptHistory.applyDelta` disjoint-jump branch:
    when `nextBefore > oldEnd` and `upserts` is non-empty, it replaces `entries`
    with `upserts` while recording `[nextBefore, total)` as covered. -/
def buggyApplyDelta (old : TranscriptState) (d : ThreadDelta) : Option TranscriptState :=
  if d.nextBefore > lastRangeEnd old.ranges then
    if d.upserts.isEmpty then none
    else some {
      total := d.total
      entries := d.upserts
      ranges := [{ start := d.nextBefore, stop := d.total }]
    }
  else
    some {
      total := d.total
      entries := old.entries ++ d.upserts
      ranges := old.ranges
    }

/-- Counterexample proving `buggyApplyDelta` violates `CoverageSound`:
    entries `11..33` are claimed covered by `[11, 35)` even though only `34` is present. -/
theorem buggy_applyDelta_violates_coverage :
    ∃ old d next,
      CoverageSound old ∧
      buggyApplyDelta old d = some next ∧
      ¬ CoverageSound next := by
  let old : TranscriptState := {
    total := 10
    entries := List.range 10
    ranges := [{ start := 0, stop := 10 }]
  }
  let d : ThreadDelta := {
    total := 35
    nextBefore := 11
    upserts := [34]
  }
  let next : TranscriptState := {
    total := 35
    entries := [34]
    ranges := [{ start := 11, stop := 35 }]
  }
  refine ⟨old, d, next, ?_, rfl, ?_⟩
  · intro i hi
    rcases hi with ⟨r, hr, hlo, hhi⟩
    simp [old] at hr
    subst hr
    exact List.mem_range.mpr hhi
  · intro hsound
    have hcov : inRanges next.ranges 11 := by
      refine ⟨{ start := 11, stop := 35 }, ?_, by decide, by decide⟩
      simp [next]
    have hmem : 11 ∈ next.entries := hsound 11 hcov
    simp [next] at hmem

/-- Fixed `applyDelta`: rejects any disjoint jump (`nextBefore > oldEnd`) with `none`
    so the client requests a full 24-entry tail snapshot instead of fabricating coverage.
    Contiguous updates only extend coverage for indices explicitly upserted. -/
def fixedApplyDelta (old : TranscriptState) (d : ThreadDelta) : Option TranscriptState :=
  if d.nextBefore > lastRangeEnd old.ranges then
    none
  else
    some {
      total := d.total
      entries := old.entries ++ d.upserts
      ranges := old.ranges
    }

/-- Soundness theorem: `fixedApplyDelta` preserves `CoverageSound` for all inputs. -/
theorem fixed_applyDelta_preserves_coverage (old next : TranscriptState) (d : ThreadDelta)
    (hOld : CoverageSound old)
    (hStep : fixedApplyDelta old d = some next) :
    CoverageSound next := by
  unfold fixedApplyDelta at hStep
  split at hStep
  · cases hStep
  · cases hStep
    intro i hi
    have hOldMem : i ∈ old.entries := hOld i hi
    exact List.mem_append_left d.upserts hOldMem

/-! ### Part 2: Android `RelayCursor` parse-before-commit invariant (`NostrChannel.kt`) -/

inductive CursorOutcome where
  | accepted
  | gap
  | stale
  | droppedMalformed
  deriving DecidableEq, Repr

structure CursorState where
  lastSeq : Nat
  deriving DecidableEq, Repr

structure DeltaEnvelope where
  seq : Nat
  baseSeq : Nat
  validPayload : Bool
  deriving DecidableEq, Repr

/-- Pre-fix `NostrChannel.Session.receive`: advances `lastSeq` before parsing payload. -/
def buggyReceiveDelta (c : CursorState) (pkt : DeltaEnvelope) : CursorState × CursorOutcome :=
  if pkt.seq ≤ c.lastSeq then
    (c, .stale)
  else if pkt.baseSeq ≠ c.lastSeq then
    (c, .gap)
  else
    let c' : CursorState := { lastSeq := pkt.seq }
    if pkt.validPayload then (c', .accepted) else (c', .droppedMalformed)

/-- Counterexample: a malformed delta (`seq=5, base=4`) advances `lastSeq` to `5`,
    causing the next delta (`seq=6, base=5`) to be accepted despite missing `seq=5`. -/
theorem buggy_cursor_swallows_malformed_delta :
    let c0 : CursorState := { lastSeq := 4 }
    let p1 : DeltaEnvelope := { seq := 5, baseSeq := 4, validPayload := false }
    let p2 : DeltaEnvelope := { seq := 6, baseSeq := 5, validPayload := true }
    let (c1, o1) := buggyReceiveDelta c0 p1
    let (_, o2) := buggyReceiveDelta c1 p2
    o1 = .droppedMalformed ∧ o2 = .accepted := by
  decide

/-- Fixed `NostrChannel.Session.receive`: validates/parses payload before advancing `lastSeq`. -/
def fixedReceiveDelta (c : CursorState) (pkt : DeltaEnvelope) : CursorState × CursorOutcome :=
  if pkt.seq ≤ c.lastSeq then
    (c, .stale)
  else if pkt.baseSeq ≠ c.lastSeq then
    (c, .gap)
  else if !pkt.validPayload then
    (c, .droppedMalformed)
  else
    ({ lastSeq := pkt.seq }, .accepted)

/-- Safety theorem: if a delta fails payload parsing, `fixedReceiveDelta` never advances
    `lastSeq`, so any subsequent delta whose `baseSeq` depends on the malformed sequence
    triggers `.gap` and forces a snapshot resync. -/
theorem fixed_cursor_detects_gap_after_malformed (c : CursorState) (p1 p2 : DeltaEnvelope)
    (h1 : (fixedReceiveDelta c p1).2 = .droppedMalformed)
    (hNextBase : p2.baseSeq = p1.seq)
    (hInc : p1.seq < p2.seq) :
    (fixedReceiveDelta (fixedReceiveDelta c p1).1 p2).2 = .gap := by
  unfold fixedReceiveDelta at h1 ⊢
  by_cases hle1 : p1.seq ≤ c.lastSeq
  · simp [hle1] at h1
  · by_cases hbase1 : p1.baseSeq ≠ c.lastSeq
    · simp [hle1, hbase1] at h1
    · by_cases hval1 : !p1.validPayload
      · have hgt2 : ¬ (p2.seq ≤ c.lastSeq) := by omega
        have hbase2 : p2.baseSeq ≠ c.lastSeq := by omega
        simp [hle1, hbase1, hval1, hgt2, hbase2]
      · simp [hle1, hbase1, hval1] at h1

/-! ### Part 3: Desktop `PeerDeltaTracker` send-before-commit invariant (`relay.rs`) -/

structure RelayPeerState where
  trackerBaselineSeq : Nat
  lastEnqueuedSeq : Nat
  deriving DecidableEq, Repr

/-- Pre-fix `Relay::serve`: commits `trackerBaselineSeq` before `outgoing.try_send`. -/
def buggyRelayEmit (s : RelayPeerState) (queueFull : Bool) : RelayPeerState :=
  let nextSeq := s.trackerBaselineSeq + 1
  if queueFull then
    { trackerBaselineSeq := nextSeq, lastEnqueuedSeq := s.lastEnqueuedSeq }
  else
    { trackerBaselineSeq := nextSeq, lastEnqueuedSeq := nextSeq }

/-- Counterexample: when `outgoing` is full, `trackerBaselineSeq` diverges from `lastEnqueuedSeq`. -/
theorem buggy_relay_desyncs_on_full_channel :
    let s0 : RelayPeerState := { trackerBaselineSeq := 32, lastEnqueuedSeq := 32 }
    let s1 := buggyRelayEmit s0 true
    s1.trackerBaselineSeq ≠ s1.lastEnqueuedSeq := by
  decide

/-- Fixed `Relay::serve`: only commits `trackerBaselineSeq` when `outgoing.try_send` succeeds. -/
def fixedRelayEmit (s : RelayPeerState) (queueFull : Bool) : RelayPeerState :=
  if queueFull then
    s
  else
    let nextSeq := s.trackerBaselineSeq + 1
    { trackerBaselineSeq := nextSeq, lastEnqueuedSeq := nextSeq }

/-- Invariant: `fixedRelayEmit` always keeps `trackerBaselineSeq = lastEnqueuedSeq`. -/
theorem fixed_relay_preserves_baseline_sync (s : RelayPeerState) (queueFull : Bool)
    (hSync : s.trackerBaselineSeq = s.lastEnqueuedSeq) :
    (fixedRelayEmit s queueFull).trackerBaselineSeq =
      (fixedRelayEmit s queueFull).lastEnqueuedSeq := by
  cases queueFull <;> simp [fixedRelayEmit, hSync]

end PraxisSpecs.ProtocolV3
