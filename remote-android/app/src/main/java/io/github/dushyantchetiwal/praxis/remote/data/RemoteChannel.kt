package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.os.SystemClock
import io.github.dushyantchetiwal.praxis.remote.R
import java.security.SecureRandom
import java.time.Instant
import java.time.OffsetDateTime
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

// The protocol is defined by crates/agent_ui/src/remote.rs on the laptop side.

const val DEVICE_TITLE_PREFIX = "Praxis \u00b7 "
private const val REQUEST_MARKER = "<!-- praxis-request -->"
private val RESPONSE_MARKER_RE = Regex("""^\s*<!--\s*praxis-response\s+(\S+?)\s*-->""")
private val DEVICE_META_RE = Regex("""<!--\s*praxis-device\s+([\s\S]*?)-->""")
private val STATE_MARKER_RE = Regex("""<!--\s*praxis-state\s*-->""")
private val REPO_RE = Regex("""^([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)$""")

const val ONLINE_THRESHOLD_MS = 3 * 60_000L
private const val REQUEST_POLL_MS = 1_500L
private const val REQUEST_TIMEOUT_MS = 45_000L

// ---------------------------------------------------------------------------
// JSON helpers (org.json turns JSON null into a "null" string if asked)
// ---------------------------------------------------------------------------

internal fun JSONObject.str(key: String): String? =
    if (isNull(key)) null else opt(key)?.let { it as? String ?: it.toString() }

internal fun JSONObject.long(key: String): Long? = (opt(key) as? Number)?.toLong()

internal fun JSONObject.int(key: String): Int? = (opt(key) as? Number)?.toInt()

internal fun JSONObject.bool(key: String): Boolean = opt(key) == true

internal fun JSONObject.obj(key: String): JSONObject? = opt(key) as? JSONObject

internal fun JSONObject.arr(key: String): JSONArray? = opt(key) as? JSONArray

internal fun JSONArray.objects(): List<JSONObject> = (0 until length()).mapNotNull { opt(it) as? JSONObject }

internal fun JSONArray.strings(): List<String> = (0 until length()).mapNotNull { opt(it) as? String }

/** RFC 3339 to epoch milliseconds; accepts both `Z` and `+00:00`. */
fun parseTime(value: String?): Long? =
    value?.takeIf { it.isNotEmpty() }?.let { runCatching { OffsetDateTime.parse(it).toInstant().toEpochMilli() }.getOrNull() }

/** The text from the first `{` to the last `}`, as both ends pull JSON out of a fenced block. */
fun jsonObjectIn(text: String): String? {
    val start = text.indexOf('{')
    val end = text.lastIndexOf('}')
    return if (start >= 0 && end > start) text.substring(start, end + 1) else null
}

fun normalizeRepo(input: String): String? {
    val cleaned = input.trim()
        .replace(Regex("^https?://github\\.com/", RegexOption.IGNORE_CASE), "")
        .replace(Regex("\\.git$", RegexOption.IGNORE_CASE), "")
        .trimEnd('/')
    return cleaned.takeIf { REPO_RE.matches(it) }
}

fun repoPath(repo: String, suffix: String): String = "/repos/$repo$suffix"

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

data class Device(val number: Long, val name: String, val startedAt: Long?, val lastSeen: Long?) {
    fun seenRecently(now: Long): Boolean = lastSeen != null && now - lastSeen < ONLINE_THRESHOLD_MS
}

data class RepoInfo(val fullName: String, val private: Boolean)

data class WatchInfo(val window: Long?, val sessionId: String?, val until: Long?)

data class ModeOption(val id: String, val name: String)

data class ModeInfo(val current: String?, val available: List<ModeOption>)

data class PermissionOption(val id: String, val name: String, val kind: String?) {
    val isAllow: Boolean get() = kind?.startsWith("allow") == true
    val isReject: Boolean get() = kind?.startsWith("reject") == true
}

data class Permission(
    val sessionId: String,
    val toolCallId: String,
    val title: String?,
    val detail: String?,
    val options: List<PermissionOption>,
) {
    val key: String get() = "$sessionId:$toolCallId"
}

data class ThreadSummary(
    val sessionId: String?,
    val title: String?,
    val status: String?,
    val queued: Int,
    val mode: ModeInfo?,
    val pending: List<Permission>,
)

data class Architect(
    val steps: Int,
    val running: Boolean,
    val currentStep: String?,
    val stepNumber: Int,
    val outcome: String?,
)

data class WindowInfo(
    val id: Long,
    val projects: List<String>,
    val active: Boolean,
    val thread: ThreadSummary?,
    val architect: Architect?,
) {
    /** The window's projects, or null when it has none open. */
    val projectsLabel: String? get() = projects.filter { it.isNotBlank() }.joinToString(", ").ifEmpty { null }
}

data class Status(val device: String?, val windows: List<WindowInfo>)

data class Entry(val index: Int, val role: String, val text: String, val status: String?)

data class ThreadView(
    val sessionId: String?,
    val title: String?,
    val status: String?,
    val total: Int,
    val entries: List<Entry>,
)

data class Snapshot(
    val updatedAtRaw: String?,
    val updatedAt: Long?,
    val watch: WatchInfo?,
    val status: Status?,
    val thread: ThreadView?,
    val threadError: String?,
)

data class ThreadItem(val sessionId: String, val title: String?, val updatedAt: Long?, val active: Boolean)

data class DirEntry(val name: String, val path: String, val dir: Boolean)

data class DirListing(val path: String, val entries: List<DirEntry>, val truncated: Boolean)

data class FileContent(val path: String, val truncated: Boolean, val size: Long?, val content: String)

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fun parseDevice(issue: JSONObject): Device? {
    if (issue.has("pull_request")) return null
    val title = issue.str("title") ?: return null
    if (!title.startsWith(DEVICE_TITLE_PREFIX)) return null
    val number = issue.long("number") ?: return null
    val meta = issue.str("body")?.let { body ->
        DEVICE_META_RE.find(body)?.groupValues?.get(1)?.trim()?.let { runCatching { JSONObject(it) }.getOrNull() }
    }
    val name = (meta?.str("device")?.takeIf { it.isNotBlank() } ?: title.removePrefix(DEVICE_TITLE_PREFIX))
        .trim()
        .ifEmpty { "Unnamed device" }
    return Device(number, name, parseTime(meta?.str("started_at")), parseTime(meta?.str("last_seen")))
}

fun parseSnapshot(body: String?): Snapshot? {
    if (body == null) return null
    val marker = STATE_MARKER_RE.find(body) ?: return null
    val json = jsonObjectIn(body.substring(marker.range.last + 1)) ?: return null
    val o = runCatching { JSONObject(json) }.getOrNull() ?: return null
    return Snapshot(
        updatedAtRaw = o.str("updated_at"),
        updatedAt = parseTime(o.str("updated_at")),
        watch = o.obj("watch")?.let { w -> WatchInfo(w.long("window"), w.str("session_id"), parseTime(w.str("until"))) },
        status = o.obj("status")?.let(::parseStatus),
        thread = o.obj("thread")?.let(::parseThreadView),
        threadError = o.str("thread_error"),
    )
}

fun parseStatus(o: JSONObject): Status = Status(
    device = o.str("device"),
    windows = o.arr("windows")?.objects()?.mapNotNull(::parseWindow).orEmpty(),
)

private fun parseWindow(o: JSONObject): WindowInfo? {
    val id = o.long("window") ?: return null
    return WindowInfo(
        id = id,
        projects = o.arr("projects")?.strings().orEmpty(),
        active = o.bool("active"),
        thread = o.obj("thread")?.let(::parseThreadSummary),
        architect = o.obj("architect")?.let { a ->
            Architect(
                steps = a.int("steps") ?: 0,
                running = a.bool("running"),
                currentStep = a.str("current_step"),
                stepNumber = a.int("step_number") ?: 0,
                outcome = a.str("outcome"),
            )
        },
    )
}

private fun parseThreadSummary(o: JSONObject): ThreadSummary = ThreadSummary(
    sessionId = o.str("session_id"),
    title = o.str("title"),
    status = o.str("status"),
    queued = o.int("queued") ?: 0,
    mode = o.obj("mode")?.let { m ->
        ModeInfo(
            current = m.str("current"),
            available = m.arr("available")?.objects()?.mapNotNull { a ->
                val id = a.str("id") ?: return@mapNotNull null
                ModeOption(id, a.str("name")?.takeIf { it.isNotBlank() } ?: id)
            }.orEmpty(),
        )
    },
    pending = o.arr("pending")?.objects()?.mapNotNull { p ->
        Permission(
            sessionId = p.str("session_id") ?: return@mapNotNull null,
            toolCallId = p.str("tool_call_id") ?: return@mapNotNull null,
            title = p.str("title"),
            detail = p.str("detail"),
            options = p.arr("options")?.objects()?.mapNotNull { option ->
                val id = option.str("id") ?: return@mapNotNull null
                PermissionOption(id, option.str("name")?.takeIf { it.isNotBlank() } ?: id, option.str("kind"))
            }.orEmpty(),
        )
    }.orEmpty(),
)

fun parseThreadView(o: JSONObject): ThreadView {
    val entries = o.arr("entries")?.objects()?.map { e ->
        Entry(
            index = e.int("index") ?: 0,
            role = e.str("role") ?: "notice",
            text = e.str("text").orEmpty(),
            status = e.str("status"),
        )
    }.orEmpty()
    return ThreadView(
        sessionId = o.str("session_id"),
        title = o.str("title"),
        status = o.str("status"),
        total = o.int("total") ?: entries.size,
        entries = entries,
    )
}

fun parseThreads(result: JSONObject?): List<ThreadItem> =
    result?.arr("threads")?.objects()?.mapNotNull { t ->
        ThreadItem(
            sessionId = t.str("session_id") ?: return@mapNotNull null,
            title = t.str("title"),
            updatedAt = parseTime(t.str("updated_at")),
            active = t.bool("active"),
        )
    }.orEmpty().sortedByDescending { it.updatedAt ?: 0L }

fun parseListing(result: JSONObject?, requested: String): DirListing {
    val entries = result?.arr("entries")?.objects()?.mapNotNull { e ->
        val name = e.str("name") ?: return@mapNotNull null
        DirEntry(name = name, path = e.str("path") ?: name, dir = e.bool("dir"))
    }.orEmpty().sortedWith(compareBy<DirEntry>({ !it.dir }, { it.name.lowercase() }))
    return DirListing(result?.str("path") ?: requested, entries, result?.bool("truncated") == true)
}

fun parseFile(result: JSONObject?, requested: String): FileContent = FileContent(
    path = result?.str("path") ?: requested,
    truncated = result?.bool("truncated") == true,
    size = result?.long("size"),
    content = result?.str("content").orEmpty(),
)

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/**
 * Sends requests as comments on a device's issue and waits for Praxis to edit
 * each into its answer. Only one request is in flight at a time.
 */
class RemoteChannel(context: Context, private val gh: GitHubClient) {
    private val context = context.applicationContext
    private val mutex = Mutex()
    private val random = SecureRandom()
    private val background = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val _pending = MutableStateFlow(0)

    /** How many requests are queued or in flight. */
    val pending: StateFlow<Int> = _pending.asStateFlow()

    /** Runs [block] once every earlier request has finished. */
    suspend fun <T> serialized(block: suspend () -> T): T {
        _pending.update { it + 1 }
        try {
            return mutex.withLock { block() }
        } finally {
            _pending.update { it - 1 }
        }
    }

    /** Posts one request and returns the `{id, ok, result | error}` envelope. */
    suspend fun exchange(repo: String, device: Device, op: String, args: JSONObject): JSONObject {
        val id = newRequestId()
        val payload = JSONObject()
            .put("id", id)
            .put("op", op)
            .put("args", args)
            .put("sent_at", Instant.now().toString())
        val body = "$REQUEST_MARKER\n```json\n$payload\n```"
        val created = gh.call(
            "POST",
            repoPath(repo, "/issues/${device.number}/comments"),
            JSONObject().put("body", body),
        ).obj()
        val commentId = created?.long("id")
            ?: throw ApiException(ErrorKind.Http, context.getString(R.string.error_no_comment))

        val path = repoPath(repo, "/issues/comments/$commentId")
        val deadline = SystemClock.elapsedRealtime() + REQUEST_TIMEOUT_MS
        var finished = false
        try {
            while (SystemClock.elapsedRealtime() < deadline) {
                delay(REQUEST_POLL_MS)
                val answer = try {
                    pollComment(path)
                } catch (e: ApiException) {
                    finished = e.status == 404 // Nothing left to delete.
                    throw e
                }
                if (answer != null) {
                    finished = true
                    background.launch { deleteQuietly(path) }
                    return readEnvelope(answer, id)
                }
            }
        } finally {
            gh.forget(path)
            if (!finished) withContext(NonCancellable) { deleteQuietly(path) }
        }
        throw ApiException(ErrorKind.Timeout, context.getString(R.string.error_not_responding, device.name))
    }

    /** The comment's body once Praxis has answered, else null. */
    private suspend fun pollComment(path: String): String? {
        return try {
            val reply = gh.call("GET", path, conditional = true)
            val body = if (reply.changed) reply.obj()?.str("body").orEmpty() else ""
            body.takeIf { RESPONSE_MARKER_RE.containsMatchIn(it) }
        } catch (e: ApiException) {
            when {
                e.kind == ErrorKind.NotFound ->
                    throw ApiException(ErrorKind.Praxis, context.getString(R.string.error_request_removed), 404)
                e.kind == ErrorKind.Network || (e.kind == ErrorKind.Http && e.status >= 500) -> null
                else -> throw e
            }
        }
    }

    private suspend fun deleteQuietly(path: String) {
        try {
            gh.call("DELETE", path, allow = setOf(404))
        } catch (_: ApiException) {
            // Praxis ignores answered and stale requests, so a leftover is harmless.
        }
    }

    private fun readEnvelope(body: String, id: String): JSONObject {
        val markerId = RESPONSE_MARKER_RE.find(body)?.groupValues?.get(1)
        val newline = body.indexOf('\n')
        val rest = if (newline == -1) "" else body.substring(newline + 1)
        val envelope = jsonObjectIn(rest)?.let { runCatching { JSONObject(it) }.getOrNull() }
            ?: return failure(id, context.getString(R.string.error_unreadable_response))
        val envelopeId = envelope.str("id")
        return if (markerId == id && (envelopeId == null || envelopeId == id)) {
            envelope
        } else {
            failure(id, context.getString(R.string.error_wrong_response))
        }
    }

    private fun failure(id: String, message: String): JSONObject =
        JSONObject().put("id", id).put("ok", false).put("error", message)

    private fun newRequestId(): String {
        val bytes = ByteArray(8).also(random::nextBytes)
        val hex = bytes.joinToString("") { "%02x".format(it) }
        return "m${System.currentTimeMillis().toString(36)}-$hex"
    }
}
