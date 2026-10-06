package io.github.dushyantchetiwal.praxis.remote.data

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
}
