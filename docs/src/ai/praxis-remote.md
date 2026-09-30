# Praxis Remote

Praxis Remote lets you follow and steer the agent on your computer from your Android phone. You can send it messages, answer its permission prompts, switch between Plan, Build and Architect, start or stop a plan, open earlier conversations, and read or download your project's files. The phone never edits files itself: ask the agent, and the agent on your computer makes the change.

Your phone and your computer never connect directly, and nothing on your computer listens on the network. Both talk to GitHub, through a secret gist in your own GitHub account, and everything they exchange there is end-to-end encrypted.

## Set up

There is no repository, configuration file or token to create. You need a GitHub account, Praxis on your computer and the app on your phone.

1. **Turn it on in Praxis.** Open the command palette and run **agent: open praxis remote**, or choose **Praxis Remote…** in the Agent panel's menu. Click **Sign in with GitHub**, then **Copy Code and Open GitHub**, paste the code on github.com and authorize Praxis Remote. It only asks for access to your gists. Praxis then shows **Signed in as @you on <computer>**.

2. **Install Praxis Remote on your Android phone.** Download the newest `PraxisRemote-….apk` from the [releases](https://github.com/DushyantChetiwal/praxis/releases) tagged `praxis-remote-android-…` and open it; Android asks you to allow installing apps from your browser the first time. The app tells you when a newer version is available.

3. **Sign in on the phone** with **the same GitHub account**, the same way: tap **Sign in with GitHub** and enter the code it shows on github.com.

4. **Pair the phone.** Tap your computer in the list, then **Start pairing**. Praxis brings a window to the front and asks whether to allow the phone, showing a six-digit code. Click **Allow** only if it is the code your phone shows. If the codes differ, click **Deny**.

Praxis must be running on the computer for the phone to pair or to reach it. The phone shows whether each computer is online.

## What you can do

- **Chat:** read the active conversation as it happens, send messages (queued if the agent is busy), stop a turn, and start a new thread.
- **Approvals:** every tool call waiting for permission appears as a card with the same choices Praxis offers, including requests from a plan step's own thread.
- **Modes:** switch the conversation between Plan, Build and Architect.
- **Architect:** see the plan's progress, and start or stop a run.
- **Threads:** open an earlier conversation from the same project.
- **Files:** browse and read the project's files, and download any of them, one at a time, with the download button next to a file or in the file viewer. On Android 10 and later downloads go to **Downloads/Praxis**; on older versions the phone asks where to save. Files up to 5 MB can be downloaded, and a large one takes a few minutes because it travels through GitHub in 32 KB pieces. Files covered by the project's `private_files` setting are never sent. There is no editing.

If several Praxis windows are open, pick one at the top of the app.

## How it works

When you sign in, Praxis creates one secret gist in your account for this computer:

- `praxis-remote.json` names the computer and says when Praxis last checked in, so your phone can list it and tell whether it is online. It also lists the ids of the paired phones.
- `state.json` holds a snapshot of what Praxis is doing (its windows, the active conversation and anything waiting for approval), encrypted separately for each paired phone. Praxis refreshes it every minute, and every few seconds while your phone is watching.
- **Requests** are encrypted comments the phone adds to the gist. Praxis checks for them every few seconds, carries them out, and answers by editing the comment into an encrypted answer. The phone reads the answer and deletes the comment. Pairing happens through a comment in the same way.

Expect a delay of a few seconds between tapping and seeing the result. The exact format is in the [protocol specification](./praxis-remote-protocol.md).

## Security

- **End-to-end encryption.** Each phone agrees its own key with the computer when it pairs (P-256 key agreement, then AES-256-GCM). Snapshots, requests and answers are encrypted with that key and bound to the computer and the phone they belong to, so they cannot be read, changed or replayed elsewhere.
- **What GitHub can see.** The gist is secret, so it is not listed publicly, but GitHub and anyone who learns its link can read it. All they can learn is encrypted data, the computer's name, the phones' random ids, and when Praxis and the phone were active. Your prompts, conversations, file names and code never leave your devices unencrypted.
- **Approval on the computer.** A phone can only be added by someone at the computer, who clicks **Allow** after checking the code. The phone commits to its key before it learns the computer's, so even someone who can write comments as you cannot make the codes match except by a one-in-a-million chance.
- **Your account only.** Praxis only acts on comments written by the account it is signed in to, and deletes anyone else's. Requests more than five minutes old are refused rather than replayed, so nothing queued while your computer was off runs later by surprise.
- **Secrets stay in the keychain.** Praxis keeps its GitHub sign-in and every phone's key in the system keychain (Keychain on macOS, Credential Manager on Windows, the Secret Service on Linux), never in a file. The phone keeps its sign-in and keys in Android's encrypted storage.
- **Unpairing.** Open **Praxis Remote…** and click **Unpair** next to a phone to remove it at once; it can no longer read anything or send requests. A phone can also unpair itself from its settings.
- **Turning it off.** **Turn Off** stops Praxis Remote, deletes the gist and forgets the sign-in and every phone. To revoke Praxis Remote's access to your gists completely, remove it under **Settings → Applications → Authorized GitHub Apps** on GitHub.

In Build mode the agent can change files and run commands, so treat a paired phone like a key to your computer: unpair it if you lose it.

## Upgrading from the first version

The first version of Praxis Remote used an issue in a private repository, a `remote/config.json` file in Praxis's data folder and a GitHub App installed on that repository. None of these is used any more:

- Praxis ignores `remote/config.json` (it only logs that it is no longer used), so you can delete it.
- The repository, its `Praxis · <device>` issue and the GitHub App installation can be deleted or uninstalled.
- Update the phone app, sign in on the computer as described above, and pair the phone again.
