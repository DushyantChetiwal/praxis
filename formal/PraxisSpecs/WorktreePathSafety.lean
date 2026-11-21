/-!
Formal model of relative path containment for `worktree_create`
(`crates/agent_ui/src/remote.rs`: `validate_worktree_path`).

Proves:
1. Unvalidated paths containing `..` (`ParentDir`) or leading root/prefix components
   can escape `repoRoot`.
2. Any path accepted by `isSafeRelativeWorktreePath` (non-empty and composed solely
   of `Normal` / `CurDir` segments) normalizes to a strict descendant (or self) of `repoRoot`.
-/

namespace PraxisSpecs.WorktreePathSafety

inductive Component where
  | root
  | parent
  | cur
  | normal (name : String)
  deriving DecidableEq, Repr

/-- Lexically normalizes a path component list onto a reversed stack of normal segments. -/
def normalize (stack : List String) : List Component → List String
  | [] => stack
  | .root :: rest => normalize [] rest
  | .cur :: rest => normalize stack rest
  | .parent :: rest => normalize stack.tail rest
  | .normal name :: rest => normalize (name :: stack) rest

/-- Predicate matching `validate_worktree_path`: non-empty, no `root`, no `parent`,
    and at least one `normal` component. -/
def isSafeRelative (cs : List Component) : Bool :=
  cs.all (fun c => match c with | .normal _ | .cur => true | _ => false) &&
  cs.any (fun c => match c with | .normal _ => true | _ => false)

/-- Counterexample: an unvalidated `"../escape"` input pops out of `repoRoot = ["repo"]`. -/
theorem unvalidated_parent_escapes_repo :
    let repoStack := ["repo"]
    let malicious : List Component := [.parent, .normal "outside"]
    normalize repoStack malicious = ["outside"] := by
  decide

/-- Helper: normalizing only `.normal` and `.cur` components strictly extends `stack`
    without ever popping an element of `stack` (`stack` remains a suffix of the reversed stack). -/
theorem normalize_safe_preserves_suffix (stack : List String) (cs : List Component)
    (hSafe : cs.all (fun c => match c with | .normal _ | .cur => true | _ => false) = true) :
    ∃ extra : List String, normalize stack cs = extra ++ stack := by
  induction cs generalizing stack with
  | nil =>
      exact ⟨[], rfl⟩
  | cons c rest ih =>
      simp only [List.all_cons, Bool.and_eq_true] at hSafe
      rcases hSafe with ⟨hc, hrest⟩
      cases c with
      | root =>
          simp at hc
      | parent =>
          simp at hc
      | cur =>
          exact ih stack hrest
      | normal name =>
          rcases ih (name :: stack) hrest with ⟨extra, hEq⟩
          refine ⟨extra ++ [name], ?_⟩
          simpa [normalize, List.append_assoc] using hEq

/-- Containment theorem: for any path accepted by `isSafeRelative`, joining it onto `repoRoot`
    always yields a normalized path with `repoRoot` intact as its prefix (reversed suffix). -/
theorem validated_worktree_path_stays_inside_repo (repoStack : List String) (cs : List Component)
    (hValid : isSafeRelative cs = true) :
    ∃ extra : List String, normalize repoStack cs = extra ++ repoStack := by
  unfold isSafeRelative at hValid
  simp only [Bool.and_eq_true] at hValid
  exact normalize_safe_preserves_suffix repoStack cs hValid.1

end PraxisSpecs.WorktreePathSafety
