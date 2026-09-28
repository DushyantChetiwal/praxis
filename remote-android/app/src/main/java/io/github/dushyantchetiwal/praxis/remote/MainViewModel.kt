package io.github.dushyantchetiwal.praxis.remote

import android.app.Application
import android.graphics.BitmapFactory
import androidx.annotation.StringRes
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.dushyantchetiwal.praxis.remote.data.ApiException
import io.github.dushyantchetiwal.praxis.remote.data.Device
import io.github.dushyantchetiwal.praxis.remote.data.DeviceFlow
import io.github.dushyantchetiwal.praxis.remote.data.DirEntry
import io.github.dushyantchetiwal.praxis.remote.data.ErrorKind
import io.github.dushyantchetiwal.praxis.remote.data.GitHubClient
import io.github.dushyantchetiwal.praxis.remote.data.Permission
import io.github.dushyantchetiwal.praxis.remote.data.PermissionOption
import io.github.dushyantchetiwal.praxis.remote.data.RemoteChannel
import io.github.dushyantchetiwal.praxis.remote.data.RepoInfo
import io.github.dushyantchetiwal.praxis.remote.data.SavedDevice
import io.github.dushyantchetiwal.praxis.remote.data.Snapshot
import io.github.dushyantchetiwal.praxis.remote.data.Status
import io.github.dushyantchetiwal.praxis.remote.data.Store
import io.github.dushyantchetiwal.praxis.remote.data.ThreadItem
import io.github.dushyantchetiwal.praxis.remote.data.UpdateInfo
import io.github.dushyantchetiwal.praxis.remote.data.arr
import io.github.dushyantchetiwal.praxis.remote.data.bool
import io.github.dushyantchetiwal.praxis.remote.data.findUpdate
import io.github.dushyantchetiwal.praxis.remote.data.long
import io.github.dushyantchetiwal.praxis.remote.data.normalizeRepo
import io.github.dushyantchetiwal.praxis.remote.data.obj
import io.github.dushyantchetiwal.praxis.remote.data.objects
import io.github.dushyantchetiwal.praxis.remote.data.parseDevice
import io.github.dushyantchetiwal.praxis.remote.data.parseFile
import io.github.dushyantchetiwal.praxis.remote.data.parseListing
import io.github.dushyantchetiwal.praxis.remote.data.parseSnapshot
import io.github.dushyantchetiwal.praxis.remote.data.parseThreads
import io.github.dushyantchetiwal.praxis.remote.data.repoPath
import io.github.dushyantchetiwal.praxis.remote.data.str
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject

private const val ISSUE_POLL_MS = 3_000L
private const val MAX_POLL_BACKOFF_MS = 30_000L
private const val WATCH_SECONDS = 300
private const val WATCH_RENEW_MS = 4 * 60_000L
private const val WATCH_RETRY_MS = 30_000L
// If snapshots still ignore our watch this long after it was sent (the laptop
// restarted, or another phone took over the watch), send it again.
private const val WATCH_SETTLE_MS = 60_000L
private const val OUTBOX_FALLBACK_MS = 90_000L
private const val MODE_OVERRIDE_MS = 20_000L
private const val DEVICE_REFRESH_MS = 60_000L
private const val UPDATE_INTERVAL_MS = 6 * 60 * 60_000L

/**
 * All app state and behaviour. State is only changed on the main thread, so
 * like the web app it needs no locking; network calls suspend on IO.
 */
class MainViewModel(application: Application) : AndroidViewModel(application) {
    private val store = Store(application)
    private val gh = GitHubClient(
        application,
        token = { store.token },
        onUnauthorized = { viewModelScope.launch(Dispatchers.Main) { onUnauthorized() } },
    )
    private val deviceFlow = DeviceFlow(application, gh.http)
    private val channel = RemoteChannel(application, gh)

    private val _app = MutableStateFlow(
        AppState(clientIdOverride = store.clientIdOverride, repo = store.repo, login = store.login),
    )
    val app: StateFlow<AppState> = _app.asStateFlow()

    private val _ui = MutableStateFlow(DeviceUi())
    val ui: StateFlow<DeviceUi> = _ui.asStateFlow()

    /** Requests queued or in flight. */
    val busy: StateFlow<Int> = channel.pending

    private val _messages = MutableSharedFlow<String>(extraBufferCapacity = 8)
    val messages: SharedFlow<String> = _messages.asSharedFlow()

    /** The composer's text, kept here so a failed send can put it back. */
    var composer by mutableStateOf("")

    val builtInClientId: String = BuildConfig.GITHUB_CLIENT_ID.trim()

    private var foreground = false
    /** Bumped whenever the selected device changes, to drop queued requests. */
    private var generation = 0
    private var pollJob: Job? = null
    private val pollNow = Channel<Unit>(Channel.CONFLATED)
    private var pollFailures = 0
    private var signInJob: Job? = null
    private var devicesJob: Job? = null
    private var outboxSeq = 0
    private val watcher = Watcher()

    private class Watcher {
        var seq = 0
        var targetSet = false
        var window: Long? = null
        var session: String? = null
        var inFlight = false
        var sentAt = 0L
        var renewAt = 0L
    }

    private val d: DeviceUi get() = _ui.value

    private inline fun edit(block: DeviceUi.() -> DeviceUi) {
        _ui.value = _ui.value.block()
    }

    private fun str(@StringRes id: Int, vararg args: Any): String = getApplication<Application>().getString(id, *args)

    private fun message(text: String?) {
        if (!text.isNullOrBlank()) _messages.tryEmit(text)
    }

    private fun now() = System.currentTimeMillis()

    init {
        viewModelScope.launch {
            withContext(Dispatchers.IO) { store.loadSecrets() }
            _app.update {
                it.copy(tokenExpiresAt = store.tokenExpiresAt, update = visibleUpdate(), latestUpdate = latestUpdate())
            }
            route()
            // onForeground may have run before the token was loaded.
            if (foreground) startDeviceRefresh()
            loadAvatar()
            maybeCheckForUpdates()
        }
    }

    // -----------------------------------------------------------------------
    // Navigation and lifecycle
    // -----------------------------------------------------------------------

    private fun go(screen: Screen) = _app.update { it.copy(screen = screen) }

    private fun route() {
        when {
            store.token == null -> go(Screen.SignIn)
            store.repo == null -> {
                go(Screen.Repos)
                discoverRepos(autoSelect = true)
            }
            else -> {
                go(Screen.Devices)
                loadDevices(initial = true)
            }
        }
    }

    /** Whether the system back gesture stays inside the app. */
    fun canGoBack(state: AppState): Boolean = when (state.screen) {
        Screen.Settings, Screen.Device -> true
        Screen.Repos -> state.repo != null
        else -> false
    }

    /** Handles the system back gesture; false lets the activity finish. */
    fun back(): Boolean {
        if (!canGoBack(_app.value)) return false
        when (_app.value.screen) {
            Screen.Settings -> go(_app.value.settingsReturn)
            Screen.Device -> leaveDevice()
            Screen.Repos -> {
                // Switching was abandoned; keep the repository already chosen.
                go(Screen.Devices)
                loadDevices()
            }
            else -> return false
        }
        return true
    }

    fun openSettings() {
        val current = _app.value.screen
        if (current != Screen.Settings) _app.update { it.copy(screen = Screen.Settings, settingsReturn = current) }
    }

    fun onForeground() {
        foreground = true
        if (store.token == null) return
        startPolling()
        startDeviceRefresh()
        if (_app.value.screen == Screen.Devices || _app.value.screen == Screen.Device) loadDevices()
        maybeCheckForUpdates()
    }

    fun onBackground() {
        // While hidden there is no polling and no watch renewal, so the laptop
        // stops publishing snapshots once the watch runs out.
        foreground = false
        pollJob?.cancel()
        pollJob = null
        devicesJob?.cancel()
        devicesJob = null
    }

    private fun startDeviceRefresh() {
        devicesJob?.cancel()
        devicesJob = viewModelScope.launch {
            while (isActive) {
                delay(DEVICE_REFRESH_MS)
                val screen = _app.value.screen
                if (screen == Screen.Devices || screen == Screen.Device) loadDevices()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Sign-in
    // -----------------------------------------------------------------------

    fun effectiveClientId(): String = store.clientIdOverride.ifBlank { builtInClientId }.trim()

    fun setClientIdOverride(value: String) {
        store.clientIdOverride = value
        _app.update { it.copy(clientIdOverride = value) }
    }

    private fun setSignIn(phase: FlowPhase, message: String?) {
        _app.update { it.copy(signIn = SignInState(phase, message)) }
    }

    fun startSignIn() {
        val clientId = effectiveClientId()
        if (clientId.isEmpty()) {
            setSignIn(FlowPhase.Idle, str(R.string.signin_need_client_id))
            return
        }
        signInJob?.cancel()
        signInJob = viewModelScope.launch {
            setSignIn(FlowPhase.Requesting, null)
            try {
                val code = deviceFlow.start(clientId)
                setSignIn(
                    FlowPhase.Code(code.userCode, code.verificationUri, now() + code.expiresInSeconds * 1000L),
                    null,
                )
                val token = deviceFlow.awaitToken(clientId, code)
                setSignIn(FlowPhase.Finishing, null)
                val expiresAt = token.expiresInSeconds?.let { now() + it * 1000L }
                withContext(Dispatchers.IO) { store.saveToken(token.accessToken, token.refreshToken, expiresAt) }
                _app.update { it.copy(tokenExpiresAt = expiresAt) }
                fetchUser()
                if (store.token == null) return@launch // Rejected straight away.
                setSignIn(FlowPhase.Idle, null)
                route()
            } catch (e: DeviceFlow.FlowException) {
                setSignIn(FlowPhase.Idle, e.message)
            }
        }
    }

    fun cancelSignIn() {
        signInJob?.cancel()
        signInJob = null
        setSignIn(FlowPhase.Idle, null)
    }

    private suspend fun fetchUser() {
        try {
            val user = gh.call("GET", "/user").obj() ?: return
            store.login = user.str("login")
            store.avatarUrl = user.str("avatar_url")
            _app.update { it.copy(login = store.login) }
            loadAvatar()
        } catch (_: ApiException) {
            // The login is only shown in Settings; a 401 signs out on its own.
        }
    }

    private fun loadAvatar() {
        val url = store.avatarUrl ?: return
        viewModelScope.launch {
            val sized = url + (if ('?' in url) "&" else "?") + "s=128"
            val bitmap = withContext(Dispatchers.IO) {
                gh.fetchBytes(sized)?.let { bytes -> BitmapFactory.decodeByteArray(bytes, 0, bytes.size) }
            }
            if (bitmap != null) _app.update { it.copy(avatar = bitmap.asImageBitmap()) }
        }
    }

    private fun onUnauthorized() {
        if (store.token == null) return
        signOut(explicit = false)
        setSignIn(FlowPhase.Idle, str(R.string.signin_expired))
    }

    /** Explicit sign-out forgets the account; a rejected token only forgets the token. */
    fun signOut(explicit: Boolean = true) {
        signInJob?.cancel()
        stopDevice()
        gh.clearCache()
        if (explicit) store.clearAccount() else store.clearToken()
        composer = ""
        _app.update {
            AppState(
                screen = Screen.SignIn,
                login = store.login,
                avatar = if (explicit) null else it.avatar,
                repo = store.repo,
                clientIdOverride = store.clientIdOverride,
                update = it.update,
                latestUpdate = it.latestUpdate,
            )
        }
    }

    // -----------------------------------------------------------------------
    // Repository
    // -----------------------------------------------------------------------

    private inline fun setRepos(block: ReposState.() -> ReposState) {
        _app.update { it.copy(repos = it.repos.block()) }
    }

    fun discoverRepos(autoSelect: Boolean = false) {
        viewModelScope.launch {
            setRepos { copy(loading = true, error = null) }
            try {
                val installations = gh.call("GET", "/user/installations?per_page=100")
                    .obj()?.arr("installations")?.objects().orEmpty()
                val repos = mutableListOf<RepoInfo>()
                for (installation in installations) {
                    val id = installation.long("id") ?: continue
                    gh.call("GET", "/user/installations/$id/repositories?per_page=100")
                        .obj()?.arr("repositories")?.objects()
                        ?.forEach { repo -> repo.str("full_name")?.let { repos += RepoInfo(it, repo.bool("private")) } }
                }
                val unique = repos.distinctBy { it.fullName.lowercase() }.sortedBy { it.fullName.lowercase() }
                setRepos {
                    copy(loading = false, loaded = true, repos = unique, noInstallations = installations.isEmpty())
                }
                val only = unique.singleOrNull()
                if (autoSelect && only != null && only.private && _app.value.screen == Screen.Repos) {
                    chooseRepo(only.fullName)
                }
            } catch (e: ApiException) {
                setRepos { copy(loading = false, loaded = true, error = e.message) }
            }
        }
    }

    fun pickRepo(repo: RepoInfo) {
        if (repo.private) chooseRepo(repo.fullName) else setRepos { copy(confirmPublic = repo) }
    }

    fun confirmPublicRepo() {
        val repo = _app.value.repos.confirmPublic ?: return
        chooseRepo(repo.fullName)
    }

    fun dismissPublicRepo() = setRepos { copy(confirmPublic = null) }

    fun useManualRepo(input: String) {
        val repo = normalizeRepo(input)
        if (repo == null) {
            setRepos { copy(manualError = str(R.string.repos_manual_invalid)) }
            return
        }
        viewModelScope.launch {
            setRepos { copy(checking = true, manualError = null) }
            try {
                val data = gh.call("GET", repoPath(repo, "")).obj()
                pickRepo(RepoInfo(data?.str("full_name") ?: repo, data?.opt("private") != false))
            } catch (e: ApiException) {
                val text = if (e.kind == ErrorKind.NotFound) str(R.string.repos_manual_not_found) else e.message
                setRepos { copy(manualError = text) }
            } finally {
                setRepos { copy(checking = false) }
            }
        }
    }

    private fun chooseRepo(fullName: String) {
        store.repo = fullName
        store.savedDevice = null
        stopDevice()
        gh.clearCache()
        _app.update {
            it.copy(repo = fullName, devices = DevicesState(), repos = it.repos.copy(confirmPublic = null))
        }
        go(Screen.Devices)
        loadDevices(initial = true)
    }

    fun switchRepo() {
        stopDevice()
        store.savedDevice = null
        go(Screen.Repos)
        discoverRepos(autoSelect = false)
    }

    // -----------------------------------------------------------------------
    // Devices
    // -----------------------------------------------------------------------

    private inline fun setDevices(block: DevicesState.() -> DevicesState) {
        _app.update { it.copy(devices = it.devices.block()) }
    }

    fun loadDevices(initial: Boolean = false) {
        val repo = store.repo ?: return
        if (store.token == null) return
        viewModelScope.launch {
            setDevices { copy(loading = true) }
            try {
                val reply = gh.call(
                    "GET",
                    repoPath(repo, "/issues?state=open&per_page=100&sort=updated&direction=desc"),
                    conditional = true,
                )
                if (store.repo != repo) return@launch
                val list = reply.array()?.objects()?.mapNotNull(::parseDevice)
                setDevices { copy(devices = list ?: devices, error = null) }
                reconcileDevice(initial)
            } catch (e: ApiException) {
                if (e.kind != ErrorKind.Auth) setDevices { copy(error = str(R.string.devices_error, e.message.orEmpty())) }
            } finally {
                setDevices { copy(loading = false, loaded = true) }
            }
        }
    }

    private fun reconcileDevice(initial: Boolean) {
        val devices = _app.value.devices.devices
        val current = d.device
        if (current != null) {
            val match = devices.find { it.number == current.number } ?: devices.find { it.name == current.name }
            if (match != null) {
                if (match.number != current.number) {
                    selectDevice(match)
                } else if ((match.lastSeen ?: 0L) >= (current.lastSeen ?: 0L)) {
                    edit { copy(device = match) }
                }
            }
            return
        }
        if (!initial || _app.value.screen != Screen.Devices) return
        val saved = store.savedDevice
        val match = saved?.let { s -> devices.find { it.number == s.number } ?: devices.find { it.name == s.name } }
        (match ?: devices.singleOrNull())?.let(::openDevice)
    }

    fun openDevice(device: Device) {
        selectDevice(device)
        go(Screen.Device)
    }

    fun leaveDevice() {
        stopDevice()
        store.savedDevice = null
        composer = ""
        go(Screen.Devices)
        loadDevices()
    }

    private fun selectDevice(device: Device) {
        val changed = device.number != d.device?.number
        store.savedDevice = SavedDevice(device.number, device.name)
        if (!changed) {
            edit { copy(device = device) }
            if (pollJob == null) startPolling()
            return
        }
        stopDevice()
        edit { copy(device = device, windowId = store.windowFor(device.name)) }
        startPolling() // The watch is sent once the first poll has resolved the window.
    }

    private fun stopDevice() {
        pollJob?.cancel()
        pollJob = null
        generation++
        resetWatcher()
        pollFailures = 0
        _ui.value = DeviceUi()
    }

    private fun resetWatcher() {
        watcher.seq++
        watcher.targetSet = false
        watcher.window = null
        watcher.session = null
        watcher.inFlight = false
        watcher.sentAt = 0L
        watcher.renewAt = 0L
    }

    private fun resetWindowView() {
        edit {
            copy(
                thread = null,
                threadKnown = false,
                threadError = null,
                modeOverride = null,
                threads = ThreadsState(),
                files = FilesState(),
                outbox = emptyList(),
                answered = emptySet(),
                busyPermissions = emptySet(),
            )
        }
    }

    private fun setWindow(id: Long?) {
        if (id == d.windowId) return
        edit { copy(windowId = id, watchSession = null) }
        val device = d.device
        if (device != null && id != null) store.setWindowFor(device.name, id)
        resetWindowView()
    }

    fun selectWindow(id: Long) {
        if (d.status?.windows?.none { it.id == id } != false) return
        setWindow(id)
        // The current snapshot may already cover this window.
        d.snapshot?.let(::applySnapshot)
        ensureWatch()
        loadActiveTabData()
    }

    private fun setBanner(key: String, text: String?, error: Boolean = false) {
        edit { copy(banners = if (text == null) banners - key else banners + (key to Banner(text, error))) }
    }

    // -----------------------------------------------------------------------
    // Live state: issue polling
    // -----------------------------------------------------------------------

    private fun startPolling() {
        pollJob?.cancel()
        pollJob = null
        if (!foreground || d.device == null || store.token == null) return
        pollJob = viewModelScope.launch {
            while (isActive) {
                val wait = pollOnce() ?: break
                withTimeoutOrNull(wait) { pollNow.receive() }
            }
        }
    }

    private fun requestPoll() {
        pollNow.trySend(Unit)
    }

    /** Reads the device's issue (free when unchanged); returns the delay before the next read. */
    private suspend fun pollOnce(): Long? {
        val device = d.device ?: return null
        val repo = store.repo ?: return null
        var wait: Long? = ISSUE_POLL_MS
        try {
            val reply = gh.call("GET", repoPath(repo, "/issues/${device.number}"), conditional = true)
            // A 304 still carries the cached issue, which matters right after reselecting a device.
            val issue = reply.obj()
            if (device.number == d.device?.number && issue != null && (reply.changed || !d.issueApplied)) {
                applyIssue(issue)
            }
            pollFailures = 0
            setBanner("poll", null)
        } catch (e: ApiException) {
            wait = handlePollError(e)
        }
        ensureWatch()
        pruneOutbox()
        return wait
    }

    private fun handlePollError(e: ApiException): Long? {
        val failures = ++pollFailures
        return when {
            e.kind == ErrorKind.Auth -> null
            e.kind == ErrorKind.RateLimit -> {
                setBanner("poll", e.message, error = true)
                e.resetAt?.let { maxOf(5_000L, it - now() + 1_000L) } ?: MAX_POLL_BACKOFF_MS
            }
            e.kind == ErrorKind.NotFound || e.status == 410 -> {
                setBanner("poll", str(R.string.banner_issue_gone, d.device?.number ?: 0L))
                loadDevices()
                MAX_POLL_BACKOFF_MS
            }
            else -> {
                if (failures >= 2) setBanner("poll", str(R.string.banner_poll_failed, e.message.orEmpty()), error = true)
                minOf(ISSUE_POLL_MS shl (failures - 1).coerceAtMost(4), MAX_POLL_BACKOFF_MS)
            }
        }
    }

    private fun applyIssue(issue: JSONObject) {
        edit { copy(issueApplied = true) }
        parseDevice(issue)?.let { device ->
            if (device.number == d.device?.number) {
                edit { copy(device = device) }
                setDevices { copy(devices = devices.map { if (it.number == device.number) device else it }) }
            }
        }
        val closed = issue.str("state") == "closed"
        setBanner("closed", if (closed) str(R.string.banner_issue_closed, issue.long("number") ?: 0L) else null)
        if (closed) loadDevices()
        parseSnapshot(issue.str("body"))?.let(::applySnapshot)
        // The window may have just been resolved or changed, which resets tab data.
        loadActiveTabData()
    }

    private fun applySnapshot(snapshot: Snapshot) {
        val previousUpdatedAt = d.snapshot?.updatedAtRaw
        edit { copy(snapshot = snapshot) }
        snapshot.status?.let { applyStatus(it, snapshot.watch?.window) }
        if (d.watchMatchesView(snapshot.watch)) {
            edit { copy(thread = snapshot.thread, threadError = snapshot.threadError, threadKnown = true) }
        }
        pruneOutbox()
        val override = d.modeOverride
        if (override != null && d.currentWindow()?.thread?.mode?.current == override.id) {
            edit { copy(modeOverride = null) }
        }
        if (previousUpdatedAt != snapshot.updatedAtRaw) recheckStaleWatch()
    }

    private fun applyStatus(status: Status, preferredWindow: Long?) {
        edit { copy(status = status) }
        val windows = status.windows
        if (windows.none { it.id == d.windowId }) {
            val next = windows.find { it.id == preferredWindow } ?: windows.find { it.active } ?: windows.firstOrNull()
            setWindow(next?.id)
        }
        val pendingKeys = d.pendingPermissions().map { it.key }.toSet()
        if (d.answered.any { it !in pendingKeys }) edit { copy(answered = answered.filter { it in pendingKeys }.toSet()) }
    }

    /** Drops optimistic messages once they show up in the transcript, or after a while. */
    private fun pruneOutbox() {
        if (d.outbox.isEmpty()) return
        val thread = d.thread
        val entries = thread?.entries.orEmpty()
        val queued = d.currentWindow()?.thread?.queued ?: 0
        val now = now()
        val kept = d.outbox.filter { item ->
            if (item.doneAt == 0L) return@filter true
            val base = if (item.session == thread?.sessionId) item.baseIndex else -1
            val delivered = entries.any { it.role == "user" && it.index > base && it.text.trim() == item.text }
            if (delivered) return@filter false
            val expired = now - item.doneAt > OUTBOX_FALLBACK_MS
            !(expired && (item.state != OutboxState.Queued || queued == 0))
        }
        if (kept.size != d.outbox.size) edit { copy(outbox = kept) }
    }

    // -----------------------------------------------------------------------
    // Live state: watch
    // -----------------------------------------------------------------------

    private fun watchNeeded(): Boolean {
        if (d.device == null || !foreground || watcher.inFlight || store.token == null) return false
        if (!watcher.targetSet || watcher.window != d.windowId || watcher.session != d.watchSession) return true
        return now() >= watcher.renewAt
    }

    /** Asks the laptop to publish snapshots of the current view; renewed about every 4 minutes. */
    private fun ensureWatch() {
        if (!watchNeeded()) return
        val seq = ++watcher.seq
        val targetWindow = d.windowId
        val targetSession = d.watchSession
        watcher.targetSet = true
        watcher.window = targetWindow
        watcher.session = targetSession
        watcher.inFlight = true
        viewModelScope.launch {
            try {
                val args = JSONObject().put("seconds", WATCH_SECONDS)
                if (targetWindow != null) args.put("window", targetWindow)
                if (targetSession != null) args.put("session_id", targetSession)
                praxis("watch", args)
                if (seq != watcher.seq) return@launch
                watcher.sentAt = now()
                watcher.renewAt = watcher.sentAt + WATCH_RENEW_MS
                setBanner("watch", null)
            } catch (e: ApiException) {
                if (seq != watcher.seq) return@launch
                watcher.renewAt = now() + WATCH_RETRY_MS
                if (e.kind == ErrorKind.Praxis && targetWindow != null && d.currentWindow() == null) {
                    // The remembered window is gone; let the laptop pick its default.
                    edit { copy(windowId = null) }
                }
                if (e.kind != ErrorKind.Cancelled && e.kind != ErrorKind.Timeout && e.kind != ErrorKind.Auth) {
                    setBanner("watch", str(R.string.banner_watch_failed, e.message.orEmpty()), error = true)
                }
            } finally {
                if (seq == watcher.seq) watcher.inFlight = false
            }
            if (watchNeeded()) ensureWatch()
        }
    }

    private fun recheckStaleWatch() {
        if (d.snapshotFresh(now()) || watcher.inFlight || watcher.sentAt == 0L) return
        if (now() - watcher.sentAt >= WATCH_SETTLE_MS) watcher.renewAt = 0L
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /** Runs an op on the selected device and unwraps the `{ok, result | error}` envelope. */
    private suspend fun praxis(op: String, args: JSONObject = JSONObject()): JSONObject? {
        val device = d.device ?: throw ApiException(ErrorKind.State, str(R.string.error_no_device))
        val repo = store.repo ?: throw ApiException(ErrorKind.State, str(R.string.error_no_device))
        val gen = generation
        val envelope = channel.serialized {
            if (gen != generation) throw ApiException(ErrorKind.Cancelled, str(R.string.error_switched_device))
            channel.exchange(repo, device, op, args)
        }
        if (gen == generation) edit { copy(lastContact = now()) }
        if (envelope.optBoolean("ok")) return envelope.obj("result")
        throw ApiException(ErrorKind.Praxis, envelope.str("error") ?: str(R.string.error_op_failed, op))
    }

    private fun windowArgs(build: JSONObject.() -> Unit = {}): JSONObject {
        val args = JSONObject()
        d.windowId?.let { args.put("window", it) }
        args.build()
        return args
    }

    private fun viewKey(): String = "${d.device?.number}:${d.windowId}"

    private fun report(e: ApiException, label: String?) {
        if (e.kind == ErrorKind.Cancelled || e.kind == ErrorKind.Auth) return
        message(if (e.kind == ErrorKind.Praxis && label != null) "$label: ${e.message}" else e.message)
    }

    /** Runs an op, reports a failure, and re-reads the issue at once on success. */
    private suspend fun act(op: String, args: JSONObject, label: String): Boolean {
        return try {
            praxis(op, args)
            requestPoll()
            true
        } catch (e: ApiException) {
            report(e, label)
            false
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    fun refresh() {
        if (!d.snapshotFresh(now()) && !watcher.inFlight) watcher.renewAt = 0L
        requestPoll()
        if (pollJob == null) startPolling()
        loadDevices()
        loadActiveTabData(force = true)
    }

    fun sendPrompt() {
        val text = composer.trim()
        if (text.isEmpty() || d.device == null) return
        val item = OutboxItem(
            id = ++outboxSeq,
            text = text,
            state = OutboxState.Sending,
            doneAt = 0L,
            session = d.thread?.sessionId,
            baseIndex = d.thread?.entries?.maxOfOrNull { it.index } ?: -1,
        )
        edit { copy(outbox = outbox + item) }
        composer = ""
        viewModelScope.launch {
            try {
                val result = praxis("prompt", windowArgs { put("text", text) })
                val queued = result?.optBoolean("queued") == true
                edit {
                    copy(outbox = outbox.map {
                        if (it.id == item.id) it.copy(state = if (queued) OutboxState.Queued else OutboxState.Sent, doneAt = now()) else it
                    })
                }
                if (queued) message(str(R.string.toast_queued))
                requestPoll()
            } catch (e: ApiException) {
                edit { copy(outbox = outbox.filterNot { it.id == item.id }) }
                if (composer.isBlank()) composer = text
                report(e, str(R.string.label_message_not_sent))
            }
        }
    }

    fun stopGenerating() {
        if (d.stopping) return
        viewModelScope.launch {
            edit { copy(stopping = true) }
            val ok = act("stop", windowArgs(), str(R.string.label_could_not_stop))
            edit { copy(stopping = false) }
            if (ok) message(str(R.string.toast_stopping))
        }
    }

    fun startNewThread() {
        if (d.startingThread || d.device == null) return
        viewModelScope.launch {
            edit { copy(startingThread = true) }
            val ok = act("new_thread", windowArgs(), str(R.string.label_could_not_start_thread))
            edit { copy(startingThread = false) }
            if (!ok) return@launch
            edit { copy(watchSession = null, thread = null, threadKnown = false, threads = ThreadsState(), tab = Tab.Chat) }
            ensureWatch()
            message(str(R.string.toast_new_thread))
        }
    }

    fun setMode(modeId: String) {
        if (d.currentWindow()?.thread?.mode == null || d.currentModeId(now()) == modeId) return
        val override = ModeOverride(d.windowId, modeId, now() + MODE_OVERRIDE_MS)
        edit { copy(modeOverride = override) }
        viewModelScope.launch {
            val ok = act("mode", windowArgs { put("mode", modeId) }, str(R.string.label_could_not_change_mode))
            if (!ok && d.modeOverride == override) edit { copy(modeOverride = null) }
        }
    }

    fun answerPermission(permission: Permission, option: PermissionOption) {
        val key = permission.key
        if (key in d.busyPermissions) return
        viewModelScope.launch {
            edit { copy(busyPermissions = busyPermissions + key) }
            val ok = act(
                "permission",
                windowArgs {
                    put("session_id", permission.sessionId)
                    put("tool_call_id", permission.toolCallId)
                    put("option_id", option.id)
                },
                str(R.string.label_could_not_answer),
            )
            edit { copy(busyPermissions = busyPermissions - key, answered = if (ok) answered + key else answered) }
        }
    }

    fun architectAction(run: Boolean) {
        viewModelScope.launch {
            val op = if (run) "run" else "stop"
            val label = str(if (run) R.string.label_could_not_run_plan else R.string.label_could_not_stop)
            if (act("architect", windowArgs { put("op", op) }, label)) {
                message(str(if (run) R.string.toast_running_plan else R.string.toast_stopping_plan))
            }
        }
    }

    fun switchTab(tab: Tab) {
        edit { copy(tab = tab) }
        loadActiveTabData()
    }

    /** Loads data for the visible tab if it is missing (or always, when forced). */
    private fun loadActiveTabData(force: Boolean = false) {
        if (d.device == null) return
        val threads = d.threads
        val files = d.files
        if (d.tab == Tab.Threads && !threads.loading && (force || (threads.items == null && threads.error == null))) {
            loadThreads()
        }
        if (d.tab == Tab.Files && !files.loading && files.file == null) {
            if (force) listDir(files.path) else if (files.entries == null && files.error == null) listDir("")
        }
    }

    fun loadThreads() {
        if (d.device == null) return
        val key = viewKey()
        edit { copy(threads = threads.copy(loading = true, error = null)) }
        viewModelScope.launch {
            try {
                val result = praxis("threads", windowArgs())
                if (key != viewKey()) return@launch
                edit { copy(threads = ThreadsState(items = parseThreads(result))) }
            } catch (e: ApiException) {
                if (key != viewKey()) return@launch
                edit { copy(threads = threads.copy(loading = false, error = e.message)) }
            }
        }
    }

    fun openThread(thread: ThreadItem) {
        if (thread.active) {
            switchTab(Tab.Chat)
            return
        }
        if (d.threads.opening != null) return
        edit { copy(threads = threads.copy(opening = thread.sessionId)) }
        viewModelScope.launch {
            val ok = act("open_thread", windowArgs { put("session_id", thread.sessionId) }, str(R.string.label_could_not_open_thread))
            edit { copy(threads = threads.copy(opening = null)) }
            if (!ok) return@launch
            edit {
                copy(
                    threads = threads.copy(items = threads.items?.map { it.copy(active = it.sessionId == thread.sessionId) }),
                    watchSession = thread.sessionId,
                    thread = null,
                    threadKnown = false,
                )
            }
            ensureWatch()
            switchTab(Tab.Chat)
        }
    }

    fun listDir(path: String) {
        if (d.device == null) return
        val key = viewKey()
        edit { copy(files = files.copy(loading = true, error = null, file = null)) }
        viewModelScope.launch {
            try {
                val listing = parseListing(praxis("list_dir", windowArgs { put("path", path) }), path)
                if (key != viewKey()) return@launch
                edit { copy(files = FilesState(path = listing.path, entries = listing.entries, truncated = listing.truncated)) }
            } catch (e: ApiException) {
                if (key != viewKey()) return@launch
                edit { copy(files = files.copy(loading = false, error = e.message)) }
            }
        }
    }

    fun openFile(entry: DirEntry) {
        val key = viewKey()
        edit { copy(files = files.copy(loading = true, error = null)) }
        viewModelScope.launch {
            try {
                val file = parseFile(praxis("read_file", windowArgs { put("path", entry.path) }), entry.path)
                if (key != viewKey()) return@launch
                edit { copy(files = files.copy(loading = false, file = file)) }
            } catch (e: ApiException) {
                if (key != viewKey()) return@launch
                edit { copy(files = files.copy(loading = false, error = e.message)) }
            }
        }
    }

    fun closeFile() {
        edit { copy(files = files.copy(file = null, error = null)) }
        if (d.files.entries == null) listDir(d.files.path)
    }

    // -----------------------------------------------------------------------
    // Updates
    // -----------------------------------------------------------------------

    private fun latestUpdate(): UpdateInfo? =
        store.latestUpdate?.takeIf { it.versionCode > BuildConfig.VERSION_CODE }

    private fun visibleUpdate(): UpdateInfo? =
        latestUpdate()?.takeIf { it.versionCode > store.dismissedUpdateCode }

    fun checkForUpdates() = maybeCheckForUpdates(force = true)

    private fun maybeCheckForUpdates(force: Boolean = false) {
        if (_app.value.checkingUpdate) return
        if (!force && now() - store.lastUpdateCheck < UPDATE_INTERVAL_MS) return
        viewModelScope.launch {
            _app.update { it.copy(checkingUpdate = true) }
            try {
                val found = findUpdate(gh, BuildConfig.VERSION_CODE)
                store.lastUpdateCheck = now()
                store.latestUpdate = found
                // Asking explicitly brings back an update that was dismissed.
                if (force && found != null) store.dismissedUpdateCode = 0
                _app.update { it.copy(update = visibleUpdate(), latestUpdate = latestUpdate()) }
                if (force) message(str(if (found == null) R.string.update_none else R.string.update_found, found?.name ?: ""))
            } catch (e: ApiException) {
                if (force) message(e.message)
            } finally {
                _app.update { it.copy(checkingUpdate = false) }
            }
        }
    }

    fun dismissUpdate() {
        store.latestUpdate?.let { store.dismissedUpdateCode = it.versionCode }
        _app.update { it.copy(update = null) }
    }
}
