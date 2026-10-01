package io.github.dushyantchetiwal.praxis.remote.data

/** Source-index coverage includes blank entries which have no rendered row. */
data class HistoryRange(val start: Int, val end: Int)
data class HistoryFailure(val message: String? = null)
data class HistoryRequest(val id: Long, val session: String, val before: Int)

data class HistoryRecord(
    val view: ThreadView,
    val ranges: List<HistoryRange> = emptyList(),
    val failure: HistoryFailure? = null,
) {
    val nextBefore: Int?
        get() {
            if (view.nextBefore == null) return null
            var before = view.total
            for (range in ranges.asReversed()) {
                if (range.end < before) break
                before = minOf(before, range.start)
            }
            return before
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

    fun live(incoming: ThreadView): TranscriptHistory {
        val session = incoming.sessionId ?: return TranscriptHistory()
        val previous = if (rootSession == session && incoming.total >= (thread?.total ?: 0)) this else TranscriptHistory()
        val records = previous.records.toMutableMap()
        var request = previous.request
        for (view in listOf(incoming) + incoming.stepThreads) {
            val id = view.sessionId ?: continue
            val old = records[id]?.takeIf { it.view.total <= view.total }
            if (old == null && request?.session == id) request = null
            val range = view.nextBefore?.takeIf { it in 0..view.total }?.let { HistoryRange(it, view.total) }
            val updated = HistoryRecord(
                view = view.copy(
                    entries = mergeEntries(old?.view?.entries.orEmpty(), view.entries, view.total),
                    stepThreads = emptyList(),
                ),
                ranges = mergeRanges(old?.ranges.orEmpty() + listOfNotNull(range)),
                failure = old?.failure,
            )
            val next = updated.nextBefore
            val progressed = next != null && old?.nextBefore?.let { next < it } == true
            records[id] = if (next == null || next == 0 || progressed) updated.copy(failure = null) else updated
        }
        val shownSteps = incoming.stepThreads.mapNotNull { it.sessionId }.filter { it != session }.distinct()
        if (request != null && request.session != session && request.session !in shownSteps) request = null
        return TranscriptHistory(session, shownSteps, records, request)
    }

    fun begin(session: String, id: Long, retry: Boolean = false): TranscriptHistory {
        if (!canLoad(session, retry)) return this
        val record = records.getValue(session)
        val before = record.nextBefore ?: return this
        return copy(
            request = HistoryRequest(id, session, before),
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
        if (page.sessionId != expected.session || page.beforeIndex != expected.before || page.total < expected.before ||
            next == null || next !in 0 until expected.before || next > end ||
            page.hasMore != (next > 0) || page.entries.any { it.index !in next until end }
        ) return failed(expected)

        // A live snapshot received during the request wins every overlap, even
        // when its text is shorter due to the snapshot's shared byte budget.
        val updated = record.copy(
            view = record.view.copy(entries = mergeEntries(page.entries, record.view.entries, record.view.total)),
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
