package io.github.dushyantchetiwal.praxis.remote.data

const val LIVE_TAIL_ENTRIES = 24

/** Source-index coverage includes blank entries which have no rendered row. */
data class HistoryRange(val start: Int, val end: Int)
data class HistoryFailure(val message: String? = null)
data class HistoryRequest(val id: Long, val session: String, val before: Int, val previews: Map<Int, Entry> = emptyMap())

data class HistoryRecord(
    val view: ThreadView,
    val ranges: List<HistoryRange> = emptyList(),
    val failure: HistoryFailure? = null,
    val live: Boolean = true,
) {
    val nextBefore: Int?
        get() {
            if (view.nextBefore == null) return null
            var before = view.total
            for (range in ranges.asReversed()) {
                if (range.end < before) break
                before = minOf(before, range.start)
            }
            val previewBefore = view.entries.filter { it.snapshotPreview }.maxOfOrNull { it.index + 1 } ?: 0
            return maxOf(before, previewBefore)
        }
}

/** Lives only for the selected device/window/root session, never in persistent storage. */
data class TranscriptHistory(
    val rootSession: String? = null,
    val shownSteps: List<String> = emptyList(),
    val records: Map<String, HistoryRecord> = emptyMap(),
    val request: HistoryRequest? = null,
) {
    val thread: ThreadView?
        get() = records[rootSession]?.view?.copy(stepThreads = shownSteps.mapNotNull { records[it]?.view })

    fun isShown(session: String): Boolean = session == rootSession || session in shownSteps

    fun canLoad(session: String, retry: Boolean = false): Boolean {
        val record = records[session] ?: return false
        return request == null && isShown(session) && (record.nextBefore ?: 0) > 0 &&
            (retry || record.failure == null)
    }

    fun replacesDisjointRoot(incoming: ThreadView): Boolean {
        if (!incoming.pagingValid) return false
        val session = incoming.sessionId ?: return false
        if (rootSession != session) return false
        val old = records[session]?.takeIf { it.view.total <= incoming.total } ?: return false
        val start = incoming.nextBefore?.takeIf { it in 0..incoming.total } ?: return false
        return incoming.entries.isNotEmpty() && start > (old.ranges.lastOrNull()?.end ?: 0)
    }

    fun trimToLatest(maxEntries: Int = LIVE_TAIL_ENTRIES): TranscriptHistory {
        if (maxEntries <= 0 || records.isEmpty()) return copy(request = null)
        val trimmed = records.mapValues { (_, record) ->
            if (!record.live || record.view.nextBefore == null || record.view.entries.size <= maxEntries) {
                record
            } else {
                val kept = record.view.entries.takeLast(maxEntries)
                val tailStart = kept.first().index
                val ranges = record.ranges.mapNotNull { range ->
                    if (range.end <= tailStart) null else HistoryRange(maxOf(range.start, tailStart), range.end)
                }
                record.copy(
                    view = record.view.copy(entries = kept, nextBefore = tailStart, hasMore = tailStart > 0),
                    ranges = ranges,
                    failure = null,
                )
            }
        }
        return copy(records = trimmed, request = null)
    }

    fun live(incoming: ThreadView): TranscriptHistory {
        if (!incoming.pagingValid) return this
        val session = incoming.sessionId ?: return TranscriptHistory()
        val previous = if (rootSession == session && incoming.total >= (thread?.total ?: 0)) this else TranscriptHistory()
        val records = previous.records.toMutableMap()
        var request = previous.request
        val liveSteps = incoming.stepThreads.filter { it.pagingValid }
        val liveIds = liveSteps.mapNotNull { it.sessionId }.toSet()
        for (id in previous.shownSteps.filter { it !in liveIds }) {
            records[id]?.let { records[id] = it.copy(view = it.view.copy(status = "idle"), live = false) }
        }
        for (view in listOf(incoming) + liveSteps) {
            val id = view.sessionId ?: continue
            val old = records[id]?.takeIf { it.view.total <= view.total }
            val range = view.nextBefore?.takeIf { it in 0..view.total }?.let { HistoryRange(it, view.total) }
            // When new activity outpaces the live tail while the phone was away,
            // drop the stale prefix rather than leaving an unmarked hole above the
            // current state; earlier entries load backward from the new tail on scroll.
            val disjointJump = old != null && range != null && view.entries.isNotEmpty() &&
                range.start > (old.ranges.lastOrNull()?.end ?: 0)
            val retained = old?.takeUnless { disjointJump }
            if (retained == null && request?.session == id) request = null
            val updated = HistoryRecord(
                view = view.copy(
                    entries = mergeEntries(retained?.view?.entries.orEmpty(), view.entries, view.total),
                    stepThreads = emptyList(),
                ),
                ranges = mergeRanges(retained?.ranges.orEmpty() + listOfNotNull(range)),
                failure = retained?.failure,
            )
            val next = updated.nextBefore
            val progressed = next != null && retained?.nextBefore?.let { next < it } == true
            records[id] = if (next == null || next == 0 || progressed) updated.copy(failure = null) else updated
        }
        val shownSteps = (previous.shownSteps + liveSteps.mapNotNull { it.sessionId }).filter { it != session }.distinct()
        return TranscriptHistory(session, shownSteps, records, request)
    }

    fun applyDelta(delta: ThreadDelta): TranscriptHistory? {
        if (rootSession != delta.sessionId) return null
        val old = records[delta.sessionId] ?: return null
        if (delta.total < old.view.total) return null
        if (delta.nextBefore !in 0..delta.total) return null
        if (delta.hasMore != (delta.nextBefore > 0)) return null

        if (delta.nextBefore > (old.ranges.lastOrNull()?.end ?: 0)) return null

        val range = HistoryRange(delta.nextBefore, delta.total)
        var nextRequest = request

        val updatedRoot = HistoryRecord(
            view = old.view.copy(
                title = delta.title ?: old.view.title,
                status = delta.threadStatus ?: old.view.status,
                total = delta.total,
                entries = mergeEntries(old.view.entries, delta.upserts, delta.total),
                nextBefore = delta.nextBefore,
                hasMore = delta.hasMore,
                pagingValid = true,
                stepThreads = emptyList(),
            ),
            ranges = mergeRanges(old.ranges + range),
            failure = old.failure,
        )
        val nextBeforeAfter = updatedRoot.nextBefore
        val progressed = nextBeforeAfter != null && old.nextBefore?.let { nextBeforeAfter < it } == true
        val finalRoot = if (nextBeforeAfter == null || nextBeforeAfter == 0 || progressed) {
            updatedRoot.copy(failure = null)
        } else {
            updatedRoot
        }

        val nextRecords = records.toMutableMap()
        nextRecords[delta.sessionId] = finalRoot
        var nextShownSteps = shownSteps
        if (delta.stepThreads != null) {
            val liveSteps = delta.stepThreads.filter { it.pagingValid }
            val liveIds = liveSteps.mapNotNull { it.sessionId }.toSet()
            for (id in shownSteps.filter { it !in liveIds }) {
                nextRecords[id]?.let { nextRecords[id] = it.copy(view = it.view.copy(status = "idle"), live = false) }
            }
            for (view in liveSteps) {
                val id = view.sessionId ?: continue
                val oldStep = nextRecords[id]?.takeIf { it.view.total <= view.total }
                val stepRange = view.nextBefore?.takeIf { it in 0..view.total }?.let { HistoryRange(it, view.total) }
                val stepDisjoint = oldStep != null && stepRange != null && view.entries.isNotEmpty() &&
                    stepRange.start > (oldStep.ranges.lastOrNull()?.end ?: 0)
                val stepRetained = oldStep?.takeUnless { stepDisjoint }
                if (stepRetained == null && nextRequest?.session == id) nextRequest = null
                val updatedStep = HistoryRecord(
                    view = view.copy(
                        entries = mergeEntries(stepRetained?.view?.entries.orEmpty(), view.entries, view.total),
                        stepThreads = emptyList(),
                    ),
                    ranges = mergeRanges(stepRetained?.ranges.orEmpty() + listOfNotNull(stepRange)),
                    failure = stepRetained?.failure,
                )
                val stepNext = updatedStep.nextBefore
                val stepProgressed = stepNext != null && stepRetained?.nextBefore?.let { stepNext < it } == true
                nextRecords[id] = if (stepNext == null || stepNext == 0 || stepProgressed) {
                    updatedStep.copy(failure = null)
                } else {
                    updatedStep
                }
            }
            nextShownSteps = (shownSteps + liveSteps.mapNotNull { it.sessionId })
                .filter { it != delta.sessionId }
                .distinct()
        }
        return TranscriptHistory(delta.sessionId, nextShownSteps, nextRecords, nextRequest)
    }

    fun begin(session: String, id: Long, retry: Boolean = false): TranscriptHistory {
        if (!canLoad(session, retry)) return this
        val record = records.getValue(session)
        val before = record.nextBefore ?: return this
        return copy(
            request = HistoryRequest(id, session, before, record.view.entries.filter { it.snapshotPreview }.associateBy { it.index }),
            records = records + (session to record.copy(failure = null)),
        )
    }

    fun failed(expected: HistoryRequest, message: String? = null): TranscriptHistory {
        if (request != expected) return this
        val record = records[expected.session] ?: return copy(request = null)
        return copy(request = null, records = records + (expected.session to record.copy(failure = HistoryFailure(message))))
    }

    fun complete(expected: HistoryRequest, page: ThreadView): TranscriptHistory {
        if (request != expected) return this
        val record = records[expected.session] ?: return copy(request = null)
        val next = page.nextBefore
        val end = minOf(expected.before, page.total)
        if (!page.pagingValid || page.sessionId != expected.session || page.beforeIndex != expected.before || page.total < expected.before ||
            next == null || next !in 0 until expected.before || next > end ||
            page.hasMore != (next > 0) || page.entries.any { it.index !in next until end }
        ) return failed(expected)

        // Replace a snapshot preview only if it has not changed since this
        // request started. A newer live version must still win the overlap.
        val existing = record.view.entries.associateBy { it.index }
        val replacements = page.entries.filter { entry ->
            val current = existing[entry.index]
            !record.live || current == null ||
                (current.snapshotPreview && current == expected.previews[entry.index] && !entry.snapshotPreview)
        }
        val total = if (record.live) record.view.total else maxOf(record.view.total, page.total)
        val updated = record.copy(
            view = record.view.copy(total = total, entries = mergeEntries(record.view.entries, replacements, total)),
            ranges = mergeRanges(record.ranges + HistoryRange(next, end)),
            failure = null,
        )
        return copy(request = null, records = records + (expected.session to updated))
    }
}

private fun mergeEntries(older: List<Entry>, newer: List<Entry>, total: Int): List<Entry> =
    (older + newer).filter { it.index in 0 until total }.associateBy { it.index }.values.sortedBy { it.index }

private fun mergeRanges(ranges: List<HistoryRange>): List<HistoryRange> {
    val merged = mutableListOf<HistoryRange>()
    for (range in ranges.filter { it.start <= it.end }.sortedBy { it.start }) {
        val last = merged.lastOrNull()
        if (last != null && range.start <= last.end) {
            merged[merged.lastIndex] = HistoryRange(last.start, maxOf(last.end, range.end))
        } else {
            merged += range
        }
    }
    return merged
}

fun historyItemKey(session: String?) = "history:$session"
fun transcriptEntryKey(session: String?, index: Int) = "entry:$session:$index"
fun stepItemKey(session: String?) = "step:$session"

/** Kept identical to the lazy-list order so prepends can restore a visible row, not its loading header. */
fun transcriptItemKeys(thread: ThreadView?): List<String> = buildList {
    if (thread == null) return@buildList
    add(historyItemKey(thread.sessionId))
    thread.entries.forEach { add(transcriptEntryKey(thread.sessionId, it.index)) }
    thread.stepThreads.forEach { step ->
        add(stepItemKey(step.sessionId))
        add(historyItemKey(step.sessionId))
        step.entries.forEach { add(transcriptEntryKey(step.sessionId, it.index)) }
    }
}

data class HistoryScrollAnchor(val requestId: Long, val key: String, val offset: Int) {
    fun indexIn(keys: List<String>): Int? = keys.indexOf(key).takeIf { it >= 0 }
}

fun followLatestAfterScroll(
    following: Boolean,
    userScrolling: Boolean,
    movingUp: Boolean,
    atBottom: Boolean,
    loadingHistory: Boolean,
): Boolean = when {
    !userScrolling -> following
    movingUp -> false
    atBottom && !loadingHistory -> true
    else -> following
}

/** Network completions and layout changes cannot trigger another page without a new drag. */
class HistoryScrollGate {
    private var previous: Pair<Int, Int>? = null
    private var used = false
    private var wasDragging = false
    private var gestureActive = false
    var userScrolling: Boolean = false
        private set
    var movingUp: Boolean = false
        private set

    fun update(
        dragging: Boolean,
        index: Int,
        offset: Int,
        canLoadVisibleHeader: Boolean,
        scrolling: Boolean = dragging,
    ): Boolean {
        if (dragging && !wasDragging) {
            gestureActive = true
            used = false
        }
        wasDragging = dragging
        if (!dragging && !scrolling) gestureActive = false
        userScrolling = gestureActive && (dragging || scrolling)
        val old = previous
        movingUp = old != null && (index < old.first || (index == old.first && offset < old.second))
        previous = index to offset
        if (!userScrolling || !movingUp || used || !canLoadVisibleHeader) return false
        used = true
        return true
    }
}
