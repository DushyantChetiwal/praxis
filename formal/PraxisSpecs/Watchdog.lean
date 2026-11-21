/-!
Formal model of Praxis Desktop watchdog heartbeat and clean shutdown synchronization
(`crates/zed/src/watchdog.rs`).

Proves:
1. In the pre-fix lockless interleaving (`HeartbeatWriter || CleanShutdown`), a heartbeat
   thread that loaded `clean_exit = false` before `mark_clean_shutdown` can overwrite
   `heartbeat.json` with `clean_exit = false`, causing a spurious supervisor restart.
2. In the fixed mutex-serialized model (reading `CLEAN_EXIT` inside the heartbeat-write
   mutex), `clean_exit = true` on disk is strictly monotone and can never regress to `false`.
-/

namespace PraxisSpecs.Watchdog

/-- Unsynchronized model where the heartbeat thread separates loading `CLEAN_EXIT`
    from writing `heartbeat.json`. -/
structure UnsyncedState where
  atomicCleanExit : Bool
  diskCleanExit : Bool
  writerCachedFlag : Option Bool
  deriving DecidableEq, Repr

inductive UnsyncedAction where
  | writerLoad
  | writerFlush
  | markCleanShutdown
  deriving DecidableEq, Repr

def stepUnsynced (s : UnsyncedState) : UnsyncedAction → UnsyncedState
  | .writerLoad =>
      { s with writerCachedFlag := some s.atomicCleanExit }
  | .writerFlush =>
      match s.writerCachedFlag with
      | some flag => { s with diskCleanExit := flag, writerCachedFlag := none }
      | none => s
  | .markCleanShutdown =>
      { s with atomicCleanExit := true, diskCleanExit := true }

/-- Explicit 3-step interleaving counterexample where `diskCleanExit` regresses from
    `true` back to `false` after `mark_clean_shutdown`. -/
theorem buggy_watchdog_regresses_clean_exit :
    let s0 : UnsyncedState := {
      atomicCleanExit := false
      diskCleanExit := false
      writerCachedFlag := none
    }
    let s1 := stepUnsynced s0 .writerLoad
    let s2 := stepUnsynced s1 .markCleanShutdown
    let s3 := stepUnsynced s2 .writerFlush
    s2.diskCleanExit = true ∧ s3.diskCleanExit = false := by
  decide

/-- Fixed mutex-serialized model: `write_current_heartbeat` acquires the heartbeat write
    mutex and reads `CLEAN_EXIT` inside that critical section. -/
structure SyncedState where
  atomicCleanExit : Bool
  diskCleanExit : Bool
  deriving DecidableEq, Repr

inductive SyncedAction where
  | lockedHeartbeatWrite
  | markCleanShutdown
  deriving DecidableEq, Repr

def stepSynced (s : SyncedState) : SyncedAction → SyncedState
  | .lockedHeartbeatWrite =>
      { s with diskCleanExit := s.atomicCleanExit }
  | .markCleanShutdown =>
      { atomicCleanExit := true, diskCleanExit := true }

def runSynced (s : SyncedState) : List SyncedAction → SyncedState
  | [] => s
  | a :: rest => runSynced (stepSynced s a) rest

/-- Safety invariant: once `atomicCleanExit = true ∧ diskCleanExit = true`, no sequence
    of concurrent heartbeat writes or shutdown calls can ever regress either flag to `false`. -/
theorem fixed_watchdog_preserves_clean_exit (s : SyncedState) (trace : List SyncedAction)
    (hClean : s.atomicCleanExit = true ∧ s.diskCleanExit = true) :
    let s' := runSynced s trace
    s'.atomicCleanExit = true ∧ s'.diskCleanExit = true := by
  induction trace generalizing s with
  | nil =>
      exact hClean
  | cons a rest ih =>
      apply ih
      cases a with
      | lockedHeartbeatWrite =>
          simp [stepSynced, hClean.1]
      | markCleanShutdown =>
          simp [stepSynced]

end PraxisSpecs.Watchdog
