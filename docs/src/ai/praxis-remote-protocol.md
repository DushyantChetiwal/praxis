# Praxis Remote protocol, version 2

This is the wire format between Praxis on a computer (`crates/agent_ui/src/remote.rs`) and the Android app (`remote-android/`). Both must follow it exactly. [Praxis Remote](./praxis-remote.md) describes the feature itself.

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

| Blob | AAD |
|---|---|
| Snapshot in `state.json` | `praxis-remote/v2/state/<channel>/<phone_id>` |
| Request | `praxis-remote/v2/request/<channel>/<phone_id>` |
| Answer | `praxis-remote/v2/response/<channel>/<phone_id>/<request_id>` |

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

The computer refuses a request when the comment or its `sent_at` is more than five minutes old, when `sent_at` is more than two minutes in the future, or when it has already seen that request id from that phone. Operations are the same as in version 1 (`status`, `threads`, `thread`, `prompt`, `stop`, `new_thread`, `open_thread`, `permission`, `mode`, `architect`, `list_dir`, `read_file`, `watch`, `batch`), plus `unpair`, which removes the phone that sends it.

Plain-text sizes are capped so that every comment fits GitHub's 65,536-character limit: 46,000 bytes for an answer. Larger answers become an error.

## Housekeeping

The computer deletes comments by other accounts, and its own answers and finished pairings that are more than ten minutes old, in case a phone could not.

## Test vectors

For checking an implementation. The phone's private key is `c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721`; the computer's is `1d9a8f1f4f3fc2a7f3b0e5e2c6e3c7b1a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4`.

| Value | |
|---|---|
| `channel` | `0123456789abcdef0123456789abcdef` |
| `phone_id` | `fedcba9876543210fedcba9876543210` |
| phone public | `BGD+1LolWp0xyWHrdMY1bWjASbiSO2H6bOZpYi5g8p+2eQP+EAi4vJmkGunpVii8ZPLxsgwtfp9Rd6PClNRGIpk=` |
| computer public | `BB79lpmknsBRWuk+JOVoyVb/iHJBHpv3ntw3J4jmMZLPcet9LblSM21sa00e9K3he8jl0LJoH0KUdF0rVly6OYY=` |
| `commit` | `OUHlaa0veVAxCFLJFkMhNYff3Kr3bKcvpSN+zQPJeLY=` |
| `shared` (hex) | `e276d9ef83f4744188147d5ad3d2bc93a5bff1dbb1a079599e29b823154e85c7` |
| `transcript` (hex) | `cfd62945d28a04d0bd609de1268684b94716abb7e19e9585bd8e4b73cc71748c` |
| `key` (hex) | `da9dd9a4d0c328b1d923cc9b4635d7be4cba432a964aec4ca8a1300efacc6646` |
| `code` | `807021` |

A request blob with nonce `000102030405060708090a0b`, AAD `praxis-remote/v2/request/0123456789abcdef0123456789abcdef/fedcba9876543210fedcba9876543210` and plain text `{"id":"r1","op":"status","args":{},"sent_at":"2026-01-01T00:00:00Z"}`:

```
AAECAwQFBgcICQoLUqeO9tn6GdeJsQhhTZTZJ534qUbzV9GRWtzN83LowPpUOoA3QnYETibYcB6JwcZqAKMwPp0/eA4COaYU6ZzNZ95NG59AAfXNKN8SIyHZ302C2I4Z
```
