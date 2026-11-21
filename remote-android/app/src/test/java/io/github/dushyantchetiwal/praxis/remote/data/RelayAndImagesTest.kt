package io.github.dushyantchetiwal.praxis.remote.data

import androidx.compose.ui.graphics.ImageBitmap
import java.lang.reflect.Proxy
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class RelayAndImagesTest {
    @Test fun liveSnapshotsRejectDuplicatesReorderingAndUnverifiedRestarts() {
        val cursor = RelayCursor()
        val first = "a".repeat(32)
        val second = "b".repeat(32)
        fun packet(epoch: String, sequence: Long) = JSONObject().put("epoch", epoch).put("sequence", sequence)
        assertFalse(cursor.accept(packet(first, 1)))
        assertFalse(cursor.handshake("not-an-epoch"))
        assertTrue(cursor.handshake(first))
        assertTrue(cursor.accept(packet(first, 2)))
        assertFalse(cursor.accept(packet(first, 2)))
        assertFalse(cursor.accept(packet(first, 1)))
        assertFalse(cursor.accept(packet(second, 3)))
        assertTrue(cursor.handshake(second))
        assertTrue(cursor.accept(packet(second, 1)))
        assertFalse(cursor.accept(packet(first, 4)))
    }

    @Test fun imageDecodingAndChunkReceiptsAreBounded() {
        assertEquals(1, imageSampleSize(800, 600))
        assertEquals(4, imageSampleSize(8000, 6000))
        assertThrows(IllegalArgumentException::class.java) { imageSampleSize(0, 300) }
        assertThrows(IllegalArgumentException::class.java) { imageSampleSize(100000, 100000) }
        assertEquals(12, nextImageOffset(JSONObject().put("next_offset", 12), 12))
        assertThrows(IllegalArgumentException::class.java) { nextImageOffset(JSONObject().put("next_offset", 11), 12) }
        assertThrows(IllegalArgumentException::class.java) { nextImageOffset(JSONObject().put("next_offset", 13), 12) }
        assertThrows(IllegalArgumentException::class.java) { nextImageOffset(JSONObject().put("next_offset", 12.5), 12) }
    }

    @Test fun identitiesAreBoundToThePairingAndDirection() {
        val link = Link("fixture", "a".repeat(32), "b".repeat(32), ByteArray(32) { 7 }, "Fixture")
        assertArrayEquals(relaySecret(link, "phone"), relaySecret(link, "phone"))
        assertFalse(relaySecret(link, "phone").contentEquals(link.key))
        assertFalse(relaySecret(link, "phone").contentEquals(relaySecret(link, "desktop")))
        val other = Link("fixture", "a".repeat(32), "c".repeat(32), link.key, "Fixture")
        assertFalse(relaySecret(link, "phone").contentEquals(relaySecret(other, "phone")))
    }

    @Test fun capturedRelayPeersCannotMatchAnotherComputerOrPairing() {
        val selected = Link("gist", "a".repeat(32), "b".repeat(32), ByteArray(32) { 7 }, "Computer")
        fun peer(channel: String = selected.channel, phoneId: String = selected.phoneId, key: ByteArray = selected.key.copyOf()) =
            Link("another-gist", channel, phoneId, key, "Renamed")
        assertTrue(sameRelayPeer(selected, peer()))
        assertFalse(sameRelayPeer(selected, peer(channel = "c".repeat(32))))
        assertFalse(sameRelayPeer(selected, peer(phoneId = "d".repeat(32))))
        assertFalse(sameRelayPeer(selected, peer(key = ByteArray(32) { 8 })))
    }

    @Test fun unavailableLiveTransportDoesNotSubmitACommand() = kotlinx.coroutines.test.runTest {
        val transport = NostrChannel()
        try {
            val link = Link("fixture", "a".repeat(32), "b".repeat(32), ByteArray(32) { 7 }, "Fixture")
            assertFalse(transport.ready(link))
            assertNull(transport.exchange(link, "request", "prompt", JSONObject().put("text", "fixture")))
        } finally { transport.close() }
    }

    @Test fun theLastQuestionAnswerModeWinsWithoutErasingTheDraft() {
        val form = QuestionForm("Which?", listOf(QuestionOption("a", "A", null), QuestionOption("b", "B", null)), false, true)
        val draft = "Use a different option"
        val custom = questionAnswerContent(form, setOf("a"), draft, freeformActive = true)!!
        assertEquals(draft, custom.getString("freeform_answer"))
        assertFalse(custom.has("answer"))
        val choice = questionAnswerContent(form, setOf("a"), draft, freeformActive = false)!!
        assertEquals("a", choice.getString("answer"))
        assertFalse(choice.has("freeform_answer"))
        assertNull(questionAnswerContent(form, setOf("a"), "", freeformActive = true))
    }

    @Test fun legacyDesktopsDoNotAdvertiseNewCapabilities() {
        val status = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{"session_id":"s","queued":0,"pending":[]}}]}"""))
        assertFalse(status.windows.single().thread!!.imageInput)
        val modern = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{"session_id":"s","queued":0,"pending":[],"image_input":true}}]}"""))
        assertTrue(modern.windows.single().thread!!.imageInput)
    }

    @Test fun uploadImagePipelinesChunksConcurrentlyWith44KiBChunks() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val totalSize = LIVE_UPLOAD_CHUNK_BYTES * 2 + 1024
        val bytes = ByteArray(totalSize) { (it * 13).toByte() }
        val preview = Proxy.newProxyInstance(
            ImageBitmap::class.java.classLoader,
            arrayOf(ImageBitmap::class.java),
        ) { _, _, _ -> error("unused") } as ImageBitmap
        val image = PromptImage("00000000-0000-4000-8000-000000000010", bytes, preview)
        val received = ByteArray(totalSize)
        val inFlight = AtomicInteger(0)
        val maxInFlight = AtomicInteger(0)

        val uploadedId = uploadImage(
            image,
            JSONObject().put("session_id", "s"),
            preferredChunkBytes = LIVE_UPLOAD_CHUNK_BYTES,
            maxParallel = MAX_PARALLEL_UPLOAD_CHUNKS,
        ) { op, args ->
            when (op) {
                "image_begin" -> {
                    assertEquals(LIVE_UPLOAD_CHUNK_BYTES, args.getInt("preferred_chunk_bytes"))
                    JSONObject()
                        .put("upload_id", id)
                        .put("chunk_bytes", LIVE_UPLOAD_CHUNK_BYTES)
                        .put("max_parallel", 4)
                        .put("next_offset", 0)
                }
                "image_chunk" -> {
                    val active = inFlight.incrementAndGet()
                    maxInFlight.accumulateAndGet(active, ::maxOf)
                    try {
                        delay(20L)
                        val offset = args.getInt("offset")
                        val data = RemoteCrypto.unbase64(args.getString("data"))
                        System.arraycopy(data, 0, received, offset, data.size)
                        JSONObject().put("received_bytes", data.size)
                    } finally {
                        inFlight.decrementAndGet()
                    }
                }
                "image_finish" -> JSONObject().put("upload_id", id).put("ready", true)
                else -> error("unexpected $op")
            }
        }

        assertEquals(id, uploadedId)
        assertTrue("expected >1 concurrent in-flight image chunk, got ${maxInFlight.get()}", maxInFlight.get() > 1)
        assertArrayEquals(bytes, received)
    }
}
