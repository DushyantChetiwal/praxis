# Praxis Remote for Android

A native Android app (Kotlin, Jetpack Compose, Material 3) for following and steering the Praxis coding agent on your
computer from your phone. It speaks protocol version 2, specified in
[`docs/src/ai/praxis-remote-protocol.md`](../docs/src/ai/praxis-remote-protocol.md): the phone and the computer
pair through a secret gist in the user's own GitHub account, then prefer end-to-end encrypted Nostr WebSocket messages.
GitHub remains the compatibility fallback. Neither device opens an inbound listening port.

- **Computers.** Praxis creates one secret gist per computer. The app lists the user's gists and shows those with a
  `praxis-remote.json` file, with the computer's name, whether it is online, and whether this phone is paired.
- **Pairing.** The phone and the computer agree a key (P-256 ECDH, HKDF-SHA-256) in one gist comment. Both show a
  six-digit code, and the user approves the phone on the computer if they match. Keys stay in encrypted preferences.
- **Live relay.** Existing paired devices automatically try the community relays at `relay.damus.io` and `relay.primal.net`.
  There is no new account, server, key entry, or pairing step. Changed compact snapshots arrive at most once a second,
  with a heartbeat every 15 seconds. The desktop runs this separately from GitHub requests, so a GitHub quota wait
  does not block an established live connection. Conversation controls show which transport is active.
  Turn off **Settings → Connection → Use live community relays** to use GitHub only on the phone.
- **Fallback state.** The computer publishes one AES-256-GCM encrypted snapshot per paired phone in the gist's
  `state.json`. While the app is in the foreground it reads the gist every 3 seconds with `If-None-Match` (a `304` is
  free) and sends a `watch` request so the computer publishes snapshots. It renews the watch about every 4 minutes and
  stops in the background. Ordinary changing snapshots are published at most once per 10 seconds, plus network and
  polling time. Partial assistant text is included, but this is not a token-streaming connection.
- **Fallback requests.** Commands are encrypted `praxis-remote/v2 request` comments. Praxis edits each one into its encrypted
  answer. The app polls that comment every 1.5 seconds, then deletes it. Only one request is in flight at a time, and a
  request times out after 45 seconds.

## Using it

1. **Turn on Praxis Remote in Praxis** on your computer and sign in to GitHub there.
2. **Sign in on the phone** with **Sign in with GitHub** and the same GitHub account, and enter the code on github.com.
3. **Pick your computer** and pair: check that the code on the phone matches the one Praxis shows, then click Allow on
   the computer.

There is no repository to create and nothing to install on GitHub.

- **Chat-first layout:** the default screen keeps one compact conversation header and the composer. The controls
  icon in the header opens **Conversation controls** for model selection, agent mode, project/window switching,
  queue inspection, and Architect plan details. **Threads** and **Files** are in **More options**; Back returns to Chat.
  Questions, permissions, connection warnings, and the active Stop control remain visible without opening a menu.
- **Models:** open **Conversation controls**, then **Model**, to load the active conversation's models from the computer.
  Search the loaded choices or use **Load more models**. Unavailable models are disabled; selection errors stay visible.
- **Queue controls:** normal sending queues a message while Praxis is working. **Steer** requests the next supported
  turn boundary when that message reaches the front. **Send Now** interrupts the current turn. Tap the queued count
  in **Conversation controls** to inspect and control the desktop queue, including messages not created on this phone.
  Other queued messages remain intact. Live delivery uses WebSockets when connected; fallback uses GitHub polling. An unconfirmed send remains in the
  current window's outbox for review instead of replacing another draft. **Restore draft** only restores to the same
  conversation with an empty composer; it does not resend. Check the conversation before retrying an uncertain send.
- **Images:** select a vision-capable model, then tap **Attach image** in the composer. Preview or remove up to four
  images and send them with or without text. The app rotates images according to their orientation, resizes the long
  edge to at most 1600 pixels, and re-encodes JPEG without source metadata. Prepared images are at most 2 MiB each.
  Upload chunks are encrypted; no public image-hosting service or project-file write is used. The same images travel
  through normal sending, Queue, Steer, and Send Now. Unconfirmed image messages can restore their draft without
  automatically resending. Image drafts are scoped to the selected conversation and retained in memory, not across app termination.
- **Conversation headers:** messages, tools, and Thinking start collapsed. Expanding fetches that body's first chunk;
  **Load more content** retrieves the rest without permanent remote truncation. Parsing runs off the UI thread and
  body blocks render lazily. Loaded bodies belong to the ViewModel rather than recycled chat rows, so scrolling away
  and back reuses them. A bounded memory cache spills into an encrypted app-private disk cache; cache pressure or
  explicit refresh can require another read. The loaded text is a snapshot; **Refresh details** includes newer output.
  Closed bodies do not generate requests. Content already absent from desktop storage cannot be recovered.
- **Questions:** pending questions appear above the conversation, including questions from running steps. **Answer on
  phone** opens the complete question and pauses its automatic recommendation while you answer. Choose options,
  select multiple where supported, or type a custom answer. **Submit answer** resumes the waiting question; **Skip
  question** declines it. Closing the form alone does not answer it. Already-resolved questions cannot be answered twice.
- **Folders:** use **Open folder on computer** from the device menu, browse or enter an absolute host path, and open
  it in a new window. Other windows and agent sessions remain open. Windows users can enter accessible WSL UNC paths.
  Select windows in **Conversation controls**. The phone preserves its pairing key if a transient or competing desktop
  snapshot fails to confirm it.

Request polling has a bounded deadline, including a stalled HTTP read. Temporary network failures are retried as
reads of the same request, not by reposting a message. Network cleanup runs separately so a timed-out or cancelled
request cannot hold the message queue while deleting its GitHub comment. Persistent GitHub connection errors are
reported separately from a desktop that has not answered; neither case erases pairing keys. The desktop uses a
short, capped recovery backoff for transient failures and observes GitHub rate-limit delays. Passive snapshot
updates slow to 30 or 60 seconds when GitHub quota is low, while explicit requests and their replies retain priority.
GitHub quota is shared with other activity, so exhausting it can still delay fallback and new pairing until reset.
Remembered paired computers remain selectable when GitHub discovery fails; an authenticated live handshake confirms
whether the desktop still accepts that pairing.

Live requests carry a fresh desktop-incarnation ID and receive an authenticated desktop receipt. Retransmissions
reuse the same request ID and payload; the desktop returns a retained receipt rather than executing again. If a live
command may have been submitted, the app never automatically reposts it through GitHub. A restart, lost receipt, or
uncertain timeout asks you to check the conversation before explicitly resending. A relay acknowledgement alone is
not evidence that the desktop executed anything.

Community relays are operated by third parties. They can observe IP addresses, derived public identities, message
sizes, and timing, but not the encrypted conversation or image contents. They may impose restrictions or become
unavailable; there is no unlimited-capacity, permanent-free-service, or uptime guarantee. Ephemeral events are not
conversation storage, and neither encryption nor requesting ephemeral delivery guarantees deletion or forward
secrecy. The desktop remains the source of history. Connections stop when the phone app goes into the background;
Android background restrictions are not bypassed.

Update both the desktop and Android app for these controls. Older desktops continue to show their existing transcript
format and do not expose the new controls. Pairing keys and encryption are unchanged. If an older Android version
already erased a pairing key, pairing again is still necessary. Restart normally after installing the updated desktop;
do not leave an older Dev process publishing to the same remote channel alongside it.

## The GitHub App

The app signs in with GitHub's device flow for a GitHub App, which needs only its client ID. To use your own app,
create one at <https://github.com/settings/apps/new>:

- _Name_: "Praxis Remote", or any other name. _Homepage URL_: any URL.
- Turn on **Enable Device Flow**. A callback URL isn't needed. Turn off _Webhook → Active_.
- _Account permissions_: **Gists: Read and write**. No repository permissions are needed, and the app doesn't need
  to be installed anywhere: account permissions apply to user access tokens directly.
- Note the **Client ID**, which starts with `Iv`. You don't need a client secret. User tokens expire after eight hours
  and the app refreshes them on its own.

Build with `PRAXIS_REMOTE_CLIENT_ID` set, or enter the client ID under **Advanced** on the sign-in screen.

## Building

You need JDK 17+ and the Android SDK (compileSdk 35). There's no Gradle wrapper, so use Gradle 8.9 or newer (8.x):

```sh
gradle -p remote-android testDebugUnitTest  # protocol crypto against the spec's test vectors
gradle -p remote-android assembleDebug      # app/build/outputs/apk/debug/app-debug.apk
gradle -p remote-android assembleRelease    # app/build/outputs/apk/release/app-release.apk (signed)
                                            # or app-release-unsigned.apk without signing variables
```

All of these environment variables are optional:

| Variable                                                                                          | Purpose                                  |
| ------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `PRAXIS_REMOTE_VERSION_CODE`                                                                      | `versionCode` (default `1`)              |
| `PRAXIS_REMOTE_VERSION_NAME`                                                                      | `versionName` (default `0.1.0-dev`)      |
| `PRAXIS_REMOTE_CLIENT_ID`                                                                         | GitHub App client ID built into the app  |
| `ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` | Release signing (absolute keystore path) |

**Update check.** The app looks for releases of `DushyantChetiwal/praxis` tagged
`praxis-remote-android-<versionCode>` with a newer `versionCode` than its own. It checks at most every 6 hours, and
the release's `.apk` asset is offered as the download.

## Layout

| Path                    | Purpose                                                                                    |
| ----------------------- | ------------------------------------------------------------------------------------------ |
| `data/GitHubClient.kt`  | OkHttp client for `api.github.com`, with an ETag cache, token refresh and friendly errors  |
| `data/DeviceFlow.kt`    | GitHub device flow sign-in and token refresh (client ID only)                              |
| `data/Tokens.kt`        | Hands out the access token, refreshing it (one refresh at a time)                          |
| `data/RemoteCrypto.kt`  | Protocol cryptography: P-256, HKDF, AES-GCM blobs (plain JVM, unit tested)                 |
| `data/Pairing.kt`       | The phone's side of pairing                                                                |
| `data/RemoteChannel.kt` | Protocol models, gist discovery, snapshot decryption, and the serialized request channel   |
| `data/NostrChannel.kt` | Live relay lifecycle, paired identities, receipts, and ordered snapshots |
| `data/Images.kt` | Bounded image preparation, previews, fingerprints, and chunk upload |
| `data/Store.kt`         | Preferences; tokens and pairing keys live in `EncryptedSharedPreferences`                  |
| `data/Updates.kt`       | Release-based update check                                                                 |
| `MainViewModel.kt`      | App state: computers, pairing, polling, watch, outbox, and actions                         |
| `ui/`                   | Compose screens: sign-in, computers, pairing, computer (Chat, Threads, Files), settings    |
