package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.os.SystemClock
import io.github.dushyantchetiwal.praxis.remote.R
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import org.json.JSONObject

private const val PAIR_TIMEOUT_MS = 5 * 60_000L
const val PHONE_NAME_MAX = 60

/** How a pairing attempt ended. */
sealed interface PairResult {
    class Approved(val key: ByteArray) : PairResult
    data object Denied : PairResult
    data object Expired : PairResult
    /** The computer found something wrong, or the comment was tampered with. */
    data object Invalid : PairResult
    data object TimedOut : PairResult
    /** The pairing comment disappeared. */
    data object Removed : PairResult
}

/**
 * The phone's side of pairing: one gist comment, headed `praxis-remote/v2 pair`,
 * whose JSON grows at each step (see the protocol's Pairing section).
 */
class Pairer(
    context: Context,
    private val gh: GitHubClient,
    /** The signed-in account; only comments it wrote are trusted. */
    private val login: () -> String?,
) {
    private val context = context.applicationContext

    /**
     * Runs one pairing attempt. [onCode] receives the six-digit code once both
     * keys are known. The comment is deleted however this ends, including
     * when the caller is cancelled.
     */
    suspend fun pair(
        gistId: String,
        channel: String,
        phoneId: String,
        phoneName: String,
        onCode: (String) -> Unit,
    ): PairResult {
        val key = RemoteCrypto.newKey()
        val phoneKey = RemoteCrypto.base64(key.public)
        val commit = RemoteCrypto.base64(RemoteCrypto.commit(key.public))
        val initial = JSONObject()
            .put("phone_id", phoneId)
            .put("name", phoneName.take(PHONE_NAME_MAX))
            .put("commit", commit)
        val created = gh.call("POST", "/gists/$gistId/comments", body(initial)).obj()
        val commentId = created?.long("id")
            ?: throw ApiException(ErrorKind.Http, context.getString(R.string.error_no_comment))

        val path = "/gists/$gistId/comments/$commentId"
        val deadline = SystemClock.elapsedRealtime() + PAIR_TIMEOUT_MS
        var desktopKey: String? = null
        var secrets: PairingSecrets? = null
        var gone = false
        try {
            while (SystemClock.elapsedRealtime() < deadline) {
                delay(COMMENT_POLL_MS)
                val reply = try {
                    gh.call("GET", path, conditional = true)
                } catch (e: ApiException) {
                    when {
                        e.kind == ErrorKind.NotFound -> {
                            gone = true
                            return PairResult.Removed
                        }
                        e.kind == ErrorKind.Network || (e.kind == ErrorKind.Http && e.status >= 500) -> continue
                        else -> throw e
                    }
                }
                if (!reply.changed) continue
                val comment = reply.obj() ?: continue
                if (!comment.writtenBy(login())) return PairResult.Invalid
                val fields = parsePairComment(comment.str("body")) ?: return PairResult.Invalid
                // The phone's own fields must be exactly what it wrote.
                if (fields.str("phone_id") != phoneId || fields.str("commit") != commit) return PairResult.Invalid
                val seenDesktopKey = fields.str("desktop_key")
                if (desktopKey != null && seenDesktopKey != desktopKey) return PairResult.Invalid

                when (fields.str("status")) {
                    null -> Unit
                    "approved" -> {
                        val agreed = secrets ?: return PairResult.Invalid
                        return if (fields.str("phone_key") == phoneKey) PairResult.Approved(agreed.key) else PairResult.Invalid
                    }
                    "denied" -> return PairResult.Denied
                    "expired" -> return PairResult.Expired
                    else -> return PairResult.Invalid
                }

                if (seenDesktopKey != null && secrets == null) {
                    val derived = try {
                        RemoteCrypto.phoneSecrets(key, RemoteCrypto.unbase64(seenDesktopKey), channel, phoneId)
                    } catch (e: CryptoException) {
                        return PairResult.Invalid
                    }
                    fields.put("phone_key", phoneKey)
                    try {
                        gh.call("PATCH", path, body(fields))
                    } catch (e: ApiException) {
                        if (e.kind == ErrorKind.NotFound) {
                            gone = true
                            return PairResult.Removed
                        }
                        // Try again on the next round.
                        if (e.kind == ErrorKind.Network || (e.kind == ErrorKind.Http && e.status >= 500)) continue
                        throw e
                    }
                    gh.forget(path)
                    desktopKey = seenDesktopKey
                    secrets = derived
                    onCode(derived.code)
                }
            }
            return PairResult.TimedOut
        } finally {
            gh.forget(path)
            if (!gone) {
                withContext(NonCancellable) {
                    try {
                        gh.call("DELETE", path, allow = setOf(404))
                    } catch (_: ApiException) {
                        // Praxis deletes finished pairings after ten minutes anyway.
                    }
                }
            }
        }
    }

    private fun body(fields: JSONObject) = JSONObject().put("body", "$PAIR_HEADER\n$fields")

    private fun parsePairComment(body: String?): JSONObject? {
        val (first, rest) = splitComment(body ?: return null)
        if (first != PAIR_HEADER) return null
        return jsonObjectIn(rest)?.let { runCatching { JSONObject(it) }.getOrNull() }
    }
}
