package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKeys
import java.util.concurrent.ConcurrentHashMap

/**
 * Everything the app remembers. Secrets (the GitHub tokens and the key agreed
 * with each paired computer) live in encrypted preferences; the rest (the
 * phone's id, where each computer's gist is, selections, update checks) is
 * not secret.
 */
class Store(context: Context) {
    private val appContext = context.applicationContext
    private val prefs: SharedPreferences =
        appContext.getSharedPreferences(PREFS_FILE, Context.MODE_PRIVATE)

    /** Null when the Keystore is unusable; secrets are then kept in memory only. */
    private var secure: SharedPreferences? = null
    private var secureLoaded = false

    @Volatile
    var token: String? = null
        private set

    @Volatile
    var refreshToken: String? = null
        private set

    /** Channel to 32-byte key, for every computer this phone is paired with. */
    private val keys = ConcurrentHashMap<String, ByteArray>()

    init {
        // Version 1 talked through a repository's issues; nothing of it is used now.
        if (LEGACY_KEYS.any(prefs::contains)) {
            prefs.edit().apply { LEGACY_KEYS.forEach(::remove) }.apply()
        }
    }

    /** Reads secrets from encrypted storage. Slow the first time; call off the main thread. */
    @Synchronized
    fun loadSecrets() {
        if (secureLoaded) return
        secureLoaded = true
        secure = openSecure()
        val stored = secure?.let { runCatching { it.all }.getOrNull() }.orEmpty()
        token = (stored[KEY_TOKEN] as? String)?.takeIf { it.isNotBlank() }
        refreshToken = (stored[KEY_REFRESH_TOKEN] as? String)?.takeIf { it.isNotBlank() }
        keys.clear()
        for ((name, value) in stored) {
            if (!name.startsWith(KEY_CHANNEL_KEY_PREFIX) || value !is String) continue
            val key = runCatching { RemoteCrypto.unbase64(value) }.getOrNull() ?: continue
            if (key.size == RemoteCrypto.KEY_BYTES) keys[name.removePrefix(KEY_CHANNEL_KEY_PREFIX)] = key
        }
    }

    private fun openSecure(): SharedPreferences? {
        fun create(): SharedPreferences {
            val alias = MasterKeys.getOrCreate(MasterKeys.AES256_GCM_SPEC)
            return EncryptedSharedPreferences.create(
                SECURE_FILE,
                alias,
                appContext,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
        }
        return try {
            create()
        } catch (error: Exception) {
            // The key can be lost (for example after a restore); start over
            // rather than crash, which only means signing in and pairing again.
            Log.w(TAG, "Encrypted preferences are unreadable; resetting them", error)
            appContext.deleteSharedPreferences(SECURE_FILE)
            try {
                create()
            } catch (again: Exception) {
                Log.e(TAG, "Encrypted preferences are unavailable", again)
                null
            }
        }
    }

    // -----------------------------------------------------------------------
    // GitHub sign-in
    // -----------------------------------------------------------------------

    @Synchronized
    fun saveToken(accessToken: String, refreshToken: String?, expiresAt: Long?) {
        loadSecrets()
        token = accessToken
        this.refreshToken = refreshToken
        secure?.edit()?.apply {
            putString(KEY_TOKEN, accessToken)
            if (refreshToken != null) putString(KEY_REFRESH_TOKEN, refreshToken) else remove(KEY_REFRESH_TOKEN)
        }?.commit()
        prefs.edit().apply {
            if (expiresAt != null) putLong(KEY_TOKEN_EXPIRES_AT, expiresAt) else remove(KEY_TOKEN_EXPIRES_AT)
        }.apply()
    }

    /** Forgets the GitHub tokens only; pairings survive signing in again. */
    @Synchronized
    fun clearToken() {
        token = null
        refreshToken = null
        secure?.edit()?.remove(KEY_TOKEN)?.remove(KEY_REFRESH_TOKEN)?.apply()
        prefs.edit().remove(KEY_TOKEN_EXPIRES_AT).remove(KEY_TOKEN_CLIENT_ID).apply()
    }

    val tokenExpiresAt: Long?
        get() = prefs.getLong(KEY_TOKEN_EXPIRES_AT, 0L).takeIf { it > 0L }

    /** The client ID the current tokens were issued to, which refreshing them needs. */
    var tokenClientId: String?
        get() = prefs.getString(KEY_TOKEN_CLIENT_ID, null)?.takeIf { it.isNotBlank() }
        set(value) = prefs.edit().putString(KEY_TOKEN_CLIENT_ID, value).apply()

    var clientIdOverride: String
        get() = prefs.getString(KEY_CLIENT_ID, "") ?: ""
        set(value) = prefs.edit().putString(KEY_CLIENT_ID, value.trim()).apply()

    var login: String?
        get() = prefs.getString(KEY_LOGIN, null)
        set(value) = prefs.edit().putString(KEY_LOGIN, value).apply()

    var avatarUrl: String?
        get() = prefs.getString(KEY_AVATAR, null)
        set(value) = prefs.edit().putString(KEY_AVATAR, value).apply()

    // -----------------------------------------------------------------------
    // Pairing
    // -----------------------------------------------------------------------

    /** This install's phone id: 32 lowercase hex digits, chosen once. */
    val phoneId: String
        @Synchronized get() {
            prefs.getString(KEY_PHONE_ID, null)?.takeIf { it.length == 32 }?.let { return it }
            val id = RemoteCrypto.newId()
            prefs.edit().putString(KEY_PHONE_ID, id).commit()
            return id
        }

    fun keyFor(channel: String): ByteArray? = keys[channel]

    fun pairedChannels(): Set<String> = keys.keys.toSet()

    @Synchronized
    fun savePairing(channel: String, key: ByteArray, pairedAt: Long) {
        loadSecrets()
        keys[channel] = key.copyOf()
        secure?.edit()?.putString(KEY_CHANNEL_KEY_PREFIX + channel, RemoteCrypto.base64(key))?.commit()
        prefs.edit().putLong(KEY_PAIRED_AT_PREFIX + channel, pairedAt).apply()
    }

    @Synchronized
    fun forgetPairing(channel: String) {
        keys.remove(channel)
        secure?.edit()?.remove(KEY_CHANNEL_KEY_PREFIX + channel)?.apply()
        prefs.edit().remove(KEY_PAIRED_AT_PREFIX + channel).apply()
    }

    fun pairedAt(channel: String): Long? = prefs.getLong(KEY_PAIRED_AT_PREFIX + channel, 0L).takeIf { it > 0L }

    // -----------------------------------------------------------------------
    // Computers
    // -----------------------------------------------------------------------

    var liveRelayEnabled: Boolean
        get() = prefs.getBoolean("live_relay_enabled", true)
        set(value) = prefs.edit().putBoolean("live_relay_enabled", value).apply()

    fun gistFor(channel: String): String? = prefs.getString(KEY_GIST_PREFIX + channel, null)

    fun rememberComputer(channel: String, gistId: String, name: String) {
        if (gistFor(channel) == gistId && prefs.getString(KEY_NAME_PREFIX + channel, null) == name) return
        prefs.edit()
            .putString(KEY_GIST_PREFIX + channel, gistId)
            .putString(KEY_NAME_PREFIX + channel, name)
            .apply()
    }

    fun cachedComputers(): List<Device> = pairedChannels().mapNotNull { channel ->
        val gist = gistFor(channel) ?: return@mapNotNull null
        Device(channel, gist, prefs.getString(KEY_NAME_PREFIX + channel, null) ?: "Paired computer",
            null, null, emptyList(), cached = true)
    }

    /** The computer opened last, by channel. */
    var savedChannel: String?
        get() = prefs.getString(KEY_SAVED_CHANNEL, null)
        set(value) = prefs.edit().putString(KEY_SAVED_CHANNEL, value).apply()

    fun windowFor(channel: String): Long? {
        val key = KEY_WINDOW_PREFIX + channel
        return if (prefs.contains(key)) prefs.getLong(key, 0L) else null
    }

    fun setWindowFor(channel: String, window: Long) {
        prefs.edit().putLong(KEY_WINDOW_PREFIX + channel, window).apply()
    }

    // -----------------------------------------------------------------------
    // Updates
    // -----------------------------------------------------------------------

    var lastUpdateCheck: Long
        get() = prefs.getLong(KEY_UPDATE_CHECKED_AT, 0L)
        set(value) = prefs.edit().putLong(KEY_UPDATE_CHECKED_AT, value).apply()

    var latestUpdate: UpdateInfo?
        get() {
            val code = prefs.getInt(KEY_UPDATE_CODE, 0)
            val url = prefs.getString(KEY_UPDATE_URL, null)
            val name = prefs.getString(KEY_UPDATE_NAME, null)
            return if (code > 0 && url != null) UpdateInfo(code, name ?: code.toString(), url) else null
        }
        set(value) {
            prefs.edit().apply {
                if (value == null) {
                    remove(KEY_UPDATE_CODE)
                    remove(KEY_UPDATE_URL)
                    remove(KEY_UPDATE_NAME)
                } else {
                    putInt(KEY_UPDATE_CODE, value.versionCode)
                    putString(KEY_UPDATE_URL, value.url)
                    putString(KEY_UPDATE_NAME, value.name)
                }
            }.apply()
        }

    var dismissedUpdateCode: Int
        get() = prefs.getInt(KEY_UPDATE_DISMISSED, 0)
        set(value) = prefs.edit().putInt(KEY_UPDATE_DISMISSED, value).apply()

    /**
     * Forgets the account and everything tied to it (tokens, pairing keys,
     * computers), keeping app-level settings and the phone's id.
     */
    @Synchronized
    fun clearAccount() {
        token = null
        refreshToken = null
        keys.clear()
        secure?.edit()?.clear()?.apply()
        val accountPrefixes = listOf(KEY_GIST_PREFIX, KEY_NAME_PREFIX, KEY_PAIRED_AT_PREFIX, KEY_WINDOW_PREFIX)
        prefs.edit().apply {
            remove(KEY_TOKEN_EXPIRES_AT)
            remove(KEY_TOKEN_CLIENT_ID)
            remove(KEY_LOGIN)
            remove(KEY_AVATAR)
            remove(KEY_SAVED_CHANNEL)
            prefs.all.keys.filter { key -> accountPrefixes.any(key::startsWith) }.forEach(::remove)
        }.apply()
    }

    private companion object {
        const val TAG = "PraxisStore"
        const val PREFS_FILE = "praxis_remote"
        const val SECURE_FILE = "praxis_remote_secure"
        const val KEY_TOKEN = "access_token"
        const val KEY_REFRESH_TOKEN = "refresh_token"
        const val KEY_CHANNEL_KEY_PREFIX = "key."
        const val KEY_TOKEN_EXPIRES_AT = "token_expires_at"
        const val KEY_TOKEN_CLIENT_ID = "token_client_id"
        const val KEY_CLIENT_ID = "client_id"
        const val KEY_LOGIN = "login"
        const val KEY_AVATAR = "avatar_url"
        const val KEY_PHONE_ID = "phone_id"
        const val KEY_GIST_PREFIX = "gist."
        const val KEY_NAME_PREFIX = "name."
        const val KEY_PAIRED_AT_PREFIX = "paired_at."
        const val KEY_SAVED_CHANNEL = "saved_channel"
        const val KEY_WINDOW_PREFIX = "window."
        const val KEY_UPDATE_CHECKED_AT = "update_checked_at"
        const val KEY_UPDATE_CODE = "update_code"
        const val KEY_UPDATE_URL = "update_url"
        const val KEY_UPDATE_NAME = "update_name"
        const val KEY_UPDATE_DISMISSED = "update_dismissed"
        val LEGACY_KEYS = listOf("repo", "device_number", "device_name")
    }
}
