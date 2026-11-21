/-!
Formal algebraic specification of the deterministic 3-way set merge used for pure Rust
`use ...;` and `mod ...;` conflict hunks in `script/resolve-upstream-sync.py`:

    merge3(base, ours, theirs) = (ours ∪ theirs) \ ((base \ ours) ∪ (base \ theirs))
-/

namespace PraxisSpecs.SyncSetMerge

/-- Characteristic-function representation of a set of declarations `α → Bool`. -/
def merge3 {α : Type} (base ours theirs : α → Bool) (x : α) : Bool :=
  (ours x || theirs x) && !((base x && !ours x) || (base x && !theirs x))

/-- Symmetry / commutativity: merging `(ours, theirs)` yields the exact same result as `(theirs, ours)`. -/
theorem merge3_comm {α : Type} (base ours theirs : α → Bool) (x : α) :
    merge3 base ours theirs x = merge3 base theirs ours x := by
  unfold merge3
  cases base x <;> cases ours x <;> cases theirs x <;> rfl

/-- Idempotence: if both sides agree on `x`, `merge3` produces that exact value. -/
theorem merge3_idem {α : Type} (base side : α → Bool) (x : α) :
    merge3 base side side x = side x := by
  unfold merge3
  cases base x <;> cases side x <;> rfl

/-- Left identity: if `ours` made no changes relative to `base`, `merge3` takes `theirs`. -/
theorem merge3_ours_unchanged {α : Type} (base theirs : α → Bool) (x : α) :
    merge3 base base theirs x = theirs x := by
  unfold merge3
  cases base x <;> cases theirs x <;> rfl

/-- Right identity: if `theirs` made no changes relative to `base`, `merge3` takes `ours`. -/
theorem merge3_theirs_unchanged {α : Type} (base ours : α → Bool) (x : α) :
    merge3 base ours base x = ours x := by
  unfold merge3
  cases base x <;> cases ours x <;> rfl

/-- Disjoint-addition preservation: any fresh declaration added by either side (not in `base`) is kept. -/
theorem merge3_preserves_new_additions {α : Type} (base ours theirs : α → Bool) (x : α)
    (hNotBase : base x = false)
    (hAdded : ours x = true ∨ theirs x = true) :
    merge3 base ours theirs x = true := by
  rcases hAdded with hO | hT
  · simp [merge3, hNotBase, hO]
  · simp [merge3, hNotBase, hT]

/-- Unilateral-deletion preservation: if an item existed in `base` and either side deleted it,
    `merge3` deletes it (preventing zombie imports/modules from reappearing after upstream cleanup). -/
theorem merge3_preserves_deletions {α : Type} (base ours theirs : α → Bool) (x : α)
    (hBase : base x = true)
    (hDel : ours x = false ∨ theirs x = false) :
    merge3 base ours theirs x = false := by
  rcases hDel with hO | hT
  · simp [merge3, hBase, hO]
  · simp [merge3, hBase, hT]

end PraxisSpecs.SyncSetMerge
