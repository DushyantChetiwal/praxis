# Praxis Remote for Android

A native Android app (Kotlin, Jetpack Compose, Material 3) for following and steering the Praxis coding agent on your
computer from your phone. It speaks protocol version 2, specified in
[`docs/src/ai/praxis-remote-protocol.md`](../docs/src/ai/praxis-remote-protocol.md): the phone and the computer
exchange end-to-end encrypted messages through a secret gist in the user's own GitHub account, and nothing listens
on the network.

- **Computers.** Praxis creates one secret gist per computer. The app lists the user's gists and shows those with a
  `praxis-remote.json` file, with the computer's name, whether it is online, and whether this phone is paired.
- **Pairing.** The phone and the computer agree a key (P-256 ECDH, HKDF-SHA-256) in one gist comment. Both show a
  six-digit code, and the user approves the phone on the computer if they match. Keys stay in encrypted preferences.
- **Live state.** The computer publishes one AES-256-GCM encrypted snapshot per paired phone in the gist's
  `state.json`. While the app is in the foreground it reads the gist every 3 seconds with `If-None-Match` (a `304` is
  free) and sends a `watch` request so the computer publishes snapshots. It renews the watch about every 4 minutes and
  stops in the background. Ordinary changing snapshots are published at most once per 10 seconds, plus network and
  polling time. Partial assistant text is included, but this is not a token-streaming connection.
- **Requests.** Commands are encrypted `praxis-remote/v2 request` comments. Praxis edits each one into its encrypted
  answer. The app polls that comment every 1.5 seconds, then deletes it. Only one request is in flight at a time, and a
  request times out after 45 seconds.

## Using it

1. **Turn on Praxis Remote in Praxis** on your computer and sign in to GitHub there.
2. **Sign in on the phone** with **Sign in with GitHub** and the same GitHub account, and enter the code on github.com.
3. **Pick your computer** and pair: check that the code on the phone matches the one Praxis shows, then click Allow on
   the computer.

There is no repository to create and nothing to install on GitHub.

- **Models:** tap **Model** in Chat to load the active conversation's models from the computer. Search the loaded
  choices or use **Load more models**. Unavailable models are disabled; selection errors stay visible in the picker.
- **Send Now:** normal sending queues a message while Praxis is working. **Send Now** interrupts the current turn,
  matching the desktop action. It is available for a new message or a phone message already acknowledged as queued.
  Other queued messages remain in the desktop queue. This still uses GitHub polling, so delivery is not instantaneous.
- **Details:** tool summaries and Thinking arrows arrive without their bodies. Expanding one fetches only that tool
  or thinking block; collapsing it removes the detail UI. Reopening or **Refresh details** fetches its current contents.
  Closed details do not generate requests. Large details are explicitly marked as shortened.

Update both the desktop and Android app for these controls. Older desktops continue to show their existing transcript
format and do not expose model selection or Send Now. Pairing keys and encryption are unchanged.

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
| `data/Store.kt`         | Preferences; tokens and pairing keys live in `EncryptedSharedPreferences`                  |
| `data/Updates.kt`       | Release-based update check                                                                 |
| `MainViewModel.kt`      | App state: computers, pairing, polling, watch, outbox, and actions                         |
| `ui/`                   | Compose screens: sign-in, computers, pairing, computer (Chat, Threads, Files), settings    |
