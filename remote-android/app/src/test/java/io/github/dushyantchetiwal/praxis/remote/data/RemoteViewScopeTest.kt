package io.github.dushyantchetiwal.praxis.remote.data

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RemoteViewScopeTest {
    @Test
    fun lateNavigationSuccessAndFailureCannotMutateAnotherView() = runBlocking {
        val original = RemoteViewScope(1, "device:1", 1)
        for (destination in listOf(
            RemoteViewScope(2, "other-device:1", 2),
            RemoteViewScope(1, "device:2", 2),
            RemoteViewScope(1, "device:1", 3), // switch away and back
        )) {
            for (fail in listOf(false, true)) {
                var current = original
                var history = "new-view-history"
                var errors = 0
                var polls = 0
                val reply = CompletableDeferred<Unit>()
                val pending = async(start = CoroutineStart.UNDISPATCHED) {
                    val ok = viewScopedAction(
                        original, { current },
                        operation = {
                            reply.await()
                            if (fail) throw ApiException(ErrorKind.Praxis, "late failure")
                        },
                        onSuccess = { polls++ },
                        onFailure = { errors++ },
                    )
                    // Both openThread and startNewThread recheck before clearing
                    // busy flags, history, watch targets, or selecting a tab.
                    if (original == current && ok) history = "cleared"
                    ok
                }
                current = destination
                reply.complete(Unit)
                assertFalse(pending.await())
                assertEquals("new-view-history", history)
                assertEquals(0, polls)
                assertEquals(0, errors)
            }
        }
    }

    @Test
    fun currentViewActionsStillReportSuccessAndFailure() = runBlocking {
        val scope = RemoteViewScope(1, "device:1", 1)
        var successes = 0
        var failures = 0
        assertTrue(viewScopedAction(scope, { scope }, {}, { successes++ }, { failures++ }))
        assertFalse(viewScopedAction(
            scope, { scope },
            { throw ApiException(ErrorKind.Praxis, "expected failure") },
            { successes++ }, { failures++ },
        ))
        assertEquals(1, successes)
        assertEquals(1, failures)
    }

    @Test
    fun queuedNavigationIsNotSentAfterSwitchingViews() = runBlocking {
        val original = RemoteViewScope(1, "device:1", 1)
        val current = RemoteViewScope(1, "device:2", 2)
        var sent = false
        assertFalse(viewScopedAction(original, { current }, { sent = true }, {}, {}))
        assertFalse(sent)
    }
}
