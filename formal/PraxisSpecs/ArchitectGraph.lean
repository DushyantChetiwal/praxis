/-!
Formal model of Architect dead-edge (`max_repeats = Some 0`) routing consistency
between `PlanRun::finish_step` and `reachable_before` / `distances_from`
(`crates/architect/src/run.rs`, `crates/architect/src/architect.rs`).
-/

namespace PraxisSpecs.ArchitectGraph

structure Edge where
  src : Nat
  dst : Nat
  maxRepeats : Option Nat
  deriving DecidableEq, Repr

/-- `finish_step` skips any edge whose `maxRepeats` is `some 0` (`uses (0) >= limit (0)`). -/
def edgeEnabled (e : Edge) : Bool :=
  match e.maxRepeats with
  | some 0 => false
  | _ => true

/-- Single-step adjacency used by pre-fix `reachable_before` / `distances_from`
    (does not filter `maxRepeats = some 0`). -/
def buggyCanStep (edges : List Edge) (u v : Nat) : Bool :=
  edges.any (fun e => e.src == u && e.dst == v)

/-- Single-step adjacency matching `finish_step` (ignores `maxRepeats = some 0`). -/
def fixedCanStep (edges : List Edge) (u v : Nat) : Bool :=
  edges.any (fun e => edgeEnabled e && e.src == u && e.dst == v)

/-- Counterexample: an edge with `maxRepeats = some 0` makes `buggyCanStep` report
    reachability from `0` to `1` even though `finish_step` can never traverse it. -/
theorem buggy_reachability_disagrees_with_finish_step :
    let edges : List Edge := [{ src := 0, dst := 1, maxRepeats := some 0 }]
    buggyCanStep edges 0 1 = true ∧ fixedCanStep edges 0 1 = false := by
  decide

/-- Reachability closure over a bounded number of hops `fuel`. -/
def reachableWith (step : Nat → Nat → Bool) (nodes : List Nat) : Nat → Nat → Nat → Bool
  | 0, u, v => u == v
  | fuel + 1, u, v =>
      (u == v) || nodes.any (fun w => step u w && reachableWith step nodes fuel w v)

/-- Dead-edge invariance theorem: adding or removing edges with `maxRepeats = some 0`
    cannot change `fixedCanStep`. -/
theorem fixed_step_ignores_zero_repeat_edge (edges : List Edge) (dead : Edge)
    (hDead : dead.maxRepeats = some 0) (u v : Nat) :
    fixedCanStep (dead :: edges) u v = fixedCanStep edges u v := by
  simp [fixedCanStep, edgeEnabled, hDead]

/-- Corollary: `reachableWith (fixedCanStep ...)` is completely invariant under dead edges. -/
theorem fixed_reachability_invariant_under_dead_edges (edges : List Edge) (dead : Edge)
    (hDead : dead.maxRepeats = some 0) (nodes : List Nat) (fuel u v : Nat) :
    reachableWith (fixedCanStep (dead :: edges)) nodes fuel u v =
      reachableWith (fixedCanStep edges) nodes fuel u v := by
  induction fuel generalizing u with
  | zero =>
      rfl
  | succ k ih =>
      simp only [reachableWith]
      congr 1
      apply congrArg (List.any nodes)
      funext w
      rw [fixed_step_ignores_zero_repeat_edge edges dead hDead u w, ih w]

end PraxisSpecs.ArchitectGraph
