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
- [ ] Run a bounded synthetic relay interoperability/latency probe without private user content; do not load-test public relays.

## Phone image input

- [ ] Add the system photo picker, preview/remove controls, and image-only or text-plus-image messages.
- [ ] Normalize selected images off the UI thread, preserve orientation, remove metadata, and bound decoding/upload size.
- [ ] Transfer encrypted chunks bound to the paired phone and target conversation; validate offsets, format, size, and expiry.
- [ ] Deliver actual ACP image content through the desktop queue/steer/Send Now paths, not an image URL or text placeholder.
- [ ] Advertise model image capability and retain recoverable drafts on upload/send failures.
- [ ] Test chunking, cross-phone/session isolation, malformed images, expiry, queue content, and mobile draft behavior.

## Validation and delivery

- [ ] Update protocol and user documentation, including relay privacy/capacity caveats.
- [ ] Generate dependency lockfile changes, formatting, Rust/GPUI tests, and Android build/lint/tests in GitHub Actions only.
- [ ] Review resulting dependency and source diffs; locally run only `git diff --check` for validation.
- [ ] Push the existing branch and use one PR; monitor exact-head CI while reviewing/fixing code.
- [ ] Merge only after all required checks pass on the final unchanged head; verify automatic post-merge builds.

Unchecked items are not implemented or validated yet. A public relay is third-party infrastructure, not a promise of unlimited traffic, delivery, or permanent free availability.
