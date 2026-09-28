package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKeys

/** A device the user picked, remembered across restarts. */
data class SavedDevice(val number: Long, val name: String)

/**
 * Everything the app remembers. The GitHub token lives in encrypted
 * preferences; the rest (repository, selections, update checks) is not secret.
 */
class Store(context: Context) {
    private val appContext = context.applicationContext
    private val prefs: SharedPreferences =
        appContext.getSharedPreferences(PREFS_FILE, Context.MODE_PRIVATE)

    /** Null when the Keystore is unusable; the token is then kept in memory only. */
    private var secure: SharedPreferences? = null
    private var secureLoaded = false

    @Volatile
    var token: String? = null
        private set

    /** Reads the token from encrypted storage. Slow the first time; call off the main thread. */
    @Synchronized
    fun loadSecrets() {
        if (secureLoaded) return
        secureLoaded = true
        secure = openSecure()
        token = secure?.getString(KEY_TOKEN, null)?.takeIf { it.isNotBlank() }
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
            // rather than crash, which only means signing in again.
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

    @Synchronized
    fun saveToken(accessToken: String, refreshToken: String?, expiresAt: Long?) {
        loadSecrets()
        token = accessToken
        secure?.edit()?.apply {
            putString(KEY_TOKEN, accessToken)
            if (refreshToken != null) putString(KEY_REFRESH_TOKEN, refreshToken) else remove(KEY_REFRESH_TOKEN)
        }?.apply()
        prefs.edit().apply {
            if (expiresAt != null) putLong(KEY_TOKEN_EXPIRES_AT, expiresAt) else remove(KEY_TOKEN_EXPIRES_AT)
        }.apply()
    }

    @Synchronized
    fun clearToken() {
        token = null
        secure?.edit()?.clear()?.apply()
        prefs.edit().remove(KEY_TOKEN_EXPIRES_AT).apply()
    }

    val tokenExpiresAt: Long?
        get() = prefs.getLong(KEY_TOKEN_EXPIRES_AT, 0L).takeIf { it > 0L }

    var clientIdOverride: String
        get() = prefs.getString(KEY_CLIENT_ID, "") ?: ""
        set(value) = prefs.edit().putString(KEY_CLIENT_ID, value.trim()).apply()

    var repo: String?
        get() = prefs.getString(KEY_REPO, null)?.takeIf { it.isNotBlank() }
        set(value) = prefs.edit().putString(KEY_REPO, value).apply()

    var login: String?
        get() = prefs.getString(KEY_LOGIN, null)
        set(value) = prefs.edit().putString(KEY_LOGIN, value).apply()

    var avatarUrl: String?
        get() = prefs.getString(KEY_AVATAR, null)
        set(value) = prefs.edit().putString(KEY_AVATAR, value).apply()

    var savedDevice: SavedDevice?
        get() {
            val number = prefs.getLong(KEY_DEVICE_NUMBER, -1L)
            val name = prefs.getString(KEY_DEVICE_NAME, null)
            return if (number > 0 && name != null) SavedDevice(number, name) else null
        }
        set(value) {
            prefs.edit().apply {
                if (value == null) {
                    remove(KEY_DEVICE_NUMBER)
                    remove(KEY_DEVICE_NAME)
                } else {
                    putLong(KEY_DEVICE_NUMBER, value.number)
                    putString(KEY_DEVICE_NAME, value.name)
                }
            }.apply()
        }

    fun windowFor(deviceName: String): Long? {
        val key = KEY_WINDOW_PREFIX + deviceName
        return if (prefs.contains(key)) prefs.getLong(key, 0L) else null
    }

    fun setWindowFor(deviceName: String, window: Long) {
        prefs.edit().putLong(KEY_WINDOW_PREFIX + deviceName, window).apply()
    }

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

    /** Forgets the account and everything tied to it, keeping app-level settings. */
    fun clearAccount() {
        clearToken()
        prefs.edit()
            .remove(KEY_REPO)
            .remove(KEY_LOGIN)
            .remove(KEY_AVATAR)
            .remove(KEY_DEVICE_NUMBER)
            .remove(KEY_DEVICE_NAME)
            .apply()
    }

    private companion object {
        const val TAG = "PraxisStore"
        const val PREFS_FILE = "praxis_remote"
        const val SECURE_FILE = "praxis_remote_secure"
        const val KEY_TOKEN = "access_token"
        const val KEY_REFRESH_TOKEN = "refresh_token"
        const val KEY_TOKEN_EXPIRES_AT = "token_expires_at"
        const val KEY_CLIENT_ID = "client_id"
        const val KEY_REPO = "repo"
        const val KEY_LOGIN = "login"
        const val KEY_AVATAR = "avatar_url"
        const val KEY_DEVICE_NUMBER = "device_number"
        const val KEY_DEVICE_NAME = "device_name"
        const val KEY_WINDOW_PREFIX = "window."
        const val KEY_UPDATE_CHECKED_AT = "update_checked_at"
        const val KEY_UPDATE_CODE = "update_code"
        const val KEY_UPDATE_URL = "update_url"
        const val KEY_UPDATE_NAME = "update_name"
        const val KEY_UPDATE_DISMISSED = "update_dismissed"
    }
}
