package io.github.dushyantchetiwal.praxis.remote.data

import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NostrChannelTest {
    private val epoch = "0123456789abcdef0123456789abcdef"

    @Test
    fun relayCursorValidateAndCommitRequireExplicitCommitToAdvance() {
        val cursor = RelayCursor()
        assertTrue(cursor.handshake(epoch))

        val snap1 = JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":1}""")
        assertEquals(1L, cursor.validateSnapshot(snap1))
        // Sequence has not advanced until commit() is called.
        assertEquals(1L, cursor.validateSnapshot(snap1))
        assertTrue(cursor.commit(snap1))
        assertNull(cursor.validateSnapshot(snap1))

        val delta2 = JSONObject("""{"type":"delta","epoch":"$epoch","sequence":2,"base_sequence":1}""")
        assertEquals(RelayCursor.DeltaDecision.Accept, cursor.validateDelta(delta2))
        // Still at sequence 1 until committed.
        assertEquals(RelayCursor.DeltaDecision.Accept, cursor.validateDelta(delta2))
        assertTrue(cursor.commit(delta2))
        assertEquals(RelayCursor.DeltaDecision.Ignore, cursor.validateDelta(delta2))
    }

    @Test
    fun malformedSnapshotDoesNotAdvanceCursor() = runTest {
        val cursor = RelayCursor()
        assertTrue(cursor.handshake(epoch))
        val snapshots = mutableListOf<LiveSnapshot>()

        val badSnapshot = JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":1}""")
        receiveLivePacket(
            packet = badSnapshot,
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = {},
            onGap = {},
        )
        assertTrue(snapshots.isEmpty())

        val goodSnapshot = JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":1,"snapshot":{}}""")
        receiveLivePacket(
            packet = goodSnapshot,
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = {},
            onGap = {},
        )
        assertEquals(1, snapshots.size)
    }

    @Test
    fun malformedDeltaDoesNotAdvanceCursorSoNextDeltaTriggersGap() = runTest {
        val cursor = RelayCursor()
        assertTrue(cursor.handshake(epoch))
        val snapshots = mutableListOf<LiveSnapshot>()
        val deltas = mutableListOf<LiveDelta>()
        var gapCount = 0

        receiveLivePacket(
            packet = JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":1,"snapshot":{}}"""),
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = { deltas += it },
            onGap = { gapCount++ },
        )
        assertEquals(1, snapshots.size)

        // Malformed delta at sequence 2 (missing/invalid delta object) must not advance sequence.
        receiveLivePacket(
            packet = JSONObject("""{"type":"delta","epoch":"$epoch","sequence":2,"base_sequence":1}"""),
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = { deltas += it },
            onGap = { gapCount++ },
        )
        assertTrue(deltas.isEmpty())
        assertEquals(0, gapCount)

        // Next delta (sequence 3, base_sequence 2) sees cursor still at 1 and triggers LiveGap.
        receiveLivePacket(
            packet = JSONObject(
                """{"type":"delta","epoch":"$epoch","sequence":3,"base_sequence":2,"delta":{"session_id":"root","total":1,"next_before":0,"has_more":false,"upserts":[]}}""",
            ),
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = { deltas += it },
            onGap = { gapCount++ },
        )
        assertTrue(deltas.isEmpty())
        assertEquals(1, gapCount)

        // Valid delta from base_sequence 1 is still accepted.
        receiveLivePacket(
            packet = JSONObject(
                """{"type":"delta","epoch":"$epoch","sequence":4,"base_sequence":1,"delta":{"session_id":"root","total":1,"next_before":0,"has_more":false,"upserts":[]}}""",
            ),
            cursor = cursor,
            channel = "ch",
            phoneId = "ph",
            onResponse = { _, _ -> },
            onSnapshot = { snapshots += it },
            onDelta = { deltas += it },
            onGap = { gapCount++ },
        )
        assertEquals(1, deltas.size)
        assertNotNull(deltas.single().delta)
        assertFalse(gapCount > 1)
    }
}
