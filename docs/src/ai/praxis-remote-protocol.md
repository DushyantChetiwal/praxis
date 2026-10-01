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

When watching a plan's owning root session, `thread.step_threads` contains bounded snapshots for its live step sessions, including parallel branches. Each has the same thread fields; its title names the step. This does not change the root session or its entry indices. A pinned child session does not include other branches. Steps disappear from this live section when they finish, except that the latest step remains visible while generating a branch decision. A client's display identity includes the session, entry index, and part index so streaming updates do not reset expansion or collide with other steps.

The root and live step transcripts share the 36,000-byte budget, including JSON escaping, typed parts, and fallback text. Recent content is retained first; long content and older parts can be omitted. Part indices are not renumbered. The existing answer and encrypted snapshot size limits still apply. With many parallel steps, a step's smaller budget share can leave only its metadata and cursor; if even its metadata does not fit, that step is omitted from the snapshot. Android retains previously loaded history for omitted steps but only shows step sessions present in the latest snapshot. A metadata-only step remains pageable, using the independent per-request budget. These fields are additive extensions to version 2; no pairing or protocol migration is needed.

`status.windows[].architect.steps` counts every node in the owning root graph recursively, including subplan containers, matching the desktop's deep step count. It does not count only the current canvas level. `step_number` remains the run's execution visit number, including retries and loops; it can exceed `steps`. Android displays **Visit 9 · 5 steps in plan**, not a progress fraction. These counts measure different things: the visit number is not a completed-step count, and the root total includes subplan containers.

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

Each page scans at most 100 source entries and fits the 36,000-byte transcript budget, including metadata, JSON escaping, reasoning parts, and fallback text. Empty source entries advance the cursor even when no rows are returned. Long individual entries retain the existing truncation limit; paging retrieves older entries, not omitted portions of one entry. A paged response contains only the requested conversation, without `step_threads`. Live snapshots also expose the cursor fields and use the same source-entry cap.

The target may be the coordinator/root conversation, an open child conversation, or a live Architect step session, even if that step has no open desktop chat view. This does not open arbitrary archived sessions. Once a step is no longer available, requests can fail; already loaded pages stay cached on Android until the device, window, or root session changes.

Android merges entries by session and source index. Live snapshots replace matching entries even when the newer text is shorter. Historical replies only fill missing indices and cannot overwrite newer live content. Covered index ranges are retained, so missed live snapshots leave a fetchable gap rather than making intermediate messages unreachable. A source total that decreases invalidates that conversation's cached index space.

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
