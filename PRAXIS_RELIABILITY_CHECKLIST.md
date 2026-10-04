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
- [x] Review the target branch against current main without overwriting other work. Merged `origin/main` into the existing branch after committing the previously uncommitted work.
- Draft PR: https://github.com/DushyantChetiwal/praxis/pull/38

Implementation boxes below describe code changes; unchecked regression and final-head CI gates remain required before merge.

## Desktop

### D1 — Main-conversation work limits

- [x] Audit the reported approximately 28k work limit. No native task-wide 28k cap was found; distinguish provider per-response/context limits from application work policy and unsupported model claims.
- [ ] Ensure main conversations do not inherit limits intended only for explicitly spawned helper agents.
- [ ] Preserve supported continuation/compaction, actual provider limits, permissions, and execution safeguards.
- [x] Add request-policy regression coverage for main conversations, node roles, and helper-only policy isolation; retain existing compaction coverage.

Assessment: no hard-coded 28k task-wide limit was found. Main completion requests do not set an output-token override. The context-limit cancellation wrapper is in `NativeSubagentHandle::send`, not the coordinator or Architect runner. Added guidance against invented task-wide budgets and regression assertions that main/Architect requests do not receive helper completion intent or an artificial output override. CI validation pending.

### D2 — Node-chat and node-execution capabilities

- [x] Audit node deliberation and execution separately from `spawn_agent` helpers.
- [x] Separate helper policy/defaults from node construction and restore legacy node-chat roles without rewriting saved model choices.
- [ ] Preserve the selected model's supported tools, project context, and context management for execution.
- [ ] Preserve intentional planning-mode and permission restrictions in deliberation chats.
- [ ] Test main, node-chat, node-execution, and explicit-helper roles independently, including reload/resume.

Implementation in progress: separate persisted child roles now distinguish helpers, Architect chats, and Architect execution without losing parent ownership. Node chats no longer inherit the helper model default; node ownership does not consume helper recursion depth. Main/node request classification and model inheritance have regression coverage awaiting CI.

### D3 — Nested-plan completion

- [x] Derive container completion from nested execution state, not the presence of a generated summary.
- [ ] Propagate status through all enclosing nodes and synchronize canvas, outline, inspector, checkpoints, and remote state.
- [ ] Preserve distinct failed, cancelled, skipped, pending, and completed outcomes.
- [ ] Test deep nesting, parallel branches, retries, conditional skips, interruption, and resume.

Implemented scheduler-owned container completion, retry invalidation, and cached UI readiness. Canvas, outline, inspector, and automation use execution state independently of summary text. Failed/cancelled/interrupted presentation is separate from scheduler eligibility. Added nested-fork, replay, and GPUI regressions; final validation pending.

### D4 — Inspect background agent terminals in terminal tabs

- [ ] Surface background agent terminals as labeled tabs in the owning workspace's terminal panel.
- [ ] Attach to the original live terminal/output; never rerun a command to inspect it.
- [ ] Preserve command/session ownership, output, exit status, and explicit stop controls.
- [ ] Do not steal focus, open docks in Architect mode, or recreate terminals during workspace mode switches.
- [ ] Cover main and node execution tasks, multiple simultaneous terminals, completed tasks, and view close/reopen behavior.
- [x] Add GPUI regression coverage for original-terminal identity, output continuity, duplicate suppression, focus/dock preservation, and close/reopen.

## Mobile and remote operation

### M1 — Preserve sessions across additional windows

- [ ] Reproduce opening another folder while existing conversations remain active.
- [ ] Inspect launcher/instance routing, channel ownership, window/session identity, and pairing reconciliation.
- [ ] Ensure opening another window never unpairs the phone or disconnects existing sessions.
- [ ] Test multiple windows and running conversations before, during, and after opening another folder.

Concrete cause found: normal Dev launches bypassed the app single-instance guard, so the CLI could spawn another process publishing to the same remote gist. Removed that exception; added a standard-library OS file lock around remote ownership and destructive local operations. Android no longer deletes a pairing key or leaves the active device merely because remote metadata/refusal is inconsistent. Existing sessions are preserved while pairing is re-confirmed. Regression validation pending.

### M2 — Open host folders from the phone

- [x] Add authenticated host-folder paging and same-process new-window opening operations.
- [ ] Allow accessible folders on the selected host to open in a new Praxis window without replacing existing windows.
- [ ] Keep host identity and path semantics explicit, including Windows versus WSL.
- [ ] Surface inaccessible/missing-folder failures without changing existing sessions.
- [ ] Test path handling and window preservation on supported desktop platforms.

### M3 — Header-first conversation rendering

- [x] Fetch/render paginated message, tool-call, and provider-supplied-thinking headers before expanded bodies.
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
- [x] Add exact-reassembly, duplicate/gap/version rejection, malformed-cursor, append-only growth, and prefix-rewrite tests.

Implemented versioned UTF-8 body chunks with a bounded escaped-text response, not a body-length cap. New phone requests do not use legacy per-entry/body truncation. Header aggregation keeps oversized part lists addressable. Parsing runs off the UI thread; blocks render lazily. Legacy endpoints remain compatible and explicit about truncation. Final backend validation pending.

### M5 — Queue, Steer, and Send Now

- [ ] Expose Queue (after the current turn), Steer (next supported turn boundary), and Send Now (interrupt and dispatch) as distinct supported actions.
- [ ] Operate on queued-message identity rather than resending phone text.
- [ ] Preserve other queued messages and accurately display pending, acknowledged, removed, and failed states.
- [ ] Prevent duplicate dispatch and stale-session actions.
- [ ] Test all three actions while generating, after cancellation, and after navigation/reconnection.

Implemented queue paging, UUID message identities, full queued-body inspection, explicit steering set/unset, and Send Now using existing desktop state machines. Phone controls work for desktop-queued messages too. Added regressions for repeat requests and identical-message acknowledgement across polls. Final validation pending.

### M6 — Answer blocking agent questions from the phone

- [ ] Publish pending native question/elicitation prompts for the active root and its running steps.
- [ ] Display the complete question, choice descriptions, multi-select/free-text controls, and recommendation/timeout state where applicable.
- [x] Wire session/request-scoped answers through the existing question validator and elicitation resolver.
- [ ] Reject stale, expired, cancelled, or already-answered requests without double submission or answering another question.
- [ ] Keep permission approvals and ordinary questions distinct; do not treat a question answer as tool permission or plan-lock approval.
- [ ] Test that a phone answer unblocks the waiting agent, including nested/parallel step questions and desktop/phone answer races.

### Existing mobile work to retain and review

- [ ] Retain model selection with authoritative model IDs, unavailable-choice handling, and surfaced selection errors.
- [ ] Reconcile the earlier mobile patch with M1–M5 rather than treating that patch as validated or shipped.
- [ ] Maintain pairing, end-to-end encryption, authentication, and replay protections.
- [ ] Preserve older-client compatibility or explicitly document a negotiated protocol migration.
- [ ] Do not describe GitHub snapshot polling as token streaming; any live transport deployment is a separate approved scope.

## CI monitoring and merge gate

- [x] Run local `git diff --check` before each push; continue this gate for subsequent fixes.
- [x] Commit focused changes and push to the confirmed existing PR branch; no force push or unrelated branch replacement.
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

- PR: https://github.com/DushyantChetiwal/praxis/pull/38 (draft, same branch).
- Initial pushed head: `8c2cfc802306b9b1b8ad33a7e732c0f8744e5287`.
- Initial Android validation: https://github.com/DushyantChetiwal/praxis/actions/runs/37195385984 — passed; this is not the final implementation.
- Initial desktop validation: https://github.com/DushyantChetiwal/praxis/actions/runs/37195385972 — formatting failures identified and addressed; integration validation still running when inspected.
- Android validation of `025e13cd1b7cc29f9e0d0ffa812ac6e2cda654ba`: https://github.com/DushyantChetiwal/praxis/actions/runs/37200886419 — passed (questions UI included).
- Desktop validation of that head: https://github.com/DushyantChetiwal/praxis/actions/runs/37200886423 — native graph/path jobs passed on Linux, Windows, and macOS; the shared terminal-catalog compile error and formatting findings are being corrected.
- Further fixes, final-head CI, readiness review, and merge remain pending.
