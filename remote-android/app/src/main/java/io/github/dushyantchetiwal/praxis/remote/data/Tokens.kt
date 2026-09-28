package io.github.dushyantchetiwal.praxis.remote.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/**
 * Hands out the GitHub access token, refreshing it shortly before it expires
 * or when GitHub rejects it. GitHub App user tokens last eight hours; the
 * refresh token from the device flow renews them with the client ID alone.
 * Only one refresh runs at a time, and callers that waited on it reuse its
 * result, since a refresh token can be used only once.
 */
class TokenManager(
    private val store: Store,
    private val flow: DeviceFlow,
    /** The client ID the tokens were issued to. */
    private val clientId: () -> String,
    /** Called with the new expiry after every successful refresh. */
    private val onRefreshed: (expiresAt: Long?) -> Unit,
) {
    sealed interface Outcome {
        data class Fresh(val token: String) : Outcome
        /** GitHub could not be reached; the token may still be fine. */
        data object Unavailable : Outcome
        /** The refresh token was refused (or there is none): sign in again. */
        data object Failed : Outcome
    }

    private val mutex = Mutex()

    /** The access token whose refresh was refused, so it is not retried on every call. */
    @Volatile
    private var refusedFor: String? = null

    fun current(): String? = store.token

    /** A token to use now, refreshed first when it is about to expire. */
    suspend fun valid(): String? {
        val token = store.token ?: return null
        val expiresAt = store.tokenExpiresAt ?: return token
        if (store.refreshToken == null || refusedFor == token) return token
        if (expiresAt - System.currentTimeMillis() > REFRESH_MARGIN_MS) return token
        return (refresh(token) as? Outcome.Fresh)?.token ?: token
    }

    /** What to do after GitHub answered 401 to [rejected]. */
    suspend fun afterUnauthorized(rejected: String): Outcome = refresh(rejected)

    private suspend fun refresh(seen: String): Outcome = mutex.withLock {
        val current = store.token ?: return@withLock Outcome.Failed
        // Someone else refreshed while this caller waited.
        if (current != seen) return@withLock Outcome.Fresh(current)
        val refreshToken = store.refreshToken ?: return@withLock Outcome.Failed
        if (refusedFor == seen) return@withLock Outcome.Failed
        try {
            val token = flow.refresh(clientId(), refreshToken)
            val expiresAt = token.expiresInSeconds?.let { System.currentTimeMillis() + it * 1000L }
            withContext(Dispatchers.IO) {
                store.saveToken(token.accessToken, token.refreshToken ?: refreshToken, expiresAt)
            }
            onRefreshed(expiresAt)
            Outcome.Fresh(token.accessToken)
        } catch (e: DeviceFlow.FlowException) {
            if (e.transient) {
                Outcome.Unavailable
            } else {
                refusedFor = seen
                Outcome.Failed
            }
        }
    }

    private companion object {
        const val REFRESH_MARGIN_MS = 5 * 60_000L
    }
}
