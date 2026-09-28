# Praxis Remote

Praxis Remote lets you follow and steer the agent on your computer from your phone. You can send it messages, answer its permission prompts, switch between Plan, Build and Architect, start or stop a plan, open earlier conversations and read your project's files. The phone never edits files itself: ask the agent, and the agent on your computer makes the change.

Your phone and your computer never connect directly, and nothing on your computer listens on the network. Both talk to GitHub, through an issue in a private repository that you own.

## Set up

1. **Create a private repository** for the channel, for example `praxis-remote`. It can stay empty.

   ```sh
   gh repo create praxis-remote --private
   ```

2. **Turn it on in Praxis.** Create `remote/config.json` in Praxis's data folder:

   - Windows: `%LOCALAPPDATA%\Praxis Dev\remote\config.json`
   - macOS: `~/Library/Application Support/Praxis Dev/remote/config.json`
   - Linux: `~/.local/share/praxis dev/remote/config.json`

   ```json
   { "repository": "your-name/praxis-remote" }
   ```

   Praxis signs in to GitHub with the GitHub CLI (`gh auth login`) if it is installed. Otherwise, add `"token": "…"` with a [fine-grained personal access token](https://github.com/settings/personal-access-tokens/new) that has **Issues: Read and write** on only that repository, or set `PRAXIS_REMOTE_TOKEN`. It must belong to the same GitHub account you sign in with on your phone. `"device_name"` changes the name your phone shows, which is the computer's name by default. Restart Praxis. Praxis opens an issue named `Praxis · <device>` in the repository.

3. **Install the Praxis Remote GitHub App** on only the channel repository. The app lets your phone sign in with GitHub and limits it to that repository's issues.

4. **Install Praxis Remote on your Android phone.** Download the newest `PraxisRemote-….apk` from the [releases](https://github.com/DushyantChetiwal/praxis/releases) tagged `praxis-remote-android-…` and open it; Android asks you to allow installing apps from your browser the first time. Tap **Sign in with GitHub**, enter the code it shows on github.com, then pick your computer. The app tells you when a newer version is available.

## What you can do

- **Chat:** read the active conversation as it happens, send messages (queued if the agent is busy), stop a turn, and start a new thread.
- **Approvals:** every tool call waiting for permission appears as a card with the same choices Praxis offers, including requests from a plan step's own thread.
- **Modes:** switch the conversation between Plan, Build and Architect.
- **Architect:** see the plan's progress, and start or stop a run.
- **Threads:** open an earlier conversation from the same project.
- **Files:** browse and read the project's files. Reading only; there is no editing.

If several Praxis windows are open, pick one at the top of the app.

## How it works

Praxis keeps one open issue per computer:

- The **issue body** holds a heartbeat and a snapshot of what Praxis is doing: its windows, the active conversation and anything waiting for approval. Praxis refreshes it every minute, and every few seconds while your phone is watching. The phone polls it with `If-None-Match`, and GitHub does not count unchanged responses against rate limits.
- **Requests** are comments the phone adds to the issue. Praxis checks for them every few seconds, carries them out, and answers by editing the comment. The phone reads the answer and deletes the comment, so the issue stays clean.

Expect a delay of a few seconds between tapping and seeing the result.

## Security

- Praxis only obeys comments written by the GitHub account its own token belongs to. Anyone else's comments are ignored.
- Requests more than five minutes old are refused rather than replayed, so nothing queued while your computer was off runs later by surprise.
- Anyone who can post comments as your account in that repository can direct the agent, and in Build mode the agent can change files and run commands. Keep the repository private, install the GitHub App on that repository alone, and if a phone is lost, revoke its access under **Settings → Applications → Authorized GitHub Apps** on GitHub.
- The issue shows your recent conversation and project file names to anyone who can read the repository.
- The phone keeps its sign-in in Android's encrypted storage. **Sign out** in the app's settings removes it.

To turn Praxis Remote off, delete `remote/config.json` and restart Praxis. You can also close or delete the issue; Praxis reopens it the next time it runs with the configuration in place.
