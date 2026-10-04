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

private val INDEX_INTEGER = Regex("0|[1-9][0-9]*")

internal fun JSONObject.index(key: String): Int? {
    val value = opt(key) as? Number ?: return null
    val text = value.toString()
    return if (INDEX_INTEGER.matches(text)) text.toIntOrNull() else null
}

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

data class ModelOption(val id: String, val name: String, val group: String?, val disabled: Boolean)
data class ModelInfo(val current: String?, val available: List<ModelOption>, val nextOffset: Int? = null) {
    fun append(page: ModelInfo): ModelInfo = page.copy(available = (available + page.available).associateBy { it.id }.values.toList())
}

fun parseModels(result: JSONObject): ModelInfo = ModelInfo(
    current = result.str("current"),
    nextOffset = result.index("next_offset"),
    available = result.arr("available")?.objects()?.mapNotNull { model ->
        val id = model.str("id") ?: return@mapNotNull null
        ModelOption(id, model.str("name") ?: id, model.str("group"), model.bool("disabled"))
    }.orEmpty(),
)

data class DetailRequest(val session: String, val entry: Int = 0, val part: Int? = null, val queueId: String? = null) {
    fun arguments(): JSONObject = JSONObject().put("session_id", session).also {
        if (queueId != null) it.put("queue_id", queueId) else it.put("entry_index", entry)
        if (part != null) it.put("part_index", part)
    }
}


data class DetailChunk(val offset: Long, val nextOffset: Long, val totalBytes: Long, val version: String, val text: String)

data class DetailBody(
    val chunks: List<DetailChunk> = emptyList(),
    val nextOffset: Long = 0,
    val totalBytes: Long? = null,
    val version: String? = null,
) {
    val complete: Boolean get() = version != null && nextOffset == totalBytes

    fun append(chunk: DetailChunk): DetailBody? {
        if (complete || chunk.offset != nextOffset || chunk.offset < 0 || chunk.nextOffset < chunk.offset ||
            chunk.nextOffset > chunk.totalBytes || chunk.totalBytes < 0 || chunk.version.isBlank() || chunk.version.length > 128 ||
            chunk.text.toByteArray(Charsets.UTF_8).size.toLong() != chunk.nextOffset - chunk.offset ||
            (chunk.nextOffset == chunk.offset && chunk.nextOffset < chunk.totalBytes) ||
            (version != null && (version != chunk.version || totalBytes != chunk.totalBytes))
        ) return null
        return DetailBody(chunks + chunk, chunk.nextOffset, chunk.totalBytes, chunk.version)
    }
}

private fun JSONObject.byteOffset(key: String): Long? {
    val value = opt(key) as? Number ?: return null
    return value.toString().takeIf(INDEX_INTEGER::matches)?.toLongOrNull()
}

fun parseDetailChunk(result: JSONObject): DetailChunk? {
    val offset = result.byteOffset("offset") ?: return null
    val next = result.byteOffset("next_offset") ?: return null
    val total = result.byteOffset("total_bytes") ?: return null
    if (result.opt("done") != (next == total)) return null
    return DetailChunk(offset, next, total, result.str("version") ?: return null, result.str("text") ?: return null)
}

data class QuestionHeader(val id: String, val sessionId: String, val title: String, val sessionTitle: String? = null) {
    val key: String get() = "$sessionId:$id"
}
data class QuestionOption(val value: String, val label: String, val description: String?)
data class QuestionForm(val question: String, val options: List<QuestionOption>, val allowMultiple: Boolean, val autoAnswerPaused: Boolean)
data class QuestionPage(val questions: List<QuestionHeader>, val nextOffset: Int?)

private fun parseQuestionHeaders(array: JSONArray?): List<QuestionHeader> = array?.objects()?.mapNotNull {
    QuestionHeader(it.str("id") ?: return@mapNotNull null, it.str("session_id") ?: return@mapNotNull null, it.str("title").orEmpty(), it.str("session_title"))
}.orEmpty()

fun parseQuestionPage(result: JSONObject): QuestionPage = QuestionPage(parseQuestionHeaders(result.arr("questions")), result.index("next_offset"))

fun parseQuestionForm(result: JSONObject): QuestionForm? {
    val question = result.str("question")?.takeIf(String::isNotBlank) ?: return null
    val options = result.arr("options")?.objects()?.map {
        QuestionOption(it.str("value") ?: return null, it.str("label") ?: return null, it.str("description"))
    }.orEmpty()
    if (options.any { it.value.isBlank() || it.label.isBlank() } || options.map { it.value }.toSet().size != options.size) return null
    val multiple = result.bool("allow_multiple")
    if (multiple && options.isEmpty()) return null
    return QuestionForm(question, options, multiple, result.bool("auto_answer_paused"))
}

fun questionAnswerContent(form: QuestionForm, selected: Set<String>, freeform: String): JSONObject? {
    val text = freeform.trim()
    if (text.isNotEmpty()) return JSONObject().put(if (form.options.isEmpty()) "answer" else "freeform_answer", text)
    if (selected.isEmpty() || (!form.allowMultiple && selected.size != 1) || selected.any { value -> form.options.none { it.value == value } }) return null
    val values = form.options.filter { it.value in selected }.map { it.value }
    return JSONObject().put("answer", if (form.allowMultiple) JSONArray(values) else values.single())
}

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
    val model: String? = null,
    val modelName: String? = null,
    val modelSelection: Boolean = false,
    val sendNow: Boolean = false,
    val steering: Boolean = false,
    val queueManagement: Boolean = false,
    val questions: List<QuestionHeader> = emptyList(),
    val questionCount: Int = 0,
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

data class Status(val device: String?, val windows: List<WindowInfo>, val openFolder: Boolean = false)

data class QueuedMessage(val id: String, val text: String, val steer: Boolean)
data class QueuePage(val entries: List<QueuedMessage>, val nextOffset: Int?, val total: Int)

fun parseQueue(result: JSONObject): QueuePage = QueuePage(
    entries = result.arr("entries")?.objects()?.mapNotNull {
        val id = it.str("id")?.takeIf(String::isNotBlank) ?: return@mapNotNull null
        QueuedMessage(id, it.str("text").orEmpty(), it.bool("steer"))
    }.orEmpty(),
    nextOffset = result.index("next_offset"),
    total = result.index("total") ?: 0,
)

data class HostFolder(val name: String, val path: String)
data class HostFolders(val host: String, val path: String, val parent: String?, val folders: List<HostFolder>, val nextOffset: Int?)

fun parseHostFolders(result: JSONObject): HostFolders = HostFolders(
    host = result.str("host").orEmpty(),
    path = result.str("path").orEmpty(),
    parent = result.str("parent"),
    folders = result.arr("folders")?.objects()?.mapNotNull {
        HostFolder(it.str("name") ?: return@mapNotNull null, it.str("path") ?: return@mapNotNull null)
    }.orEmpty(),
    nextOffset = result.index("next_offset"),
)

data class RemoteViewScope(val generation: Int, val viewKey: String, val revision: Long)

internal suspend fun viewScopedAction(
    scope: RemoteViewScope,
    currentScope: () -> RemoteViewScope,
    operation: suspend () -> Unit,
    onSuccess: () -> Unit,
    onFailure: (ApiException) -> Unit,
): Boolean {
    if (scope != currentScope()) return false
    return try {
        operation()
        if (scope != currentScope()) return false
        onSuccess()
        true
    } catch (error: ApiException) {
        if (scope == currentScope()) onFailure(error)
        false
    }
}

fun transcriptFingerprint(text: String): String = java.util.Base64.getEncoder().encodeToString(
    java.security.MessageDigest.getInstance("SHA-256").digest(text.trim().toByteArray(Charsets.UTF_8)),
)

data class EntryPart(val index: Int, val role: String, val text: String, val detailsPending: Boolean = false)

data class Entry(
    val index: Int,
    val role: String,
    val text: String,
    val status: String?,
    val parts: List<EntryPart> = emptyList(),
    val truncated: Boolean = false,
    val truncation: String? = null,
    val detailsPending: Boolean = false,
    val fingerprint: String? = null,
) {
    val snapshotPreview: Boolean get() = truncated && truncation == "snapshot_budget"
}

data class ThreadView(
    val sessionId: String?,
    val title: String?,
    val status: String?,
    val total: Int,
    val entries: List<Entry>,
    val stepThreads: List<ThreadView> = emptyList(),
    val beforeIndex: Int? = null,
    val nextBefore: Int? = null,
    val hasMore: Boolean? = null,
    val pagingValid: Boolean = true,
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
    openFolder = o.obj("capabilities")?.bool("open_folder") == true,
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
    model = o.str("model"),
    modelName = o.str("model_name"),
    modelSelection = o.bool("model_selection"),
    sendNow = o.bool("send_now"),
    steering = o.bool("steering"),
    queueManagement = o.bool("queue_management"),
    questions = parseQuestionHeaders(o.arr("questions")),
    questionCount = o.index("question_count") ?: 0,
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

fun parseThreadView(o: JSONObject): ThreadView = parseThreadView(o, includeSteps = true)

private fun parseThreadView(o: JSONObject, includeSteps: Boolean): ThreadView {
    var valid = true
    val paging = o.has("next_before") || !o.isNull("before_index") || o.has("has_more")
    fun index(objectValue: JSONObject, key: String, optional: Boolean = false): Int? {
        if (optional && objectValue.isNull(key)) return null
        return objectValue.index(key).also { if (it == null) valid = false }
    }
    val before = index(o, "before_index", optional = true)
    val next = if (paging) index(o, "next_before") else null
    val total = if (paging || o.has("total")) index(o, "total") else null
    val hasMore = o.opt("has_more") as? Boolean
    if (paging && hasMore == null) valid = false
    val rawEntries = o.arr("entries")
    if (paging && rawEntries == null) valid = false
    if (rawEntries != null && rawEntries.objects().size != rawEntries.length()) valid = false
    val entries = o.arr("entries")?.objects()?.map { e ->
        Entry(
            index = index(e, "index") ?: -1,
            role = e.str("role") ?: "notice",
            text = e.str("text").orEmpty(),
            status = e.str("status"),
            truncated = e.bool("truncated"),
            truncation = e.str("truncation"),
            detailsPending = e.bool("details_pending"),
            fingerprint = e.str("fingerprint"),
            parts = e.arr("parts")?.objects()?.mapIndexedNotNull { partOffset, part ->
                val text = part.str("text").orEmpty()
                val pending = part.bool("details_pending")
                if (text.isBlank() && !pending) return@mapIndexedNotNull null
                val partIndex = if (part.has("index")) index(part, "index") ?: -1 else partOffset
                EntryPart(partIndex, part.str("role") ?: "notice", text, pending)
            }.orEmpty(),
        )
    }.orEmpty()
    return ThreadView(
        sessionId = o.str("session_id"),
        title = o.str("title"),
        status = o.str("status"),
        total = total ?: entries.size,
        entries = entries,
        beforeIndex = before,
        nextBefore = next,
        hasMore = hasMore,
        pagingValid = valid,
        stepThreads = if (includeSteps) {
            o.arr("step_threads")?.objects()?.map { parseThreadView(it, includeSteps = false) }.orEmpty()
        } else {
            emptyList()
        },
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

