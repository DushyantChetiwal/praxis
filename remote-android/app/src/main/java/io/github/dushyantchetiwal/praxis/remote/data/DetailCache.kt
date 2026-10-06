package io.github.dushyantchetiwal.praxis.remote.data

import java.io.File
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.LinkedHashMap
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/** ViewModel-owned cache; recycled chat rows do not own downloaded bodies. */
class DetailCache<T : Any>(
    private val directory: File,
    private val weight: (T) -> Long,
    private val encode: (T) -> ByteArray,
    private val decode: (ByteArray) -> T,
    private val memoryBytes: Long = 16L * 1024 * 1024,
    private val diskBytes: Long = 128L * 1024 * 1024,
) {
    private val key = ByteArray(32).also(SecureRandom()::nextBytes)
    private val entries = LinkedHashMap<String, T>(16, 0.75f, true)
    private val revisions = LinkedHashMap<String, Long>(16, 0.75f, true)
    private val persisted = mutableMapOf<String, Long>()
    private val io = Mutex()
    private var retainedBytes = 0L
    private var sequence = 0L
    private var closed = false

    @Synchronized fun peek(identity: String): T? = if (closed) null else entries[identity]

    @Synchronized fun remember(identity: String, value: T): Long {
        if (closed) return -1
        val revision = ++sequence
        revisions[identity] = revision
        persisted.remove(identity)
        trimMetadata()
        entries.remove(identity)?.let { retainedBytes -= weight(it) }
        val bytes = weight(value)
        if (bytes <= memoryBytes) {
            entries[identity] = value
            retainedBytes += bytes
        }
        val iterator = entries.entries.iterator()
        while (retainedBytes > memoryBytes && iterator.hasNext()) {
            retainedBytes -= weight(iterator.next().value)
            iterator.remove()
        }
        return revision
    }

    private fun trimMetadata() {
        val iterator = revisions.entries.iterator()
        while (revisions.size > 2048 && iterator.hasNext()) {
            val identity = iterator.next().key
            iterator.remove()
            persisted.remove(identity)
            entries.remove(identity)?.let { retainedBytes -= weight(it) }
        }
    }

    @Synchronized private fun current(identity: String, revision: Long): Boolean =
        !closed && revisions[identity] == revision

    @Synchronized fun invalidate(identity: String): Long {
        entries.remove(identity)?.let { retainedBytes -= weight(it) }
        val revision = ++sequence
        revisions[identity] = revision
        persisted.remove(identity)
        trimMetadata()
        return revision
    }

    private fun file(identity: String): File {
        val hash = MessageDigest.getInstance("SHA-256").digest(identity.toByteArray(Charsets.UTF_8))
            .joinToString("") { "%02x".format(it) }
        return File(directory, "$hash.cache")
    }

    suspend fun persist(identity: String, value: T, revision: Long) = withContext(Dispatchers.IO) {
        io.withLock {
            if (!current(identity, revision)) return@withLock
            check(directory.isDirectory || directory.mkdirs()) { "Could not create the conversation cache" }
            val destination = file(identity)
            val encrypted = RemoteCrypto.seal(key, encode(value), "praxis-remote/detail-cache/v1/$identity")
            if (encrypted.length > diskBytes) return@withLock
            val temporary = File(directory, destination.name + ".tmp")
            temporary.writeText(encrypted, Charsets.US_ASCII)
            if (current(identity, revision)) {
                if (destination.exists()) check(destination.delete()) { "Could not replace a cached conversation body" }
                check(temporary.renameTo(destination)) { "Could not save a conversation body" }
                synchronized(this@DetailCache) {
                    if (current(identity, revision)) persisted[identity] = revision
                }
                pruneDisk()
            } else if (temporary.exists()) {
                check(temporary.delete()) { "Could not remove a stale cache write" }
            }
        }
    }

    suspend fun restore(identity: String): T? {
        peek(identity)?.let { return it }
        val revision = synchronized(this) { if (closed) return null; revisions[identity] ?: return null }
        val bytes = withContext(Dispatchers.IO) {
            io.withLock {
                if (!current(identity, revision) || synchronized(this@DetailCache) { persisted[identity] != revision }) return@withLock null
                val source = file(identity)
                if (!source.isFile || source.length() > diskBytes) return@withLock null
                val plain = RemoteCrypto.open(key, source.readText(Charsets.US_ASCII), "praxis-remote/detail-cache/v1/$identity")
                source.setLastModified(System.currentTimeMillis())
                plain
            }
        } ?: return null
        val value = withContext(Dispatchers.Default) { decode(bytes) }
        return synchronized(this) {
            if (!current(identity, revision)) return@synchronized null
            // Preserve the revision so an older disk read cannot supersede a newer load.
            entries.remove(identity)?.let { retainedBytes -= weight(it) }
            if (weight(value) <= memoryBytes) {
                entries[identity] = value
                retainedBytes += weight(value)
                val iterator = entries.entries.iterator()
                while (retainedBytes > memoryBytes && iterator.hasNext()) {
                    retainedBytes -= weight(iterator.next().value)
                    iterator.remove()
                }
            }
            value
        }
    }

    suspend fun remove(identity: String, revision: Long) = withContext(Dispatchers.IO) {
        io.withLock {
            if (current(identity, revision)) {
                val source = file(identity)
                if (source.exists()) check(source.delete()) { "Could not remove a cached conversation body" }
            }
        }
    }

    private fun pruneDisk() {
        val files = directory.listFiles()?.filter { it.extension == "cache" }?.sortedBy(File::lastModified).orEmpty()
        var bytes = files.sumOf(File::length)
        var count = files.size
        for (file in files) {
            if (bytes <= diskBytes && count <= 2048) break
            val size = file.length()
            check(file.delete()) { "Could not evict an old conversation cache entry" }
            bytes -= size
            count--
        }
    }

    @Synchronized fun close() {
        closed = true
        entries.clear()
        revisions.clear()
        persisted.clear()
        retainedBytes = 0
    }

    suspend fun removeFiles() = withContext(Dispatchers.IO) {
        io.withLock {
            if (directory.exists()) check(directory.deleteRecursively()) { "Could not clear the conversation cache" }
        }
    }
}

fun detailCacheIdentity(channel: String?, window: Long?, rootSession: String?, request: DetailRequest): String =
    org.json.JSONArray(listOf(channel, window, rootSession, request.session, request.entry, request.part, request.queueId)).toString()

fun encodeDetailBody(body: DetailBody): ByteArray {
    val chunks = org.json.JSONArray()
    body.chunks.forEach { chunk ->
        chunks.put(org.json.JSONObject().put("offset", chunk.offset).put("next_offset", chunk.nextOffset)
            .put("total_bytes", chunk.totalBytes).put("version", chunk.version).put("text", chunk.text)
            .put("done", chunk.nextOffset == chunk.totalBytes))
    }
    return chunks.toString().toByteArray(Charsets.UTF_8)
}

fun decodeDetailBody(bytes: ByteArray): DetailBody {
    var body = DetailBody()
    val chunks = org.json.JSONArray(String(bytes, Charsets.UTF_8))
    for (index in 0 until chunks.length()) {
        val chunk = parseDetailChunk(chunks.getJSONObject(index)) ?: error("Invalid cached conversation chunk")
        body = body.append(chunk) ?: error("Inconsistent cached conversation body")
    }
    return body
}
