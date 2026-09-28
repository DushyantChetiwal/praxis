package io.github.dushyantchetiwal.praxis.remote.data

/** A newer build published as a GitHub release. */
data class UpdateInfo(val versionCode: Int, val name: String, val url: String)

private const val RELEASES_PATH = "/repos/DushyantChetiwal/praxis/releases?per_page=100"
private const val TAG_PREFIX = "praxis-remote-android-"

/**
 * The newest release tagged `praxis-remote-android-<versionCode>` above
 * [currentVersionCode], pointing at its APK when it has one. Releases of a
 * public repository need no token, so this works signed out too.
 */
suspend fun findUpdate(gh: GitHubClient, currentVersionCode: Int): UpdateInfo? {
    val releases = gh.call("GET", RELEASES_PATH, authenticated = false).array() ?: return null
    return releases.objects().mapNotNull { release ->
        if (release.bool("draft")) return@mapNotNull null
        val tag = release.str("tag_name") ?: return@mapNotNull null
        if (!tag.startsWith(TAG_PREFIX)) return@mapNotNull null
        val code = tag.removePrefix(TAG_PREFIX).toIntOrNull() ?: return@mapNotNull null
        if (code <= currentVersionCode) return@mapNotNull null
        val apk = release.arr("assets")?.objects()
            ?.firstOrNull { it.str("name")?.endsWith(".apk", ignoreCase = true) == true }
            ?.str("browser_download_url")
        val url = apk ?: release.str("html_url") ?: return@mapNotNull null
        UpdateInfo(code, release.str("name")?.takeIf { it.isNotBlank() } ?: tag, url)
    }.maxByOrNull { it.versionCode }
}
