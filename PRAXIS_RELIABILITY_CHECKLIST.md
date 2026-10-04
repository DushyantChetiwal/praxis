# Praxis desktop and mobile reliability checklist

## Delivery agreement

- Reuse `fix-loading-thread-selections`. The user approved a new PR on this branch after GitHub confirmed that PR #37 was already merged and no PR remained open.
- Preserve existing desktop windows, active conversations, pairings, and Architect checkpoints.
- Do not use subagents.
- Run builds, Rust tests, Clippy, formatting, and Android validation in GitHub Actions. Locally validate with `git diff --check` only.
- Keep the required human-review notice in the root README.
- Complete the fixes and resolve CI failures before merging. Do not merge a superseded or unvalidated head.

## Repository and PR preparation

- [x] Verify the current branch and preserve the existing uncommitted mobile work.
- [x] Verify the root README review notice is present.
- [x] Confirm the branch and PR handling: reuse `fix-loading-thread-selections`; create one new PR as explicitly approved.
- [ ] Review the target branch against current main without overwriting other work.

## Desktop

### D1 — Main-conversation work limits

- [ ] Identify the source of the reported approximately 28k work limit, distinguishing application policy, provider context/output limits, compaction, and unsupported model claims.
- [ ] Ensure main conversations do not inherit limits intended only for explicitly spawned helper agents.
- [ ] Preserve supported continuation/compaction, actual provider limits, permissions, and execution safeguards.
- [ ] Add regression coverage for long main conversations and helper-only policy isolation.

Assessment: no hard-coded 28k work limit has yet been confirmed in the inspected agent code. Do not remove unrelated byte or resource limits based on that number alone.

### D2 — Node-chat and node-execution capabilities

- [ ] Audit node deliberation and execution separately from `spawn_agent` helpers.
- [ ] Prevent helper-only limits and unintended helper model defaults from affecting node work.
- [ ] Preserve the selected model's supported tools, project context, and context management for execution.
- [ ] Preserve intentional planning-mode and permission restrictions in deliberation chats.
- [ ] Test main, node-chat, node-execution, and explicit-helper roles independently, including reload/resume.

Assessment: both Architect constructors reuse `Thread::new_subagent`; execution explicitly restores the coordinator's model. Classification and inherited policy require review rather than assuming this proves a numeric cap.

### D3 — Nested-plan completion

- [ ] Derive container completion from nested execution state, not the presence of a generated summary.
- [ ] Propagate status through all enclosing nodes and synchronize canvas, outline, inspector, checkpoints, and remote state.
- [ ] Preserve distinct failed, cancelled, skipped, pending, and completed outcomes.
- [ ] Test deep nesting, parallel branches, retries, conditional skips, interruption, and resume.

Assessment: scheduler completion can advance out of a subplan while node rendering still requires a non-empty result summary. Reproduce and fix the authoritative-state mismatch; do not invent summaries to paint nodes complete.

## Mobile and remote operation

### M1 — Preserve sessions across additional windows

- [ ] Reproduce opening another folder while existing conversations remain active.
- [ ] Inspect launcher/instance routing, channel ownership, window/session identity, and pairing reconciliation.
- [ ] Ensure opening another window never unpairs the phone or disconnects existing sessions.
- [ ] Test multiple windows and running conversations before, during, and after opening another folder.

Assessment: the disconnect is reported, not yet reproduced. The previous launcher check established only desktop-process liveness, not mobile continuity. Android currently forgets pairing when received device metadata omits the phone; determine whether that path was involved.

### M2 — Open host folders from the phone

- [ ] Add an explicit authenticated folder-browsing/opening operation instead of depending on shell commands.
- [ ] Allow accessible folders on the selected host to open in a new Praxis window without replacing existing windows.
- [ ] Keep host identity and path semantics explicit, including Windows versus WSL.
- [ ] Surface inaccessible/missing-folder failures without changing existing sessions.
- [ ] Test path handling and window preservation on supported desktop platforms.

### M3 — Header-first conversation rendering

- [ ] Initially fetch/render paginated message, tool-call, and provider-supplied-thinking headers rather than full bodies.
- [ ] Fetch and render a body only when its entry is expanded.
- [ ] Preserve ordering, stable identity, status, expansion state, and scroll position across updates.
- [ ] Avoid background body fetches for collapsed entries and discard stale replies after navigation.
- [ ] Test long histories, active steps, completed step histories, collapse/reopen, and navigation races.

### M4 — Lossless chunked body retrieval

- [ ] Replace permanent per-entry/detail truncation in the remote representation with resumable chunk retrieval.
- [ ] Keep individual requests bounded while making all desktop-stored content retrievable.
- [ ] Include cursor/offset and content-version checks to prevent gaps, duplicates, mixed revisions, and broken Unicode.
- [ ] Support complete message, tool, and thinking bodies, including large and growing entries.
- [ ] Clearly distinguish content already absent from desktop storage; do not present missing source content as complete.
- [ ] Add exact-reassembly, reconnect/retry, malformed-cursor, and mutation-during-fetch tests.

Assessment: current transcript entries are capped at 6,000 serialized bytes, and the uncommitted detail-fetch implementation also truncates large details. Lazy rendering alone is not lossless. This requirement supersedes that draft behavior.

### M5 — Queue, Steer, and Send Now

- [ ] Expose Queue (after the current turn), Steer (next supported turn boundary), and Send Now (interrupt and dispatch) as distinct supported actions.
- [ ] Operate on queued-message identity rather than resending phone text.
- [ ] Preserve other queued messages and accurately display pending, acknowledged, removed, and failed states.
- [ ] Prevent duplicate dispatch and stale-session actions.
- [ ] Test all three actions while generating, after cancellation, and after navigation/reconnection.

Assessment: desktop queue logic already supports steering and immediate dispatch. The existing uncommitted phone patch adds Send Now but not steering and is not deployed.

### Existing mobile work to retain and review

- [ ] Retain model selection with authoritative model IDs, unavailable-choice handling, and surfaced selection errors.
- [ ] Reconcile the earlier mobile patch with M1–M5 rather than treating that patch as validated or shipped.
- [ ] Maintain pairing, end-to-end encryption, authentication, and replay protections.
- [ ] Preserve older-client compatibility or explicitly document a negotiated protocol migration.
- [ ] Do not describe GitHub snapshot polling as token streaming; any live transport deployment is a separate approved scope.

## CI monitoring and merge gate

- [ ] Run local `git diff --check` before each push.
- [ ] Commit focused changes and push to the confirmed existing PR branch; no force push or unrelated branch replacement.
- [ ] Update the PR description with scope, regression coverage, remaining limitations, and final Release Notes section.
- [ ] Inspect Actions status for the exact pushed head SHA once, then use the wait to review code, investigate defects, and improve regression coverage.
- [ ] Retrieve actionable failed-job logs, fix failures at their cause, push, and repeat validation on the new head.
- [ ] Confirm desktop quality checks, formatting, configured Clippy checks, Android unit tests, lint, and APK build pass for the final head.
- [ ] Confirm the PR is mergeable and required reviews/checks are satisfied; do not bypass failures or disable validation.
- [ ] Recheck the final head SHA immediately before merging and use an exact-head guard.
- [ ] Merge the existing PR after all fixes and checks are clear.
- [ ] Verify GitHub records the merge and the expected post-merge automatic validation/build workflow is triggered; do not dispatch duplicate builds.
- [ ] Record the PR URL, final head, successful CI run URLs, merge SHA, and post-merge workflow status below.

## Delivery evidence

Pending: target PR confirmation, implementation, CI validation, and merge.
