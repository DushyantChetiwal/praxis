package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.os.SystemClock
import io.github.dushyantchetiwal.praxis.remote.R
import java.io.IOException
import kotlinx.coroutines.delay
import okhttp3.FormBody
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject

/**
 * GitHub's device flow for a GitHub App, which needs only the client ID: the
 * user enters a short code at github.com while the app polls for the token.
 * Tokens from the device flow are refreshed with the client ID alone too.
 */
class DeviceFlow(context: Context, private val http: OkHttpClient) {
    private val context = context.applicationContext

    data class Code(
        val deviceCode: String,
        val userCode: String,
        val verificationUri: String,
        val expiresInSeconds: Int,
        val intervalSeconds: Int,
    )

    data class Token(val accessToken: String, val refreshToken: String?, val expiresInSeconds: Long?)

    class FlowException(message: String, val transient: Boolean = false) : Exception(message)

    suspend fun start(clientId: String): Code {
        val json = post(DEVICE_CODE_URL, FormBody.Builder().add("client_id", clientId).build())
        json.optString("error").takeIf { it.isNotEmpty() }?.let { throw FlowException(describe(it, json)) }
        val deviceCode = json.optString("device_code")
        val userCode = json.optString("user_code")
        if (deviceCode.isEmpty() || userCode.isEmpty()) {
            throw FlowException(context.getString(R.string.flow_error, json.toString().take(200)))
        }
        return Code(
            deviceCode = deviceCode,
            userCode = userCode,
            verificationUri = json.optString("verification_uri").ifEmpty { "https://github.com/login/device" },
            expiresInSeconds = json.optInt("expires_in", 900),
            intervalSeconds = json.optInt("interval", 5).coerceAtLeast(1),
        )
    }

    /** Polls until the user authorizes (or refuses, or the code expires). */
    suspend fun awaitToken(clientId: String, code: Code): Token {
        var interval = code.intervalSeconds
        val deadline = SystemClock.elapsedRealtime() + code.expiresInSeconds * 1000L
        while (true) {
            delay(interval * 1000L)
            if (SystemClock.elapsedRealtime() > deadline) throw FlowException(context.getString(R.string.flow_expired))
            val form = FormBody.Builder()
                .add("client_id", clientId)
                .add("device_code", code.deviceCode)
                .add("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
                .build()
            val json = try {
                post(ACCESS_TOKEN_URL, form)
            } catch (e: FlowException) {
                // A dropped connection is retried until the code expires.
                if (e.transient) continue
                throw e
            }
            val accessToken = json.optString("access_token")
            if (accessToken.isNotEmpty()) {
                return Token(
                    accessToken = accessToken,
                    refreshToken = json.optString("refresh_token").takeIf { it.isNotEmpty() },
                    expiresInSeconds = json.optLong("expires_in", 0L).takeIf { it > 0L },
                )
            }
            when (val error = json.optString("error")) {
                "authorization_pending" -> Unit
                "slow_down" -> interval = maxOf(interval + 5, json.optInt("interval", 0))
                else -> throw FlowException(describe(error, json))
            }
        }
    }

    /**
     * Trades a refresh token for a new access token. A [FlowException] that is
     * `transient` means GitHub could not be reached; any other means the
     * refresh token is no longer accepted.
     */
    suspend fun refresh(clientId: String, refreshToken: String): Token {
        val form = FormBody.Builder()
            .add("client_id", clientId)
            .add("grant_type", "refresh_token")
            .add("refresh_token", refreshToken)
            .build()
        val json = post(ACCESS_TOKEN_URL, form)
        val accessToken = json.optString("access_token")
        if (accessToken.isEmpty()) throw FlowException(describe(json.optString("error"), json))
        return Token(
            accessToken = accessToken,
            refreshToken = json.optString("refresh_token").takeIf { it.isNotEmpty() },
            expiresInSeconds = json.optLong("expires_in", 0L).takeIf { it > 0L },
        )
    }

    private fun describe(error: String, json: JSONObject): String = when (error) {
        "expired_token" -> context.getString(R.string.flow_expired)
        "access_denied" -> context.getString(R.string.flow_denied)
        "device_flow_disabled" -> context.getString(R.string.flow_disabled)
        "incorrect_client_credentials", "unauthorized_client" -> context.getString(R.string.flow_bad_client)
        else -> context.getString(
            R.string.flow_error,
            json.optString("error_description").ifEmpty { error.ifEmpty { json.toString().take(200) } },
        )
    }

    private suspend fun post(url: String, form: FormBody): JSONObject {
        val request = Request.Builder()
            .url(url)
            .header("Accept", "application/json")
            .header("User-Agent", GitHubClient.USER_AGENT)
            .post(form)
            .build()
        val response = try {
            http.fetch(request)
        } catch (e: IOException) {
            throw FlowException(context.getString(R.string.error_network), transient = true)
        }
        if (response.code >= 500) {
            throw FlowException(context.getString(R.string.flow_unexpected, response.code), transient = true)
        }
        val json = response.body?.let { runCatching { JSONObject(it) }.getOrNull() }
        if (json == null) {
            // A bad client ID gets a 404 page rather than JSON.
            if (response.code == 404) throw FlowException(context.getString(R.string.flow_bad_client))
            throw FlowException(context.getString(R.string.flow_unexpected, response.code))
        }
        if (response.code !in 200..299 && !json.has("error")) {
            if (response.code == 404) throw FlowException(context.getString(R.string.flow_bad_client))
            throw FlowException(context.getString(R.string.flow_unexpected, response.code))
        }
        return json
    }

    private companion object {
        const val DEVICE_CODE_URL = "https://github.com/login/device/code"
        const val ACCESS_TOKEN_URL = "https://github.com/login/oauth/access_token"
    }
}
