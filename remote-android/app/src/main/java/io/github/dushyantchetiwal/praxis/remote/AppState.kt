package io.github.dushyantchetiwal.praxis.remote

import androidx.compose.ui.graphics.ImageBitmap
import io.github.dushyantchetiwal.praxis.remote.data.Device
import io.github.dushyantchetiwal.praxis.remote.data.DirEntry
import io.github.dushyantchetiwal.praxis.remote.data.FileContent
import io.github.dushyantchetiwal.praxis.remote.data.ONLINE_THRESHOLD_MS
import io.github.dushyantchetiwal.praxis.remote.data.Permission
import io.github.dushyantchetiwal.praxis.remote.data.Snapshot
import io.github.dushyantchetiwal.praxis.remote.data.Status
import io.github.dushyantchetiwal.praxis.remote.data.ThreadItem
import io.github.dushyantchetiwal.praxis.remote.data.ThreadView
import io.github.dushyantchetiwal.praxis.remote.data.TranscriptHistory
import io.github.dushyantchetiwal.praxis.remote.data.UpdateInfo
import io.github.dushyantchetiwal.praxis.remote.data.WatchInfo
import io.github.dushyantchetiwal.praxis.remote.data.WindowInfo

enum class Screen { Loading, SignIn, Devices, Pair, Device, Settings }

enum class Tab { Chat, Threads, Files }

sealed interface FlowPhase {
    data object Idle : FlowPhase
    data object Requesting : FlowPhase
    data class Code(val userCode: String, val verificationUri: String, val expiresAt: Long) : FlowPhase
    data object Finishing : FlowPhase
}

data class SignInState(val phase: FlowPhase = FlowPhase.Idle, val message: String? = null)

data class DevicesState(
    val loading: Boolean = false,
    val loaded: Boolean = false,
    val devices: List<Device> = emptyList(),
    /** Channels of the computers this phone is paired with. */
    val paired: Set<String> = emptySet(),
    /** Channels of computers that removed this phone since it paired. */
    val unpairedByComputer: Set<String> = emptySet(),
    val error: String? = null,
)

sealed interface PairPhase {
    /** Not started; the screen explains what will happen. */
    data object Ready : PairPhase
    /** Posting the request and waiting for the computer's key. */
    data object Waiting : PairPhase
    /** Both keys are known; the user compares [code] with the computer's. */
    data class Code(val code: String) : PairPhase
    data class Failed(val message: String) : PairPhase
}

data class PairingState(
    val device: Device? = null,
    val phase: PairPhase = PairPhase.Ready,
    val startedAt: Long = 0L,
)

data class AppState(
    val screen: Screen = Screen.Loading,
    val settingsReturn: Screen = Screen.Devices,
    val login: String? = null,
    val avatar: ImageBitmap? = null,
    val tokenExpiresAt: Long? = null,
    val clientIdOverride: String = "",
    /** How this phone introduces itself when pairing. */
    val phoneName: String = "",
    val signIn: SignInState = SignInState(),
    val devices: DevicesState = DevicesState(),
    val pairing: PairingState = PairingState(),
    /** An update the user has not dismissed, for the banner. */
    val update: UpdateInfo? = null,
    /** The newest known update, dismissed or not, for Settings. */
    val latestUpdate: UpdateInfo? = null,
    val checkingUpdate: Boolean = false,
)

enum class OutboxState { Sending, Queued, Sent }

/** A message shown optimistically until it appears in the transcript. */
data class OutboxItem(
    val id: Int,
    val text: String,
    val state: OutboxState,
    val doneAt: Long,
    val session: String?,
    val baseIndex: Int,
)

/** A mode picked on the phone, shown until a snapshot confirms it. */
data class ModeOverride(val window: Long?, val id: String, val expiresAt: Long)

data class ThreadsState(
    val loading: Boolean = false,
    val items: List<ThreadItem>? = null,
    val error: String? = null,
    val opening: String? = null,
)

data class FilesState(
    val loading: Boolean = false,
    val path: String = "",
    val entries: List<DirEntry>? = null,
    val truncated: Boolean = false,
    val error: String? = null,
    val file: FileContent? = null,
)

/** The one file downloading, or the result of the last download until dismissed. */
data class DownloadState(
    val path: String,
    val name: String,
    val received: Long = 0L,
    val size: Long? = null,
    /** Where the finished file is, once it is complete. */
    val savedUri: String? = null,
    /** Whether it went to Downloads/Praxis rather than a place the user chose. */
    val inDownloads: Boolean = false,
    val error: String? = null,
) {
    val active: Boolean get() = savedUri == null && error == null

    /** Between 0 and 1, or null until the size is known. */
    val fraction: Float? get() = size?.let { total -> if (total == 0L) 1f else (received.toFloat() / total).coerceIn(0f, 1f) }
}

data class Banner(val text: String, val error: Boolean)

/** Everything about the selected device, mirroring the web app's state. */
data class DeviceUi(
    val device: Device? = null,
    /** Whether the computer's gist has been read at least once. */
    val stateApplied: Boolean = false,
    val snapshot: Snapshot? = null,
    val status: Status? = null,
    val windowId: Long? = null,
    /** A thread pinned with open_thread; null follows the window's active thread. */
    val watchSession: String? = null,
    val history: TranscriptHistory = TranscriptHistory(),
    /** Whether [thread] reflects a snapshot for the current view. */
    val threadKnown: Boolean = false,
    val threadError: String? = null,
    val modeOverride: ModeOverride? = null,
    val tab: Tab = Tab.Chat,
    val threads: ThreadsState = ThreadsState(),
    val files: FilesState = FilesState(),
    val download: DownloadState? = null,
    val outbox: List<OutboxItem> = emptyList(),
    val answered: Set<String> = emptySet(),
    val busyPermissions: Set<String> = emptySet(),
    val lastContact: Long = 0L,
    val banners: Map<String, Banner> = emptyMap(),
    val stopping: Boolean = false,
    val startingThread: Boolean = false,
) {
    val thread: ThreadView? get() = history.thread

    fun currentWindow(): WindowInfo? = status?.windows?.find { it.id == windowId }

    fun isGenerating(): Boolean =
        thread?.status == "generating" || currentWindow()?.thread?.status == "generating"

    fun pendingPermissions(): List<Permission> = currentWindow()?.thread?.pending.orEmpty()

    fun visiblePermissions(): List<Permission> = pendingPermissions().filter { it.key !in answered }

    fun currentModeId(now: Long): String? {
        val override = modeOverride
        if (override != null && override.window == windowId && override.expiresAt > now) return override.id
        return currentWindow()?.thread?.mode?.current
    }

    /** Whether a snapshot's watch describes what the user is looking at. */
    fun watchMatchesView(watch: WatchInfo?): Boolean {
        if (watch == null) return false
        val windowOk = if (watch.window == null) {
            windowId == null || windowId == status?.windows?.firstOrNull()?.id
        } else {
            watch.window == windowId
        }
        // When following the active thread, accept whichever session the laptop reports.
        val sessionOk = watchSession == null || watch.sessionId == watchSession
        return windowOk && sessionOk
    }

    fun snapshotFresh(now: Long): Boolean {
        val watch = snapshot?.watch ?: return false
        val until = watch.until ?: return false
        return until > now && watchMatchesView(watch)
    }

    fun isOnline(now: Long): Boolean {
        val device = device ?: return false
        // A successful round trip is proof of life even before the gist is re-read.
        return device.seenRecently(now) || now - lastContact < ONLINE_THRESHOLD_MS
    }

    fun isOnline(other: Device, now: Long): Boolean =
        other.seenRecently(now) || (other.channel == device?.channel && now - lastContact < ONLINE_THRESHOLD_MS)
}
