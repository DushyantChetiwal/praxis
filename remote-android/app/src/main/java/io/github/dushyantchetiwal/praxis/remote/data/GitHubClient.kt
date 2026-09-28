package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.text.format.DateFormat
import io.github.dushyantchetiwal.praxis.remote.R
import java.io.IOException
import java.util.Date
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import okhttp3.Call
import okhttp3.Callback
import okhttp3.Headers
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import org.json.JSONArray
import org.json.JSONObject

enum class ErrorKind { Network, Auth, RateLimit, Forbidden, NotFound, Http, Timeout, Praxis, State, Cancelled, Unpaired }

class ApiException(
    val kind: ErrorKind,
    message: String,
    val status: Int = 0,
    /** When a rate limit lifts, in epoch milliseconds. */
    val resetAt: Long? = null,
) : Exception(message)

/** A response read in full on a background thread. */
class RawResponse(val code: Int, val headers: Headers, val body: String?)

/** Runs a call without blocking the caller, reading the body on OkHttp's thread. */
suspend fun OkHttpClient.fetch(request: Request): RawResponse = withContext(Dispatchers.IO) {
    suspendCancellableCoroutine { continuation ->
        val call = newCall(request)
        continuation.invokeOnCancellation { call.cancel() }
        call.enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) {
                continuation.resumeWithException(e)
            }

            override fun onResponse(call: Call, response: Response) {
                val result = try {
                    response.use { RawResponse(it.code, it.headers, it.body?.string()) }
                } catch (e: IOException) {
                    continuation.resumeWithException(e)
                    return
                }
                continuation.resume(result)
            }
        })
    }
}

/**
 * The GitHub REST API, with conditional requests: a GET made with
 * `conditional = true` sends the last ETag for that path and, on
 * `304 Not Modified` (which does not count against the rate limit), returns
 * the cached body with `changed = false`. A rejected token is refreshed and
 * the call retried once before giving up.
 */
class GitHubClient(
    context: Context,
    val http: OkHttpClient,
    private val tokens: TokenManager,
    /** Called when GitHub rejects the token and it cannot be refreshed. */
    private val onUnauthorized: () -> Unit,
) {
    private val context = context.applicationContext

    class Reply(val status: Int, val changed: Boolean, val body: String?) {
        fun obj(): JSONObject? = body?.let { runCatching { JSONObject(it) }.getOrNull() }
        fun array(): JSONArray? = body?.let { runCatching { JSONArray(it) }.getOrNull() }
    }

    private class Cached(val etag: String, val body: String?)

    private val etags = ConcurrentHashMap<String, Cached>()

    suspend fun call(
        method: String,
        path: String,
        body: JSONObject? = null,
        conditional: Boolean = false,
        allow: Set<Int> = emptySet(),
        authenticated: Boolean = true,
    ): Reply {
        var bearer = if (authenticated) {
            tokens.valid() ?: throw ApiException(ErrorKind.Auth, context.getString(R.string.error_not_signed_in))
        } else {
            null
        }
        val cached = if (conditional) etags[path] else null
        fun build(token: String?): Request = Request.Builder()
            .url(API_BASE + path)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .apply {
                if (token != null) header("Authorization", "Bearer $token")
                if (cached != null) header("If-None-Match", cached.etag)
                val requestBody: RequestBody? = when {
                    body != null -> body.toString().toRequestBody(JSON)
                    method == "POST" || method == "PATCH" || method == "PUT" -> ByteArray(0).toRequestBody(null)
                    else -> null
                }
                method(method, requestBody)
            }
            .build()

        var response = send(build(bearer))
        if (response.code == 401 && bearer != null) {
            when (val outcome = tokens.afterUnauthorized(bearer)) {
                is TokenManager.Outcome.Fresh -> {
                    bearer = outcome.token
                    response = send(build(bearer))
                }
                TokenManager.Outcome.Unavailable ->
                    throw ApiException(ErrorKind.Network, context.getString(R.string.error_network))
                TokenManager.Outcome.Failed -> Unit // Reported as a 401 below, which signs out.
            }
        }

        if (response.code == 304) return Reply(304, false, cached?.body)
        if (response.code in allow) return Reply(response.code, true, null)
        if (response.code !in 200..299) throw toError(response, bearer)

        val text = response.body?.takeIf { it.isNotEmpty() }
        if (conditional) {
            val etag = response.headers["ETag"]
            if (etag != null) etags[path] = Cached(etag, text) else etags.remove(path)
        }
        return Reply(response.code, true, text)
    }

    private suspend fun send(request: Request): RawResponse = try {
        http.fetch(request)
    } catch (e: IOException) {
        throw ApiException(ErrorKind.Network, context.getString(R.string.error_network))
    }

    /**
     * Downloads a gist file from its `raw_url`, for content the API truncated,
     * sending the token since the gist is secret.
     */
    suspend fun fetchRaw(url: String): String {
        val bearer = tokens.valid()
        val request = Request.Builder()
            .url(url)
            .header("User-Agent", USER_AGENT)
            .apply { if (bearer != null) header("Authorization", "Bearer $bearer") }
            .build()
        val response = send(request)
        if (response.code !in 200..299) throw toError(response, bearer)
        return response.body.orEmpty()
    }

    /** Downloads a small public file, such as an avatar; null on any failure. */
    suspend fun fetchBytes(url: String): ByteArray? = withContext(Dispatchers.IO) {
        val request = Request.Builder().url(url).header("User-Agent", USER_AGENT).build()
        try {
            http.newCall(request).execute().use { response ->
                if (response.isSuccessful) response.body?.bytes() else null
            }
        } catch (e: IOException) {
            null
        }
    }

    fun forget(path: String) {
        etags.remove(path)
    }

    fun clearCache() {
        etags.clear()
    }

    private fun toError(response: RawResponse, bearer: String?): ApiException {
        val message = response.body
            ?.let { runCatching { JSONObject(it).optString("message") }.getOrNull() }
            .orEmpty()
        val detail = if (message.isNotEmpty()) ": $message" else ""
        val remaining = response.headers["x-ratelimit-remaining"]
        val reset = response.headers["x-ratelimit-reset"]?.toLongOrNull() ?: 0L
        val retryAfter = response.headers["retry-after"]?.toLongOrNull() ?: 0L
        val resetAt = when {
            retryAfter > 0 -> System.currentTimeMillis() + retryAfter * 1000
            reset > 0 -> reset * 1000
            else -> null
        }
        val code = response.code
        return when {
            code == 401 -> {
                if (bearer != null && bearer == tokens.current()) onUnauthorized()
                ApiException(ErrorKind.Auth, context.getString(R.string.error_unauthorized), 401)
            }
            code == 429 || (code == 403 && (remaining == "0" || message.contains("rate limit", ignoreCase = true))) -> {
                val text = if (resetAt != null) {
                    context.getString(R.string.error_rate_limit_until, DateFormat.getTimeFormat(context).format(Date(resetAt)))
                } else {
                    context.getString(R.string.error_rate_limit)
                }
                ApiException(ErrorKind.RateLimit, text, code, resetAt)
            }
            code == 403 -> ApiException(ErrorKind.Forbidden, context.getString(R.string.error_forbidden, detail), 403)
            code == 404 -> ApiException(ErrorKind.NotFound, context.getString(R.string.error_not_found), 404)
            else -> ApiException(ErrorKind.Http, context.getString(R.string.error_http, code, detail), code)
        }
    }

    companion object {
        const val API_BASE = "https://api.github.com"
        const val USER_AGENT = "PraxisRemote-Android"
        private val JSON = "application/json; charset=utf-8".toMediaType()

        fun newHttpClient(): OkHttpClient = OkHttpClient.Builder()
            .connectTimeout(15, TimeUnit.SECONDS)
            .readTimeout(30, TimeUnit.SECONDS)
            .writeTimeout(30, TimeUnit.SECONDS)
            .callTimeout(60, TimeUnit.SECONDS)
            .build()
    }
}
