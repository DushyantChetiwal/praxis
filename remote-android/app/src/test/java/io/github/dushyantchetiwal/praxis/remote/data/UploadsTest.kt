package io.github.dushyantchetiwal.praxis.remote.data

import java.io.ByteArrayInputStream
import java.util.Collections
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class UploadsTest {
    @Test
    fun readsUpToTheLimitAndNoMore() {
        assertArrayEquals(byteArrayOf(1, 2, 3), readLimited(ByteArrayInputStream(byteArrayOf(1, 2, 3)), 3))
        assertThrows(UploadTooLargeException::class.java) {
            readLimited(ByteArrayInputStream(ByteArray(4)), 3)
        }
    }

    @Test
    fun statusAdvertisesTheUploadLimit() {
        val status = parseStatus(
            JSONObject("""{"device":"pc","windows":[],"capabilities":{"open_folder":true,"file_upload":{"max_bytes":26214400}}}"""),
        )
        assertEquals(26214400L, status.uploadLimit)
        assertNull(parseStatus(JSONObject("""{"device":"pc","windows":[]}""")).uploadLimit)
    }

    @Test
    fun uploadsInPiecesAndReturnsTheSavedPath() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val file = PickedFile("00000000-0000-4000-8000-000000000001", "notes.txt", ByteArray(50_000) { it.toByte() })
        val received = java.io.ByteArrayOutputStream()
        val operations = mutableListOf<String>()
        val progress = mutableListOf<Long>()
        val path = uploadFile(file, JSONObject().put("attachment", true), { progress += it }) { op, args ->
            operations += op
            when (op) {
                "file_begin" -> {
                    assertEquals("notes.txt", args.getString("name"))
                    assertEquals(true, args.getBoolean("attachment"))
                    JSONObject().put("upload_id", id).put("chunk_bytes", 24 * 1024).put("next_offset", 0)
                }
                "file_chunk" -> {
                    assertEquals(received.size(), args.getInt("offset"))
                    received.write(RemoteCrypto.unbase64(args.getString("data")))
                    JSONObject().put("next_offset", received.size())
                }
                "file_finish" -> JSONObject().put("upload_id", id).put("path", "project/.praxis/uploads/notes.txt")
                else -> error("unexpected $op")
            }
        }
        assertEquals("project/.praxis/uploads/notes.txt", path)
        assertEquals(listOf("file_begin", "file_chunk", "file_chunk", "file_chunk", "file_finish"), operations)
        assertArrayEquals(file.bytes, received.toByteArray())
        assertEquals(50_000L, progress.last())
    }

    @Test
    fun aFinishedUploadIsNotSentAgain() = runTest {
        val file = PickedFile("00000000-0000-4000-8000-000000000002", "a.bin", byteArrayOf(1))
        val operations = mutableListOf<String>()
        val path = uploadFile(file, JSONObject(), {}) { op, _ ->
            operations += op
            JSONObject().put("upload_id", "0123456789abcdef0123456789abcdef").put("chunk_bytes", 1).put("path", "project/a.bin")
        }
        assertEquals("project/a.bin", path)
        assertEquals(listOf("file_begin"), operations)
    }

    @Test
    fun pipelinesChunksConcurrentlyWithOutOfOrderCompletion() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val chunkSize = 1024
        val totalSize = chunkSize * 6 + 300
        val file = PickedFile(
            "00000000-0000-4000-8000-000000000003",
            "large.bin",
            ByteArray(totalSize) { (it * 31).toByte() },
        )
        val received = ByteArray(totalSize)
        val inFlight = AtomicInteger(0)
        val maxInFlight = AtomicInteger(0)
        val completedOffsets = Collections.synchronizedList(mutableListOf<Int>())
        val progress = Collections.synchronizedList(mutableListOf<Long>())

        val path = uploadFile(
            file,
            JSONObject().put("attachment", true),
            { progress += it },
            preferredChunkBytes = LIVE_UPLOAD_CHUNK_BYTES,
            maxParallel = MAX_PARALLEL_UPLOAD_CHUNKS,
        ) { op, args ->
            when (op) {
                "file_begin" -> {
                    assertEquals(LIVE_UPLOAD_CHUNK_BYTES, args.getInt("preferred_chunk_bytes"))
                    JSONObject()
                        .put("upload_id", id)
                        .put("chunk_bytes", chunkSize)
                        .put("max_parallel", 4)
                        .put("next_offset", 0)
                }
                "file_chunk" -> {
                    val active = inFlight.incrementAndGet()
                    maxInFlight.accumulateAndGet(active, ::maxOf)
                    try {
                        val offset = args.getInt("offset")
                        // Delay earlier offsets longer so later chunks finish first.
                        delay(if (offset == 0) 50L else 10L)
                        val data = RemoteCrypto.unbase64(args.getString("data"))
                        System.arraycopy(data, 0, received, offset, data.size)
                        completedOffsets += offset
                        JSONObject().put("received_bytes", data.size)
                    } finally {
                        inFlight.decrementAndGet()
                    }
                }
                "file_finish" -> JSONObject().put("upload_id", id).put("path", "project/.praxis/uploads/large.bin")
                else -> error("unexpected $op")
            }
        }

        assertEquals("project/.praxis/uploads/large.bin", path)
        assertTrue("expected >1 concurrent in-flight chunk, got ${maxInFlight.get()}", maxInFlight.get() > 1)
        assertTrue("expected <=4 concurrent in-flight chunks, got ${maxInFlight.get()}", maxInFlight.get() <= 4)
        assertTrue("expected out-of-order completion, got $completedOffsets", completedOffsets.first() != 0)
        assertArrayEquals(file.bytes, received)
        assertEquals(totalSize.toLong(), progress.last())
        assertTrue(
            "expected monotonic progress, got $progress",
            progress.zipWithNext().all { (prev, next) -> next >= prev },
        )
    }

    @Test
    fun fallsBackToSequentialWhenServerOmitsMaxParallel() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val chunkSize = 24 * 1024
        val totalSize = chunkSize * 2 + 512
        val file = PickedFile(
            "00000000-0000-4000-8000-000000000004",
            "legacy.bin",
            ByteArray(totalSize) { it.toByte() },
        )
        val received = java.io.ByteArrayOutputStream()
        val inFlight = AtomicInteger(0)
        val maxInFlight = AtomicInteger(0)

        val path = uploadFile(file, JSONObject(), {}) { op, args ->
            when (op) {
                "file_begin" -> JSONObject()
                    .put("upload_id", id)
                    .put("chunk_bytes", chunkSize)
                    .put("next_offset", 0)
                "file_chunk" -> {
                    val active = inFlight.incrementAndGet()
                    maxInFlight.accumulateAndGet(active, ::maxOf)
                    try {
                        delay(10L)
                        assertEquals(received.size(), args.getInt("offset"))
                        received.write(RemoteCrypto.unbase64(args.getString("data")))
                        JSONObject().put("next_offset", received.size())
                    } finally {
                        inFlight.decrementAndGet()
                    }
                }
                "file_finish" -> JSONObject().put("upload_id", id).put("path", "project/legacy.bin")
                else -> error("unexpected $op")
            }
        }

        assertEquals("project/legacy.bin", path)
        assertEquals(1, maxInFlight.get())
        assertArrayEquals(file.bytes, received.toByteArray())
    }

    @Test
    fun negotiates44KiBChunksOverLiveChannel() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val totalSize = LIVE_UPLOAD_CHUNK_BYTES + 128
        val file = PickedFile(
            "00000000-0000-4000-8000-000000000005",
            "live.bin",
            ByteArray(totalSize) { (it + 7).toByte() },
        )
        val received = ByteArray(totalSize)
        val chunkSizes = Collections.synchronizedList(mutableListOf<Int>())

        val path = uploadFile(
            file,
            JSONObject(),
            {},
            preferredChunkBytes = LIVE_UPLOAD_CHUNK_BYTES,
            maxParallel = MAX_PARALLEL_UPLOAD_CHUNKS,
        ) { op, args ->
            when (op) {
                "file_begin" -> {
                    assertEquals(LIVE_UPLOAD_CHUNK_BYTES, args.getInt("preferred_chunk_bytes"))
                    JSONObject()
                        .put("upload_id", id)
                        .put("chunk_bytes", LIVE_UPLOAD_CHUNK_BYTES)
                        .put("max_parallel", 4)
                        .put("next_offset", 0)
                }
                "file_chunk" -> {
                    val offset = args.getInt("offset")
                    val data = RemoteCrypto.unbase64(args.getString("data"))
                    chunkSizes += data.size
                    System.arraycopy(data, 0, received, offset, data.size)
                    JSONObject().put("received_bytes", data.size)
                }
                "file_finish" -> JSONObject().put("upload_id", id).put("path", "project/live.bin")
                else -> error("unexpected $op")
            }
        }

        assertEquals("project/live.bin", path)
        assertEquals(setOf(LIVE_UPLOAD_CHUNK_BYTES, 128), chunkSizes.toSet())
        assertArrayEquals(file.bytes, received)
    }
}
