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
    fun snapshotGapsSnapToCurrentVisibleTailAndPageBackwardFromCurrentState() {
        var history = TranscriptHistory().live(view()).begin("root", 1)
        history = history.complete(history.request!!, page(history.request!!))
        assertFalse(history.replacesDisjointRoot(view(start = 20, total = 25)))
        val touching = history.live(view(start = 20, total = 25))
        assertEquals((0 until 25).toList(), touching.thread!!.entries.map { it.index })

        val inFlight = TranscriptHistory().live(view()).begin("root", 99)
        val jumped = view(start = 30, total = 40)
        assertTrue(history.replacesDisjointRoot(jumped))
        assertTrue(inFlight.replacesDisjointRoot(jumped))
        assertNull(inFlight.live(jumped).request)

        history = history.live(jumped)
        assertEquals((30 until 40).toList(), history.thread!!.entries.map { it.index })
        assertEquals(30, history.records.getValue("root").nextBefore)

        history = history.begin("root", 2)
        history = history.complete(history.request!!, page(history.request!!, start = 15, total = 40))
        assertEquals((15 until 40).toList(), history.thread!!.entries.map { it.index })
        assertEquals(15, history.records.getValue("root").nextBefore)

        history = history.begin("root", 3)
        history = history.complete(history.request!!, page(history.request!!, start = 0, total = 40))
        assertEquals((0 until 40).toList(), history.thread!!.entries.map { it.index })
        assertEquals(0, history.records.getValue("root").nextBefore)
    }

    @Test
    fun trimToLatestKeepsOnlyVisibleTailAndPagesBackwardCleanly() {
        val full = view(start = 0, total = 60).copy(
            stepThreads = listOf(view("done-step", start = 0, total = 40)),
        )
        var history = TranscriptHistory().live(full)
        // Mark done-step idle/non-live by omitting it from a subsequent live update.
        history = history.live(view(start = 45, total = 60))
        assertEquals((0 until 60).toList(), history.thread!!.entries.map { it.index })

        val trimmed = history.trimToLatest(maxEntries = 24)
        assertEquals((36 until 60).toList(), trimmed.thread!!.entries.map { it.index })
        assertEquals(36, trimmed.records.getValue("root").nextBefore)
        assertTrue(trimmed.canLoad("root"))
        // Non-live completed step history keeps its entries since no live snapshot will resend its tail.
        assertEquals(40, trimmed.thread!!.stepThreads.single().entries.size)

        val loading = trimmed.begin("root", 1)
        assertEquals(36, loading.request!!.before)
        val paged = loading.complete(loading.request!!, page(loading.request!!, start = 12, total = 60))
        assertEquals((12 until 60).toList(), paged.thread!!.entries.map { it.index })
        assertEquals(12, paged.records.getValue("root").nextBefore)
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
    fun budgetEmptyStepSnapshotPreservesHistoryAndLeavesNewEntriesFetchable() {
        val incoming = view().copy(stepThreads = listOf(view("step")))
        var history = TranscriptHistory().live(incoming).begin("step", 1)
        history = history.complete(history.request!!, page(history.request!!))

        val bounded = view("step", start = 25, total = 25)
        history = history.live(incoming.copy(stepThreads = listOf(bounded)))
        assertEquals((0 until 20).toList(), history.thread!!.stepThreads.single().entries.map { it.index })
        assertEquals(25, history.records.getValue("step").nextBefore)
        assertTrue(history.canLoad("step"))

        history = history.begin("step", 2)
        history = history.complete(history.request!!, page(history.request!!, start = 18, total = 25))
        assertEquals((0 until 25).toList(), history.thread!!.stepThreads.single().entries.map { it.index })
        assertEquals(0, history.records.getValue("step").nextBefore)
        assertEquals(10, history.thread!!.entries.size)
    }

    @Test
    fun completedStepHistoryStaysVisibleIdleAndPageable() {
        val step = view("step").let { original ->
            original.copy(entries = original.entries.map { entry ->
                if (entry.index == 10) entry.copy(parts = listOf(EntryPart(0, "reasoning", "Provider thought"))) else entry
            })
        }
        val incoming = view().copy(stepThreads = listOf(step, view("parallel")))
        var history = TranscriptHistory().live(incoming).begin("step", 1)
        history = history.complete(history.request!!, page(history.request!!))
        history = history.live(incoming.copy(stepThreads = listOf(view("step", start = 19), view("parallel"))))
        assertEquals(20, history.thread!!.stepThreads.first().entries.size)
        assertEquals(10, history.thread!!.entries.size)
        assertEquals(10, history.thread!!.stepThreads.last().entries.size)
        history = history.begin("parallel", 2)
        val request = history.request!!
        val ended = history.live(view())
        assertEquals(listOf("step", "parallel"), ended.shownSteps)
        assertEquals(request, ended.request)
        assertTrue(ended.thread!!.stepThreads.all { it.status == "idle" })
        assertEquals("Provider thought", ended.thread!!.stepThreads.first().entries.single { it.index == 10 }.parts.single().text)
        assertTrue(ended.shownSteps.all { !ended.records.getValue(it).live })
        val loaded = ended.complete(request, page(request))
        assertEquals(20, loaded.thread!!.stepThreads.last().entries.size)
        assertFalse(loaded.records.getValue("parallel").live)
        val returned = loaded.live(incoming)
        assertEquals(20, returned.thread!!.stepThreads.first().entries.size)
        assertTrue(returned.records.getValue("step").live)
        val switched = ended.live(view("other-root"))
        assertTrue(switched.shownSteps.isEmpty())
        assertEquals(switched, switched.complete(request, page(request)))
    }

    @Test
    fun snapshotPreviewsAreRevisitedWithoutOverwritingNewerLiveVersions() {
        val incoming = parseThreadView(JSONObject("""{
            "session_id":"root","total":20,"next_before":20,"has_more":true,
            "entries":[{"index":19,"role":"assistant","text":"preview…","truncated":true,"truncation":"snapshot_budget"}]
        }"""))
        val preview = incoming.entries.single()
        assertTrue(preview.snapshotPreview)
        val started = TranscriptHistory().live(incoming).begin("root", 1)
        val request = started.request!!
        assertEquals(20, request.before)
        val restored = started.complete(request, page(request, start = 10))
        assertEquals("older 19", restored.thread!!.entries.last().text)
        assertFalse(restored.thread!!.entries.last().truncated)
        assertEquals(10, restored.records.getValue("root").nextBefore)

        val changed = started.live(incoming.copy(entries = listOf(preview.copy(text = "newer preview…"))))
        val delayed = changed.complete(request, page(request, start = 10))
        assertEquals("newer preview…", delayed.thread!!.entries.last().text)
        assertEquals(20, delayed.records.getValue("root").nextBefore)
        assertTrue(delayed.canLoad("root"))
    }

    @Test
    fun invalidJsonPagingNumbersFailRetryablyInsteadOfCompletingHistory() {
        val started = TranscriptHistory().live(view()).begin("root", 1)
        val request = started.request!!
        val base = """{"session_id":"root","total":20,"before_index":10,"next_before":0,"has_more":false,"entries":[]}"""
        val badNumbers = listOf(
            "-0.5", "0.5", "10.5", "20.5", "-1", "2147483648", "4294967296",
            "4294967306", "4294967316", "1e100", "1.0", "true", "\"0\"", "null",
        )
        for (field in listOf("next_before", "before_index", "total")) {
            for (bad in badNumbers) {
                val json = JSONObject(base)
                json.put(field, JSONObject("""{"value":$bad}""").get("value"))
                val parsed = parseThreadView(json)
                val failed = started.complete(request, parsed)
                assertNotNull("$field=$bad", failed.records.getValue("root").failure)
                assertEquals(10, failed.records.getValue("root").nextBefore)
                assertTrue(failed.canLoad("root", retry = true))
            }
        }
        val invalidIndex = JSONObject(base).put("entries", org.json.JSONArray("""[{"index":-0.5,"role":"assistant","text":"bad"}]"""))
        assertNotNull(started.complete(request, parseThreadView(invalidIndex)).records.getValue("root").failure)
        val invalidPartIndex = JSONObject(base).put("entries", org.json.JSONArray("""[
            {"index":0,"role":"assistant","text":"","parts":[{"index":-0.5,"role":"reasoning","text":"thought"}]}
        ]"""))
        assertNotNull(started.complete(request, parseThreadView(invalidPartIndex)).records.getValue("root").failure)
        val valid = started.complete(request, parseThreadView(JSONObject(base)))
        assertNull(valid.records.getValue("root").failure)
        assertEquals(0, valid.records.getValue("root").nextBefore)
    }

    @Test
    fun strictJsonIndicesAcceptRangeEndpointsAndRejectInvalidLiveSnapshots() {
        assertEquals(0, JSONObject("""{"index":0}""").index("index"))
        assertEquals(Int.MAX_VALUE, JSONObject("""{"index":2147483647}""").index("index"))
        assertNull(JSONObject("""{"index":2147483648}""").index("index"))
        val history = TranscriptHistory().live(view())
        val malformed = parseThreadView(JSONObject("""{
            "session_id":"root","total":-0.5,"next_before":0,"has_more":false,"entries":[]
        }"""))
        assertFalse(malformed.pagingValid)
        assertEquals(history, history.live(malformed))
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

    @Test
    fun applyDeltaUpdatesAndAppendsAndRejectsMissingBaseline() {
        val base = TranscriptHistory().live(view(start = 10, total = 12))
        val delta = parseThreadDelta(
            """
            {
              "session_id": "root",
              "title": "Updated title",
              "thread_status": "idle",
              "total": 13,
              "next_before": 10,
              "has_more": true,
              "upserts": [
                {"index": 11, "role": "assistant", "text": "message 11 updated"},
                {"index": 12, "role": "assistant", "text": "message 12"}
              ]
            }
            """.trimIndent(),
        )
        assertNotNull(delta)
        val updated = base.applyDelta(delta!!)
        assertNotNull(updated)
        val thread = updated!!.thread!!
        assertEquals("Updated title", thread.title)
        assertEquals("idle", thread.status)
        assertEquals(13, thread.total)
        assertEquals(10, updated.records.getValue("root").nextBefore)
        assertEquals("message 11 updated", thread.entries.first { it.index == 11 }.text)
        assertEquals("message 12", thread.entries.last().text)

        assertNull(TranscriptHistory().applyDelta(delta))
        assertNull(base.applyDelta(delta.copy(sessionId = "other")))
    }

    @Test
    fun applyDeltaDisjointJumpReturnsNullToForceSnapshotResync() {
        val base = TranscriptHistory().live(view(start = 0, total = 5))
        val disjoint = ThreadDelta(
            updatedAtRaw = null,
            updatedAt = null,
            watch = null,
            status = null,
            sessionId = "root",
            title = null,
            threadStatus = "generating",
            total = 20,
            nextBefore = 15,
            hasMore = true,
            upserts = (15 until 20).map { Entry(it, "assistant", "tail $it", null) },
            stepThreads = null,
        )
        assertNull(base.applyDelta(disjoint))
        assertNull(base.applyDelta(disjoint.copy(upserts = emptyList())))
        assertEquals((0 until 5).toList(), base.thread!!.entries.map { it.index })
    }

    @Test
    fun relayCursorAcceptsSequentialDeltasDetectsGapsAndRecoversViaSnapshot() {
        val cursor = RelayCursor()
        val epoch = "0123456789abcdef0123456789abcdef"
        assertTrue(cursor.handshake(epoch))

        assertTrue(cursor.accept(JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":1}""")))
        assertEquals(
            RelayCursor.DeltaDecision.Accept,
            cursor.acceptDelta(JSONObject("""{"type":"delta","epoch":"$epoch","sequence":2,"base_sequence":1}""")),
        )
        assertEquals(
            RelayCursor.DeltaDecision.Ignore,
            cursor.acceptDelta(JSONObject("""{"type":"delta","epoch":"$epoch","sequence":2,"base_sequence":1}""")),
        )
        assertEquals(
            RelayCursor.DeltaDecision.Ignore,
            cursor.acceptDelta(JSONObject("""{"type":"delta","epoch":"deadbeef","sequence":3,"base_sequence":2}""")),
        )
        assertEquals(
            RelayCursor.DeltaDecision.Gap,
            cursor.acceptDelta(JSONObject("""{"type":"delta","epoch":"$epoch","sequence":4,"base_sequence":3}""")),
        )
        assertTrue(cursor.accept(JSONObject("""{"type":"snapshot","epoch":"$epoch","sequence":5}""")))
        assertEquals(
            RelayCursor.DeltaDecision.Accept,
            cursor.acceptDelta(JSONObject("""{"type":"delta","epoch":"$epoch","sequence":6,"base_sequence":5}""")),
        )
    }
}
