package io.github.dushyantchetiwal.praxis.remote.data

import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class LocalStateCacheTest {
    private fun key(seed: Int): ByteArray = ByteArray(RemoteCrypto.KEY_BYTES) { (seed + it).toByte() }

    private fun sampleStatus(deviceName: String, windowId: Long): Status = Status(
        device = deviceName,
        openFolder = true,
        uploadLimit = 1024L * 1024L,
        windows = listOf(
            WindowInfo(
                id = windowId,
                projects = listOf("praxis"),
                active = true,
                thread = ThreadSummary(
                    sessionId = "sess-1",
                    title = "Investigate cache",
                    status = "running",
                    queued = 1,
                    mode = ModeInfo("code", listOf(ModeOption("code", "Code"))),
                    pending = listOf(
                        Permission(
                            sessionId = "sess-1",
                            toolCallId = "tool-1",
                            title = "Run tests",
                            detail = "./gradlew test",
                            options = listOf(PermissionOption("allow", "Allow", "allow_once")),
                        ),
                    ),
                    model = "gemini-2.5-pro",
                    modelName = "Gemini 2.5 Pro",
                    modelSelection = true,
                    sendNow = true,
                    steering = true,
                    queueManagement = true,
                    plan = listOf(PlanStep("Add cache", "in_progress")),
                ),
                architect = Architect(
                    steps = 4,
                    running = true,
                    currentStep = "Step 3",
                    stepNumber = 3,
                    outcome = null,
                ),
                connection = RemoteConnection(host = "workstation", state = "connected"),
            ),
        ),
    )

    @Test
    fun saveAndRestoreBoundsTailTo24EntriesAndTruncatesLongPreviews() = runTest {
        val table = InMemoryKeyValueTable()
        val cache = LocalStateCache(table = table, maxRows = 16, maxMemoryEntries = 8)
        val channel = "0123456789abcdef0123456789abcdef"
        val channelKey = key(1)
        val longText = "x".repeat(400)
        val entries = (0 until 30).map { idx ->
            Entry(
                index = idx,
                role = if (idx % 2 == 0) "user" else "assistant",
                text = if (idx == 29) longText else "entry-$idx",
                status = "completed",
                parts = if (idx == 28) listOf(EntryPart(0, "tool", "y".repeat(300), detailsPending = false)) else emptyList(),
            )
        }
        val thread = ThreadView(
            sessionId = "sess-1",
            title = "Long conversation",
            status = "idle",
            total = 30,
            entries = entries,
            beforeIndex = null,
            nextBefore = null,
            hasMore = false,
            pagingValid = true,
        )
        val status = sampleStatus("Desktop-Alpha", windowId = 42L)

        try {
            cache.save(channel, channelKey, 42L, status, thread, nowMillis = 1_000L)

            // Stored payload is encrypted and does not leak plaintext.
            val rawRow = table.get("$channel:42")
            assertNotNull(rawRow)
            assertFalse(rawRow!!.contains("Desktop-Alpha"))
            assertFalse(rawRow.contains("Long conversation"))
            assertFalse(rawRow.contains("entry-29"))

            // Cold-start cache instance restores from encrypted storage.
            val coldCache = LocalStateCache(table = table, maxRows = 16, maxMemoryEntries = 8)
            assertNull(coldCache.peek(channel, 42L))

            val restored = coldCache.restore(channel, channelKey, 42L)
            assertNotNull(restored)
            assertEquals(42L, restored!!.windowId)
            assertEquals(1_000L, restored.savedAtMillis)
            assertEquals("Desktop-Alpha", restored.status?.device)
            assertEquals("sess-1", restored.status?.windows?.single()?.thread?.sessionId)

            val restoredThread = restored.thread
            assertNotNull(restoredThread)
            assertTrue(restoredThread!!.pagingValid)
            assertEquals(24, restoredThread.entries.size)
            assertEquals(6, restoredThread.entries.first().index)
            assertEquals(29, restoredThread.entries.last().index)
            assertEquals(6, restoredThread.nextBefore)
            assertEquals(true, restoredThread.hasMore)

            val lastEntry = restoredThread.entries.last()
            assertEquals(LocalStateCache.MAX_PREVIEW_CHARS, lastEntry.text.length)
            assertTrue(lastEntry.detailsPending)
            assertTrue(lastEntry.truncated)

            val partEntry = restoredThread.entries[restoredThread.entries.size - 2]
            assertEquals(LocalStateCache.MAX_PREVIEW_CHARS, partEntry.parts.single().text.length)
            assertTrue(partEntry.parts.single().detailsPending)
            assertTrue(partEntry.detailsPending)
            assertTrue(partEntry.truncated)
            assertEquals("snapshot_budget", partEntry.truncation)
            assertTrue(partEntry.snapshotPreview)

            // Restore populates memory LRU for subsequent 0ms peeks.
            assertEquals(restored, coldCache.peek(channel, 42L))
            coldCache.close()
        } finally {
            cache.close()
        }
    }

    @Test
    fun wrongKeyWrongChannelOrCorruptCiphertextReturnsNullAndCleansUpRow() = runTest {
        val table = InMemoryKeyValueTable()
        val cache = LocalStateCache(table = table)
        val channelA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        val channelB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        val keyA = key(10)
        val keyB = key(20)

        try {
            cache.save(channelA, keyA, 1L, sampleStatus("PC-A", 1L), null, nowMillis = 500L)
            assertEquals(1, table.size())

            // Wrong channel cannot see or decrypt channelA's row.
            assertNull(cache.peek(channelB, 1L))
            assertNull(cache.restore(channelB, keyA, 1L))
            assertEquals(1, table.size())

            // Cold restore with wrong key fails decryption and deletes the unreadable row.
            val coldWrongKey = LocalStateCache(table = table)
            assertNull(coldWrongKey.restore(channelA, keyB, 1L))
            assertEquals(0, table.size())
            coldWrongKey.close()

            // Corrupt ciphertext is also safely discarded.
            table.put("$channelA:1", "not-valid-base64-or-gcm", 600L)
            val coldCorrupt = LocalStateCache(table = table)
            assertNull(coldCorrupt.restore(channelA, keyA, 1L))
            assertNull(table.get("$channelA:1"))
            coldCorrupt.close()
        } finally {
            cache.close()
        }
    }

    @Test
    fun peekHitAfterSaveAndForgetChannelWipesMemoryAndStorage() = runTest {
        val table = InMemoryKeyValueTable()
        val cache = LocalStateCache(table = table)
        val channelA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        val channelB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        val keyA = key(1)
        val keyB = key(2)

        try {
            cache.save(channelA, keyA, 1L, sampleStatus("PC-A1", 1L), null, nowMillis = 100L)
            cache.save(channelA, keyA, 2L, sampleStatus("PC-A2", 2L), null, nowMillis = 200L)
            cache.save(channelB, keyB, 1L, sampleStatus("PC-B1", 1L), null, nowMillis = 300L)

            assertEquals("PC-A1", cache.peek(channelA, 1L)?.status?.device)
            assertEquals("PC-A2", cache.peek(channelA, 2L)?.status?.device)
            assertEquals("PC-B1", cache.peek(channelB, 1L)?.status?.device)
            assertEquals(3, table.size())

            cache.forgetChannel(channelA)

            assertNull(cache.peek(channelA, 1L))
            assertNull(cache.peek(channelA, 2L))
            assertNull(table.get("$channelA:1"))
            assertNull(table.get("$channelA:2"))

            // Other channels remain intact.
            assertEquals("PC-B1", cache.peek(channelB, 1L)?.status?.device)
            assertEquals(1, table.size())
        } finally {
            cache.close()
        }
    }

    @Test
    fun trimToMaxRowsEvictsOldestEntriesLruOrder() = runTest {
        val table = InMemoryKeyValueTable()
        val cache = LocalStateCache(table = table, maxRows = 3, maxMemoryEntries = 2)
        val channel = "cccccccccccccccccccccccccccccccc"
        val channelKey = key(42)

        try {
            cache.save(channel, channelKey, 1L, sampleStatus("W1", 1L), null, nowMillis = 100L)
            cache.save(channel, channelKey, 2L, sampleStatus("W2", 2L), null, nowMillis = 200L)
            cache.save(channel, channelKey, 3L, sampleStatus("W3", 3L), null, nowMillis = 300L)
            cache.save(channel, channelKey, 4L, sampleStatus("W4", 4L), null, nowMillis = 400L)

            assertEquals(3, table.size())
            assertNull(table.get("$channel:1"))
            assertNotNull(table.get("$channel:2"))
            assertNotNull(table.get("$channel:3"))
            assertNotNull(table.get("$channel:4"))
        } finally {
            cache.close()
        }
    }

    @Test
    fun closeDefersTableCloseUntilInFlightIoCompletes() = runTest {
        val delegate = InMemoryKeyValueTable()
        val putStarted = CountDownLatch(1)
        val allowPutToFinish = CountDownLatch(1)
        val closeCount = AtomicInteger(0)
        val blockingTable = object : KeyValueTable {
            override fun get(key: String): String? = delegate.get(key)
            override fun put(key: String, value: String, updatedAt: Long) {
                putStarted.countDown()
                allowPutToFinish.await(5, TimeUnit.SECONDS)
                delegate.put(key, value, updatedAt)
            }
            override fun delete(key: String) = delegate.delete(key)
            override fun deletePrefix(prefix: String) = delegate.deletePrefix(prefix)
            override fun trimToMaxRows(maxRows: Int) = delegate.trimToMaxRows(maxRows)
            override fun close() {
                closeCount.incrementAndGet()
                delegate.close()
            }
        }

        val cache = LocalStateCache(table = blockingTable, ioDispatcher = Dispatchers.IO)
        val saveJob = launch(Dispatchers.IO) {
            cache.save("dddddddddddddddddddddddddddddddd", key(7), 1L, sampleStatus("PC", 1L), null, nowMillis = 100L)
        }

        assertTrue(putStarted.await(5, TimeUnit.SECONDS))
        cache.close()
        assertEquals(0, closeCount.get())

        allowPutToFinish.countDown()
        saveJob.join()
        assertEquals(1, closeCount.get())

        // Subsequent close() is a no-op and does not double-close.
        cache.close()
        assertEquals(1, closeCount.get())
    }
}
