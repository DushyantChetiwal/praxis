/-!
Formal model of Architect `max_repeats` edge normalization across graph-mutation entrypoints
(`ProposedGraph::into_graph`, `set_edge_max_repeats_at`, and `GraphEdit::InsertEdge`
in `crates/architect/src/edit.rs`).

Proves:
1. Without clamping on `InsertEdge`, `maxRepeats = some 0` produces a disabled edge that
   `finish_step` never traverses (`uses (0) >= limit (0)`).
2. With `clampMaxRepeats` (`some n => some (max n 1)`), every inserted edge is enabled
   (`edgeEnabled = true`) and retains positive limits unchanged.
-/

namespace PraxisSpecs.ArchitectGraph

structure Edge where
  src : Nat
  dst : Nat
  maxRepeats : Option Nat
  deriving DecidableEq, Repr

/-- `finish_step` treats an edge with `maxRepeats = some 0` as closed (`uses 0 >= limit 0`). -/
def edgeEnabled (e : Edge) : Bool :=
  match e.maxRepeats with
  | some 0 => false
  | _ => true

/-- Normalization applied by `ProposedGraph::into_graph`, `set_edge_max_repeats_at`,
    and fixed `GraphEdit::InsertEdge`. -/
def clampMaxRepeats : Option Nat → Option Nat
  | none => none
  | some n => some (max n 1)

def normalizeEdge (e : Edge) : Edge :=
  { e with maxRepeats := clampMaxRepeats e.maxRepeats }

/-- Counterexample: pre-fix `GraphEdit::InsertEdge` accepted `maxRepeats = some 0`,
    producing a dead edge that `finish_step` never takes. -/
theorem unnormalized_insert_allows_dead_edge :
    let dead : Edge := { src := 0, dst := 1, maxRepeats := some 0 }
    edgeEnabled dead = false := by
  decide

/-- Soundness theorem: every edge inserted through `normalizeEdge` is enabled (`edgeEnabled = true`). -/
theorem normalized_insert_always_enabled (e : Edge) :
    edgeEnabled (normalizeEdge e) = true := by
  unfold normalizeEdge edgeEnabled clampMaxRepeats
  cases h : e.maxRepeats with
  | none => rfl
  | some n =>
      cases n with
      | zero => rfl
      | succ k =>
          have hpos : max (k + 1) 1 = k + 1 := by omega
          simp [hpos]

/-- Preservation theorem: positive repeat limits (`n ≥ 1`) are preserved untouched. -/
theorem clamp_preserves_positive_limit (n : Nat) (hPos : 1 ≤ n) :
    clampMaxRepeats (some n) = some n := by
  unfold clampMaxRepeats
  exact congrArg some (Nat.max_eq_left hPos)

end PraxisSpecs.ArchitectGraph
