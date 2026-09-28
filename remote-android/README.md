# Praxis Remote for Android

A native Android app (Kotlin, Jetpack Compose, Material 3) for driving the Praxis coding agent on your laptop from your
phone. It speaks the protocol defined in `crates/agent_ui/src/remote.rs`: the phone and the laptop exchange messages
through issues in a private GitHub repository, and nothing listens on the network.

- **Live state.** Each laptop keeps an issue titled `Praxis · <device>`. Its body carries a heartbeat
  (`<!-- praxis-device {...} -->`) and a snapshot (`<!-- praxis-state -->`). While the app is in the foreground it
  polls the selected device's issue every 3 seconds with `If-None-Match` (a `304` is free) and sends a `watch`
  request so the laptop publishes snapshots. It renews the watch about every 4 minutes and stops in the background.
- **Requests.** Commands are `<!-- praxis-request -->` comments. Praxis edits each one into a
  `<!-- praxis-response <id> -->` answer. The app polls that comment every 1.5 seconds, then deletes it. Only one
  request is in flight at a time, and a request times out after 45 seconds.

## One-time setup

1. **Create a private repository** for the channel, for example `praxis-remote`. It can be empty.
2. **Create a GitHub App** at <https://github.com/settings/apps/new>:
   - _Name_: "Praxis Remote", or any other name. _Homepage URL_: any URL.
   - Turn on **Enable Device Flow**. A callback URL isn't needed.
   - Turn off **Expire user authorization tokens**. Otherwise you'll have to sign in again when the token expires,
     about every 8 hours.
   - Turn off _Webhook → Active_.
   - _Repository permissions_: **Issues: Read and write**. _Metadata: Read-only_ is added automatically.
   - _Where can this GitHub App be installed?_: **Only on this account**.
   - After creating it, note the **Client ID**, which starts with `Iv`. You don't need a client secret.
3. **Install the app** from its page (**Install App**) on **only** the channel repository.
4. **Turn on Remote in Praxis** on your laptop and point it at the same repository. See `remote.rs`.
5. **Sign in on the phone** with **Sign in with GitHub**, enter the code on github.com, and pick the repository.
   If the app was built without a client ID, the sign-in screen asks for it.

Praxis on the laptop only obeys comments written by the account that owns its token. With the GitHub App, the
comments are posted as your own account, so the laptop must use a token for that same account.

## Building

You need JDK 17+ and the Android SDK (compileSdk 35). There's no Gradle wrapper, so use Gradle 8.10.x:

```sh
gradle -p remote-android assembleDebug    # app/build/outputs/apk/debug/app-debug.apk
gradle -p remote-android assembleRelease  # app/build/outputs/apk/release/app-release.apk (signed)
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

| Path                    | Purpose                                                                                       |
| ----------------------- | --------------------------------------------------------------------------------------------- |
| `data/GitHubClient.kt`  | OkHttp client for `api.github.com`, with an ETag cache and friendly errors                    |
| `data/DeviceFlow.kt`    | GitHub device flow sign-in (client ID only)                                                   |
| `data/RemoteChannel.kt` | Protocol models, snapshot parsing, and the serialized request channel                         |
| `data/Store.kt`         | Preferences; the token lives in `EncryptedSharedPreferences`                                  |
| `data/Updates.kt`       | Release-based update check                                                                    |
| `MainViewModel.kt`      | App state: polling, watch, outbox, and actions                                                |
| `ui/`                   | Compose screens: sign-in, repository picker, devices, device (Chat, Threads, Files), settings |
