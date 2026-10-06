# Praxis Remote protocol, version 2

This is the wire format between Praxis on a computer (`crates/agent_ui/src/remote/`) and the Android app (`remote-android/`). Both must follow it exactly. [Praxis Remote](./praxis-remote.md) describes the feature itself.

## Goals

- Anyone with Praxis and the app can use it with their own GitHub account and nothing else: no repository, configuration file or app installation.
- Nobody but the user's own computer and approved phones can read or send anything: not GitHub, not whoever holds a gist's link, and not the maintainers of Praxis.
- A new phone can only be added by someone at the computer.

## Accounts

Both sides sign in with GitHub's device flow for the Praxis Remote GitHub App, which needs only its client ID. The app requests one permission, **Account permissions → Gists: Read and write**, which applies to user access tokens without the app being installed anywhere. Tokens expire after eight hours and are refreshed with `POST https://github.com/login/oauth/access_token` (`client_id`, `grant_type=refresh_token`, `refresh_token`); no client secret is needed for tokens from the device flow.

The computer and the phone must be signed in to the same GitHub account. Each side only ever acts on comments written by its own account.

## The channel

Each computer owns one **secret gist** in the user's account, created by Praxis. It has two files:

- `praxis-remote.json`, in plain text, so an unpaired phone can find and name the computer:

  ```json
  {
    "protocol": "praxis-remote/v2",
    "channel": "0123456789abcdef0123456789abcdef",
    "device": "Laptop",
    "started_at": "2026-01-01T00:00:00Z",
    "last_seen": "2026-01-01T00:05:00Z",
    "phones": ["fedcba9876543210fedcba9876543210"]
  }
  ```

  `channel` is 32 lowercase hex digits chosen at random when Praxis is first set up, and identifies the computer even if its gist is recreated. `phones` lists the paired phones' ids. `last_seen` is refreshed at least every minute while Praxis runs; a computer is online if it is under three minutes old.

- `state.json`: one encrypted snapshot per paired phone, `{ "<phone_id>": "<blob>" }`, or `{}` with no phones. The snapshot is only produced while at least one phone is paired.

A phone finds computers by listing the user's gists (`GET /gists`, newest first) and reading those that have a `praxis-remote.json` file with `"protocol": "praxis-remote/v2"`.

Requests, answers and pairing travel as **comments on the gist**. The comments list is read oldest first, 100 per page; the computer also reads the last page when the `Link` header has one.

## Identifiers and encodings

- `channel` and `phone_id`: 32 lowercase hex digits. A phone chooses its id at random once and keeps it.
- Request ids: 1 to 64 characters from `A-Z a-z 0-9 - _`.
- Binary values are base64 with padding (RFC 4648, standard alphabet).
- Public keys are P-256 points in uncompressed form: 65 bytes, `0x04 || X || Y`.
- A **blob** is `base64(nonce || ciphertext || tag)`: AES-256-GCM with a random 12-byte nonce and a 16-byte tag.
- Times are RFC 3339 in UTC.

Every blob is bound to where it belongs through its additional authenticated data (AAD), a UTF-8 string:

| Blob                     | AAD                                                           |
| ------------------------ | ------------------------------------------------------------- |
| Snapshot in `state.json` | `praxis-remote/v2/state/<channel>/<phone_id>`                 |
| Request                  | `praxis-remote/v2/request/<channel>/<phone_id>`               |
| Answer                   | `praxis-remote/v2/response/<channel>/<phone_id>/<request_id>` |

## Optional live Nostr transport

Updated desktops advertise `"nostr": 1` in their public gist metadata. A paired phone can also probe the live channel
when that metadata is unavailable; no command is sent until an authenticated desktop handshake succeeds. GitHub
still handles pairing, discovery, unpairing, and compatibility fallback. Its network calls run separately from the
live service, including during rate-limit backoff.

The prototype uses `wss://relay.damus.io` and `wss://nos.lol`, NIP-01 WebSockets, and application-specific ephemeral
kind `21761`. This is **not** a NIP-17 message or a NIP-59 gift wrap. The application retains its existing AES-GCM
paired-channel encryption instead of introducing a second private-message cryptosystem. Nostr signing and event
validation use the maintained Rust SDK and its Android bindings.

For each paired channel, derive separate desktop and phone signing secrets:

```text
prk = HKDF-Extract("praxis-remote/nostr/v1", existing_pairing_key)
secret(role) = HKDF-Expand(prk, "praxis-remote/v2/nostr/<channel>/<phone_id>/<role>", 32)
role = "desktop" or "phone"
```

Import the result as a secp256k1 secret scalar using the SDK; invalid scalars fail safely to fallback. These are not
the P-256 pairing keys. The existing pairing secret authenticates the derived identities without trusting keys
announced by a relay. They remain private and are never written into public metadata or project files.

Packets have the expected author's signature and a `p` tag naming the peer. Content is an AES-GCM blob whose AAD is
`praxis-remote/v2/nostr/<channel>/<phone_id>/request` toward the desktop, or
`praxis-remote/v2/nostr/<channel>/<phone_id>/desktop` toward the phone. Receivers verify the signature, author,
recipient, kind, encrypted authentication tag, and size. Relay event timestamps must be within the request age window.
Packets are bounded to 96,000 base64 characters, leaving room below the tested relays' WebSocket limits.

A request is `{id, op, args, sent_at, epoch}`. The read-only `hello` operation may omit `epoch`; its authenticated reply
returns the desktop's fresh random incarnation ID. Every subsequent operation must match that ID and the normal
five-minute request/two-minute clock-skew window. An old request cannot execute in a restarted desktop session.
Responses carry `{type: "response", epoch, id, ok, result | error}`. The desktop records a request before dispatch,
rejects different payloads reusing the same ID, and retains bounded result receipts until requests expire. A duplicate
with an unavailable receipt is explicitly ambiguous, not executed again. Caches have both entry and byte bounds.

The app may use GitHub when no live command has been submitted. After possible submission it can only retry the
**same** request in that desktop incarnation; it never silently falls back by creating another GitHub command.
This does not promise exactly-once arbitrary commands across manual resends with new IDs. Relay acceptance is not
peer delivery or execution confirmation.

`watch` requests produce `{type: "snapshot", epoch, sequence, snapshot}` packets. Sequence numbers increase within
an incarnation. Phones ignore duplicates, older sequences, and unverified incarnations. The inner snapshot has the
same compact-header and lossless-detail contract as GitHub snapshots. Changes are coalesced to at most one snapshot
per second, with a 15-second heartbeat. Only recently live phones receive them; the phone closes relay connections
in the background. `batch` and `unpair` continue through GitHub.

Relays observe connection metadata and can reject or throttle traffic. Ephemeral delivery is not durable storage,
a deletion guarantee, forward secrecy, or a permanent free-capacity promise. No paid relay, central service, or
public image-hosting account is configured.

## Pairing

Pairing agrees a key for one phone and shows a six-digit code on both screens. The user approves the phone on the computer only if the codes match. The phone commits to its key before it sees the computer's, so someone who can edit comments as the user still cannot choose keys that make the codes match (a one in a million chance per attempt).

It all happens in one comment whose first line is `praxis-remote/v2 pair` and whose remaining text is a JSON object that grows at each step. Whoever writes a step rewrites the whole comment, keeping every field already there.

1. **Phone** creates a fresh P-256 key pair and posts:
   `{"phone_id": "…", "name": "Pixel 8", "commit": "<base64 SHA-256("praxis-remote/v2/commit" || 0x00 || phone_public)>"}`
   `name` is at most 60 characters.
2. **Computer** creates a fresh P-256 key pair and adds `"desktop_key": "<base64 desktop_public>"`.
3. **Phone** adds `"phone_key": "<base64 phone_public>"`.
4. **Computer** checks the key against `commit`, derives the key and code, and asks the user. It then adds `"status"`: `"approved"`, `"denied"`, `"expired"` or `"invalid"`. The phone deletes the comment once it sees a status.

The computer reads `phone_id`, `name` and `commit` once, at step 1, and ignores later changes to them. It ignores pairing comments more than five minutes old, and marks any pairing still unfinished five minutes after it began `"expired"`.

Both sides derive the same secrets:

```
shared     = x-coordinate of ECDH(own private key, other public key)     (32 bytes)
transcript = SHA-256("praxis-remote/v2/pair" || 0x00 || channel || 0x00 || phone_id || 0x00
                     || phone_public || desktop_public)
prk        = HKDF-Extract(salt = transcript, ikm = shared)                 (HKDF-SHA-256)
key        = HKDF-Expand(prk, "praxis-remote/v2/key", 32)
code       = big-endian u32 of HKDF-Expand(prk, "praxis-remote/v2/code", 4) mod 1,000,000,
             as six digits with leading zeros, shown as "807 021"
```

`channel` and `phone_id` are their ASCII hex text.

## Requests and answers

The phone posts a comment:

```
praxis-remote/v2 request <phone_id>
<blob of {"id": "<request_id>", "op": "…", "args": {…}, "sent_at": "<time>"}>
```

The computer carries it out and edits that same comment into its answer:

```
praxis-remote/v2 response <phone_id> <request_id>
<blob of {"id": "<request_id>", "ok": true, "result": …}  or  {"id": …, "ok": false, "error": "…"}>
```

The phone reads the answer, then deletes the comment.

If the computer cannot use a request at all (the phone is not paired, or the blob does not decrypt), it answers in plain text so the phone can explain:

```
praxis-remote/v2 rejected <phone_id>
<reason>
```

The computer refuses a request when the comment or its `sent_at` is more than five minutes old, when `sent_at` is more than two minutes in the future, or when it has already seen that request id from that phone. Operations are the same as in version 1 (`status`, `threads`, `thread`, `prompt`, `stop`, `new_thread`, `open_thread`, `permission`, `mode`, `architect`, `list_dir`, `read_file`, `watch`, `batch`), plus `unpair`, which removes the phone that sends it, and `download`.

Plain-text sizes are capped so that every comment fits GitHub's 65,536-character limit: 46,000 bytes for an answer. Larger answers become an error.

### Model selection and queued messages

`status.windows[].thread` advertises `model_selection`, `model` (the opaque current model ID), `model_name`, `send_now`, `queue_management`, and `steering`.
Clients hide the new controls when these flags are absent. Model and Send Now requests require the active root's
`session_id` and may include `window`; a different active root is rejected rather than silently targeted.

- `models`: returns `current`, `available` (objects with opaque `id`, `name`, optional `group`, and `disabled`), and
  `next_offset`. Pass a returned non-null cursor as `offset` to load another page. Each page holds at most 100 models
  and fits a 40,000-byte array budget; IDs are never shortened or split into provider/model components.
- `model`: accepts `model` from that list, validates availability, and awaits the session selector's result. Errors
  are returned to the phone. Selection follows desktop behavior, including the native selector's saved default.
  It does not replace models already assigned to running Architect steps.
- `prompt`: optionally accepts `session_id` and either `send_now: true` or `steer: true`, never both. Normal sending
  returns `queued`, `steer`, optional `queue_id`, and `session_id`. Send Now uses the desktop queue/cancellation state
  machine to interrupt the current turn; other queue entries are preserved.
- `send_now`: accepts the acknowledged `session_id` and `queue_id`. It sends that queue entry, not a copy of the
  phone's text. A missing ID returns `sent: false` and never resends a delivered or removed message.
- `queue`: requires the root `session_id` and returns bounded pages of `entries` (`id`, text preview, `steer`),
  `total`, and `next_offset`. Queue IDs are opaque UUIDs, not row indices or counters reused after reopening a view.
- `steer`: sets the identified entry's `steer` boolean explicitly and reports `found`. It does not toggle blindly
  or reorder the queue. Only the front message's flag controls the next supported turn boundary. This operation is
  exposed only for agents with native steering support.
- `queue_content`: retrieves the full queued content by `session_id` and `queue_id`, using the body-chunk contract
  below. A removed message cannot be fetched or recreated by an old action.

These are additive version-2 extensions. They do not change pairing, encryption, replay protection, or transport
cadence for legacy clients. Updated clients prefer the optional live channel described above; the serialized GitHub
comment channel remains the compatibility fallback.

### Image input

A root's `image_input` flag reflects its current ACP image capability. Clients only offer image attachment when it
is true. Image operations require the exact root `session_id` and optional `window`; both uploads and consumption
are scoped to the authenticated phone and conversation, not to caller-provided phone identifiers.

- `image_begin`: `{session_id, window?, client_id?, size, mime_type}` reserves a bounded in-memory upload and returns
  `{upload_id, chunk_bytes, next_offset, ready}`. `client_id` is a phone-generated UUID that makes retries resume the
  same image; reusing it with different target or metadata is rejected. Valid formats are PNG, JPEG, and WebP.
- `image_chunk`: `{session_id, window?, upload_id, offset, data}` appends at the exact next byte offset. `data` is
  base64 for at most 24 KiB. Identical already-received chunks are idempotent; missing, conflicting, or overflowing
  chunks are rejected. The reply contains the authoritative `next_offset`.
- `image_finish`: validates completeness and decodes the image on a background executor with explicit dimension and
  allocation limits. It returns `{upload_id, ready: true}` only after validation.
- `image_discard`: removes that phone's upload in that conversation. Uploads also expire after five idle minutes and
  are removed on unpair. The desktop reserves at most 16 MiB across uploads and four uploads per phone.
- `prompt`: adds `images: [upload_id, ...]` to existing arguments. Text may be empty when an image is present. The
  desktop checks the current model again, consumes complete uploads, and sends real ACP Image content through the
  same Queue/Steer/Send Now state machine. It never replaces images with URLs or text descriptions.

An individual prepared image is at most 2 MiB. Android reads at most 20 MiB from the photo picker, bounds bitmap
sampling, applies orientation, resizes to a 1600-pixel long edge, and re-encodes JPEG without the original metadata.
These are image/resource bounds, not conversation truncation. Failed sends retain recoverable in-memory image drafts.
Image-only transcript headers remain visible. Their fingerprints include image content hashes as well as the caption,
so unrelated image messages do not incorrectly acknowledge each other's outbox entries.

### Conversation snapshots and thinking

`thread` answers and the snapshot's `thread` object have `session_id`, `title`, `status`, `total`, and `entries`. Each entry keeps its original thread `index`, `role`, `text`, and optional tool `status`. `total` counts source entries, not message parts.

An assistant entry containing provider-supplied thoughts also has an optional ordered `parts` array. Each part has its source chunk `index`, `role` (`reasoning` or `assistant`), and `text`. Only ACP `Thought` chunks become `reasoning`; ordinary answers and tool output are never used to infer thoughts. Blank parts are omitted. For example:

```json
{
  "index": 12,
  "role": "assistant",
  "text": "The answer",
  "status": null,
  "parts": [
    { "index": 0, "role": "reasoning", "text": "Provider-supplied thinking" },
    { "index": 1, "role": "assistant", "text": "The answer" }
  ]
}
```

Clients that understand `parts` render those instead of the fallback `text`. Android labels reasoning **Thinking** and collapses it until tapped. Older clients can ignore `parts` and continue displaying the answer. A thought-only entry has empty fallback text. Clients reading an older desktop's snapshot use the original entry fields when `parts` is absent.

When watching a plan's owning root session, `thread.step_threads` contains bounded snapshots for its live step sessions, including parallel branches. Each has the same thread fields; its title names the step. This does not change the root session or its entry indices. A pinned child session does not include other branches. Steps leave the published live section when they finish, except that the latest step can remain while generating a branch decision. Android keeps previously seen step sections for the root session, labels absent sessions **Step history**, and clears their cached generating status. Absence can also mean snapshot-budget omission, so the label does not assert successful completion. A client's display identity includes the session, entry index, and part index so streaming updates do not reset expansion or collide with other steps.

The root and live step transcripts share the 36,000-byte budget, including JSON escaping, typed parts, and fallback text. Recent content is retained first; long content and older parts can be omitted. Part indices are not renumbered. The existing answer and encrypted snapshot size limits still apply. With many parallel steps, a step's smaller budget share can leave only its metadata and cursor; if even its metadata does not fit, that step is omitted from the snapshot. Android retains and displays previously loaded history for omitted steps as non-live sections. A metadata-only step remains pageable, using the independent per-request budget. These fields are additive extensions to version 2; no pairing or protocol migration is needed.

`status.windows[].architect.steps` counts every node in the owning root graph recursively, including subplan containers, matching the desktop's deep step count. It does not count only the current canvas level. `step_number` remains the run's execution visit number, including retries and loops; it can exceed `steps`. Android displays **Visit 9 · 5 steps in plan**, not a progress fraction. These counts measure different things: the visit number is not a completed-step count, and the root total includes subplan containers.

### Header pages and lossless body chunks

New clients send `include_details: false` with `watch` and paged `thread` requests. The default remains `true` for
older clients. Compact entries carry labels/previews, statuses, source indices, and `details_pending: true`, not
full message bodies. This applies to user messages, assistant responses, tools, and provider-supplied thinking.
Thinking placeholders may have empty text and must not be discarded. Only actual ACP Thought chunks create them.
If one entry has too many part headers for a page, an aggregate header keeps its entire ordered body addressable;
the body is not shortened to fit the header. User headers include an optional SHA-256/base64 `fingerprint` of their
trimmed text so the phone can reconcile its outbox without downloading that text again.

On expansion, send `thread` with `session_id`, `entry_index`, `chunked: true`, `offset: 0`, optional `window`, and
optional `part_index` for an individual assistant or thinking part. Without a part index, the whole entry is
retrievable, including ordered thinking/response sections in an aggregate entry. Completed Architect steps use
the same ownership-checked loader as history pages.

The result contains `offset`, `next_offset`, `total_bytes`, `version`, `text`, and `done`. Offsets count UTF-8 bytes,
not characters, and must be non-negative integers at character boundaries. Continue with the returned
`next_offset`, `total_bytes`, and opaque `version`. Each chunk's JSON-escaped text fits 32,000 bytes, leaving room
for the response envelope; this is a request-size bound, not a body-length limit. Reassembly preserves the full
stored text, including whitespace, Unicode, and control characters.

The first chunk selects a content snapshot. Appending live output does not invalidate its original prefix;
rewriting or shortening that prefix causes an explicit refresh error rather than mixing revisions. Refresh starts
a new snapshot to include later output. Clients reject duplicate, non-advancing, inconsistent, and out-of-order
chunks. Content already discarded by a provider, tool capture, or desktop scrollback cannot be reconstructed.

Android fetches only expanded bodies, offers **Load more content** and refresh/retry, parses assembled Markdown
on a worker dispatcher, and renders its blocks lazily. Collapsing cancels the fetch. Old-view responses are
ignored. Legacy desktops continue to send their existing bodies; upgrading the desktop is required for complete
chunk retrieval. The old non-chunked detail response remains bounded and explicitly marks truncation for older
clients; the new client does not mistake it for a complete body.

Ordinary snapshots still use a 10-second publication gap plus desktop and phone polling/network time. Content is
read from source text independently of desktop reveal animation. This protocol does not provide token streaming.

### Blocking questions

Thread summaries expose `question_count` and up to eight question headers (`id`, `session_id`, `title`,
`session_title`). They include the root, related conversations, and live Architect steps. `questions` pages the
pending headers with `offset` and `next_offset`; it never answers them.

`question_content` requires `session_id` and `question_id` and uses the body-chunk contract to return a JSON
question form: `question`, `options` (value, label, description), `allow_multiple`, and `auto_answer_paused`.
Explicitly opening the phone answer form marks manual interaction through the existing desktop question state,
pausing its recommendation timer before paging or typing. Closing the form without answering leaves it pending.
Supported forms match the native question tool's answer/freeform schema; unrelated ACP forms and URL flows
remain desktop operations.

`question_answer` accepts the same identity and either `content` or `decline: true`. Content contains `answer`
(a string or a string array), or `freeform_answer` for an explicit custom answer when choices exist. Freeform text
takes precedence. The native question validator runs before the existing elicitation resolver consumes the
pending request. Empty/invalid choices, another session, cancelled waiters, and already-resolved IDs are rejected.
A question response is never a tool permission grant or plan-lock approval.

### Opening folders without replacing sessions

`status.capabilities.open_folder` advertises the host folder browser. `host_folders` takes an absolute native-host
`path` (empty starts at the computer's home) and an optional `offset`. It returns `host`, canonical `path`, `parent`,
`folders` (name/path), and `next_offset`. Pages contain at most 100 folders and respect the response-size budget.
Paths are interpreted by the computer, never by Android or a foreign client's filename rules. Windows UNC paths,
including accessible WSL paths, retain their host semantics.

`open_folder` validates the selected folder and calls the existing workspace opening API in the current desktop
process with `NewWindow`, no workspace matching, and no sidebar reuse. It returns `opened` and the new `window`.
Existing windows, conversations, and phone identity are left intact; it does not launch another executable.
Normal Dev launches also use the app single-instance guard, and a standard-library OS file lock prevents two
participating processes from owning the same remote state. A transient metadata mismatch/refusal does not erase
the phone's local pairing key; explicit unpair or sign-out still can.

### Paging older conversation entries

Send `thread` with an optional, exclusive `before_index` cursor:

```json
{
  "op": "thread",
  "args": { "window": 1, "session_id": "root-session", "before_index": 120 }
}
```

The response uses the same thread shape, plus:

- `before_index`: the requested cursor, or `null` for a live snapshot.
- `next_before`: the next exclusive cursor. Pass this value unchanged to retrieve the next older page.
- `has_more`: whether `next_before` is greater than zero.

`before_index` must be a non-negative integer. Zero returns an empty terminal page. A cursor beyond the current source length is clamped for reading, while the requested value is echoed. Every returned entry has its original index strictly below the requested cursor. `total` remains the whole source-entry count, not the page size.

Each page scans at most 100 source entries and fits the 36,000-byte transcript budget. In header mode, these are previews with separately retrievable bodies as described above. The following truncation rules describe the legacy full-body mode, including metadata, JSON escaping, reasoning parts, and fallback text. Empty source entries advance the cursor even when no rows are returned. Entries are serialized against their own 6,000-byte limit before fitting a page. A boundary entry that cannot fit the remaining page budget is deferred intact, and the cursor does not advance past it. Entries that exceed the per-entry limit have `truncated: true` and `truncation: "entry_limit"`; that accepted per-entry truncation remains in history. Only a live snapshot's newest entry may be clipped further to its smaller shared budget, with `truncation: "snapshot_budget"`. That preview does not consume the entry's cursor, so a history request can retrieve its canonical version, still subject to the 6,000-byte per-entry limit. Paging does not recover content omitted by `entry_limit` truncation. Android labels shortened entries rather than presenting them as complete. A paged response contains only the requested conversation, without `step_threads`. Live snapshots also expose the cursor fields and use the same source-entry cap.

The target may be the coordinator/root conversation, an open child conversation, or a live Architect step session, even if that step has no open desktop chat view. Completed steps recorded in the owning root's native run history can also be resolved through the native session loader. Ownership is checked before and after loading; this does not open arbitrary archived sessions. Once a step is no longer available, requests can fail; already loaded pages stay cached on Android until the device, window, or root session changes.

Android merges entries by session and source index. Live snapshots replace matching entries even when the newer text is shorter. Historical replies fill missing indices and may replace an unchanged snapshot-budget preview. A live version changed since the request began still wins; its preview remains fetchable. Replies for non-live step sections refresh cached entries without restoring a generating indicator. Covered index ranges are retained, so missed live snapshots leave a fetchable gap rather than making intermediate messages unreachable. A source total that decreases invalidates that conversation's cached index space.

Android parses paging cursors, totals, and entry indices as non-negative integer numbers within `0..2147483647`. Fractional values, strings, booleans, and overflow are invalid, not rounded or wrapped. Invalid pages become retryable failures without advancing coverage. Thread-opening and new-thread actions also capture device generation, window identity, and a view revision; late successes and failures cannot mutate another view, including after switching away and back.

The phone permits one history request at a time. Request identities reject stale successes and failures after navigation or reset. Upward scrolling at a conversation's history header fetches at most one page per user gesture, including its fling; **Load older messages** and **Retry loading older messages** are also available. An invalid or non-advancing cursor becomes a retryable error, never an automatic fetch loop. A response whose source length has fallen below the requested boundary is also rejected until a live snapshot resets the index space. Older desktops without paging metadata still display live conversations but require a desktop update for history paging.

### Downloading a file

`download` sends one file of any kind, text or binary, in pieces small enough for one answer each. The phone asks for the pieces in order:

```json
{
  "op": "download",
  "args": { "path": "app/assets/logo.png", "offset": 0, "window": 1 }
}
```

`path` is a project path as for `read_file`, which refuses the same paths: outside the window's projects, inside `.git`, or covered by the project's `private_files` setting. `offset` is how many bytes the phone already has, 0 at first. The answer is:

```json
{
  "size": 81234,
  "offset": 0,
  "version": "81234:1759200000000000000",
  "data": "<base64>"
}
```

- `data` holds at most 32,768 bytes, starting at `offset`.
- The phone asks again at `offset` plus the bytes it received, and stops once it has `size` bytes.
- `version` identifies the file's contents on disk, currently its size and modification time. If it changes between pieces, or a piece does not start where the phone expects, the file changed while downloading: the phone discards what it has and says so.
- Files larger than 5 MB are refused with an error. Each piece is a comment the phone writes and the computer rewrites, and GitHub limits how many comments an account may write in an hour.

A phone runs one download at a time, one piece in flight, so its other requests still get through.

## Housekeeping

The computer deletes comments by other accounts, and its own answers and finished pairings that are more than ten minutes old, in case a phone could not.

## Test vectors

For checking an implementation. The phone's private key is `c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721`; the computer's is `1d9a8f1f4f3fc2a7f3b0e5e2c6e3c7b1a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4`.

| Value              |                                                                                            |
| ------------------ | ------------------------------------------------------------------------------------------ |
| `channel`          | `0123456789abcdef0123456789abcdef`                                                         |
| `phone_id`         | `fedcba9876543210fedcba9876543210`                                                         |
| phone public       | `BGD+1LolWp0xyWHrdMY1bWjASbiSO2H6bOZpYi5g8p+2eQP+EAi4vJmkGunpVii8ZPLxsgwtfp9Rd6PClNRGIpk=` |
| computer public    | `BB79lpmknsBRWuk+JOVoyVb/iHJBHpv3ntw3J4jmMZLPcet9LblSM21sa00e9K3he8jl0LJoH0KUdF0rVly6OYY=` |
| `commit`           | `OUHlaa0veVAxCFLJFkMhNYff3Kr3bKcvpSN+zQPJeLY=`                                             |
| `shared` (hex)     | `e276d9ef83f4744188147d5ad3d2bc93a5bff1dbb1a079599e29b823154e85c7`                         |
| `transcript` (hex) | `cfd62945d28a04d0bd609de1268684b94716abb7e19e9585bd8e4b73cc71748c`                         |
| `key` (hex)        | `da9dd9a4d0c328b1d923cc9b4635d7be4cba432a964aec4ca8a1300efacc6646`                         |
| `code`             | `807021`                                                                                   |

A request blob with nonce `000102030405060708090a0b`, AAD `praxis-remote/v2/request/0123456789abcdef0123456789abcdef/fedcba9876543210fedcba9876543210` and plain text `{"id":"r1","op":"status","args":{},"sent_at":"2026-01-01T00:00:00Z"}`:

```
AAECAwQFBgcICQoLUqeO9tn6GdeJsQhhTZTZJ534qUbzV9GRWtzN83LowPpUOoA3QnYETibYcB6JwcZqAKMwPp0/eA4COaYU6ZzNZ95NG59AAfXNKN8SIyHZ302C2I4Z
```
