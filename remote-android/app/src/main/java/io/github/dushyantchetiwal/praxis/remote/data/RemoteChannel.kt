package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.os.SystemClock
import io.github.dushyantchetiwal.praxis.remote.R
import java.security.SecureRandom
import java.time.Instant
import java.time.OffsetDateTime
import java.time.temporal.ChronoUnit
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

// The protocol is docs/src/ai/praxis-remote-protocol.md (version 2); the
// computer's side is crates/agent_ui/src/remote.rs.

const val PROTOCOL = "praxis-remote/v2"
const val META_FILE = "praxis-remote.json"
const val STATE_FILE = "state.json"
const val PAIR_HEADER = "$PROTOCOL pair"
private const val REQUEST_HEADER = "$PROTOCOL request"
private const val RESPONSE_HEADER = "$PROTOCOL response"
private const val REJECTED_HEADER = "$PROTOCOL rejected"
private val HEX32_RE = Regex("^[0-9a-f]{32}$")

const val ONLINE_THRESHOLD_MS = 3 * 60_000L
const val COMMENT_POLL_MS = 1_500L
private const val REQUEST_TIMEOUT_MS = 45_000L
private const val GIST_PAGES = 3
private const val GIST_PAGE_SIZE = 100

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

/** The text from the first `{` to the last `}`. */
fun jsonObjectIn(text: String): String? {
    val start = text.indexOf('{')
    val end = text.lastIndexOf('}')
    return if (start >= 0 && end > start) text.substring(start, end + 1) else null
}

fun isHexId(value: String?): Boolean = value != null && HEX32_RE.matches(value)

/** A comment's first line and the rest, tolerating `\r\n` line ends. */
internal fun splitComment(body: String): Pair<String, String> {
    val text = body.replace("\r\n", "\n")
    val newline = text.indexOf('\n')
    return if (newline == -1) text.trim() to "" else text.substring(0, newline).trim() to text.substring(newline + 1)
}

/** Whether a comment was written by [login] (any account when the login is unknown). */
internal fun JSONObject.writtenBy(login: String?): Boolean =
    login == null || obj("user")?.str("login")?.equals(login, ignoreCase = true) == true

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/** A computer running Praxis, found through the `praxis-remote.json` file of its gist. */
data class Device(
    val channel: String,
    val gistId: String,
    val name: String,
    val startedAt: Long?,
    val lastSeen: Long?,
    /** Ids of the phones the computer has paired. */
    val phones: List<String>,
) {
    fun seenRecently(now: Long): Boolean = lastSeen != null && now - lastSeen < ONLINE_THRESHOLD_MS
}

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

/** A computer from its gist's `praxis-remote.json`, or null if it is not protocol version 2. */
fun parseMeta(text: String?, gistId: String): Device? {
    val o = text?.let { runCatching { JSONObject(it) }.getOrNull() } ?: return null
    if (o.str("protocol") != PROTOCOL) return null
    val channel = o.str("channel")?.takeIf(::isHexId) ?: return null
    return Device(
        channel = channel,
        gistId = gistId,
        name = o.str("device")?.trim()?.takeIf { it.isNotEmpty() } ?: "Unnamed computer",
        startedAt = parseTime(o.str("started_at")),
        lastSeen = parseTime(o.str("last_seen")),
        phones = o.arr("phones")?.strings().orEmpty(),
    )
}

/** A decrypted snapshot: `{updated_at, watch, status, thread, thread_error}`. */
fun parseSnapshot(json: String?): Snapshot? {
    val o = json?.let { runCatching { JSONObject(it) }.getOrNull() } ?: return null
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
// Gists
// ---------------------------------------------------------------------------

/** What a paired phone needs to talk to one computer. */
class Link(
    val gistId: String,
    val channel: String,
    val phoneId: String,
    val key: ByteArray,
    val deviceName: String,
)

/**
 * One read of a computer's gist; [gist] is the cached copy when unchanged.
 * [body] is the raw response, to tell versions apart: the ETag cache is shared
 * with discovery, so `304` does not mean this caller has seen the version.
 */
class GistRead(val body: String?, val gist: JSONObject?, val device: Device?)

/** Finds computers among the user's gists and reads their files. */
class Gists(private val gh: GitHubClient) {
    suspend fun read(gistId: String): GistRead {
        val reply = gh.call("GET", "/gists/$gistId", conditional = true)
        val gist = reply.obj()
        val device = gist?.let { parseMeta(file(it, META_FILE), gistId) }
        return GistRead(reply.body, gist, device)
    }

    /** A file's text, fetched from its `raw_url` when the API truncated it. */
    suspend fun file(gist: JSONObject, name: String): String? {
        val file = gist.obj("files")?.obj(name) ?: return null
        val content = file.str("content")
        if (content != null && !file.bool("truncated")) return content
        val raw = file.str("raw_url") ?: return content
        return gh.fetchRaw(raw)
    }

    /**
     * Every computer in the user's gists, one per channel (the most recently
     * seen). [known] gist ids are read too, in case they are past the pages listed.
     */
    suspend fun discover(known: Collection<String> = emptyList()): List<Device> {
        val ids = LinkedHashSet<String>()
        for (page in 1..GIST_PAGES) {
            val list = gh.call("GET", "/gists?per_page=$GIST_PAGE_SIZE&page=$page", conditional = true).array() ?: break
            for (gist in list.objects()) {
                if (gist.obj("files")?.has(META_FILE) == true) gist.str("id")?.let(ids::add)
            }
            if (list.length() < GIST_PAGE_SIZE) break
        }
        ids += known
        val found = mutableListOf<Device>()
        for (id in ids) {
            try {
                read(id).device?.let(found::add)
            } catch (e: ApiException) {
                // Deleted since it was listed.
                if (e.kind != ErrorKind.NotFound) throw e
            }
        }
        return found.groupBy { it.channel }.values.map { copies -> copies.maxBy { it.lastSeen ?: 0L } }
    }

    /**
     * This phone's snapshot from `state.json`, or null when the computer has
     * not published one for it. Throws [CryptoException] if it does not decrypt.
     */
    suspend fun snapshot(gist: JSONObject, link: Link): Snapshot? {
        val text = file(gist, STATE_FILE) ?: return null
        val blob = runCatching { JSONObject(text) }.getOrNull()?.str(link.phoneId) ?: return null
        val plain = RemoteCrypto.open(link.key, blob, RemoteCrypto.stateAad(link.channel, link.phoneId))
        return parseSnapshot(String(plain, Charsets.UTF_8))
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/**
 * Sends encrypted requests as comments on a computer's gist and waits for
 * Praxis to edit each into its answer. Only one request is in flight at a time.
 */
class RemoteChannel(
    context: Context,
    private val gh: GitHubClient,
    /** The signed-in account; only comments it wrote are trusted. */
    private val login: () -> String?,
) {
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

    /**
     * Posts one request and returns the `{id, ok, result | error}` envelope.
     * Throws [ErrorKind.Unpaired] when the computer rejects this phone.
     */
    suspend fun exchange(link: Link, op: String, args: JSONObject): JSONObject {
        val id = newRequestId()
        val payload = JSONObject()
            .put("id", id)
            .put("op", op)
            .put("args", args)
            .put("sent_at", Instant.now().truncatedTo(ChronoUnit.SECONDS).toString())
        val blob = RemoteCrypto.seal(
            link.key,
            payload.toString().toByteArray(Charsets.UTF_8),
            RemoteCrypto.requestAad(link.channel, link.phoneId),
        )
        val created = gh.call(
            "POST",
            "/gists/${link.gistId}/comments",
            JSONObject().put("body", "$REQUEST_HEADER ${link.phoneId}\n$blob"),
        ).obj()
        val commentId = created?.long("id")
            ?: throw ApiException(ErrorKind.Http, context.getString(R.string.error_no_comment))

        val path = "/gists/${link.gistId}/comments/$commentId"
        val deadline = SystemClock.elapsedRealtime() + REQUEST_TIMEOUT_MS
        var finished = false
        try {
            while (SystemClock.elapsedRealtime() < deadline) {
                delay(COMMENT_POLL_MS)
                val answer = try {
                    pollComment(path, link.phoneId)
                } catch (e: ApiException) {
                    finished = e.status == 404 // Nothing left to delete.
                    throw e
                }
                if (answer != null) {
                    finished = true
                    background.launch { deleteQuietly(path) }
                    return readAnswer(answer, link, id)
                }
            }
        } finally {
            gh.forget(path)
            if (!finished) withContext(NonCancellable) { deleteQuietly(path) }
        }
        throw ApiException(ErrorKind.Timeout, context.getString(R.string.error_not_responding, link.deviceName))
    }

    /** The comment once it is no longer our request (Praxis answered), else null. */
    private suspend fun pollComment(path: String, phoneId: String): JSONObject? {
        return try {
            val reply = gh.call("GET", path, conditional = true)
            if (!reply.changed) return null
            val comment = reply.obj() ?: return null
            val (first, _) = splitComment(comment.str("body").orEmpty())
            comment.takeIf { first != "$REQUEST_HEADER $phoneId" }
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
            // Praxis refuses stale requests and cleans up old comments, so a leftover is harmless.
        }
    }

    private fun readAnswer(comment: JSONObject, link: Link, id: String): JSONObject {
        if (!comment.writtenBy(login())) return failure(id, context.getString(R.string.error_unreadable_response))
        val (first, rest) = splitComment(comment.str("body").orEmpty())
        val words = first.split(' ').filter { it.isNotEmpty() }
        if (words.size == 3 && "${words[0]} ${words[1]}" == REJECTED_HEADER && words[2] == link.phoneId) {
            val reason = rest.trim().ifEmpty { context.getString(R.string.error_rejected_no_reason) }
            throw ApiException(ErrorKind.Unpaired, context.getString(R.string.error_rejected, link.deviceName, reason))
        }
        if (words.size != 4 || "${words[0]} ${words[1]}" != RESPONSE_HEADER || words[2] != link.phoneId) {
            return failure(id, context.getString(R.string.error_unreadable_response))
        }
        if (words[3] != id) return failure(id, context.getString(R.string.error_wrong_response))
        val plain = try {
            RemoteCrypto.open(link.key, rest.trim(), RemoteCrypto.responseAad(link.channel, link.phoneId, id))
        } catch (e: CryptoException) {
            return failure(id, context.getString(R.string.error_undecryptable_response))
        }
        val envelope = runCatching { JSONObject(String(plain, Charsets.UTF_8)) }.getOrNull()
            ?: return failure(id, context.getString(R.string.error_unreadable_response))
        if (envelope.str("id") != id) return failure(id, context.getString(R.string.error_wrong_response))
        return envelope
    }

    private fun failure(id: String, message: String): JSONObject =
        JSONObject().put("id", id).put("ok", false).put("error", message)

    private fun newRequestId(): String {
        val bytes = ByteArray(8).also(random::nextBytes)
        val hex = bytes.joinToString("") { "%02x".format(it) }
        return "m${System.currentTimeMillis().toString(36)}-$hex"
    }
}

