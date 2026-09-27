// Praxis Remote: drive the Praxis agent on your laptop through GitHub Issues.
//
// Transport: each laptop owns an open issue titled "Praxis · <device>".
// - Live state: while the phone "watches", the laptop publishes a snapshot (status + the
//   watched thread) in the issue body. The phone polls the issue with ETags; 304s are free
//   and nothing is created, so refreshing never touches GitHub's content-creation limits.
// - Commands: the phone posts a request comment, the laptop edits that same comment into
//   the response, and the phone reads it and deletes the comment.
// All traffic goes to api.github.com with a fine-grained token scoped to one private repo.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const API_BASE = "https://api.github.com";
const DEVICE_TITLE_PREFIX = "Praxis \u00b7 ";
const REQUEST_MARKER = "<!-- praxis-request -->";
const RESPONSE_MARKER_RE = /^\s*<!--\s*praxis-response\s+(\S+?)\s*-->/;
const DEVICE_META_RE = /<!--\s*praxis-device\s+([\s\S]*?)-->/;
const STATE_MARKER_RE = /<!--\s*praxis-state\s*-->/;
const REPO_RE = /^([A-Za-z0-9-]+)\/([A-Za-z0-9._-]+)$/;

// Request comments are polled until the laptop edits them into a response.
const REQUEST_POLL_MS = 1500;
const REQUEST_TIMEOUT_MS = 45_000;

const ISSUE_POLL_MS = 3000;
const MAX_POLL_BACKOFF_MS = 30_000;
const WATCH_SECONDS = 300;
const WATCH_RENEW_MS = 4 * 60_000;
const WATCH_RETRY_MS = 30_000;
// If a newly published snapshot still ignores our watch this long after we sent it
// (laptop restarted, or another viewer took over the watch), send the watch again.
const WATCH_SETTLE_MS = 60_000;
// Optimistic UI (sent messages, mode changes) gives up waiting for a snapshot after this.
const OUTBOX_FALLBACK_MS = 90_000;
const MODE_OVERRIDE_MS = 20_000;
const DEVICE_REFRESH_MS = 60_000;
const ONLINE_THRESHOLD_MS = 3 * 60_000;
const TICK_MS = 1000;
const SLOW_TICK_MS = 15_000;

const STORAGE = {
  config: "praxisRemote.config",
  device: "praxisRemote.device",
  windowPrefix: "praxisRemote.window.",
};

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function loadJson(key) {
  try {
    const raw = localStorage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

function saveJson(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage may be unavailable (private mode); the app still works for this session.
  }
}

function removeKey(key) {
  try {
    localStorage.removeItem(key);
  } catch {
    // Ignore.
  }
}

const HTML_ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => HTML_ESCAPES[c]);
}

function parseDate(value) {
  if (typeof value !== "string" || !value) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

function formatClock(date) {
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

const RELATIVE_UNITS = [
  ["minute", 60],
  ["hour", 3600],
  ["day", 86_400],
  ["week", 604_800],
  ["month", 2_592_000],
  ["year", 31_536_000],
];

function relativeTime(date) {
  const seconds = Math.round((Date.now() - date.getTime()) / 1000);
  if (Math.abs(seconds) < 45) return "just now";
  let [unit, size] = RELATIVE_UNITS[0];
  for (const [u, s] of RELATIVE_UNITS) {
    if (Math.abs(seconds) >= s) [unit, size] = [u, s];
  }
  return new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }).format(-Math.round(seconds / size), unit);
}

function ageText(date) {
  const seconds = Math.max(0, Math.round((Date.now() - date.getTime()) / 1000));
  return seconds < 60 ? `${seconds}s ago` : relativeTime(date);
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function plural(count, word, pluralWord = `${word}s`) {
  return `${count} ${count === 1 ? word : pluralWord}`;
}

function compact(obj) {
  return Object.fromEntries(Object.entries(obj).filter(([, v]) => v !== undefined && v !== null));
}

function newRequestId() {
  const bytes = new Uint8Array(8);
  crypto.getRandomValues(bytes);
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `m${Date.now().toString(36)}-${hex}`;
}

function firstLine(text) {
  const line = String(text ?? "")
    .split("\n")
    .find((l) => l.trim());
  return (line ?? "").trim().replace(/^#{1,6}\s+/, "");
}

function normalizeRepo(input) {
  const cleaned = String(input ?? "")
    .trim()
    .replace(/^https?:\/\/github\.com\//i, "")
    .replace(/\.git$/i, "")
    .replace(/\/+$/, "");
  return REPO_RE.test(cleaned) ? cleaned : null;
}

/** Tiny DOM builder. Strings become text nodes, so untrusted text is always safe here. */
function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else el.setAttribute(key, value === true ? "" : String(value));
  }
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    el.append(child instanceof Node ? child : String(child));
  }
  return el;
}

// ---------------------------------------------------------------------------
// Minimal, safe markdown
// ---------------------------------------------------------------------------
// Every piece of text is HTML-escaped before it is wrapped in markup. Only http(s)
// links are produced, and they always open in a new tab with rel="noopener".

const INLINE_RE =
  /`([^`\n]+)`|\[([^\]\n]+)\]\(([^()\s]+)\)|\*\*([^*\n]+)\*\*|(https?:\/\/[^\s<>"'`]+[^\s<>"'`.,:;!?)\]}])/g;

function safeUrl(raw) {
  try {
    const url = new URL(raw);
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch {
    return null;
  }
}

function linkHtml(href, labelHtml) {
  return `<a href="${escapeHtml(href)}" target="_blank" rel="noopener noreferrer">${labelHtml}</a>`;
}

function renderInline(text) {
  let html = "";
  let last = 0;
  for (const m of text.matchAll(INLINE_RE)) {
    html += escapeHtml(text.slice(last, m.index));
    last = m.index + m[0].length;
    if (m[1] !== undefined) {
      html += `<code>${escapeHtml(m[1])}</code>`;
    } else if (m[2] !== undefined) {
      const href = safeUrl(m[3]);
      html += href ? linkHtml(href, escapeHtml(m[2])) : escapeHtml(m[0]);
    } else if (m[4] !== undefined) {
      html += `<strong>${renderInline(m[4])}</strong>`;
    } else if (m[5] !== undefined) {
      const href = safeUrl(m[5]);
      html += href ? linkHtml(href, escapeHtml(m[5])) : escapeHtml(m[5]);
    }
  }
  return html + escapeHtml(text.slice(last));
}

function renderMarkdown(source) {
  const lines = String(source ?? "")
    .replace(/\r\n?/g, "\n")
    .split("\n");
  const out = [];
  let paragraph = [];
  let quote = [];
  let listTag = null;

  const flushParagraph = () => {
    if (paragraph.length) out.push(`<p>${paragraph.map(renderInline).join("<br>")}</p>`);
    paragraph = [];
  };
  const flushQuote = () => {
    if (quote.length) out.push(`<blockquote>${quote.map(renderInline).join("<br>")}</blockquote>`);
    quote = [];
  };
  const closeList = () => {
    if (listTag) out.push(`</${listTag}>`);
    listTag = null;
  };
  const flushAll = () => {
    flushParagraph();
    flushQuote();
    closeList();
  };
  const openList = (tag, start) => {
    if (listTag === tag) return;
    closeList();
    out.push(tag === "ol" && start && start !== "1" ? `<ol start="${Number(start)}">` : `<${tag}>`);
    listTag = tag;
  };
  const listItem = (indent, content) => {
    const level = Math.min(3, Math.floor(indent.length / 2));
    out.push(`<li${level ? ` class="md-i${level}"` : ""}>${renderInline(content)}</li>`);
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    const fence = /^\s*(`{3,}|~{3,})(.*)$/.exec(line);
    if (fence) {
      flushAll();
      const closer = new RegExp(`^\\s*${fence[1][0] === "`" ? "`" : "~"}{${fence[1].length},}\\s*$`);
      const lang = fence[2].trim().split(/\s+/)[0];
      const body = [];
      for (i++; i < lines.length && !closer.test(lines[i]); i++) body.push(lines[i]);
      const langAttr = lang ? ` data-lang="${escapeHtml(lang)}"` : "";
      out.push(`<pre${langAttr}><code>${escapeHtml(body.join("\n"))}</code></pre>`);
      continue;
    }

    if (!line.trim()) {
      flushAll();
      continue;
    }

    const heading = /^\s{0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$/.exec(line);
    if (heading) {
      flushAll();
      const tag = `h${Math.min(6, heading[1].length + 2)}`;
      out.push(`<${tag}>${renderInline(heading[2])}</${tag}>`);
      continue;
    }

    if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flushAll();
      out.push("<hr>");
      continue;
    }

    const quoteLine = /^\s{0,3}>\s?(.*)$/.exec(line);
    if (quoteLine) {
      flushParagraph();
      closeList();
      quote.push(quoteLine[1]);
      continue;
    }
    flushQuote();

    const bullet = /^(\s*)[-*+]\s+(.*)$/.exec(line);
    if (bullet) {
      flushParagraph();
      openList("ul");
      listItem(bullet[1], bullet[2]);
      continue;
    }

    const ordered = /^(\s*)(\d{1,9})[.)]\s+(.*)$/.exec(line);
    if (ordered) {
      flushParagraph();
      openList("ol", ordered[2]);
      listItem(ordered[1], ordered[3]);
      continue;
    }

    if (listTag && /^\s+\S/.test(line)) {
      // Indented continuation of the previous list item.
      out[out.length - 1] = out[out.length - 1].replace(/<\/li>$/, `<br>${renderInline(line.trim())}</li>`);
      continue;
    }

    closeList();
    paragraph.push(line);
  }
  flushAll();
  return out.join("");
}

/** The single place where generated HTML is inserted; its input is escaped by renderMarkdown. */
function markdownBlock(text, className = "") {
  const el = h("div", { class: `md ${className}`.trim() });
  el.innerHTML = renderMarkdown(text);
  return el;
}

function inlineMarkdown(tag, className, text) {
  const el = h(tag, { class: className });
  el.innerHTML = renderInline(String(text ?? ""));
  return el;
}

// ---------------------------------------------------------------------------
// GitHub client
// ---------------------------------------------------------------------------

class ApiError extends Error {
  constructor(message, { kind = "http", status = 0, resetAt = null } = {}) {
    super(message);
    this.name = "ApiError";
    // kind: network | auth | rate_limit | forbidden | not_found | http | timeout | praxis | state | cancelled
    this.kind = kind;
    this.status = status;
    this.resetAt = resetAt;
  }
}

/** path -> { etag, data } for conditional GETs. */
const etagCache = new Map();

async function gh(path, { method = "GET", body, token = state.config?.token, conditional = false, allow = [] } = {}) {
  if (!token) throw new ApiError("Not signed in.", { kind: "auth" });
  const headers = {
    Authorization: `Bearer ${token}`,
    Accept: "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  const cached = conditional ? etagCache.get(path) : undefined;
  if (cached) headers["If-None-Match"] = cached.etag;
  if (body !== undefined) headers["Content-Type"] = "application/json";

  let res;
  try {
    res = await fetch(API_BASE + path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
  } catch {
    throw new ApiError("Can't reach GitHub. Check your connection.", { kind: "network" });
  }

  if (res.status === 304) return { status: 304, changed: false, data: cached?.data ?? null };
  if (allow.includes(res.status)) return { status: res.status, changed: true, data: null };
  if (!res.ok) throw await toApiError(res);

  const data = res.status === 204 ? null : await res.json().catch(() => null);
  if (conditional) {
    const etag = res.headers.get("ETag");
    if (etag) etagCache.set(path, { etag, data });
    else etagCache.delete(path);
  }
  return { status: res.status, changed: true, data };
}

async function toApiError(res) {
  let message = "";
  try {
    message = (await res.json())?.message ?? "";
  } catch {
    // Non-JSON error body.
  }
  const detail = message ? `: ${message}` : "";
  const remaining = res.headers.get("x-ratelimit-remaining");
  const reset = Number(res.headers.get("x-ratelimit-reset"));
  const retryAfter = Number(res.headers.get("retry-after"));
  const resetAt = retryAfter > 0 ? new Date(Date.now() + retryAfter * 1000) : reset > 0 ? new Date(reset * 1000) : null;

  if (res.status === 401) {
    return new ApiError("GitHub rejected the token (401). It may be expired or revoked.", {
      kind: "auth",
      status: 401,
    });
  }
  if (res.status === 429 || (res.status === 403 && (remaining === "0" || /rate limit/i.test(message)))) {
    const when = resetAt ? ` Try again after ${formatClock(resetAt)}.` : "";
    return new ApiError(`GitHub rate limit reached.${when}`, { kind: "rate_limit", status: res.status, resetAt });
  }
  if (res.status === 403) {
    return new ApiError(
      `GitHub denied access (403)${detail}. Check the token's repository access and Issues permission.`,
      {
        kind: "forbidden",
        status: 403,
      },
    );
  }
  if (res.status === 404) {
    return new ApiError("Not found (404). Check the repository name and that the token can access it.", {
      kind: "not_found",
      status: 404,
    });
  }
  return new ApiError(`GitHub error ${res.status}${detail}`, { kind: "http", status: res.status });
}

function repoPath(suffix, repo = state.config?.repo) {
  const [owner, name] = String(repo).split("/");
  return `/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}${suffix}`;
}

const github = {
  repo: (repo, token) => gh(repoPath("", repo), { token }),
  user: (token) => gh("/user", { token }),
  probeIssues: (repo, token) => gh(repoPath("/issues?state=open&per_page=1", repo), { token }),
  deviceIssues: () =>
    gh(repoPath("/issues?state=open&per_page=100&sort=updated&direction=desc"), { conditional: true }),
  issue: (number) => gh(repoPath(`/issues/${number}`), { conditional: true }),
  commentPath: (id) => repoPath(`/issues/comments/${id}`),
  createComment: (issue, body) => gh(repoPath(`/issues/${issue}/comments`), { method: "POST", body: { body } }),
  deleteComment: (id) => gh(repoPath(`/issues/comments/${id}`), { method: "DELETE", allow: [404] }),
};

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

function parseDeviceMeta(body) {
  const match = DEVICE_META_RE.exec(body ?? "");
  if (!match) return null;
  try {
    const meta = JSON.parse(match[1].trim());
    return meta && typeof meta === "object" ? meta : null;
  } catch {
    return null;
  }
}

function parseDevice(issue) {
  if (!issue || issue.pull_request || typeof issue.title !== "string") return null;
  if (!issue.title.startsWith(DEVICE_TITLE_PREFIX)) return null;
  const meta = parseDeviceMeta(issue.body);
  const name = String(meta?.device || issue.title.slice(DEVICE_TITLE_PREFIX.length)).trim() || "Unnamed device";
  return {
    number: issue.number,
    name,
    startedAt: parseDate(meta?.started_at),
    lastSeen: parseDate(meta?.last_seen),
  };
}

function isOnline(device) {
  if (!device) return false;
  const now = Date.now();
  if (device.lastSeen && now - device.lastSeen.getTime() < ONLINE_THRESHOLD_MS) return true;
  // A successful round trip is proof of life even if the issue body hasn't been re-read yet.
  return device.number === state.device?.number && now - state.lastContact < ONLINE_THRESHOLD_MS;
}

function presenceText(device) {
  if (isOnline(device)) return "Online";
  return device.lastSeen ? `Last seen ${relativeTime(device.lastSeen)}` : "Not seen yet";
}

// ---------------------------------------------------------------------------
// Request channel (one request in flight at a time)
// ---------------------------------------------------------------------------

const channel = { queue: [], busy: false };

function enqueue(task) {
  return new Promise((resolve, reject) => {
    channel.queue.push({ ...task, resolve, reject });
    pumpQueue();
  });
}

async function pumpQueue() {
  if (channel.busy) return;
  channel.busy = true;
  renderActivity();
  while (channel.queue.length) {
    const task = channel.queue.shift();
    try {
      task.resolve(await exchange(task));
    } catch (err) {
      task.reject(err);
      if (err.kind === "auth") rejectQueued(err);
    }
  }
  channel.busy = false;
  renderActivity();
}

function rejectQueued(err) {
  for (const task of channel.queue.splice(0)) task.reject(err);
}

function requestBody(payload) {
  return `${REQUEST_MARKER}\n\`\`\`json\n${JSON.stringify(payload)}\n\`\`\``;
}

function parseEnvelope(body) {
  const newline = body.indexOf("\n");
  const rest = newline === -1 ? "" : body.slice(newline + 1);
  const start = rest.indexOf("{");
  const end = rest.lastIndexOf("}");
  if (start === -1 || end <= start) throw new Error("Malformed response");
  return JSON.parse(rest.slice(start, end + 1));
}

function deleteQuietly(commentId) {
  if (!commentId) return Promise.resolve();
  return github.deleteComment(commentId).catch(() => {});
}

/** Reads the request comment; returns its body once the laptop has answered, else null. */
async function pollRequestComment(path) {
  try {
    const res = await gh(path, { conditional: true });
    const body = res.changed ? String(res.data?.body ?? "") : "";
    return RESPONSE_MARKER_RE.test(body) ? body : null;
  } catch (err) {
    if (err.kind === "not_found") {
      throw new ApiError("The request was removed before Praxis answered.", { kind: "praxis", status: 404 });
    }
    if (err.kind === "network" || (err.kind === "http" && err.status >= 500)) return null;
    throw err;
  }
}

function readEnvelope(body, id) {
  try {
    const envelope = parseEnvelope(body);
    const markerId = RESPONSE_MARKER_RE.exec(body)?.[1];
    if (markerId === id && (envelope?.id === undefined || envelope.id === id)) return envelope;
    return { id, ok: false, error: "Praxis answered a different request." };
  } catch {
    return { id, ok: false, error: "Praxis sent a response that couldn't be read." };
  }
}

/** Posts one request comment and waits for the laptop to edit it into the response. */
async function exchange({ device, op, args }) {
  const id = newRequestId();
  const payload = { id, op, args, sent_at: new Date().toISOString() };
  const { data: comment } = await github.createComment(device.number, requestBody(payload));
  if (!comment?.id) throw new ApiError("GitHub didn't return the new comment.", { kind: "http" });

  const path = github.commentPath(comment.id);
  const deadline = Date.now() + REQUEST_TIMEOUT_MS;
  let finished = false;
  try {
    while (Date.now() < deadline) {
      await sleep(REQUEST_POLL_MS);
      let body;
      try {
        body = await pollRequestComment(path);
      } catch (err) {
        finished = err.status === 404; // Nothing left to delete.
        throw err;
      }
      if (body) {
        finished = true;
        if (device.number === state.device?.number) state.lastContact = Date.now();
        deleteQuietly(comment.id);
        return readEnvelope(body, id);
      }
    }
  } finally {
    etagCache.delete(path);
    if (!finished) await deleteQuietly(comment.id);
  }
  throw new ApiError(`Praxis on ${device.name} is not responding.`, { kind: "timeout" });
}

/** Calls an op on the selected device and unwraps the { ok, result | error } envelope. */
async function praxis(op, args = {}) {
  const device = state.device;
  if (!device) throw new ApiError("No device selected.", { kind: "state" });
  const envelope = await enqueue({ device, op, args });
  if (envelope?.ok) return envelope.result;
  throw new ApiError(envelope?.error || `Praxis couldn't run "${op}".`, { kind: "praxis" });
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const emptyThreads = () => ({ loading: false, items: null, error: null, opening: null });
const emptyFiles = () => ({ loading: false, path: "", entries: null, error: null, file: null });

const state = {
  config: null, // { repo, token, login }
  devices: [],
  devicesLoaded: false,
  device: null,
  issueApplied: false, // whether the selected device's issue has been read at least once
  snapshot: null, // last parsed praxis-state block
  snapshotAt: null, // its updated_at
  status: null,
  windowId: null,
  watchSession: null, // thread pinned via open_thread; null follows the window's active thread
  thread: null,
  threadKnown: false, // whether `thread` reflects a snapshot for the current view
  threadError: null,
  modeOverride: null, // optimistic mode until a snapshot confirms it
  tab: "chat",
  threads: emptyThreads(),
  files: emptyFiles(),
  outbox: [],
  answered: new Set(),
  openTools: new Set(),
  lastContact: 0,
  stickToBottom: true,
  transcriptSig: "",
  transcriptSession: undefined,
  permissionsSig: "",
  windowsSig: "",
  banners: new Map(),
};

const poller = { timer: 0, running: false, again: false, failures: 0 };
const watcher = { seq: 0, window: undefined, session: undefined, inFlight: false, sentAt: 0, renewAt: 0 };
const timers = { devices: 0, tick: 0, slowTick: 0 };
let outboxSeq = 0;
let dom = {};

function currentWindow() {
  return state.status?.windows?.find((w) => w.window === state.windowId) ?? null;
}

function isGenerating() {
  return state.thread?.status === "generating" || currentWindow()?.thread?.status === "generating";
}

function pendingPermissions() {
  const pending = currentWindow()?.thread?.pending;
  return Array.isArray(pending) ? pending : [];
}

const permissionKey = (p) => `${p.session_id}:${p.tool_call_id}`;
const viewKey = () => `${state.device?.number}:${state.windowId}`;

function windowArgs(extra = {}) {
  return compact({ window: state.windowId, ...extra });
}

function currentModeId() {
  const override = state.modeOverride;
  if (override && override.window === state.windowId && override.expires > Date.now()) return override.id;
  return currentWindow()?.thread?.mode?.current ?? null;
}

// ---------------------------------------------------------------------------
// Live state: issue polling
// ---------------------------------------------------------------------------

function parseSnapshot(body) {
  const marker = STATE_MARKER_RE.exec(body ?? "");
  if (!marker) return null;
  const rest = body.slice(marker.index + marker[0].length);
  const start = rest.indexOf("{");
  const end = rest.lastIndexOf("}");
  if (start === -1 || end <= start) return null;
  try {
    const snapshot = JSON.parse(rest.slice(start, end + 1));
    return snapshot && typeof snapshot === "object" ? snapshot : null;
  } catch {
    return null;
  }
}

/** Whether a snapshot's watch describes what the user is looking at (window + thread). */
function watchMatchesView(watch) {
  if (!watch || typeof watch !== "object") return false;
  const windowOk =
    watch.window == null
      ? state.windowId == null || state.windowId === state.status?.windows?.[0]?.window
      : watch.window === state.windowId;
  // When following the active thread, accept whichever session the laptop reports.
  const sessionOk = !state.watchSession || watch.session_id === state.watchSession;
  return windowOk && sessionOk;
}

function snapshotFresh() {
  const watch = state.snapshot?.watch;
  const until = parseDate(watch?.until);
  return !!until && until.getTime() > Date.now() && watchMatchesView(watch);
}

function schedulePoll(delay = ISSUE_POLL_MS) {
  clearTimeout(poller.timer);
  poller.timer = 0;
  if (delay == null || !state.device || document.hidden) return;
  poller.timer = setTimeout(pollIssue, delay);
}

/** Polls the device issue (free when unchanged) and applies what changed. Never overlaps. */
async function pollIssue() {
  if (!state.device || !state.config || document.hidden) return;
  if (poller.running) {
    poller.again = true;
    return;
  }
  clearTimeout(poller.timer);
  poller.running = true;
  const device = state.device;
  let delay = ISSUE_POLL_MS;
  try {
    const res = await github.issue(device.number);
    // A 304 still carries the cached issue, which matters right after switching devices.
    if (device.number === state.device?.number && res.data && (res.changed || !state.issueApplied)) {
      applyIssue(res.data);
    }
    poller.failures = 0;
    setBanner("poll", null);
  } catch (err) {
    delay = handlePollError(err);
  } finally {
    poller.running = false;
  }
  ensureWatch();
  if (poller.again) {
    poller.again = false;
    return pollIssue();
  }
  schedulePoll(delay);
}

function applyIssue(issue) {
  state.issueApplied = true;
  const device = parseDevice(issue);
  if (device && device.number === state.device?.number) {
    state.device = device;
    state.devices = state.devices.map((d) => (d.number === device.number ? device : d));
  }
  const closed = issue.state === "closed";
  setBanner("closed", closed ? `Praxis closed issue #${issue.number}. Looking for the device again…` : null);
  if (closed) loadDevices();

  const snapshot = parseSnapshot(issue.body);
  if (snapshot) applySnapshot(snapshot);
  renderAll();
  // The window may have just been resolved (or changed), which resets tab data.
  loadActiveTabData();
}

function applySnapshot(snapshot) {
  const previousUpdatedAt = state.snapshot?.updated_at;
  state.snapshot = snapshot;
  state.snapshotAt = parseDate(snapshot.updated_at);
  if (snapshot.status && typeof snapshot.status === "object") applyStatus(snapshot.status, snapshot.watch?.window);
  if (watchMatchesView(snapshot.watch)) {
    state.thread = snapshot.thread && typeof snapshot.thread === "object" ? snapshot.thread : null;
    state.threadError = snapshot.thread_error ?? null;
    state.threadKnown = true;
  }
  pruneOutbox();
  if (state.modeOverride && currentWindow()?.thread?.mode?.current === state.modeOverride.id) {
    state.modeOverride = null;
  }
  if (previousUpdatedAt !== snapshot.updated_at) recheckStaleWatch();
}

function applyStatus(status, preferredWindow = null) {
  const windows = Array.isArray(status?.windows) ? status.windows : [];
  state.status = { ...(status ?? {}), windows };
  if (!windows.some((w) => w.window === state.windowId)) {
    const next = windows.find((w) => w.window === preferredWindow) ?? windows.find((w) => w.active) ?? windows[0];
    setWindow(next ? next.window : null);
  }
  const pendingKeys = new Set(pendingPermissions().map(permissionKey));
  for (const key of state.answered) if (!pendingKeys.has(key)) state.answered.delete(key);
}

/** Drops optimistic messages once they show up in the transcript (or after a fallback delay). */
function pruneOutbox() {
  const thread = state.thread;
  const entries = Array.isArray(thread?.entries) ? thread.entries : [];
  const queued = Number(currentWindow()?.thread?.queued) || 0;
  const now = Date.now();
  state.outbox = state.outbox.filter((item) => {
    if (!item.doneAt) return true;
    const base = item.session === (thread?.session_id ?? null) ? item.baseIndex : -1;
    const delivered = entries.some(
      (e) => e.role === "user" && Number(e.index) > base && String(e.text ?? "").trim() === item.text,
    );
    if (delivered) return false;
    const expired = now - item.doneAt > OUTBOX_FALLBACK_MS;
    return !(expired && (item.state !== "queued" || queued === 0));
  });
}

function handlePollError(err) {
  const failures = ++poller.failures;
  if (err.kind === "auth") {
    setBanner("poll", `${err.message} Open Settings to sign in again.`, "error");
    return null;
  }
  if (err.kind === "rate_limit") {
    setBanner("poll", err.message, "error");
    return err.resetAt ? Math.max(5000, err.resetAt.getTime() - Date.now() + 1000) : MAX_POLL_BACKOFF_MS;
  }
  if (err.kind === "not_found" || err.status === 410) {
    setBanner("poll", `The issue for this device (#${state.device?.number}) is gone. Looking for it again…`);
    loadDevices();
    return MAX_POLL_BACKOFF_MS;
  }
  if (failures >= 2) setBanner("poll", `Couldn't check for updates: ${err.message}`, "error");
  return Math.min(ISSUE_POLL_MS * 2 ** (failures - 1), MAX_POLL_BACKOFF_MS);
}

// ---------------------------------------------------------------------------
// Live state: watch
// ---------------------------------------------------------------------------

function resetWatcher() {
  Object.assign(watcher, {
    seq: watcher.seq + 1,
    window: undefined,
    session: undefined,
    inFlight: false,
    sentAt: 0,
    renewAt: 0,
  });
}

function watchNeeded() {
  if (!state.device || !state.config || document.hidden || watcher.inFlight) return false;
  if (watcher.window !== state.windowId || watcher.session !== state.watchSession) return true;
  return Date.now() >= watcher.renewAt;
}

/** Asks the laptop to publish snapshots for the current view; renews about every 4 minutes. */
async function ensureWatch() {
  if (!watchNeeded()) return;
  const seq = ++watcher.seq;
  const target = { window: state.windowId, session: state.watchSession };
  Object.assign(watcher, target, { inFlight: true });
  try {
    await praxis("watch", compact({ window: target.window, session_id: target.session, seconds: WATCH_SECONDS }));
    if (seq !== watcher.seq) return;
    watcher.sentAt = Date.now();
    watcher.renewAt = watcher.sentAt + WATCH_RENEW_MS;
    setBanner("watch", null);
  } catch (err) {
    if (seq !== watcher.seq) return;
    watcher.renewAt = Date.now() + WATCH_RETRY_MS;
    if (err.kind === "praxis" && target.window != null && !currentWindow()) {
      // The remembered window no longer exists; let the laptop pick its default instead.
      state.windowId = null;
    }
    if (err.kind !== "cancelled" && err.kind !== "timeout") {
      setBanner("watch", `Live updates unavailable: ${err.message}`, "error");
    }
  } finally {
    if (seq === watcher.seq) watcher.inFlight = false;
  }
  renderBanners();
  if (watchNeeded()) ensureWatch();
}

function recheckStaleWatch() {
  if (snapshotFresh() || watcher.inFlight || !watcher.sentAt) return;
  if (Date.now() - watcher.sentAt >= WATCH_SETTLE_MS) watcher.renewAt = 0;
}

// ---------------------------------------------------------------------------
// Devices & windows
// ---------------------------------------------------------------------------

async function loadDevices({ initial = false } = {}) {
  if (!state.config) return;
  try {
    const { data } = await github.deviceIssues();
    state.devices = (Array.isArray(data) ? data : []).map(parseDevice).filter(Boolean);
    setBanner("devices", null);
    reconcileDevice(initial);
  } catch (err) {
    const suffix = err.kind === "auth" ? " Open Settings to sign in again." : "";
    setBanner("devices", `Couldn't load devices: ${err.message}${suffix}`, "error");
  }
  state.devicesLoaded = true;
  renderTopbar();
  renderBanners();
  renderChat();
  if (dom.deviceSheet.open) renderDeviceList();
}

function reconcileDevice(initial) {
  const current = state.device;
  const wanted = current ?? loadJson(STORAGE.device);
  const match =
    (wanted && state.devices.find((d) => d.number === wanted.number)) ??
    (wanted && state.devices.find((d) => d.name === wanted.name)) ??
    null;

  if (match) {
    if (current && match.number === current.number) state.device = match;
    else selectDevice(match);
    return;
  }
  if (current) return; // Keep the selection; the presence banner explains staleness.
  if (state.devices.length === 1) selectDevice(state.devices[0]);
  else if (state.devices.length > 1 && initial) openDeviceSheet();
}

function selectDevice(device) {
  const changed = device.number !== state.device?.number;
  state.device = device;
  saveJson(STORAGE.device, { number: device.number, name: device.name });
  if (!changed) return;
  rejectQueued(new ApiError("Switched device.", { kind: "cancelled" }));
  resetDeviceView();
  state.windowId = loadJson(STORAGE.windowPrefix + device.name);
  renderAll();
  pollIssue(); // Sends the watch once the first snapshot has resolved the window.
}

function resetDeviceView() {
  clearTimeout(poller.timer);
  poller.failures = 0;
  resetWatcher();
  state.issueApplied = false;
  state.snapshot = null;
  state.snapshotAt = null;
  state.status = null;
  state.windowId = null;
  state.watchSession = null;
  state.lastContact = 0;
  for (const key of ["poll", "watch", "closed"]) setBanner(key, null);
  resetWindowView();
}

function resetWindowView() {
  state.thread = null;
  state.threadKnown = false;
  state.threadError = null;
  state.modeOverride = null;
  state.threads = emptyThreads();
  state.files = emptyFiles();
  state.outbox = [];
  state.answered.clear();
  state.openTools.clear();
  state.transcriptSig = "";
  state.permissionsSig = "";
  state.stickToBottom = true;
}

function setWindow(id) {
  if (id === state.windowId) return;
  state.windowId = id;
  state.watchSession = null;
  if (state.device && id != null) saveJson(STORAGE.windowPrefix + state.device.name, id);
  resetWindowView();
}

function windowLabel(w) {
  const projects = Array.isArray(w.projects) ? w.projects.filter(Boolean) : [];
  return projects.length ? projects.join(", ") : `Window ${w.window}`;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

function reportError(err, label) {
  if (err.kind === "cancelled") return;
  const message = err.kind === "praxis" && label ? `${label}: ${err.message}` : err.message;
  toast(message, "error", 5000);
  if (err.kind === "auth") setBanner("poll", `${err.message} Open Settings to sign in again.`, "error");
}

/** Runs an op, reports failures, and re-polls the issue right away on success. */
async function act(op, args, failureLabel) {
  try {
    const result = await praxis(op, args);
    pollIssue();
    return { ok: true, result };
  } catch (err) {
    reportError(err, failureLabel);
    return { ok: false, error: err };
  }
}

async function sendPrompt() {
  const text = dom.prompt.value.trim();
  if (!text || !state.device) return;
  const entries = Array.isArray(state.thread?.entries) ? state.thread.entries : [];
  const item = {
    id: ++outboxSeq,
    text,
    state: "sending",
    doneAt: 0,
    session: state.thread?.session_id ?? null,
    baseIndex: Math.max(-1, ...entries.map((e) => Number(e.index) || 0)),
  };
  state.outbox.push(item);
  dom.prompt.value = "";
  autoGrow();
  state.stickToBottom = true;
  renderTranscript();

  try {
    const result = await praxis("prompt", windowArgs({ text }));
    item.state = result?.queued ? "queued" : "sent";
    item.doneAt = Date.now();
    if (result?.queued) toast("Queued — Praxis will pick it up after the current turn.");
    renderTranscript();
    pollIssue();
  } catch (err) {
    state.outbox = state.outbox.filter((o) => o !== item);
    if (!dom.prompt.value.trim()) {
      dom.prompt.value = text;
      autoGrow();
    }
    renderTranscript();
    reportError(err, "Message not sent");
  }
}

async function stopGenerating() {
  dom.stopButton.disabled = true;
  const res = await act("stop", windowArgs(), "Couldn't stop");
  dom.stopButton.disabled = false;
  if (res.ok) toast("Stopping…");
}

async function startNewThread() {
  dom.newThread.disabled = true;
  const res = await act("new_thread", windowArgs(), "Couldn't start a new thread");
  dom.newThread.disabled = false;
  if (!res.ok) return;
  state.watchSession = null; // Follow the window's (new) active thread.
  state.thread = null;
  state.threadKnown = false;
  state.openTools.clear();
  state.threads = emptyThreads();
  state.transcriptSig = "";
  renderChat();
  ensureWatch();
  toast("New thread started", "success");
}

async function setMode(modeId) {
  if (!currentWindow()?.thread?.mode || currentModeId() === modeId) return;
  const override = { window: state.windowId, id: modeId, expires: Date.now() + MODE_OVERRIDE_MS };
  state.modeOverride = override;
  renderMode();
  const res = await act("mode", windowArgs({ mode: modeId }), "Couldn't change mode");
  if (!res.ok && state.modeOverride === override) {
    state.modeOverride = null;
    renderMode();
  }
}

async function answerPermission(permission, option, card) {
  setCardBusy(card, true);
  const res = await act(
    "permission",
    windowArgs({ session_id: permission.session_id, tool_call_id: permission.tool_call_id, option_id: option.id }),
    "Couldn't answer",
  );
  if (!res.ok) {
    setCardBusy(card, false);
    return;
  }
  state.answered.add(permissionKey(permission));
  renderPermissions();
}

async function architectAction(op) {
  const res = await act("architect", windowArgs({ op }), op === "run" ? "Couldn't run the plan" : "Couldn't stop");
  if (res.ok) toast(op === "run" ? "Running plan…" : "Stopping plan…");
}

async function loadThreads() {
  if (!state.device) return;
  const key = viewKey();
  state.threads = { ...state.threads, loading: true, error: null };
  renderThreads();
  try {
    const result = await praxis("threads", windowArgs());
    if (key !== viewKey()) return;
    const items = Array.isArray(result?.threads) ? [...result.threads] : [];
    items.sort((a, b) => (parseDate(b.updated_at)?.getTime() ?? 0) - (parseDate(a.updated_at)?.getTime() ?? 0));
    state.threads = { ...emptyThreads(), items };
  } catch (err) {
    if (key !== viewKey()) return;
    state.threads = { ...state.threads, loading: false, error: err.message };
    if (err.kind === "auth") reportError(err);
  }
  renderThreads();
}

async function openThread(thread) {
  if (thread.active) {
    switchTab("chat");
    return;
  }
  state.threads = { ...state.threads, opening: thread.session_id };
  renderThreads();
  const res = await act("open_thread", windowArgs({ session_id: thread.session_id }), "Couldn't open thread");
  state.threads = { ...state.threads, opening: null };
  if (res.ok) {
    for (const t of state.threads.items ?? []) t.active = t.session_id === thread.session_id;
    state.watchSession = thread.session_id;
    state.thread = null;
    state.threadKnown = false;
    state.openTools.clear();
    state.transcriptSig = "";
    ensureWatch();
    switchTab("chat");
  }
  renderThreads();
}

async function listDir(path) {
  if (!state.device) return;
  const key = viewKey();
  state.files = { ...state.files, loading: true, error: null, file: null };
  renderFiles();
  try {
    const result = await praxis("list_dir", windowArgs({ path }));
    if (key !== viewKey()) return;
    const entries = Array.isArray(result?.entries) ? [...result.entries] : [];
    entries.sort((a, b) => Number(!!b.dir) - Number(!!a.dir) || String(a.name).localeCompare(String(b.name)));
    state.files = { ...emptyFiles(), path: typeof result?.path === "string" ? result.path : path, entries };
  } catch (err) {
    if (key !== viewKey()) return;
    state.files = { ...state.files, loading: false, error: err.message };
    if (err.kind === "auth") reportError(err);
  }
  renderFiles();
}

async function openFile(entry) {
  const key = viewKey();
  state.files = { ...state.files, loading: true, error: null };
  renderFiles();
  try {
    const result = await praxis("read_file", windowArgs({ path: entry.path }));
    if (key !== viewKey()) return;
    state.files = { ...state.files, loading: false, file: { ...result, path: result?.path ?? entry.path } };
  } catch (err) {
    if (key !== viewKey()) return;
    state.files = { ...state.files, loading: false, error: err.message };
    if (err.kind === "auth") reportError(err);
  }
  renderFiles();
}

/** Loads data for the visible tab if it's missing (or always, when forced). */
function loadActiveTabData({ force = false } = {}) {
  if (!state.device) return;
  const { threads, files } = state;
  if (state.tab === "threads" && !threads.loading && (force || (!threads.items && !threads.error))) loadThreads();
  if (state.tab === "files" && !files.loading && !files.file) {
    if (force) listDir(files.path);
    else if (!files.entries && !files.error) listDir("");
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function renderAll() {
  renderTopbar();
  renderBanners();
  renderChat();
  renderThreads();
  renderFiles();
  renderActivity();
}

function renderActivity() {
  dom.refreshButton?.classList.toggle("spinning", channel.busy);
}

function renderTopbar() {
  const device = state.device;
  dom.deviceName.textContent = device ? device.name : state.devicesLoaded ? "No device" : "Loading…";
  dom.deviceDot.className = `dot ${device ? (isOnline(device) ? "online" : "offline") : ""}`;
  const parts = [];
  if (device) parts.push(presenceText(device));
  else if (state.devicesLoaded) parts.push(state.devices.length ? "Tap to choose a device" : "No devices found");
  if (device && state.snapshotAt) parts.push(`Updated ${ageText(state.snapshotAt)}`);
  dom.deviceSub.textContent = parts.join(" · ");
  renderWindowSelect();
}

function renderWindowSelect() {
  const windows = state.status?.windows ?? [];
  dom.windowSelect.hidden = windows.length <= 1;
  const sig = JSON.stringify([windows.map((w) => [w.window, windowLabel(w)]), state.windowId]);
  if (sig === state.windowsSig) return;
  state.windowsSig = sig;
  dom.windowSelect.replaceChildren(
    ...windows.map((w) =>
      h("option", { value: String(w.window), selected: w.window === state.windowId }, windowLabel(w)),
    ),
  );
}

function setBanner(key, text, tone = "warning") {
  if (text) state.banners.set(key, { text, tone });
  else state.banners.delete(key);
  if (dom.banners) renderBanners();
}

function renderBanners() {
  const items = [];
  const device = state.device;
  if (device && !isOnline(device)) {
    const seen = device.lastSeen ? `was last seen ${relativeTime(device.lastSeen)}` : "hasn't checked in yet";
    items.push({ text: `Praxis on ${device.name} ${seen}. Requests may time out.`, tone: "warning" });
  } else if (device && state.issueApplied && !snapshotFresh()) {
    items.push({ text: "Waiting for live updates from Praxis… Showing the last published state.", tone: "info" });
  }
  items.push(...state.banners.values());
  dom.banners.replaceChildren(
    ...items.map((b) => h("div", { class: `banner ${b.tone}`, role: b.tone === "error" ? "alert" : "status" }, b.text)),
  );
}

function renderChat() {
  const win = currentWindow();
  const info = win?.thread ?? null;
  let title;
  if (!state.device) title = "No device selected";
  else if (!state.status) title = "Connecting…";
  else if (!win) title = "No Praxis window";
  else title = state.thread?.title || info?.title || (info ? "Untitled thread" : "No active thread");
  dom.threadTitle.textContent = title;

  renderStatusPill(info);
  renderMode();
  renderArchitect(win);
  renderPermissions();
  renderTranscript();

  const generating = isGenerating();
  dom.stopButton.hidden = !generating;
  dom.newThread.hidden = !win;
  dom.prompt.disabled = !state.device;
  dom.sendButton.disabled = !state.device;
}

function renderStatusPill(info) {
  const status = state.thread?.status ?? info?.status;
  if (!status) {
    dom.threadStatus.hidden = true;
    return;
  }
  const generating = status === "generating";
  const queued = Number(info?.queued) || 0;
  dom.threadStatus.hidden = false;
  dom.threadStatus.className = `pill${generating ? " working" : ""}`;
  dom.threadStatus.textContent = (generating ? "Working…" : "Idle") + (queued ? ` · ${queued} queued` : "");
}

function renderMode() {
  const mode = currentWindow()?.thread?.mode;
  const available = Array.isArray(mode?.available) ? mode.available : [];
  const current = currentModeId();
  dom.modeControl.hidden = available.length === 0;
  dom.modeControl.replaceChildren(
    ...available.map((m) =>
      h(
        "button",
        { type: "button", class: "seg", "aria-pressed": String(m.id === current), onclick: () => setMode(m.id) },
        m.name || m.id,
      ),
    ),
  );
}

function renderArchitect(win) {
  const a = win?.architect;
  const steps = Number(a?.steps) || 0;
  if (!a || steps <= 0) {
    dom.architect.hidden = true;
    return;
  }
  const line = a.running
    ? `Step ${a.step_number || "?"} of ${steps}${a.current_step ? ` · ${a.current_step}` : ""}`
    : `${plural(steps, "step")} planned`;
  dom.architect.hidden = false;
  dom.architect.replaceChildren(
    h(
      "div",
      { class: "architect-info" },
      h("div", { class: "architect-label" }, "Architect plan"),
      h("div", { class: "architect-line" }, line),
      !a.running && a.outcome ? h("div", { class: "architect-outcome" }, String(a.outcome)) : null,
    ),
    a.running
      ? h("button", { type: "button", class: "btn small danger", onclick: () => architectAction("stop") }, "Stop")
      : h("button", { type: "button", class: "btn small primary", onclick: () => architectAction("run") }, "Run plan"),
  );
}

function optionClass(kind) {
  switch (kind) {
    case "allow_once":
      return "primary";
    case "allow_always":
      return "positive";
    case "reject_once":
      return "danger";
    case "reject_always":
      return "danger-outline";
    default:
      return "secondary";
  }
}

function setCardBusy(card, busy) {
  card.classList.toggle("busy", busy);
  for (const button of card.querySelectorAll("button")) button.disabled = busy;
}

function renderPermissions() {
  const pending = pendingPermissions().filter((p) => !state.answered.has(permissionKey(p)));
  const sig = JSON.stringify(pending);
  if (sig === state.permissionsSig) return;
  state.permissionsSig = sig;
  dom.permissions.replaceChildren(...pending.map(permissionCard));
}

function permissionCard(permission) {
  const options = Array.isArray(permission.options) ? permission.options : [];
  const card = h(
    "article",
    { class: "perm-card", "aria-label": "Permission request" },
    h("div", { class: "perm-kicker" }, "Permission needed"),
    inlineMarkdown("div", "perm-title", permission.title || "Tool call"),
    permission.detail ? markdownBlock(permission.detail, "perm-detail") : null,
  );
  card.append(
    h(
      "div",
      { class: "perm-actions" },
      options.map((option) =>
        h(
          "button",
          {
            type: "button",
            class: `btn ${optionClass(option.kind)}`,
            onclick: () => answerPermission(permission, option, card),
          },
          option.name || option.id,
        ),
      ),
    ),
  );
  return card;
}

const STATUS_ICONS = {
  completed: ["✓", "Completed"],
  failed: ["✕", "Failed"],
  running: ["", "Running"],
  waiting: ["!", "Waiting for permission"],
  rejected: ["⊘", "Rejected"],
  canceled: ["–", "Canceled"],
  pending: ["•", "Pending"],
};

function statusIcon(status) {
  const [glyph, label] = STATUS_ICONS[status] ?? ["•", "Tool call"];
  return h("span", { class: `status-icon s-${status ?? "none"}`, role: "img", "aria-label": label }, glyph);
}

function renderEntry(entry, sessionId) {
  const text = String(entry?.text ?? "");
  switch (entry?.role) {
    case "user":
      return h("div", { class: "msg user" }, h("div", { class: "bubble" }, text));
    case "assistant":
      return h("div", { class: "msg assistant" }, markdownBlock(text));
    case "tool":
      return renderToolEntry(entry, text, `${sessionId}:${entry.index}`);
    default:
      return h("div", { class: "notice" }, text);
  }
}

function renderToolEntry(entry, text, key) {
  const summaryText = firstLine(text) || "Tool call";
  const hasMore = text.trim().split("\n").length > 1 || summaryText.length > 60;
  if (!hasMore) {
    return h(
      "div",
      { class: "tool" },
      h("div", { class: "tool-summary" }, statusIcon(entry.status), inlineMarkdown("span", "tool-line", summaryText)),
    );
  }
  const details = h(
    "details",
    { class: "tool", open: state.openTools.has(key) },
    h("summary", { class: "tool-summary" }, statusIcon(entry.status), inlineMarkdown("span", "tool-line", summaryText)),
    h("div", { class: "tool-body" }, markdownBlock(text)),
  );
  details.addEventListener("toggle", () => {
    if (details.open) state.openTools.add(key);
    else state.openTools.delete(key);
  });
  return details;
}

function outboxEntry(item) {
  const label = { sending: "Sending…", queued: "Queued", sent: "Sent" }[item.state];
  return h(
    "div",
    { class: "msg user pending" },
    h("div", { class: "bubble" }, item.text),
    h("div", { class: "msg-meta" }, label),
  );
}

function transcriptEmptyState() {
  if (!state.device) {
    return h(
      "div",
      { class: "empty" },
      h("strong", {}, "No device selected"),
      state.devices.length ? "Choose the laptop you want to control." : "Turn on Remote in Praxis on your laptop.",
      h("br"),
      h("button", { type: "button", class: "btn", onclick: openDeviceSheet }, "Choose device"),
    );
  }
  if (!state.status || !state.threadKnown) {
    let text = "Waiting for Praxis to come online…";
    if (isOnline(state.device)) text = state.status ? "Loading thread…" : "Waiting for Praxis…";
    return h("div", { class: "loading" }, h("span", { class: "spinner" }), text);
  }
  if (state.threadError) {
    return h("div", { class: "empty" }, h("strong", {}, "Couldn't load the thread"), state.threadError);
  }
  return h(
    "div",
    { class: "empty" },
    h("strong", {}, "No active thread"),
    "Send a message to get started, or open one from Threads.",
  );
}

function renderTranscript() {
  const thread = state.thread;
  const entries = Array.isArray(thread?.entries) ? thread.entries : [];
  const sig = JSON.stringify([
    !!state.device,
    !!state.status,
    state.threadKnown,
    isOnline(state.device),
    thread?.session_id,
    thread?.total,
    entries,
    state.threadError,
    state.outbox.map((o) => [o.id, o.state]),
  ]);
  if (sig === state.transcriptSig) return;
  state.transcriptSig = sig;

  const session = thread?.session_id ?? null;
  const sessionChanged = session !== state.transcriptSession;
  state.transcriptSession = session;

  const children = [];
  if (!thread && state.outbox.length === 0) {
    children.push(transcriptEmptyState());
  } else if (thread) {
    const hidden = (Number(thread.total) || entries.length) - entries.length;
    if (hidden > 0)
      children.push(h("div", { class: "earlier" }, `${plural(hidden, "earlier entry", "earlier entries")} not shown`));
    for (const entry of entries) children.push(renderEntry(entry, session));
  }
  children.push(...state.outbox.map(outboxEntry));

  const el = dom.transcript;
  const previousTop = el.scrollTop;
  el.replaceChildren(...children);
  if (state.stickToBottom || sessionChanged) {
    state.stickToBottom = true;
    el.scrollTop = el.scrollHeight;
    dom.jumpBottom.hidden = true;
  } else {
    el.scrollTop = previousTop;
    dom.jumpBottom.hidden = false;
  }
}

function renderThreads() {
  const view = dom.viewThreads;
  const { loading, items, error, opening } = state.threads;
  const head = h(
    "div",
    { class: "section-head" },
    h("h2", {}, "Threads"),
    h(
      "button",
      { type: "button", class: "btn small ghost", disabled: loading || !state.device, onclick: () => loadThreads() },
      loading ? "Loading…" : "Reload",
    ),
  );
  const body = [];
  if (error) body.push(h("div", { class: "inline-error", role: "alert" }, error));
  if (!state.device) {
    body.push(h("div", { class: "empty" }, "Select a device first."));
  } else if (!items) {
    if (loading) body.push(h("div", { class: "loading" }, h("span", { class: "spinner" }), "Loading threads…"));
  } else if (items.length === 0) {
    body.push(h("div", { class: "empty" }, h("strong", {}, "No threads yet"), "Start one from the Chat tab."));
  } else {
    body.push(
      h(
        "ul",
        { class: "card-list" },
        items.map((t) => {
          const updated = parseDate(t.updated_at);
          return h(
            "li",
            {},
            h(
              "button",
              { type: "button", class: "row", disabled: !!opening, onclick: () => openThread(t) },
              h(
                "span",
                { class: "row-main" },
                h("span", { class: "row-title" }, t.title || "Untitled thread"),
                h("span", { class: "row-sub" }, updated ? relativeTime(updated) : ""),
              ),
              t.active ? h("span", { class: "chip" }, "Active") : null,
              opening === t.session_id ? h("span", { class: "spinner" }) : h("span", { class: "row-chevron" }),
            ),
          );
        }),
      ),
    );
  }
  view.replaceChildren(head, ...body);
}

function renderFiles() {
  const view = dom.viewFiles;
  const { file } = state.files;
  view.replaceChildren(...(file ? fileViewer(file) : directoryListing()));
}

function breadcrumbs(path) {
  const segments = path ? path.split("/").filter(Boolean) : [];
  const crumbs = [
    h(
      "button",
      { type: "button", class: "crumb", "aria-current": segments.length ? null : "page", onclick: () => listDir("") },
      "Projects",
    ),
  ];
  segments.forEach((segment, i) => {
    const target = segments.slice(0, i + 1).join("/");
    const isLast = i === segments.length - 1;
    crumbs.push(
      h("span", { class: "crumb-sep", "aria-hidden": "true" }, "/"),
      h(
        "button",
        { type: "button", class: "crumb", "aria-current": isLast ? "page" : null, onclick: () => listDir(target) },
        segment,
      ),
    );
  });
  return h("nav", { class: "crumbs", "aria-label": "Path" }, crumbs);
}

function directoryListing() {
  const { loading, path, entries, error } = state.files;
  if (!state.device) return [h("div", { class: "empty" }, "Select a device first.")];
  const out = [breadcrumbs(path)];
  if (error) out.push(h("div", { class: "inline-error", role: "alert" }, error));
  if (loading) {
    out.push(h("div", { class: "loading" }, h("span", { class: "spinner" }), "Loading…"));
  } else if (entries && entries.length === 0) {
    out.push(h("div", { class: "empty" }, path ? "This folder is empty." : "No projects are open in this window."));
  } else if (entries) {
    out.push(
      h(
        "ul",
        { class: "card-list" },
        entries.map((entry) =>
          h(
            "li",
            {},
            h(
              "button",
              {
                type: "button",
                class: "row",
                onclick: () => (entry.dir ? listDir(entry.path) : openFile(entry)),
              },
              h("span", { class: `ico ${entry.dir ? "ico-dir" : "ico-file"}`, "aria-hidden": "true" }),
              h("span", { class: "row-main" }, h("span", { class: "row-title" }, entry.name)),
              entry.dir ? h("span", { class: "row-chevron" }) : null,
            ),
          ),
        ),
      ),
    );
  } else if (error) {
    out.push(h("button", { type: "button", class: "btn", onclick: () => listDir(path) }, "Try again"));
  }
  return out;
}

function fileViewer(file) {
  const name = String(file.path).split("/").pop() || file.path;
  const meta = [file.path, Number.isFinite(file.size) ? formatBytes(file.size) : null].filter(Boolean).join(" · ");
  const back = h(
    "button",
    {
      type: "button",
      class: "btn small",
      onclick: () => {
        state.files = { ...state.files, file: null, error: null };
        if (!state.files.entries) listDir(state.files.path);
        else renderFiles();
      },
    },
    "‹ Back",
  );
  const out = [
    h(
      "div",
      { class: "viewer-head" },
      back,
      h(
        "div",
        { class: "viewer-title" },
        h("span", { class: "viewer-name" }, name),
        h("span", { class: "viewer-meta" }, meta),
      ),
    ),
  ];
  if (file.truncated) out.push(h("div", { class: "inline-note" }, "This file is large; only the beginning is shown."));
  out.push(codeView(String(file.content ?? "")));
  return out;
}

function codeView(content) {
  const lines = content.replace(/\r\n?/g, "\n").split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  const code = h("code");
  const fragment = document.createDocumentFragment();
  for (const line of lines) {
    const span = document.createElement("span");
    span.className = "ln";
    span.textContent = line || " ";
    fragment.append(span);
  }
  code.append(fragment);
  const pre = h("pre", { class: "code-view", tabindex: "0" }, code);
  pre.style.setProperty("--gutter", `${String(lines.length).length}ch`);
  return pre;
}

function renderDeviceList() {
  const list = dom.deviceList;
  if (!state.devicesLoaded) {
    list.replaceChildren(h("div", { class: "loading" }, h("span", { class: "spinner" }), "Looking for devices…"));
    return;
  }
  if (state.devices.length === 0) {
    list.replaceChildren(
      h(
        "div",
        { class: "empty" },
        h("strong", {}, "No devices yet"),
        `Turn on Remote in Praxis on your laptop. It creates an issue titled "${DEVICE_TITLE_PREFIX}<name>" in ${state.config?.repo ?? "your repository"}.`,
      ),
    );
    return;
  }
  list.replaceChildren(
    h(
      "ul",
      { class: "card-list" },
      state.devices.map((device) =>
        h(
          "li",
          {},
          h(
            "button",
            {
              type: "button",
              class: "row",
              "aria-current": device.number === state.device?.number ? "true" : null,
              onclick: () => {
                dom.deviceSheet.close();
                selectDevice(device);
              },
            },
            h("span", { class: `dot ${isOnline(device) ? "online" : "offline"}`, "aria-hidden": "true" }),
            h(
              "span",
              { class: "row-main" },
              h("span", { class: "row-title" }, device.name),
              h("span", { class: "row-sub" }, `${presenceText(device)} · issue #${device.number}`),
            ),
            device.number === state.device?.number ? h("span", { class: "chip" }, "Selected") : null,
          ),
        ),
      ),
    ),
  );
}

function toast(message, tone = "info", duration = 3500) {
  const el = h("div", { class: `toast ${tone}`, role: tone === "error" ? "alert" : "status" }, message);
  const dismiss = () => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 250);
  };
  el.addEventListener("click", dismiss);
  dom.toasts.append(el);
  while (dom.toasts.children.length > 3) dom.toasts.firstElementChild.remove();
  requestAnimationFrame(() => el.classList.add("show"));
  setTimeout(dismiss, duration);
}

// ---------------------------------------------------------------------------
// Navigation, sheets, composer
// ---------------------------------------------------------------------------

function switchTab(tab) {
  state.tab = tab;
  for (const button of dom.tabs) button.setAttribute("aria-selected", String(button.dataset.tab === tab));
  dom.viewChat.hidden = tab !== "chat";
  dom.viewFiles.hidden = tab !== "files";
  dom.viewThreads.hidden = tab !== "threads";
  if (tab === "chat") {
    renderChat();
    if (state.stickToBottom) dom.transcript.scrollTop = dom.transcript.scrollHeight;
  }
  loadActiveTabData();
}

function openDeviceSheet() {
  renderDeviceList();
  if (!dom.deviceSheet.open) dom.deviceSheet.showModal();
  loadDevices();
}

function openSettings() {
  dom.settingsRepo.textContent = state.config?.repo ?? "";
  dom.settingsUser.textContent = state.config?.login ?? "Unknown";
  dom.settingsDevice.textContent = state.device ? `${state.device.name} (issue #${state.device.number})` : "None";
  if (!dom.settingsSheet.open) dom.settingsSheet.showModal();
}

function autoGrow() {
  const textarea = dom.prompt;
  textarea.style.height = "auto";
  textarea.style.height = `${Math.min(textarea.scrollHeight + 2, Math.round(window.innerHeight * 0.4))}px`;
}

function signOut() {
  if (!confirm("Forget the token and repository on this device?")) return;
  removeKey(STORAGE.config);
  removeKey(STORAGE.device);
  rejectQueued(new ApiError("Signed out.", { kind: "cancelled" }));
  stopTimers();
  etagCache.clear();
  state.config = null;
  state.devices = [];
  state.devicesLoaded = false;
  state.device = null;
  state.banners.clear();
  resetDeviceView();
  dom.settingsSheet.close();
  showSetup();
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

function showSetup() {
  dom.app.hidden = true;
  dom.setup.hidden = false;
  dom.setupError.hidden = true;
}

function setupError(message) {
  dom.setupError.textContent = message;
  dom.setupError.hidden = !message;
}

function explainSetupError(err, stage) {
  if (err.kind === "auth")
    return "GitHub rejected the token. Check that you copied all of it and that it hasn't expired.";
  if (err.kind === "not_found") {
    return "Repository not found, or the token can't see it. Fine-grained tokens must be granted access to this specific repository.";
  }
  if (err.kind === "forbidden" && stage === "issues") return "The token needs the “Issues: Read and write” permission.";
  return err.message;
}

async function onSetupSubmit(event) {
  event.preventDefault();
  const repo = normalizeRepo(dom.setupRepo.value);
  const token = dom.setupToken.value.trim();
  if (!repo) return setupError("Enter the repository as owner/name.");
  if (!token) return setupError("Paste your access token.");
  setupError("");
  dom.setupSubmit.disabled = true;
  dom.setupSubmit.textContent = "Checking…";

  let stage = "repo";
  try {
    const { data: repoData } = await github.repo(repo, token);
    stage = "user";
    let login = null;
    try {
      login = (await github.user(token)).data?.login ?? null;
    } catch (err) {
      if (err.kind === "auth") throw err;
    }
    stage = "issues";
    await github.probeIssues(repo, token);
    if (
      repoData?.private === false &&
      !confirm("This repository is public, so anyone could read your prompts and code. Continue anyway?")
    ) {
      return;
    }
    state.config = { repo: repoData?.full_name || repo, token, login };
    saveJson(STORAGE.config, state.config);
    dom.setupToken.value = "";
    startApp();
  } catch (err) {
    setupError(explainSetupError(err, stage));
  } finally {
    dom.setupSubmit.disabled = false;
    dom.setupSubmit.textContent = "Connect";
  }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

function startApp() {
  state.banners.clear();
  dom.setup.hidden = true;
  dom.app.hidden = false;
  switchTab("chat");
  renderAll();
  loadDevices({ initial: true });
  stopTimers();
  timers.devices = setInterval(() => {
    if (!document.hidden) loadDevices();
  }, DEVICE_REFRESH_MS);
  // Keeps "Updated Ns ago" current.
  timers.tick = setInterval(() => {
    if (!document.hidden) renderTopbar();
  }, TICK_MS);
  timers.slowTick = setInterval(() => {
    if (document.hidden) return;
    renderBanners();
    if (state.tab === "threads") renderThreads();
  }, SLOW_TICK_MS);
}

function stopTimers() {
  clearTimeout(poller.timer);
  clearInterval(timers.devices);
  clearInterval(timers.tick);
  clearInterval(timers.slowTick);
}

function bindDom() {
  const $ = (id) => document.getElementById(id);
  dom = {
    setup: $("setup"),
    setupForm: $("setup-form"),
    setupRepo: $("setup-repo"),
    setupToken: $("setup-token"),
    setupError: $("setup-error"),
    setupSubmit: $("setup-submit"),
    app: $("app"),
    deviceButton: $("device-button"),
    deviceDot: $("device-dot"),
    deviceName: $("device-name"),
    deviceSub: $("device-sub"),
    windowSelect: $("window-select"),
    refreshButton: $("refresh-button"),
    settingsButton: $("settings-button"),
    tabs: [...document.querySelectorAll(".tab")],
    banners: $("banners"),
    viewChat: $("view-chat"),
    viewFiles: $("view-files"),
    viewThreads: $("view-threads"),
    threadTitle: $("thread-title"),
    threadStatus: $("thread-status"),
    newThread: $("new-thread"),
    modeControl: $("mode-control"),
    architect: $("architect"),
    permissions: $("permissions"),
    transcript: $("transcript"),
    jumpBottom: $("jump-bottom"),
    composer: $("composer"),
    prompt: $("prompt"),
    stopButton: $("stop-button"),
    sendButton: $("send-button"),
    settingsSheet: $("settings-sheet"),
    settingsRepo: $("settings-repo"),
    settingsUser: $("settings-user"),
    settingsDevice: $("settings-device"),
    settingsChangeDevice: $("settings-change-device"),
    settingsSignout: $("settings-signout"),
    deviceSheet: $("device-sheet"),
    deviceSheetReload: $("device-sheet-reload"),
    deviceList: $("device-list"),
    toasts: $("toasts"),
  };
}

function bindEvents() {
  dom.setupForm.addEventListener("submit", onSetupSubmit);

  dom.deviceButton.addEventListener("click", openDeviceSheet);
  dom.settingsButton.addEventListener("click", openSettings);
  dom.refreshButton.addEventListener("click", () => {
    if (!snapshotFresh() && !watcher.inFlight) watcher.renewAt = 0;
    pollIssue();
    loadDevices();
    loadActiveTabData({ force: true });
  });
  dom.windowSelect.addEventListener("change", () => {
    const id = state.status?.windows?.find((w) => String(w.window) === dom.windowSelect.value)?.window;
    if (id === undefined) return;
    setWindow(id);
    // The current snapshot may already cover this window.
    if (state.snapshot) applySnapshot(state.snapshot);
    renderAll();
    ensureWatch();
    loadActiveTabData();
  });
  for (const button of dom.tabs) button.addEventListener("click", () => switchTab(button.dataset.tab));

  dom.newThread.addEventListener("click", startNewThread);
  dom.stopButton.addEventListener("click", stopGenerating);
  dom.composer.addEventListener("submit", (event) => {
    event.preventDefault();
    sendPrompt();
  });
  dom.prompt.addEventListener("input", autoGrow);
  dom.prompt.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      sendPrompt();
    }
  });

  dom.transcript.addEventListener("scroll", () => {
    const el = dom.transcript;
    state.stickToBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    if (state.stickToBottom) dom.jumpBottom.hidden = true;
  });
  dom.jumpBottom.addEventListener("click", () => {
    dom.transcript.scrollTo({ top: dom.transcript.scrollHeight, behavior: "smooth" });
    dom.jumpBottom.hidden = true;
  });

  dom.settingsChangeDevice.addEventListener("click", () => {
    dom.settingsSheet.close();
    openDeviceSheet();
  });
  dom.settingsSignout.addEventListener("click", signOut);
  dom.deviceSheetReload.addEventListener("click", () => {
    state.devicesLoaded = false;
    renderDeviceList();
    loadDevices();
  });

  for (const sheet of [dom.settingsSheet, dom.deviceSheet]) {
    sheet.addEventListener("click", (event) => {
      if (event.target === sheet || event.target.closest("[data-close]")) sheet.close();
    });
  }

  // While hidden: no polling and no watch renewals. On return, poll at once; the watch is
  // renewed at the end of that poll if it's due.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      clearTimeout(poller.timer);
      return;
    }
    if (!state.config || dom.app.hidden) return;
    renderTopbar();
    renderBanners();
    pollIssue();
    loadDevices();
  });
  window.addEventListener("online", () => {
    if (state.config && !dom.app.hidden) pollIssue();
  });
}

function registerServiceWorker() {
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("./sw.js").catch(() => {
      // Offline support is optional.
    });
  }
}

function boot() {
  bindDom();
  bindEvents();
  registerServiceWorker();
  const config = loadJson(STORAGE.config);
  if (config?.repo && config?.token) {
    state.config = config;
    startApp();
  } else {
    showSetup();
  }
}

boot();
