package io.github.dushyantchetiwal.praxis.remote.data

import java.io.File
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

class DetailCacheTest {
    private fun cache(directory: File, memory: Long = 8) = DetailCache(
        directory, { value: String -> value.length.toLong() },
        { value -> value.toByteArray() }, { bytes -> String(bytes) }, memoryBytes = memory,
    )

    @Test fun scrollingBackReusesMemoryOrEncryptedDiskWithoutAnotherFetch() = runTest {
        val directory = kotlin.io.path.createTempDirectory("details").toFile()
        val cache = cache(directory)
        var fetches = 0
        suspend fun open(identity: String): String {
            cache.peek(identity)?.let { return it }
            cache.restore(identity)?.let { return it }
            fetches++
            val body = "body-$identity"
            val revision = cache.remember(identity, body)
            cache.persist(identity, body, revision)
            return body
        }
        try {
            assertEquals("body-a", open("a"))
            assertEquals("body-a", open("a"))
            assertEquals(1, fetches)
            assertEquals("body-b", open("b"))
            assertNull(cache.peek("a"))
            assertEquals("body-a", open("a"))
            assertEquals(2, fetches)
            assertTrue(directory.listFiles().orEmpty().all { !it.readText().contains("body-") })
        } finally { cache.close(); cache.removeFiles() }
    }

    @Test fun refreshAndLateWritesCannotRestoreAnOlderSnapshot() = runTest {
        val directory = kotlin.io.path.createTempDirectory("details").toFile()
        val cache = cache(directory)
        try {
            val old = cache.remember("a", "old")
            cache.persist("a", "old", old)
            val refresh = cache.invalidate("a")
            assertNull(cache.restore("a"))
            val current = cache.remember("a", "new")
            cache.persist("a", "new", current)
            cache.persist("a", "old", old)
            cache.remove("a", refresh)
            val other = cache.remember("b", "12345678")
            cache.persist("b", "12345678", other)
            assertNull(cache.peek("a"))
            assertEquals("new", cache.restore("a"))
        } finally { cache.close(); cache.removeFiles() }
    }

    @Test fun startupRemovesUnreadableOldCachesWithoutRemovingLiveViewModels() = runTest {
        val root = kotlin.io.path.createTempDirectory("cache-root").toFile()
        val abandoned = File(root, "details-00000000-0000-0000-0000-000000000000")
        val olderProcess = File(root, "details-00000000-0000-0000-0000-000000000000-11111111-1111-1111-1111-111111111111")
        val live = listOf(DetailCache.newDirectory(root), DetailCache.newDirectory(root))
        val unrelated = File(root, "details-unrelated")
        try {
            (live + listOf(abandoned, olderProcess, unrelated)).forEach { directory ->
                assertTrue(directory.mkdirs())
                File(directory, "body.cache").writeText("cache fixture")
            }
            DetailCache.removeAbandonedDirectories(root)
            assertFalse(abandoned.exists())
            assertFalse(olderProcess.exists())
            live.forEach { assertTrue(File(it, "body.cache").isFile) }
            assertTrue(File(unrelated, "body.cache").isFile)
            DetailCache.removeAbandonedDirectories(root)
            live.forEach { assertTrue(it.isDirectory) }
        } finally { assertTrue(root.deleteRecursively()) }
    }

    @Test fun identityIncludesWindowRootMessagePartAndQueue() {
        val request = DetailRequest("child", 3, 1)
        val identity = detailCacheIdentity("device", 1, "root", request)
        assertNotEquals(identity, detailCacheIdentity("other", 1, "root", request))
        assertNotEquals(identity, detailCacheIdentity("device", 2, "root", request))
        assertNotEquals(identity, detailCacheIdentity("device", 1, "other", request))
        assertNotEquals(identity, detailCacheIdentity("device", 1, "root", request.copy(part = 2)))
        assertNotEquals(identity, detailCacheIdentity("device", 1, "root", request.copy(queueId = "queued")))
    }

    @Test fun cacheCodecPreservesPartialUnicodeBodiesAndContinuation() {
        val text = "Ω complete text\n"
        val bytes = text.toByteArray().size.toLong()
        val first = DetailBody().append(DetailChunk(0, bytes, bytes + 3, "version", text))!!
        val restored = decodeDetailBody(encodeDetailBody(first))
        assertEquals(first, restored)
        assertFalse(restored.complete)
        val complete = restored.append(DetailChunk(bytes, bytes + 3, bytes + 3, "version", "end"))!!
        assertTrue(complete.complete)
        assertEquals(text + "end", complete.chunks.joinToString("") { it.text })
    }
}
