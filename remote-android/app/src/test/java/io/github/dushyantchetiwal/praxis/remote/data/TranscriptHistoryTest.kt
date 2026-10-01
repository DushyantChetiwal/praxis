package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TranscriptHistoryTest {
    private fun view(session: String = "root", start: Int = 10, total: Int = 20) = ThreadView(
        session, session, "generating", total,
        (start until total).map { Entry(it, "assistant", "message $it", null) },
        nextBefore = start, hasMore = start > 0,
    )

    private fun page(request: HistoryRequest, start: Int = 0, total: Int = 20) = view(request.session, start, total).copy(
        beforeIndex = request.before,
        entries = (start until minOf(request.before, total)).map { Entry(it, "assistant", "older $it", null) },
    )

    @Test
    fun pagingMetadataParsesAndOldDesktopsDoNotAutoPage() {
        val parsed = parseThreadView(JSONObject("""{
            "session_id":"root","total":40,"before_index":20,"next_before":7,"has_more":true,"entries":[]
        }"""))
        assertEquals(20, parsed.beforeIndex)
        assertEquals(7, parsed.nextBefore)
        assertEquals(true, parsed.hasMore)
        val legacy = parseThreadView(JSONObject("""{"session_id":"root","total":40,"entries":[]}"""))
        assertNull(legacy.nextBefore)
        assertFalse(TranscriptHistory().live(legacy).canLoad("root"))
    }

    @Test
    fun prependsSurviveSnapshotsAndNewestIndicesReplaceEvenWithShorterText() {
        var history = TranscriptHistory().live(view()).begin("root", 1)
        val request = history.request!!
        history = history.complete(request, page(request))
        val shorter = view(start = 18, total = 21).copy(entries = listOf(
            Entry(18, "assistant", "short…", null, listOf(EntryPart(3, "reasoning", "live"))),
            Entry(19, "tool", "Now completed", "completed"),
            Entry(20, "assistant", "New message", null),
        ))
        history = history.live(shorter)
        assertEquals((0..20).toList(), history.thread!!.entries.map { it.index })
        assertEquals("older 0", history.thread!!.entries.first().text)
        assertEquals("short…", history.thread!!.entries[18].text)
        assertEquals("reasoning", history.thread!!.entries[18].parts.single().role)
        assertEquals("completed", history.thread!!.entries[19].status)
        assertEquals(0, history.records.getValue("root").nextBefore)
        assertFalse(history.canLoad("root"))
    }

    @Test
    fun liveSnapshotWinsOverDelayedPageAndDuplicateIndicesStayUnique() {
        var history = TranscriptHistory().live(view()).begin("root", 2)
        val request = history.request!!
        val live = view(start = 5).copy(entries = listOf(Entry(7, "assistant", "new live value", null)))
        history = history.live(live)
        val older = page(request).copy(entries = page(request).entries + Entry(7, "assistant", "duplicate old", null))
        history = history.complete(request, older)
        assertEquals(1, history.thread!!.entries.count { it.index == 7 })
        assertEquals("new live value", history.thread!!.entries.single { it.index == 7 }.text)
    }

    @Test
    fun snapshotGapsArePagedBeforeContinuingOlderHistory() {
        var history = TranscriptHistory().live(view()).begin("root", 1)
        history = history.complete(history.request!!, page(history.request!!))
        history = history.live(view(start = 30, total = 40))
        assertEquals(30, history.records.getValue("root").nextBefore)
        history = history.begin("root", 2)
        history = history.complete(history.request!!, page(history.request!!, start = 15, total = 40))
        assertEquals((0 until 40).toList(), history.thread!!.entries.map { it.index })
        assertEquals(0, history.records.getValue("root").nextBefore)
    }

    @Test
    fun emptyPagesCanAdvanceAcrossBlankSourceEntries() {
        var history = TranscriptHistory().live(view(start = 100, total = 101)).begin("root", 1)
        val request = history.request!!
        history = history.complete(request, page(request, start = 50, total = 101).copy(entries = emptyList()))
        assertEquals(50, history.records.getValue("root").nextBefore)
        assertNull(history.records.getValue("root").failure)
        assertEquals(listOf(100), history.thread!!.entries.map { it.index })
    }

    @Test
    fun noProgressOrMalformedReplyRequiresExplicitRetry() {
        val started = TranscriptHistory().live(view()).begin("root", 1)
        val request = started.request!!
        for (bad in listOf(
            page(request).copy(nextBefore = request.before),
            page(request).copy(nextBefore = null),
            page(request).copy(sessionId = "other"),
            page(request).copy(beforeIndex = 11),
            page(request).copy(hasMore = true),
            page(request, start = 0, total = 5),
            page(request).copy(entries = listOf(Entry(10, "assistant", "outside page", null))),
        )) {
            val failed = started.complete(request, bad)
            assertNotNull(failed.records.getValue("root").failure)
            assertFalse(failed.canLoad("root"))
            assertNull(failed.begin("root", 2).request)
            val retried = failed.begin("root", 3, retry = true)
            assertEquals(3L, retried.request!!.id)
            assertNull(retried.records.getValue("root").failure)
        }
    }

    @Test
    fun liveCoverageProgressClearsErrorsWithoutLeavingADeadRetryControl() {
        val started = TranscriptHistory().live(view()).begin("root", 1)
        val failed = started.failed(started.request!!, "Offline")
        val unchanged = failed.live(view(start = 15, total = 21))
        assertNotNull(unchanged.records.getValue("root").failure)
        val recovered = failed.live(view(start = 0))
        assertNull(recovered.records.getValue("root").failure)
        assertFalse(recovered.canLoad("root", retry = true))
    }

    @Test
    fun requestsDoNotOverlapAcrossRootAndStepsAndNetworkFailuresCanRetry() {
        val incoming = view().copy(stepThreads = listOf(view("step")))
        val started = TranscriptHistory().live(incoming).begin("root", 1)
        assertEquals(started, started.begin("root", 2))
        assertEquals(started, started.begin("step", 3))
        val failed = started.failed(started.request!!, "Offline")
        assertEquals("Offline", failed.records.getValue("root").failure!!.message)
        assertTrue(failed.canLoad("root", retry = true))
        assertTrue(failed.canLoad("step"))
    }

    @Test
    fun sessionAndDeviceWindowResetsRejectRepliesIncludingReturningToSameSession() {
        val started = TranscriptHistory().live(view()).begin("root", 1)
        val request = started.request!!
        val switched = started.live(view("another"))
        assertEquals(switched, switched.complete(request, page(request)))
        // Device/window changes discard TranscriptHistory even if session IDs match.
        val reset = TranscriptHistory().live(view()).begin("root", 2)
        assertEquals(reset, reset.complete(request, page(request)))
        assertEquals(reset, reset.failed(request, "Late error"))
        val shrunk = started.live(view(start = 2, total = 5))
        assertNull(shrunk.request)
        assertEquals((2 until 5).toList(), shrunk.thread!!.entries.map { it.index })
    }

    @Test
    fun stepHistoryIsIndependentCachedAndNotResurrectedByLateReplies() {
        val incoming = view().copy(stepThreads = listOf(view("step"), view("parallel")))
        var history = TranscriptHistory().live(incoming).begin("step", 1)
        history = history.complete(history.request!!, page(history.request!!))
        history = history.live(incoming.copy(stepThreads = listOf(view("step", start = 19), view("parallel"))))
        assertEquals(20, history.thread!!.stepThreads.first().entries.size)
        assertEquals(10, history.thread!!.entries.size)
        assertEquals(10, history.thread!!.stepThreads.last().entries.size)
        history = history.begin("parallel", 2)
        val request = history.request!!
        val hidden = history.live(view())
        assertTrue(hidden.thread!!.stepThreads.isEmpty())
        assertNull(hidden.request)
        assertEquals(hidden, hidden.complete(request, page(request)))
        val returned = hidden.live(incoming)
        assertEquals(20, returned.thread!!.stepThreads.first().entries.size)
    }

    @Test
    fun automaticPagingRequiresUpwardUserMotionAndOnlyOncePerDrag() {
        val gate = HistoryScrollGate()
        assertFalse(gate.update(false, 10, 0, true))
        assertFalse(gate.update(true, 9, 0, false))
        assertTrue(gate.update(true, 8, 0, true))
        assertFalse(gate.update(true, 7, 0, true))
        assertFalse(gate.update(false, 1, 0, true)) // completion/layout shift, not a drag
        assertFalse(gate.update(true, 2, 0, true)) // downward
        assertTrue(gate.update(true, 1, 0, true))
    }

    @Test
    fun userFlingCanReachHistoryButProgrammaticScrollingCannotFetch() {
        val gate = HistoryScrollGate()
        assertFalse(gate.update(false, 100, 0, false))
        assertFalse(gate.update(true, 99, 0, false))
        assertFalse(gate.update(false, 20, 0, false, scrolling = true))
        assertTrue(gate.update(false, 1, 0, true, scrolling = true))
        assertFalse(gate.update(false, 0, 0, true, scrolling = true))
        assertFalse(gate.update(false, 10, 0, false, scrolling = false))
        assertFalse(gate.update(false, 0, 0, true, scrolling = true))
    }

    @Test
    fun liveUpdatesFollowOnlyWhenAlreadyFollowingOrUserReturnsToBottom() {
        assertTrue(followLatestAfterScroll(true, false, false, false, false))
        assertFalse(followLatestAfterScroll(false, false, false, true, false))
        assertFalse(followLatestAfterScroll(true, true, true, true, false))
        assertFalse(followLatestAfterScroll(false, true, false, true, true))
        assertTrue(followLatestAfterScroll(false, true, false, true, false))
    }

    @Test
    fun visibleRowAnchorSurvivesRootAndStepPrependsWithStableKeys() {
        val initial = view().copy(stepThreads = listOf(view("step")))
        val rootAnchor = HistoryScrollAnchor(1, transcriptEntryKey("root", 12), -23)
        val stepAnchor = HistoryScrollAnchor(2, transcriptEntryKey("step", 12), -17)
        val before = transcriptItemKeys(initial)
        val after = transcriptItemKeys(view(start = 0).copy(stepThreads = listOf(view("step", start = 0))))
        assertEquals(rootAnchor.indexIn(before)!! + 10, rootAnchor.indexIn(after))
        assertEquals(stepAnchor.indexIn(before)!! + 20, stepAnchor.indexIn(after))
        assertEquals(-23, rootAnchor.offset)
        assertEquals(after.size, after.toSet().size)
        assertNull(rootAnchor.indexIn(transcriptItemKeys(view("other"))))
    }
}
