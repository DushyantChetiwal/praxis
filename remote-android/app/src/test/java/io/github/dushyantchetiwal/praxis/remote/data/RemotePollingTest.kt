package io.github.dushyantchetiwal.praxis.remote.data

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.currentTime
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class RemotePollingTest {
    @Test
    fun pendingRepliesStopAtTheDeadline() = runTest {
        var polls = 0
        val reply = awaitRemoteReply<String> {
            polls++
            null
        }
        assertNull(reply)
        assertEquals(45_000L, currentTime)
        assertTrue(polls in 1..30)
    }

    @Test
    fun aStalledHttpPollCannotHoldTheRequestQueuePastTheDeadline() = runTest {
        val queue = Mutex()
        val stalled = async { queue.withLock { awaitRemoteReply<String> { awaitCancellation() } } }
        runCurrent()
        val next = async { queue.withLock { "next request" } }
        assertNull(stalled.await())
        assertEquals("next request", next.await())
        assertEquals(45_000L, currentTime)
    }

    @Test
    fun temporaryTransportFailuresRecoverWithoutRepostingTheRequest() = runTest {
        var polls = 0
        val reply = awaitRemoteReply {
            when (++polls) {
                1 -> throw ApiException(ErrorKind.Network, "GitHub is unreachable")
                2 -> throw ApiException(ErrorKind.Http, "Gateway unavailable", 502)
                3 -> null
                else -> "response"
            }
        }
        assertEquals("response", reply)
        assertEquals(4, polls)
        assertEquals(6_000L, currentTime)
    }

    @Test
    fun persistentNetworkFailuresAreNotReportedAsAPraxisDisconnection() = runTest {
        val error = ApiException(ErrorKind.Network, "GitHub is unreachable")
        try {
            awaitRemoteReply<String> { throw error }
            fail("expected the transport error")
        } catch (actual: ApiException) {
            assertSame(error, actual)
        }
        assertEquals(45_000L, currentTime)
    }

    @Test
    fun successfulPendingPollsClearOldTransportFailures() = runTest {
        var polls = 0
        val reply = awaitRemoteReply<String> {
            if (++polls == 1) throw ApiException(ErrorKind.Network, "Temporary outage")
            null
        }
        assertNull(reply)
        assertEquals(45_000L, currentTime)
    }

    @Test
    fun rateLimitsAndExternalCancellationAreNotSwallowed() = runTest {
        val error = ApiException(ErrorKind.RateLimit, "Wait for GitHub", 429)
        try {
            awaitRemoteReply<String> { throw error }
            fail("expected rate-limit feedback")
        } catch (actual: ApiException) {
            assertSame(error, actual)
        }
        val request = async { awaitRemoteReply<String> { awaitCancellation() } }
        runCurrent()
        advanceTimeBy(2_000L)
        request.cancelAndJoin()
        assertTrue(request.isCancelled)
    }
}
