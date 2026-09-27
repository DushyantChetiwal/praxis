# Praxis Remote

A small installable Progressive Web App for controlling the Praxis coding agent on your laptop from an Android phone
(or any modern browser). It's plain static files: no framework, no dependencies, and no build step.

It's served from `https://dushyantchetiwal.github.io/praxis/remote/`, and every path is relative, so it works from any
sub-path.

## How it works

The phone and the laptop never connect to each other directly. They exchange messages through **GitHub Issues** in
a private repository you own:

Each laptop running Praxis with Remote turned on keeps one open issue titled `Praxis · <device name>`.

**Live state (read-only, creates nothing).** The issue body holds a `<!-- praxis-device {...} -->` heartbeat that
the laptop refreshes about every 60 seconds. It also holds a `<!-- praxis-state -->` JSON snapshot with the status of
every window, the watched thread's transcript, and pending permission prompts.

- While the app is visible, it polls the issue every 3 seconds with `If-None-Match`. An unchanged issue returns
  `304 Not Modified`, which doesn't count against the rate limit.
- The laptop only publishes snapshots while someone is **watching**. The app sends a `watch` request when you open a
  device, switch windows, or open a thread, and renews it about every 4 minutes. Each watch lasts 5 minutes.
- While watched, the laptop rewrites the snapshot at most every ~10 seconds when something changed, and a few seconds
  after any action. The top bar shows how old the snapshot is. If it doesn't match what you're looking at, the app
  says it's waiting for live updates.

**Commands (request/response).** Sending a prompt, stopping, answering a permission, switching mode, listing
threads, browsing files, and so on work like this:

1. The phone posts a `<!-- praxis-request -->` comment containing a JSON request.
2. The laptop checks for requests every ~3 seconds. It runs the request and **edits that same comment** into a
   `<!-- praxis-response <id> -->` reply.
3. The phone polls that one comment every 1.5 seconds (with ETags), reads the reply, and deletes the comment.

Only one request is in flight at a time. All traffic goes from your browser straight to `api.github.com`. There is
no other server.

## Setup

1. **Create a private repository**, for example `praxis-remote`. It can be empty.
2. **Create a fine-grained personal access token** at
   <https://github.com/settings/personal-access-tokens/new>:
   - _Repository access_: **Only select repositories**, then pick the repository from step 1.
   - _Permissions → Repository permissions_: **Issues: Read and write**. GitHub adds _Metadata: Read-only_
     automatically.
   - Pick an expiration date. You'll need a new token when this one expires.
3. **Turn on Remote in Praxis** on your laptop and point it at the same repository.
4. **Open the app** on your phone at `https://dushyantchetiwal.github.io/praxis/remote/`, enter `owner/repo` and the
   token, and tap **Connect**. In Chrome, use the menu and choose **Add to Home screen** or **Install app** to install
   it.

The repository and token are stored only in this browser's `localStorage`. To remove them, open **Settings → Sign
out / forget token**. You can also revoke the token on GitHub at any time.

## Features

- **Chat**: the thread title and live status, a mode switcher (Build / Plan / Architect…), permission prompts pinned
  to the top with a button for each option, a markdown transcript with collapsible tool calls, a composer with Send and
  Stop, and a New thread button. Messages you send while the agent is busy are queued.
- **Files**: browse the open projects with breadcrumbs and view files read-only (monospace, with line numbers).
- **Threads**: recent threads with relative times. Tap one to open it on the laptop.
- **Architect**: run or stop the current plan.
- Several Praxis windows are supported through the window picker in the top bar.
- Updates arrive automatically while the app is visible. When it's hidden, the app stops polling and stops renewing
  its watch, so the laptop stops publishing snapshots once the watch expires.

## Limits and good practice

- Watching and refreshing create no comments. Only explicit requests (including a watch renewal about every 4
  minutes) create one comment each, which keeps well within GitHub's content-creation limits (about 80 per minute
  and 500 per hour per account). If GitHub rate-limits you anyway, the app shows when you can try again and backs
  off.
- A request times out after 45 seconds with a "not responding" message, and the unanswered request comment is
  deleted. If the comment disappears before it's answered, the app reports that the request was removed.
- Only one view is watched per device, so two phones watching different windows or threads take turns. Each one
  re-sends its watch at most once a minute when it notices the snapshot is for a different view.
- Use a **private** repository. Transcripts and file contents pass through the issue body and comments. Comments are
  deleted right away and the snapshot is overwritten, but GitHub may keep edit history internally, and activity can
  appear in notification emails if you watch the repository. Consider turning off notifications for the
  repository.

## Files

| File                     | Purpose                                                                                      |
| ------------------------ | -------------------------------------------------------------------------------------------- |
| `index.html`             | App shell and static markup                                                                  |
| `app.js`                 | Everything else: GitHub client, issue poller and watch, request queue, rendering (ES module) |
| `styles.css`             | Mobile-first styles, dark by default with a light theme                                      |
| `manifest.webmanifest`   | PWA manifest (relative `start_url` / `scope`)                                                |
| `sw.js`                  | Service worker that caches the app shell only, never API responses                           |
| `icon.svg`, `icon-*.png` | App icons, including a maskable variant                                                      |

## Development

Serve the folder over HTTP. Service workers need `http://localhost` or HTTPS:

```sh
python -m http.server 8080
```

Then open <http://localhost:8080/>. To format the code, run `npx --yes prettier@3.5.0 --write .` in this folder.

When you change a shell file, bump `CACHE` in `sw.js` so installed copies drop the old cache. The service worker
fetches from the network first, so an online app picks up changes on the next load either way.
