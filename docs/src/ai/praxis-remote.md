# Praxis Remote

Praxis Remote lets you follow and steer the agent on your computer from your phone. You can send it messages, answer its permission prompts, switch between Plan, Build and Architect, start or stop a plan, open earlier conversations and read your project's files. The phone never edits files itself: ask the agent, and the agent on your computer makes the change.

Your phone and your computer never connect directly, and nothing on your computer listens on the network. Both talk to GitHub, through an issue in a private repository that you own.

## Set up

1. **Create a private repository** for the channel, for example `praxis-remote`. It can stay empty.

   ```sh
   gh repo create praxis-remote --private
   ```

2. **Create a token for your phone.** Create a [fine-grained personal access token](https://github.com/settings/personal-access-tokens/new) with access to **only** that repository and the **Issues: Read and write** permission. Nothing else is needed.

3. **Turn it on in Praxis.** Create `remote/config.json` in Praxis's data folder:

   - Windows: `%LOCALAPPDATA%\Praxis Dev\remote\config.json`
   - macOS: `~/Library/Application Support/Praxis Dev/remote/config.json`
   - Linux: `~/.local/share/praxis dev/remote/config.json`

   ```json
   { "repository": "your-name/praxis-remote" }
   ```

   Praxis signs in to GitHub with the GitHub CLI (`gh auth login`) if it is installed. Otherwise, add `"token": "…"` with a token like the phone's, or set `PRAXIS_REMOTE_TOKEN`. `"device_name"` changes the name your phone shows, which is the computer's name by default. Restart Praxis. Praxis opens an issue named `Praxis · <device>` in the repository.

4. **Install the app on your phone.** Open [dushyantchetiwal.github.io/praxis/remote](https://dushyantchetiwal.github.io/praxis/remote/) in Chrome on Android, then choose **Add to Home screen** (or **Install app**). Enter the repository and the phone's token, then pick your computer.

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
- Anyone who can post comments as your account in that repository can direct the agent, and in Build mode the agent can change files and run commands. Keep the repository private, give tokens access to that repository alone with only the Issues permission, and revoke a token from GitHub's settings if a phone is lost.
- The issue shows your recent conversation and project file names to anyone who can read the repository.
- The phone keeps its token in the browser's local storage for that site. Use **Forget token** in the app's settings to remove it.

To turn Praxis Remote off, delete `remote/config.json` and restart Praxis. You can also close or delete the issue; Praxis reopens it the next time it runs with the configuration in place.
