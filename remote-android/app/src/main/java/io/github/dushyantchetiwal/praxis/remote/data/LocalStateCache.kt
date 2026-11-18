package io.github.dushyantchetiwal.praxis.remote.data

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import java.util.LinkedHashMap
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

interface KeyValueTable {
    fun get(key: String): String?
    fun put(key: String, value: String, updatedAt: Long)
    fun delete(key: String)
    fun deletePrefix(prefix: String)
    fun trimToMaxRows(maxRows: Int)
    fun close()
}

class SqliteKeyValueTable(
    context: Context,
    dbName: String = DEFAULT_DB_NAME,
) : KeyValueTable {
    private val helper = object : SQLiteOpenHelper(context.applicationContext, dbName, null, DB_VERSION) {
        init {
            setWriteAheadLoggingEnabled(true)
        }

        override fun onCreate(db: SQLiteDatabase) {
            db.execSQL(
                """
                CREATE TABLE IF NOT EXISTS $TABLE_NAME (
                    cache_key TEXT PRIMARY KEY,
                    payload TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                )
                """.trimIndent(),
            )
            db.execSQL(
                "CREATE INDEX IF NOT EXISTS $INDEX_NAME ON $TABLE_NAME (updated_at)",
            )
        }

        override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) {
            db.execSQL("DROP TABLE IF EXISTS $TABLE_NAME")
            onCreate(db)
        }
    }

    override fun get(key: String): String? {
        val db = helper.readableDatabase
        db.rawQuery(
            "SELECT payload FROM $TABLE_NAME WHERE cache_key = ? LIMIT 1",
            arrayOf(key),
        ).use { cursor ->
            return if (cursor.moveToFirst()) cursor.getString(0) else null
        }
    }

    override fun put(key: String, value: String, updatedAt: Long) {
        val values = ContentValues().apply {
            put("cache_key", key)
            put("payload", value)
            put("updated_at", updatedAt)
        }
        helper.writableDatabase.insertWithOnConflict(
            TABLE_NAME,
            null,
            values,
            SQLiteDatabase.CONFLICT_REPLACE,
        )
    }

    override fun delete(key: String) {
        helper.writableDatabase.delete(TABLE_NAME, "cache_key = ?", arrayOf(key))
    }

    override fun deletePrefix(prefix: String) {
        val escaped = prefix
            .replace("\\", "\\\\")
            .replace("%", "\\%")
            .replace("_", "\\_")
        helper.writableDatabase.delete(
            TABLE_NAME,
            "cache_key LIKE ? ESCAPE '\\'",
            arrayOf("$escaped%"),
        )
    }

    override fun trimToMaxRows(maxRows: Int) {
        val db = helper.writableDatabase
        if (maxRows <= 0) {
            db.delete(TABLE_NAME, null, null)
            return
        }
        db.execSQL(
            """
            DELETE FROM $TABLE_NAME
            WHERE cache_key NOT IN (
                SELECT cache_key FROM $TABLE_NAME
                ORDER BY updated_at DESC, rowid DESC
                LIMIT ?
            )
            """.trimIndent(),
            arrayOf<Any>(maxRows),
        )
    }

    override fun close() {
        helper.close()
    }

    companion object {
        const val DEFAULT_DB_NAME = "praxis_local_state.db"
        private const val DB_VERSION = 1
        private const val TABLE_NAME = "cached_state"
        private const val INDEX_NAME = "idx_cached_state_updated_at"
    }
}

class InMemoryKeyValueTable : KeyValueTable {
    private data class Row(val value: String, val updatedAt: Long, val sequence: Long)

    private val lock = Any()
    private val rows = mutableMapOf<String, Row>()
    private var nextSequence = 0L

    override fun get(key: String): String? = synchronized(lock) { rows[key]?.value }

    override fun put(key: String, value: String, updatedAt: Long) {
        synchronized(lock) {
            rows[key] = Row(value, updatedAt, ++nextSequence)
        }
    }

    override fun delete(key: String) {
        synchronized(lock) {
            rows.remove(key)
        }
    }

    override fun deletePrefix(prefix: String) {
        synchronized(lock) {
            val iterator = rows.entries.iterator()
            while (iterator.hasNext()) {
                if (iterator.next().key.startsWith(prefix)) iterator.remove()
            }
        }
    }

    override fun trimToMaxRows(maxRows: Int) {
        synchronized(lock) {
            if (maxRows <= 0) {
                rows.clear()
                return
            }
            while (rows.size > maxRows) {
                val oldest = rows.entries.minWithOrNull(
                    compareBy<Map.Entry<String, Row>> { it.value.updatedAt }
                        .thenBy { it.value.sequence },
                ) ?: break
                rows.remove(oldest.key)
            }
        }
    }

    override fun close() {
        synchronized(lock) {
            rows.clear()
        }
    }

    fun keys(): Set<String> = synchronized(lock) { rows.keys.toSet() }

    fun size(): Int = synchronized(lock) { rows.size }
}

data class CachedWindowState(
    val status: Status?,
    val thread: ThreadView?,
    val windowId: Long?,
    val savedAtMillis: Long,
)

class LocalStateCache(
    private val table: KeyValueTable,
    private val maxRows: Int = DEFAULT_MAX_ROWS,
    private val maxMemoryEntries: Int = DEFAULT_MAX_MEMORY_ENTRIES,
    private val ioDispatcher: CoroutineDispatcher = Dispatchers.IO,
) {
    constructor(
        context: Context,
        dbName: String = SqliteKeyValueTable.DEFAULT_DB_NAME,
        maxRows: Int = DEFAULT_MAX_ROWS,
        maxMemoryEntries: Int = DEFAULT_MAX_MEMORY_ENTRIES,
    ) : this(SqliteKeyValueTable(context, dbName), maxRows, maxMemoryEntries)

    private val memoryLock = Any()
    private val memory = object : LinkedHashMap<String, CachedWindowState>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, CachedWindowState>?): Boolean =
            size > maxMemoryEntries
    }
    private val ioMutex = Mutex()

    @Volatile
    private var closed = false

    fun peek(channel: String, windowId: Long?): CachedWindowState? {
        if (closed) return null
        val key = storageKey(channel, slotFor(windowId))
        return synchronized(memoryLock) {
            if (closed) null else memory[key]
        }
    }

    suspend fun restore(channel: String, channelKey: ByteArray, windowId: Long?): CachedWindowState? {
        if (closed) return null
        peek(channel, windowId)?.let { return it }
        if (channelKey.size != RemoteCrypto.KEY_BYTES) return null

        val slot = slotFor(windowId)
        val key = storageKey(channel, slot)
        val restored = withContext(ioDispatcher) {
            ioMutex.withLock {
                if (closed) return@withLock null
                val sealed = table.get(key) ?: return@withLock null
                val decoded = runCatching {
                    val derived = cacheKey(channel, channelKey)
                    val plain = RemoteCrypto.open(derived, sealed, aad(channel, slot))
                    decodeCachedWindowState(String(plain, Charsets.UTF_8))
                }.getOrNull()
                if (decoded == null) {
                    runCatching { table.delete(key) }
                }
                decoded
            }
        } ?: return null

        synchronized(memoryLock) {
            if (!closed && maxMemoryEntries > 0) {
                memory[key] = restored
            }
        }
        return restored
    }

    suspend fun save(
        channel: String,
        channelKey: ByteArray,
        windowId: Long?,
        status: Status?,
        thread: ThreadView?,
        nowMillis: Long = System.currentTimeMillis(),
    ) {
        if (closed || channelKey.size != RemoteCrypto.KEY_BYTES) return
        val slot = slotFor(windowId)
        val key = storageKey(channel, slot)
        val boundedThread = thread?.let(::boundThreadView)
        val state = CachedWindowState(
            status = status,
            thread = boundedThread,
            windowId = windowId,
            savedAtMillis = nowMillis,
        )

        synchronized(memoryLock) {
            if (!closed && maxMemoryEntries > 0) {
                memory[key] = state
            }
        }

        withContext(ioDispatcher) {
            ioMutex.withLock {
                if (closed) return@withLock
                val derived = cacheKey(channel, channelKey)
                val jsonBytes = encodeCachedWindowState(state).toByteArray(Charsets.UTF_8)
                val sealed = RemoteCrypto.seal(derived, jsonBytes, aad(channel, slot))
                table.put(key, sealed, nowMillis)
                table.trimToMaxRows(maxRows)
            }
        }
    }

    suspend fun forgetChannel(channel: String) {
        val prefix = "$channel:"
        synchronized(memoryLock) {
            val iterator = memory.entries.iterator()
            while (iterator.hasNext()) {
                if (iterator.next().key.startsWith(prefix)) iterator.remove()
            }
        }
        withContext(ioDispatcher) {
            ioMutex.withLock {
                if (!closed) {
                    table.deletePrefix(prefix)
                }
            }
        }
    }

    fun close() {
        closed = true
        synchronized(memoryLock) {
            memory.clear()
        }
        table.close()
    }

    companion object {
        const val MAX_CACHED_TAIL_ENTRIES = 24
        const val MAX_PREVIEW_CHARS = 256
        const val DEFAULT_MAX_ROWS = 64
        const val DEFAULT_MAX_MEMORY_ENTRIES = 32

        fun cacheKey(channel: String, channelKey: ByteArray): ByteArray = RemoteCrypto.hkdfExpand(
            RemoteCrypto.hkdfExtract("praxis-remote/local-cache/v1".toByteArray(Charsets.UTF_8), channelKey),
            "praxis-remote/v2/cache/$channel".toByteArray(Charsets.UTF_8),
            RemoteCrypto.KEY_BYTES,
        )

        internal fun slotFor(windowId: Long?): String = windowId?.toString() ?: "default"

        internal fun storageKey(channel: String, slot: String): String = "$channel:$slot"

        internal fun aad(channel: String, slot: String): String =
            "praxis-remote/v2/local-state/$channel/$slot"

        internal fun boundThreadView(thread: ThreadView): ThreadView {
            val entries = thread.entries
            val dropped = entries.size > MAX_CACHED_TAIL_ENTRIES
            val tail = if (dropped) entries.takeLast(MAX_CACHED_TAIL_ENTRIES) else entries
            val boundedEntries = tail.map(::boundEntry)
            val tailStart = boundedEntries.firstOrNull()?.index
            val nextBefore = when {
                dropped && tailStart != null && tailStart >= 0 -> tailStart
                else -> thread.nextBefore
            }
            val hasMore = when {
                dropped && tailStart != null -> tailStart > 0 || thread.hasMore == true
                else -> thread.hasMore
            }
            val beforeIndex = if (nextBefore != null) thread.beforeIndex else null
            return thread.copy(
                entries = boundedEntries,
                beforeIndex = beforeIndex,
                nextBefore = nextBefore,
                hasMore = hasMore,
                stepThreads = thread.stepThreads.map(::boundThreadView),
            )
        }

        private fun boundEntry(entry: Entry): Entry {
            val textTruncated = entry.text.length > MAX_PREVIEW_CHARS
            val boundedText = if (textTruncated) entry.text.take(MAX_PREVIEW_CHARS) else entry.text
            val boundedParts = entry.parts.map { part ->
                val partTruncated = part.text.length > MAX_PREVIEW_CHARS
                if (partTruncated) {
                    part.copy(text = part.text.take(MAX_PREVIEW_CHARS), detailsPending = true)
                } else {
                    part
                }
            }
            val anyPartTruncated = boundedParts != entry.parts
            return if (textTruncated || anyPartTruncated) {
                entry.copy(
                    text = boundedText,
                    parts = boundedParts,
                    truncated = entry.truncated || textTruncated,
                    truncation = entry.truncation ?: if (textTruncated) "snapshot_budget" else null,
                    detailsPending = true,
                )
            } else {
                entry
            }
        }

        private fun encodeCachedWindowState(state: CachedWindowState): String {
            val root = JSONObject()
                .put("saved_at", state.savedAtMillis)
            if (state.windowId != null) root.put("window_id", state.windowId)
            if (state.status != null) root.put("status", encodeStatus(state.status))
            if (state.thread != null) root.put("thread", encodeThreadView(state.thread))
            return root.toString()
        }

        private fun decodeCachedWindowState(json: String): CachedWindowState {
            val root = JSONObject(json)
            val savedAt = root.long("saved_at") ?: 0L
            val windowId = root.long("window_id")
            val status = root.obj("status")?.let(::parseStatus)
            val thread = root.obj("thread")?.let(::parseThreadView)
            return CachedWindowState(
                status = status,
                thread = thread,
                windowId = windowId,
                savedAtMillis = savedAt,
            )
        }

        private fun encodeStatus(status: Status): JSONObject {
            val capabilities = JSONObject().put("open_folder", status.openFolder)
            if (status.uploadLimit != null) {
                capabilities.put("file_upload", JSONObject().put("max_bytes", status.uploadLimit))
            }
            val windows = JSONArray()
            status.windows.forEach { window ->
                windows.put(encodeWindow(window))
            }
            return JSONObject()
                .put("device", status.device)
                .put("capabilities", capabilities)
                .put("windows", windows)
        }

        private fun encodeWindow(window: WindowInfo): JSONObject {
            val o = JSONObject()
                .put("window", window.id)
                .put("projects", JSONArray(window.projects))
                .put("active", window.active)
            window.thread?.let { o.put("thread", encodeThreadSummary(it)) }
            window.architect?.let { a ->
                o.put(
                    "architect",
                    JSONObject()
                        .put("steps", a.steps)
                        .put("running", a.running)
                        .put("current_step", a.currentStep)
                        .put("step_number", a.stepNumber)
                        .put("outcome", a.outcome),
                )
            }
            window.connection?.let { c ->
                o.put(
                    "connection",
                    JSONObject()
                        .put("host", c.host)
                        .put("state", c.state),
                )
            }
            return o
        }

        private fun encodeThreadSummary(summary: ThreadSummary): JSONObject {
            val o = JSONObject()
                .put("session_id", summary.sessionId)
                .put("title", summary.title)
                .put("status", summary.status)
                .put("queued", summary.queued)
                .put("model", summary.model)
                .put("model_name", summary.modelName)
                .put("model_selection", summary.modelSelection)
                .put("send_now", summary.sendNow)
                .put("steering", summary.steering)
                .put("queue_management", summary.queueManagement)
                .put("image_input", summary.imageInput)
                .put("dismiss_error", summary.dismissError)
                .put("compact", summary.compact)
                .put("terminals", summary.terminals)
                .put("background_commands", summary.backgroundCommands)
                .put("question_count", summary.questionCount)
            summary.error?.let { e ->
                o.put(
                    "error",
                    JSONObject()
                        .put("kind", e.kind)
                        .put("message", e.message)
                        .put("retryable", e.retryable),
                )
            }
            summary.retrying?.let { r ->
                o.put(
                    "retrying",
                    JSONObject()
                        .put("attempt", r.attempt)
                        .put("max_attempts", r.maxAttempts)
                        .put("message", r.message),
                )
            }
            if (summary.plan.isNotEmpty()) {
                val planArray = JSONArray()
                summary.plan.forEach { step ->
                    planArray.put(JSONObject().put("step", step.step).put("status", step.status))
                }
                o.put("plan", planArray)
            }
            if (summary.questions.isNotEmpty()) {
                val questionsArray = JSONArray()
                summary.questions.forEach { q ->
                    questionsArray.put(
                        JSONObject()
                            .put("id", q.id)
                            .put("session_id", q.sessionId)
                            .put("title", q.title)
                            .put("session_title", q.sessionTitle),
                    )
                }
                o.put("questions", questionsArray)
            }
            summary.mode?.let { m ->
                val available = JSONArray()
                m.available.forEach { opt ->
                    available.put(JSONObject().put("id", opt.id).put("name", opt.name))
                }
                o.put("mode", JSONObject().put("current", m.current).put("available", available))
            }
            if (summary.pending.isNotEmpty()) {
                val pendingArray = JSONArray()
                summary.pending.forEach { p ->
                    val options = JSONArray()
                    p.options.forEach { opt ->
                        options.put(
                            JSONObject()
                                .put("id", opt.id)
                                .put("name", opt.name)
                                .put("kind", opt.kind),
                        )
                    }
                    pendingArray.put(
                        JSONObject()
                            .put("session_id", p.sessionId)
                            .put("tool_call_id", p.toolCallId)
                            .put("title", p.title)
                            .put("detail", p.detail)
                            .put("options", options),
                    )
                }
                o.put("pending", pendingArray)
            }
            return o
        }

        private fun encodeThreadView(thread: ThreadView): JSONObject {
            val o = JSONObject()
                .put("session_id", thread.sessionId)
                .put("title", thread.title)
                .put("status", thread.status)
                .put("total", thread.total)
            if (thread.nextBefore != null) {
                if (thread.beforeIndex != null) o.put("before_index", thread.beforeIndex)
                o.put("next_before", thread.nextBefore)
                o.put("has_more", thread.hasMore ?: (thread.nextBefore > 0))
            }
            val entriesArray = JSONArray()
            thread.entries.forEach { entry ->
                val e = JSONObject()
                    .put("index", entry.index)
                    .put("role", entry.role)
                    .put("text", entry.text)
                    .put("status", entry.status)
                    .put("truncated", entry.truncated)
                    .put("truncation", entry.truncation)
                    .put("details_pending", entry.detailsPending)
                    .put("fingerprint", entry.fingerprint)
                if (entry.parts.isNotEmpty()) {
                    val partsArray = JSONArray()
                    entry.parts.forEach { part ->
                        partsArray.put(
                            JSONObject()
                                .put("index", part.index)
                                .put("role", part.role)
                                .put("text", part.text)
                                .put("details_pending", part.detailsPending),
                        )
                    }
                    e.put("parts", partsArray)
                }
                entriesArray.put(e)
            }
            o.put("entries", entriesArray)
            if (thread.stepThreads.isNotEmpty()) {
                val stepsArray = JSONArray()
                thread.stepThreads.forEach { step ->
                    stepsArray.put(encodeThreadView(step))
                }
                o.put("step_threads", stepsArray)
            }
            return o
        }
    }
}
