# Praxis Nostr transport and phone images

## Delivery constraints

- [x] Keep the existing branch, README review notice, live windows, pairings, and user work intact; no subagents.
- [x] Fast-forward the existing branch to the merged main before making source changes.
- [ ] Use maintained Nostr libraries; no paid infrastructure or new account/pairing steps.
- [ ] Keep GitHub pairing and compatibility fallback. Never fall back by reposting a command that may already have executed.
- [ ] Keep encrypted payloads, peer authentication, session ownership, permissions, and bounded memory/traffic.

## Live transport

- [ ] Add a separate WebSocket transport so GitHub throttling cannot block paired live sessions.
- [ ] Bind direction-specific relay identities to existing pairing secrets, without exposing those secrets.
- [ ] Use a desktop incarnation handshake, request IDs, result receipts, replay rejection, and reconnect-safe duplicate handling.
- [ ] Push changed compact snapshots; keep full history on the desktop and fetch details only on demand.
- [ ] Back off and report relay failure honestly; retain the existing fallback for older desktops and unavailable relays.
- [ ] Add deterministic tests for authentication, stale/duplicate/reordered traffic, restart ambiguity, lifecycle cleanup, and fallback boundaries.
- [x] Run a bounded synthetic probe with no user content: Actions run 37460811242 delivered Damus encrypted round trips in 204–336 ms; nos.lol timed out. Replacement probe 37472159528 delivered all three round trips on both Damus (216–231 ms) and Primal (454–530 ms); the transport now selects those two relays. These are runner measurements, not phone latency or capacity guarantees.

## Phone image input

- [ ] Add the system photo picker, preview/remove controls, and image-only or text-plus-image messages.
- [ ] Normalize selected images off the UI thread, preserve orientation, remove metadata, and bound decoding/upload size.
- [ ] Transfer encrypted chunks bound to the paired phone and target conversation; validate offsets, format, size, and expiry.
- [ ] Deliver actual ACP image content through the desktop queue/steer/Send Now paths, not an image URL or text placeholder.
- [ ] Advertise model image capability and retain recoverable drafts on upload/send failures.
- [ ] Test chunking, cross-phone/session isolation, malformed images, expiry, queue content, and mobile draft behavior.

## Desktop memory — added after the reported system crash

- [x] Inspect Windows events and process memory without closing windows or collecting a large heap dump. Windows records an unexpected shutdown, not a confirmed OOM cause; Praxis was using about 19 GiB resident / 23 GiB private memory.
- [x] Measure saved-thread storage without materializing its JSON object tree: the database-generation thread expands from about 69 MiB compressed to 612 MiB, including 609 MiB of messages and 2.4 MiB of Architect state.
- [x] Audit eager native replay, intermediate JSON loading/serialization, and idle-view retention. These are confirmed amplification paths, not a complete attribution of the observed process footprint.
- [x] Implement streaming current-format thread decode/encode without the expanded JSON serialization buffer or full intermediate JSON value tree; retain the legacy migration path. CI regression pending.
- [x] Defer parsing of off-screen agent Markdown and formatting of closed historical raw inputs; reclaim parsing artifacts on collapse. Original source and native model context remain intact. This is not yet disk paging of the entire native message history. CI regression pending.
- [ ] Bound disposable detail/rendering caches without stopping active agents, deleting history, or limiting task capability.
- [ ] Add regression coverage for long histories, lazy details, save/load compatibility, and unaffected checkpoints.
- [ ] Validate in Actions and include the memory work in PR #40 before merge, as requested.

## Additional requested regressions

- [x] Implement ViewModel-owned loaded Android details, bounded memory and encrypted app-private disk caching, scoped identities, explicit refresh, and stale-write rejection. CI regressions pending.
- [ ] Test cache reuse, memory eviction/disk restoration, refresh invalidation, and message/window isolation.
- [x] Add and run an Actions-only on/off comparison for Architect test and Clippy rebuilds, including unchanged builds, three controlled edits, and artifact size. Release policy remains unchanged. The first run exposed rust-cache overriding the enabled matrix to 0; a measured-step override, mode assertion, and required artifact corrected it. Valid run 37484749958 measured median edited test rebuilds at 4.105 s off / 1.619 s on and Clippy at 2.668 s off / 1.525 s on, with 237,112,844 bytes of incremental artifacts. This is a small same-runner Architect workload, not a full desktop build or cross-run cache benchmark.

## Additional desktop/mobile interaction fixes — same PR #40

- [x] Implement compact terminal-wait status rows, monotonic GPUI-executor countdown updates, and no UUID in normal display; coalesce concurrent waits to one active timer, keep failures inspectable, and preserve task data and early wake/cancellation semantics. CI regression pending.
- [x] Implement last-interaction answer modes on desktop and Android; freeform interaction clears visual choice selection, while a later choice overrides retained text. Passive window activation does not change the answer mode. CI regressions pending.
- [x] Add desktop GPUI and Android regressions for interaction-order precedence without erasing the inactive text draft; execution pending in CI.
- [x] Implement a shared current-plan status projection that hides stopped empty-plan banners/counts on desktop and mobile without erasing run history; preserve Stop for active runs. CI regressions pending.
- [x] Add GPUI regressions for wait countdown/early completion and empty-plan status cleanup; keep existing expiry/cancellation coverage. Execution pending in CI.
- [x] Investigate reported missing file tools in Build: saved settings enable write tools; the affected transcript first describes the Architect-mode read-only inventory, then reports the full inventory and calls write_file/edit_file. Mode refresh already occurs at mode changes and before requests. Exact earlier request mode is not recorded, so the historical cause remains unconfirmed. Added content-free request capability/mode-change logs and a GPUI regression checking actual provider requests across in-flight Plan/Architect ↔ Build switches, without weakening permissions. CI pending. Also found and fixed a native notification gap: mode changes did not emit ACP ModeUpdated for conversation controls; added event coverage and explicit selector refresh. This gap is confirmed in source, but its role in the historical report is not proven.
- [x] Add Android crash-orphan cache cleanup that preserves every current-process ViewModel cache; regression added, CI pending.
- [x] Make relay shutdown interrupt blocked queue/network work, bound socket shutdown, and retry unexpected service exits with a new desktop epoch; regression added, CI pending.

All requested changes above belong to PR #40 on the existing branch; do not split them into another PR or merge a partially validated head.

## Validation and delivery

- [ ] Update protocol and user documentation, including relay privacy/capacity caveats.
- [ ] Generate dependency lockfile changes, formatting, Rust/GPUI tests, and Android build/lint/tests in GitHub Actions only.
- [ ] Review resulting dependency and source diffs; locally run only `git diff --check` for validation.
- [ ] Push the existing branch and use one PR; monitor exact-head CI while reviewing/fixing code.
- [ ] Merge only after all required checks pass on the final unchanged head; verify automatic post-merge builds.

Unchecked items are not implemented or validated yet. A public relay is third-party infrastructure, not a promise of unlimited traffic, delivery, or permanent free availability.
