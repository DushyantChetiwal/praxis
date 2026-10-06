package io.github.dushyantchetiwal.praxis.remote

import android.app.Application
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Build
import androidx.annotation.StringRes
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.dushyantchetiwal.praxis.remote.data.ApiException
import io.github.dushyantchetiwal.praxis.remote.data.CryptoException
import io.github.dushyantchetiwal.praxis.remote.data.Device
import io.github.dushyantchetiwal.praxis.remote.data.DetailRequest
import io.github.dushyantchetiwal.praxis.remote.data.DetailBody
import io.github.dushyantchetiwal.praxis.remote.data.DetailCache
import io.github.dushyantchetiwal.praxis.remote.data.encodeDetailBody
import io.github.dushyantchetiwal.praxis.remote.data.decodeDetailBody
import io.github.dushyantchetiwal.praxis.remote.ui.MdBlock
import io.github.dushyantchetiwal.praxis.remote.ui.parseMarkdown
import io.github.dushyantchetiwal.praxis.remote.data.DetailChunk
import io.github.dushyantchetiwal.praxis.remote.data.parseDetailChunk
import io.github.dushyantchetiwal.praxis.remote.data.ModelOption
import io.github.dushyantchetiwal.praxis.remote.data.parseModels
import io.github.dushyantchetiwal.praxis.remote.data.parseHostFolders
import io.github.dushyantchetiwal.praxis.remote.data.parseQueue
import io.github.dushyantchetiwal.praxis.remote.data.QuestionHeader
import io.github.dushyantchetiwal.praxis.remote.data.parseQuestionPage
import io.github.dushyantchetiwal.praxis.remote.data.parseQuestionForm
import io.github.dushyantchetiwal.praxis.remote.data.questionAnswerContent
import io.github.dushyantchetiwal.praxis.remote.data.DeviceFlow
import io.github.dushyantchetiwal.praxis.remote.data.DirEntry
import io.github.dushyantchetiwal.praxis.remote.data.DownloadException
import io.github.dushyantchetiwal.praxis.remote.data.DownloadSink
import io.github.dushyantchetiwal.praxis.remote.data.checkDownloadChunk
import io.github.dushyantchetiwal.praxis.remote.data.ErrorKind
import io.github.dushyantchetiwal.praxis.remote.data.GistRead
import io.github.dushyantchetiwal.praxis.remote.data.Gists
import io.github.dushyantchetiwal.praxis.remote.data.GitHubClient
import io.github.dushyantchetiwal.praxis.remote.data.Link
import io.github.dushyantchetiwal.praxis.remote.data.PHONE_NAME_MAX
import io.github.dushyantchetiwal.praxis.remote.data.PairResult
import io.github.dushyantchetiwal.praxis.remote.data.Pairer
import io.github.dushyantchetiwal.praxis.remote.data.Permission
import io.github.dushyantchetiwal.praxis.remote.data.PermissionOption
import io.github.dushyantchetiwal.praxis.remote.data.RemoteChannel
import io.github.dushyantchetiwal.praxis.remote.data.PromptImage
import io.github.dushyantchetiwal.praxis.remote.data.prepareImage
import io.github.dushyantchetiwal.praxis.remote.data.uploadImage
import io.github.dushyantchetiwal.praxis.remote.data.MAX_PROMPT_IMAGES
import io.github.dushyantchetiwal.praxis.remote.data.MAX_RETAINED_IMAGES
import io.github.dushyantchetiwal.praxis.remote.data.RemoteViewScope
import io.github.dushyantchetiwal.praxis.remote.data.viewScopedAction
import io.github.dushyantchetiwal.praxis.remote.data.Snapshot
import io.github.dushyantchetiwal.praxis.remote.data.Status
import io.github.dushyantchetiwal.praxis.remote.data.Store
import io.github.dushyantchetiwal.praxis.remote.data.ThreadItem
import io.github.dushyantchetiwal.praxis.remote.data.TranscriptHistory
import io.github.dushyantchetiwal.praxis.remote.data.parseThreadView
import io.github.dushyantchetiwal.praxis.remote.data.TokenManager
import io.github.dushyantchetiwal.praxis.remote.data.UpdateInfo
import io.github.dushyantchetiwal.praxis.remote.data.findUpdate
import io.github.dushyantchetiwal.praxis.remote.data.obj
import io.github.dushyantchetiwal.praxis.remote.data.parseDownloadChunk
import io.github.dushyantchetiwal.praxis.remote.data.parseFile
import io.github.dushyantchetiwal.praxis.remote.data.parseListing
import io.github.dushyantchetiwal.praxis.remote.data.parseThreads
import io.github.dushyantchetiwal.praxis.remote.data.str
import java.io.IOException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
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

private const val STATE_POLL_MS = 3_000L
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
// Right after pairing, the computer's praxis-remote.json may not list this
// phone yet; it is rewritten at least every minute.
private const val PAIR_GRACE_MS = 3 * 60_000L
/** Screens that show computers, whose list is refreshed in the background. */
private val LISTING_SCREENS = setOf(Screen.Devices, Screen.Pair, Screen.Device)

/**
 * All app state and behaviour. State is only changed on the main thread, so
 * like the web app it needs no locking; network calls suspend on IO.
 */
data class LoadedDetail(val body: DetailBody, val blocks: List<MdBlock>) {
    fun cacheWeight(): Long = body.chunks.sumOf { it.text.length.toLong() * 2 + 192 } + blocks.sumOf { block ->
        val text = when (block) {
            is MdBlock.Paragraph -> block.text
            is MdBlock.Heading -> block.text
            is MdBlock.Code -> block.code
            is MdBlock.Item -> block.text
            is MdBlock.Quote -> block.text
            MdBlock.Rule -> ""
        }
        text.length.toLong() * 2 + 96
    }
}

class MainViewModel(application: Application) : AndroidViewModel(application) {
    private val store = Store(application)
    private val http = GitHubClient.newHttpClient()
    private val deviceFlow = DeviceFlow(application, http)
    private val tokens = TokenManager(
        store,
        deviceFlow,
        clientId = { store.tokenClientId ?: effectiveClientId() },
        onRefreshed = { expiresAt -> _app.update { it.copy(tokenExpiresAt = expiresAt) } },
    )
    private val gh = GitHubClient(
        application,
        http,
        tokens,
        onUnauthorized = { viewModelScope.launch(Dispatchers.Main) { onUnauthorized() } },
    )
    private val channel = RemoteChannel(application, gh) { store.login }
    private val gists = Gists(gh)
    private val pairer = Pairer(application, gh) { store.login }
    private val phoneId: String by lazy { store.phoneId }
    private var detailCache = newDetailCache()

    private fun newDetailCache() = DetailCache(
        directory = java.io.File(getApplication<Application>().cacheDir, "details-${java.util.UUID.randomUUID()}"),
        weight = LoadedDetail::cacheWeight,
        encode = { detail: LoadedDetail -> encodeDetailBody(detail.body) },
        decode = { bytes ->
            val body = decodeDetailBody(bytes)
            LoadedDetail(body, parseMarkdown(body.chunks.joinToString("") { it.text }))
        },
    )

    fun detailIdentity(request: DetailRequest): String = io.github.dushyantchetiwal.praxis.remote.data.detailCacheIdentity(
        d.device?.channel, d.windowId, d.currentWindow()?.thread?.sessionId, request,
    )

    fun cachedDetail(identity: String): LoadedDetail? = detailCache.peek(identity)

    suspend fun restoreDetail(identity: String): LoadedDetail? = try {
        detailCache.restore(identity)
    } catch (error: CancellationException) { throw error }
    catch (error: Exception) { android.util.Log.w("PraxisRemote", "Conversation cache unavailable", error); null }

    fun rememberDetail(identity: String, detail: LoadedDetail) {
        val cache = detailCache
        val revision = cache.remember(identity, detail)
        viewModelScope.launch {
            try { cache.persist(identity, detail, revision) }
            catch (error: CancellationException) { throw error }
            catch (error: Exception) { android.util.Log.w("PraxisRemote", "Could not persist conversation cache", error) }
        }
    }

    fun invalidateDetail(identity: String) {
        val cache = detailCache
        val revision = cache.invalidate(identity)
        viewModelScope.launch {
            try { cache.remove(identity, revision) }
            catch (error: CancellationException) { throw error }
            catch (error: Exception) { android.util.Log.w("PraxisRemote", "Could not remove cached details", error) }
        }
    }

    private fun closeDetailCache() {
        val cache = detailCache
        cache.close()
        kotlinx.coroutines.CoroutineScope(Dispatchers.IO).launch {
            try { cache.removeFiles() }
            catch (error: Exception) { android.util.Log.w("PraxisRemote", "Could not clear conversation cache", error) }
        }
    }

    private val _app = MutableStateFlow(
        AppState(clientIdOverride = store.clientIdOverride, login = store.login, phoneName = phoneName()),
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
    private val imageDrafts = mutableStateMapOf<String, List<PromptImage>>()
    private fun imageDraftKey() = "${viewKey()}:${d.currentWindow()?.thread?.sessionId}"
    val composerImages: List<PromptImage> get() = imageDrafts[imageDraftKey()].orEmpty()
    var preparingImage by mutableStateOf(false)
        private set
    val hasPrompt: Boolean get() = composer.isNotBlank() || composerImages.isNotEmpty()
    var liveRelayEnabled by mutableStateOf(store.liveRelayEnabled)
        private set

    fun setLiveRelayEnabled(enabled: Boolean) {
        store.liveRelayEnabled = enabled
        liveRelayEnabled = enabled
        channel.live.stop()
        resetWatcher()
        startPolling()
        requestPoll()
    }

    fun addImage(uri: Uri) {
        if (preparingImage || d.currentWindow()?.thread?.imageInput != true) return
        val draft = imageDraftKey()
        if (composerImages.size >= MAX_PROMPT_IMAGES || imageDrafts.values.sumOf { it.size } + d.outbox.sumOf { it.images.size } >= MAX_RETAINED_IMAGES) {
            message(str(R.string.image_limit))
            return
        }
        preparingImage = true
        viewModelScope.launch {
            try {
                val image = prepareImage(getApplication(), uri)
                imageDrafts[draft] = imageDrafts[draft].orEmpty() + image
            } catch (error: CancellationException) { throw error }
            catch (error: Exception) { message(error.message ?: str(R.string.image_failed)) }
            finally { preparingImage = false }
        }
    }

    fun removeImage(id: String) {
        imageDrafts[imageDraftKey()] = composerImages.filterNot { it.id == id }
    }

    val builtInClientId: String = BuildConfig.GITHUB_CLIENT_ID.trim()

    private var foreground = false
    /** Bumped whenever the selected device changes, to drop queued requests. */
    private var generation = 0
    private var viewRevision = 0L
    private var folderRequest = 0L
    private var queueRequest = 0L
    private var questionRequest = 0L
    private var questionListRequest = 0L
    private var questionReadJob: Job? = null
    private var pollJob: Job? = null
    private var downloadJob: Job? = null
    private var historyJob: Job? = null
    private var historySequence = 0L
    private val pollNow = Channel<Unit>(Channel.CONFLATED)
    private var pollFailures = 0
    /** The gist response last applied to the selected computer. */
    private var appliedGist: String? = null
    private var signInJob: Job? = null
    private var devicesJob: Job? = null
    private var pairJob: Job? = null
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
            channel.live.snapshots.collect { update ->
                if (foreground && d.device?.channel == update.channel && phoneId == update.phoneId) {
                    edit { copy(lastContact = now(), stateApplied = true) }
                    applySnapshot(update.snapshot)
                    setBanner("poll", null)
                    setBanner("state", null)
                    ensureWatch()
                }
            }
        }
        viewModelScope.launch {
            var previousEpoch: String? = null
            var wasLive = false
            channel.live.connection.collect { connection ->
                if (connection?.channel == d.device?.channel) {
                    edit { copy(liveTransport = connection?.connected == true) }
                    if (connection?.connected == true) {
                        if (!wasLive || previousEpoch != connection.epoch) { resetWatcher(); previousEpoch = connection.epoch }
                        wasLive = true
                        ensureWatch()
                    } else { wasLive = false }
                    requestPoll()
                } else {
                    previousEpoch = null
                    wasLive = false
                    edit { copy(liveTransport = false) }
                }
            }
        }
        viewModelScope.launch {
            withContext(Dispatchers.IO) {
                store.loadSecrets()
                phoneId // Chosen and saved on first launch.
            }
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
        if (store.token == null) {
            go(Screen.SignIn)
        } else {
            go(Screen.Devices)
            loadDevices(initial = true)
        }
    }

    /** Whether the system back gesture stays inside the app. */
    fun canGoBack(state: AppState): Boolean = when (state.screen) {
        Screen.Settings, Screen.Device, Screen.Pair -> true
        else -> false
    }

    /** Handles the system back gesture; false lets the activity finish. */
    fun back(): Boolean {
        if (!canGoBack(_app.value)) return false
        when (_app.value.screen) {
            Screen.Settings -> go(_app.value.settingsReturn)
            Screen.Device -> d.parentTab()?.let(::switchTab) ?: leaveDevice()
            Screen.Pair -> cancelPairing()
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
        if (_app.value.screen in LISTING_SCREENS) loadDevices()
        maybeCheckForUpdates()
    }

    fun onBackground() {
        // While hidden there is no polling and no watch renewal, so the laptop
        // stops publishing snapshots once the watch runs out.
        foreground = false
        channel.live.stop()
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
                if (_app.value.screen in LISTING_SCREENS) loadDevices()
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
                store.tokenClientId = clientId
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

    /** GitHub rejected the token and refreshing it failed. */
    private fun onUnauthorized() {
        if (store.token == null) return
        signOut(explicit = false)
        setSignIn(FlowPhase.Idle, str(R.string.signin_expired))
    }

    /**
     * Explicit sign-out forgets the account and every pairing; a token that
     * can no longer be refreshed only forgets the tokens.
     */
    fun signOut(explicit: Boolean = true) {
        signInJob?.cancel()
        pairJob?.cancel()
        pairJob = null
        stopDevice()
        gh.clearCache()
        if (explicit) {
            store.clearAccount()
            imageDrafts.clear()
            closeDetailCache()
            detailCache = newDetailCache()
        } else store.clearToken()
        composer = ""
        _app.update {
            AppState(
                screen = Screen.SignIn,
                login = store.login,
                avatar = if (explicit) null else it.avatar,
                clientIdOverride = store.clientIdOverride,
                phoneName = it.phoneName,
                update = it.update,
                latestUpdate = it.latestUpdate,
            )
        }
    }

    private fun phoneName(): String {
        val maker = Build.MANUFACTURER.orEmpty().trim().replaceFirstChar { it.titlecase() }
        val model = Build.MODEL.orEmpty().trim()
        val name = if (maker.isNotEmpty() && model.startsWith(maker, ignoreCase = true)) model else "$maker $model"
        return name.trim().ifEmpty { "Android phone" }.take(PHONE_NAME_MAX)
    }

    // -----------------------------------------------------------------------
    // Computers
    // -----------------------------------------------------------------------

    private inline fun setDevices(block: DevicesState.() -> DevicesState) {
        _app.update { it.copy(devices = it.devices.block()) }
    }

    fun loadDevices(initial: Boolean = false) {
        if (store.token == null) return
        viewModelScope.launch {
            setDevices { copy(loading = true) }
            try {
                // Comments are only trusted when written by this account, so learn it first.
                if (store.login == null) fetchUser()
                val found = gists.discover(known = store.pairedChannels().mapNotNull(store::gistFor))
                if (store.token == null) return@launch
                found.forEach { store.rememberComputer(it.channel, it.gistId, it.name) }
                setDevices { copy(devices = found, error = null) }
                _app.value.pairing.device?.let { pairing ->
                    val fresh = found.find { it.channel == pairing.channel }
                    if (fresh != null) setPairing { copy(device = fresh) }
                }
                found.forEach(::checkPairing)
                setDevices { copy(paired = found.filter(::isPaired).map { it.channel }.toSet()) }
                reconcileDevice(initial)
            } catch (e: ApiException) {
                if (e.kind != ErrorKind.Auth) {
                    val cached = store.cachedComputers()
                    setDevices { copy(
                        devices = (devices + cached).distinctBy { it.channel },
                        paired = paired + cached.map { it.channel },
                        error = str(R.string.devices_error, e.message.orEmpty()),
                    ) }
                    reconcileDevice(initial)
                }
            } finally {
                setDevices { copy(loading = false, loaded = true) }
            }
        }
    }

    /** Whether this phone holds a key for [device] that the computer still honours. */
    private fun isPaired(device: Device): Boolean {
        if (store.keyFor(device.channel) == null) return false
        return device.cached || phoneId in device.phones || withinPairingGrace(device.channel)
    }

    private fun withinPairingGrace(channel: String): Boolean =
        now() - (store.pairedAt(channel) ?: 0L) < PAIR_GRACE_MS

    /** A stale or competing publisher must not erase the only key that can reconnect. */
    private fun checkPairing(device: Device): Boolean {
        if (store.keyFor(device.channel) == null) return false
        if (isPaired(device)) {
            setBanner("pairing", null)
            setDevices { copy(paired = paired + device.channel, unpairedByComputer = unpairedByComputer - device.channel) }
            return true
        }
        setBanner("pairing", str(R.string.banner_pairing_unconfirmed, device.name), error = true)
        return false
    }

    /** Refusal disables commands, but only explicit unpair/sign-out destroys the local key. */
    private fun onUnpaired(device: Device, text: String) {
        setDevices {
            copy(paired = paired - device.channel, unpairedByComputer = unpairedByComputer + device.channel)
        }
        if (d.device?.channel == device.channel) {
            setBanner("pairing", str(R.string.banner_pairing_unconfirmed, device.name), error = true)
        }
        message(text)
    }

    private fun reconcileDevice(initial: Boolean) {
        val devices = _app.value.devices.devices
        val current = d.device
        if (current != null) {
            val match = devices.find { it.channel == current.channel } ?: return
            if (match.gistId != current.gistId) {
                // Praxis recreated its gist; follow it.
                appliedGist = null
                edit { copy(device = match, stateApplied = false) }
                setBanner("poll", null)
                requestPoll()
            } else if ((match.lastSeen ?: 0L) >= (current.lastSeen ?: 0L)) {
                edit { copy(device = match) }
            }
            return
        }
        if (!initial || _app.value.screen != Screen.Devices) return
        val paired = devices.filter { it.channel in _app.value.devices.paired }
        val saved = store.savedChannel
        val match = saved?.let { s -> paired.find { it.channel == s } }
        (match ?: paired.singleOrNull()?.takeIf { devices.size == 1 })?.let(::openDevice)
    }

    /** Opens a paired computer, or offers to pair with one that is not. */
    fun openDevice(device: Device) {
        if (device.channel !in _app.value.devices.paired) {
            showPairing(device)
            return
        }
        selectDevice(device)
        go(Screen.Device)
    }

    fun leaveDevice() {
        stopDevice()
        store.savedChannel = null
        composer = ""
        go(Screen.Devices)
        loadDevices()
    }

    private fun selectDevice(device: Device) {
        val changed = device.channel != d.device?.channel
        store.savedChannel = device.channel
        if (!changed) {
            edit { copy(device = device) }
            if (pollJob == null) startPolling()
            return
        }
        stopDevice()
        edit { copy(device = device, windowId = store.windowFor(device.channel)) }
        startPolling() // The watch is sent once the first poll has resolved the window.
    }

    private fun linkFor(device: Device): Link? =
        store.keyFor(device.channel)?.let { Link(device.gistId, device.channel, phoneId, it, device.name) }

    // -----------------------------------------------------------------------
    // Pairing
    // -----------------------------------------------------------------------

    private inline fun setPairing(block: PairingState.() -> PairingState) {
        _app.update { it.copy(pairing = it.pairing.block()) }
    }

    private fun showPairing(device: Device) {
        pairJob?.cancel()
        pairJob = null
        _app.update { it.copy(pairing = PairingState(device = device), screen = Screen.Pair) }
    }

    /** Starts (or restarts) pairing with the computer on the pairing screen. */
    fun startPairing() {
        val device = _app.value.pairing.device ?: return
        pairJob?.cancel()
        setPairing { copy(phase = PairPhase.Waiting, startedAt = now()) }
        pairJob = viewModelScope.launch {
            val result = try {
                pairer.pair(device.gistId, device.channel, phoneId, _app.value.phoneName) { code ->
                    setPairing { copy(phase = PairPhase.Code(code)) }
                }
            } catch (e: ApiException) {
                if (e.kind != ErrorKind.Auth) setPairing { copy(phase = PairPhase.Failed(e.message.orEmpty())) }
                return@launch
            }
            val failure = when (result) {
                is PairResult.Approved -> {
                    withContext(Dispatchers.IO) { store.savePairing(device.channel, result.key, now()) }
                    setDevices {
                        copy(paired = paired + device.channel, unpairedByComputer = unpairedByComputer - device.channel)
                    }
                    _app.update { it.copy(pairing = PairingState()) }
                    message(str(R.string.pair_done, device.name))
                    selectDevice(device)
                    go(Screen.Device)
                    return@launch
                }
                PairResult.Denied -> str(R.string.pair_denied, device.name)
                PairResult.Expired -> str(R.string.pair_expired)
                PairResult.Invalid -> str(R.string.pair_invalid, device.name)
                PairResult.TimedOut -> str(R.string.pair_timed_out, device.name)
                PairResult.Removed -> str(R.string.pair_removed)
            }
            setPairing { copy(phase = PairPhase.Failed(failure)) }
        }
    }

    /** Leaves the pairing screen; a pairing in progress is withdrawn (its comment deleted). */
    fun cancelPairing() {
        pairJob?.cancel()
        pairJob = null
        _app.update { it.copy(pairing = PairingState()) }
        go(Screen.Devices)
        loadDevices()
    }

    /** Asks the computer to forget this phone (best effort) and forgets its key. */
    fun unpairCurrent() {
        val device = d.device ?: return
        val link = linkFor(device)
        store.forgetPairing(device.channel)
        setDevices { copy(paired = paired - device.channel) }
        leaveDevice()
        message(str(R.string.unpair_done, device.name))
        if (link != null) {
            viewModelScope.launch {
                try {
                    channel.serialized { channel.exchange(link, "unpair", JSONObject()) }
                } catch (_: ApiException) {
                    // The computer drops the phone when it next refuses a request, or the user can remove it there.
                }
            }
        }
    }

    private fun stopDevice() {
        channel.live.stop()
        invalidateQuestionRequests()
        viewRevision++
        cancelHistoryRequest()
        pollJob?.cancel()
        pollJob = null
        downloadJob?.cancel()
        downloadJob = null
        generation++
        resetWatcher()
        pollFailures = 0
        appliedGist = null
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

    private fun cancelHistoryRequest() {
        historyJob?.cancel()
        historyJob = null
    }

    private fun resetWindowView() {
        invalidateQuestionRequests()
        viewRevision++
        cancelHistoryRequest()
        edit {
            copy(
                history = TranscriptHistory(),
                startingThread = false,
                threadKnown = false,
                threadError = null,
                modeOverride = null,
                models = ModelsState(),
                folderBrowser = FolderBrowserState(),
                queue = QueueState(),
                question = QuestionState(),
                questionList = QuestionListState(),
                answeredQuestions = emptySet(),
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
        if (device != null && id != null) store.setWindowFor(device.channel, id)
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
    // Live state: gist polling
    // -----------------------------------------------------------------------

    override fun onCleared() {
        closeDetailCache()
        channel.live.close()
        super.onCleared()
    }

    private fun startPolling() {
        pollJob?.cancel()
        pollJob = null
        if (!foreground || d.device == null || store.token == null) return
        if (liveRelayEnabled) d.device?.let { device -> linkFor(device)?.let(channel.live::start) }
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

    /** Reads the computer's gist (free when unchanged); returns the delay before the next read. */
    private suspend fun pollOnce(): Long? {
        val device = d.device ?: return null
        val link = linkFor(device)
        if (liveRelayEnabled && link != null) {
            channel.live.start(link)
            if (channel.live.ready(link)) {
                ensureWatch()
                pruneOutbox()
                return 5_000L
            }
        }
        var wait: Long? = STATE_POLL_MS
        try {
            // A 304 still carries the cached gist, which matters right after reselecting a computer.
            val read = gists.read(device.gistId)
            val same = d.device?.let { it.channel == device.channel && it.gistId == device.gistId } == true
            if (same && read.gist != null && read.body != appliedGist) {
                appliedGist = read.body
                applyGist(read)
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
                // The gist was deleted; Praxis makes a new one, found again by channel.
                setBanner("poll", str(R.string.banner_channel_gone, d.device?.name.orEmpty()))
                loadDevices()
                MAX_POLL_BACKOFF_MS
            }
            else -> {
                if (failures >= 2) setBanner("poll", str(R.string.banner_poll_failed, e.message.orEmpty()), error = true)
                minOf(STATE_POLL_MS shl (failures - 1).coerceAtMost(4), MAX_POLL_BACKOFF_MS)
            }
        }
    }

    private suspend fun applyGist(read: GistRead) {
        val gist = read.gist ?: return
        val current = d.device ?: return
        edit { copy(stateApplied = true) }
        val device = read.device
        if (device == null || device.channel != current.channel) {
            // The gist no longer describes this computer; look for it again.
            setBanner("poll", str(R.string.banner_channel_gone, current.name))
            loadDevices()
            return
        }
        edit { copy(device = device) }
        setDevices { copy(devices = devices.map { if (it.channel == device.channel) device else it }) }
        if (!checkPairing(device)) return
        val link = linkFor(device) ?: return
        try {
            if (!channel.live.ready(link)) gists.snapshot(gist, link)?.let(::applySnapshot)
            setBanner("state", null)
        } catch (e: CryptoException) {
            setBanner("state", str(R.string.banner_state_unreadable, device.name), error = true)
        }
        // The window may have just been resolved or changed, which resets tab data.
        loadActiveTabData()
    }

    private fun applySnapshot(snapshot: Snapshot) {
        val previousTime = d.snapshot?.updatedAt
        if (previousTime != null && snapshot.updatedAt != null && snapshot.updatedAt < previousTime) return
        val previousUpdatedAt = d.snapshot?.updatedAtRaw
        edit { copy(snapshot = snapshot) }
        snapshot.status?.let { applyStatus(it, snapshot.watch?.window) }
        if (d.watchMatchesView(snapshot.watch)) {
            val incoming = snapshot.thread
            val history = when {
                incoming != null -> d.history.live(incoming)
                snapshot.threadError != null -> d.history
                else -> TranscriptHistory()
            }
            if (d.history.request != null && history.request == null) cancelHistoryRequest()
            edit { copy(history = history, threadError = snapshot.threadError, threadKnown = true) }
        }
        pruneOutbox()
        val override = d.modeOverride
        if (override != null && d.currentWindow()?.thread?.mode?.current == override.id) {
            edit { copy(modeOverride = null) }
        }
        if (previousUpdatedAt != snapshot.updatedAtRaw) recheckStaleWatch()
    }

    private fun applyStatus(status: Status, preferredWindow: Long?) {
        val previousThread = d.currentWindow()?.thread
        edit { copy(status = status) }
        val currentThread = d.currentWindow()?.thread
        if (currentThread?.sessionId != previousThread?.sessionId) {
            invalidateQuestionRequests()
            edit { copy(models = ModelsState(), queue = QueueState(), question = QuestionState(), questionList = QuestionListState(), answeredQuestions = emptySet()) }
        } else if (currentThread?.model != previousThread?.model && !d.models.changing) {
            edit { copy(models = models.copy(info = models.info?.copy(current = currentThread?.model))) }
        }
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
        val kept = reconcileOutbox(d.outbox, d.thread, d.currentWindow()?.thread?.queued ?: 0, now(), OUTBOX_FALLBACK_MS)
        if (kept != d.outbox) edit { copy(outbox = kept) }
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
                val args = JSONObject().put("seconds", WATCH_SECONDS).put("include_details", false)
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

    /** Runs an op on the selected computer and unwraps the `{ok, result | error}` envelope. */
    private suspend fun praxis(op: String, args: JSONObject = JSONObject(), scope: RemoteViewScope = requestScope()): JSONObject? {
        val device = d.device ?: throw ApiException(ErrorKind.State, str(R.string.error_no_device))
        val link = linkFor(device) ?: throw ApiException(ErrorKind.Unpaired, str(R.string.error_not_paired, device.name))
        val envelope = try {
            channel.serialized {
                if (scope != requestScope()) throw ApiException(ErrorKind.Cancelled, str(R.string.error_switched_device))
                channel.exchange(link, op, args)
            }
        } catch (e: ApiException) {
            if (e.kind == ErrorKind.Unpaired && scope == requestScope()) onUnpaired(device, e.message.orEmpty())
            throw e
        }
        if (scope == requestScope()) edit { copy(lastContact = now()) }
        if (envelope.optBoolean("ok")) return envelope.obj("result")
        throw ApiException(ErrorKind.Praxis, envelope.str("error") ?: str(R.string.error_op_failed, op))
    }

    private fun windowArgs(build: JSONObject.() -> Unit = {}): JSONObject {
        val args = JSONObject()
        d.windowId?.let { args.put("window", it) }
        args.build()
        return args
    }

    private fun viewKey(): String = "${d.device?.channel}:${d.windowId}"

    private fun requestScope() = RemoteViewScope(generation, viewKey(), viewRevision)

    private fun report(e: ApiException, label: String?) {
        // An unpaired computer has already been reported by onUnpaired.
        if (e.kind == ErrorKind.Cancelled || e.kind == ErrorKind.Auth || e.kind == ErrorKind.Unpaired) return
        message(if (e.kind == ErrorKind.Praxis && label != null) "$label: ${e.message}" else e.message)
    }

    /** Runs an op, reports a failure, and re-reads the gist at once on success. */
    private suspend fun act(op: String, args: JSONObject, label: String, scope: RemoteViewScope = requestScope()): Boolean =
        viewScopedAction(
            scope, ::requestScope,
            operation = { praxis(op, args, scope) },
            onSuccess = { requestPoll() },
            onFailure = { report(it, label) },
        )

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

    fun sendPrompt(sendNow: Boolean = false, steer: Boolean = false) {
        val text = composer.trim()
        val images = composerImages
        val draft = imageDraftKey()
        if ((text.isEmpty() && images.isEmpty()) || d.device == null || preparingImage) return
        if (images.isNotEmpty() && d.currentWindow()?.thread?.imageInput != true) {
            message(str(R.string.image_unsupported))
            return
        }
        val scope = requestScope()
        val session = d.currentWindow()?.thread?.sessionId
        val args = windowArgs {
            put("text", text)
            put("send_now", sendNow)
            put("steer", steer)
            session?.let { put("session_id", it) }
        }
        val item = OutboxItem(
            id = ++outboxSeq,
            text = text,
            state = OutboxState.Sending,
            doneAt = 0L,
            session = session,
            baseIndex = d.thread?.takeIf { it.sessionId == session }?.entries?.maxOfOrNull { it.index } ?: -1,
            images = images,
            fingerprint = io.github.dushyantchetiwal.praxis.remote.data.promptFingerprint(text, images),
        )
        edit { copy(outbox = outbox + item) }
        composer = ""
        imageDrafts.remove(draft)
        viewModelScope.launch {
            try {
                if (images.isNotEmpty()) {
                    val target = JSONObject().put("session_id", session)
                    args.opt("window")?.let { target.put("window", it) }
                    val ids = images.map { image -> uploadImage(image, target) { op, upload -> praxis(op, upload, scope) } }
                    args.put("images", org.json.JSONArray(ids))
                }
                val result = praxis("prompt", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (scope != requestScope()) return@launch
                val queued = result.optBoolean("queued")
                edit {
                    copy(outbox = outbox.map {
                        if (it.id == item.id) it.copy(
                            state = if (queued) OutboxState.Queued else OutboxState.Sent,
                            doneAt = now(),
                            session = result.str("session_id") ?: it.session,
                            queueId = result.str("queue_id"),
                            steer = result.optBoolean("steer"),
                        ) else it
                    })
                }
                if (queued) message(str(R.string.toast_queued))
                requestPoll()
            } catch (error: CancellationException) {
                if (images.isNotEmpty() && imageDrafts[draft].isNullOrEmpty()) imageDrafts[draft] = images
                throw error
            } catch (error: Exception) {
                if (scope != requestScope()) {
                    if (images.isNotEmpty() && imageDrafts[draft].isNullOrEmpty()) imageDrafts[draft] = images
                    return@launch
                }
                edit { copy(outbox = outbox.map {
                    if (it.id == item.id) it.copy(state = OutboxState.Unconfirmed, doneAt = now()) else it
                }) }
                val failure = error as? ApiException ?: ApiException(ErrorKind.State, error.message ?: str(R.string.image_failed))
                report(failure, str(R.string.label_message_unconfirmed))
            }
        }
    }

    fun canRestorePromptDraft(item: OutboxItem): Boolean = d.outbox.any {
        it.id == item.id && composerImages.isEmpty() && it.canRestoreDraft(d.currentWindow()?.thread?.sessionId, composer)
    }

    fun restorePromptDraft(item: OutboxItem) {
        val retained = d.outbox.firstOrNull { it.id == item.id } ?: return
        if (composerImages.isNotEmpty() || !retained.canRestoreDraft(d.currentWindow()?.thread?.sessionId, composer)) return
        composer = retained.text
        imageDrafts[imageDraftKey()] = retained.images
        edit { copy(outbox = outbox.filterNot { it.id == retained.id }) }
    }

    fun sendQueuedNow(item: OutboxItem) {
        queueAction(item.queueId ?: return, item.session ?: return)
    }

    fun steerQueuedMessage(item: OutboxItem) {
        queueAction(item.queueId ?: return, item.session ?: return, !item.steer)
    }

    fun queueAction(id: String, session: String, steer: Boolean? = null) {
        if (id in d.queue.busy || d.currentWindow()?.thread?.sessionId != session) return
        val scope = requestScope()
        val args = windowArgs { put("session_id", session); put("queue_id", id); steer?.let { put("steer", it) } }
        edit { copy(
            queue = queue.copy(session = session, busy = queue.busy + id),
            outbox = outbox.map { if (it.queueId == id) it.copy(sendingNow = true) else it },
        ) }
        viewModelScope.launch {
            try {
                val result = praxis(if (steer == null) "send_now" else "steer", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (scope != requestScope() || d.currentWindow()?.thread?.sessionId != session) return@launch
                val found = result.optBoolean(if (steer == null) "sent" else "found")
                edit { copy(
                    queue = queue.copy(busy = queue.busy - id),
                    outbox = if (!found) outbox.filterNot { it.queueId == id } else outbox.map {
                        if (it.queueId == id) it.copy(
                            sendingNow = false, steer = steer ?: it.steer,
                            state = if (steer == null) OutboxState.Sent else it.state, doneAt = now(),
                        ) else it
                    },
                ) }
                if (!found) message(str(R.string.toast_no_longer_queued))
                requestPoll()
                if (d.queue.visible) loadQueue()
            } catch (error: ApiException) {
                if (scope != requestScope() || d.currentWindow()?.thread?.sessionId != session) return@launch
                edit { copy(
                    queue = queue.copy(busy = queue.busy - id, error = error.message ?: str(R.string.queue_failed)),
                    outbox = outbox.map { if (it.queueId == id) it.copy(sendingNow = false) else it },
                ) }
                report(error, str(R.string.label_message_not_sent))
            }
        }
    }

    fun loadQueue(more: Boolean = false) {
        val session = d.currentWindow()?.thread?.sessionId ?: return
        if (d.queue.loading) return
        val request = ++queueRequest
        val scope = requestScope()
        val previous = d.queue.takeIf { more && it.session == session }
        val offset = if (more) previous?.nextOffset ?: return else 0
        val args = windowArgs { put("session_id", session); put("offset", offset) }
        edit { copy(queue = QueueState(session, visible = true, loading = true, entries = previous?.entries.orEmpty(), busy = queue.busy)) }
        viewModelScope.launch {
            try {
                val result = praxis("queue", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (request != queueRequest || scope != requestScope() || d.currentWindow()?.thread?.sessionId != session) return@launch
                val page = parseQueue(result)
                edit { copy(queue = queue.copy(
                    loading = false, entries = (previous?.entries.orEmpty() + page.entries).associateBy { it.id }.values.toList(),
                    nextOffset = page.nextOffset,
                )) }
            } catch (error: ApiException) {
                if (request == queueRequest && scope == requestScope() && d.currentWindow()?.thread?.sessionId == session) {
                    edit { copy(queue = queue.copy(loading = false, error = error.message ?: str(R.string.queue_failed))) }
                }
            }
        }
    }

    fun dismissQueue() {
        queueRequest++
        edit { copy(queue = queue.copy(visible = false, loading = false)) }
    }

    fun browseHostFolder(path: String = "", more: Boolean = false) {
        if (d.folderBrowser.loading || d.folderBrowser.opening) return
        val scope = requestScope()
        val request = ++folderRequest
        val previous = d.folderBrowser.listing.takeIf { more && it?.path == path }
        val offset = if (more) previous?.nextOffset ?: return else 0
        edit { copy(folderBrowser = FolderBrowserState(visible = true, loading = true, listing = previous)) }
        viewModelScope.launch {
            try {
                val result = praxis("host_folders", JSONObject().put("path", path).put("offset", offset), scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (request != folderRequest || scope != requestScope() || !d.folderBrowser.visible) return@launch
                val page = parseHostFolders(result)
                val listing = if (previous == null) page else page.copy(folders = (previous.folders + page.folders).distinctBy { it.path })
                edit { copy(folderBrowser = FolderBrowserState(visible = true, listing = listing)) }
            } catch (error: ApiException) {
                if (request == folderRequest && scope == requestScope() && d.folderBrowser.visible) {
                    edit { copy(folderBrowser = folderBrowser.copy(loading = false, error = error.message)) }
                }
            }
        }
    }

    fun dismissFolderBrowser() {
        folderRequest++
        edit { copy(folderBrowser = FolderBrowserState()) }
    }

    fun openHostFolder(path: String) {
        if (path.isBlank() || d.folderBrowser.loading || d.folderBrowser.opening) return
        val scope = requestScope()
        val request = ++folderRequest
        edit { copy(folderBrowser = folderBrowser.copy(opening = true, error = null)) }
        viewModelScope.launch {
            val ok = act("open_folder", JSONObject().put("path", path), str(R.string.folder_open_failed), scope)
            if (request != folderRequest || scope != requestScope()) return@launch
            edit { copy(folderBrowser = folderBrowser.copy(opening = false, visible = !ok)) }
            if (ok) message(str(R.string.folder_opened))
        }
    }

    fun loadModels(more: Boolean = false) {
        val session = d.currentWindow()?.thread?.sessionId ?: return
        if (d.models.loading || d.models.changing) return
        val previous = d.models.info.takeIf { more && d.models.session == session }
        val offset = if (more) previous?.nextOffset ?: return else 0
        val scope = requestScope()
        val args = windowArgs { put("session_id", session); put("offset", offset) }
        edit { copy(models = ModelsState(session = session, loading = true, info = previous)) }
        viewModelScope.launch {
            try {
                val result = praxis("models", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (scope != requestScope() || d.currentWindow()?.thread?.sessionId != session) return@launch
                val page = parseModels(result)
                edit { copy(models = ModelsState(session = session, info = previous?.append(page) ?: page)) }
            } catch (error: ApiException) {
                if (scope == requestScope() && d.currentWindow()?.thread?.sessionId == session) {
                    edit { copy(models = ModelsState(session = session, info = previous, error = error.message ?: str(R.string.error_unreadable_response))) }
                }
            }
        }
    }

    fun setModel(model: ModelOption) {
        val session = d.models.session ?: return
        if (d.models.loading || d.models.changing || model.disabled || d.currentWindow()?.thread?.sessionId != session) return
        if (d.models.info?.current == model.id) return
        val previous = d.models.info
        val scope = requestScope()
        val args = windowArgs { put("session_id", session); put("model", model.id) }
        edit { copy(models = models.copy(changing = true, error = null)) }
        viewModelScope.launch {
            try {
                val result = praxis("model", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (scope != requestScope() || d.currentWindow()?.thread?.sessionId != session) return@launch
                val selected = parseModels(result)
                edit { copy(models = ModelsState(session = session, info = previous?.copy(current = selected.current) ?: selected)) }
                requestPoll()
            } catch (error: ApiException) {
                if (scope == requestScope() && d.currentWindow()?.thread?.sessionId == session) {
                    edit { copy(models = models.copy(changing = false, error = error.message ?: str(R.string.error_unreadable_response))) }
                }
            }
        }
    }

    suspend fun loadDetail(request: DetailRequest, body: DetailBody): DetailChunk {
        val scope = requestScope()
        val args = request.arguments().put("chunked", true).put("offset", body.nextOffset)
        body.version?.let { args.put("version", it).put("total_bytes", body.totalBytes) }
        d.windowId?.let { args.put("window", it) }
        fun targetVisible(): Boolean = if (request.queueId == null) d.history.isShown(request.session) else d.currentWindow()?.thread?.sessionId == request.session
        if (!targetVisible()) throw CancellationException("Conversation changed")
        val result = praxis(if (request.queueId == null) "thread" else "queue_content", args, scope)
            ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
        if (scope != requestScope() || !targetVisible()) throw CancellationException("Conversation changed")
        if (!result.has("offset")) throw ApiException(ErrorKind.State, str(R.string.details_upgrade))
        return parseDetailChunk(result)
            ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
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
        if (d.startingThread || d.threads.opening != null || d.device == null) return
        val scope = requestScope()
        val args = windowArgs()
        edit { copy(startingThread = true) }
        viewModelScope.launch {
            val ok = act("new_thread", args, str(R.string.label_could_not_start_thread), scope)
            if (scope != requestScope()) return@launch
            edit { copy(startingThread = false) }
            if (!ok) return@launch
            cancelHistoryRequest()
            edit { copy(watchSession = null, history = TranscriptHistory(), threadKnown = false, threads = ThreadsState(), tab = Tab.Chat) }
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

    private fun invalidateQuestionRequests() {
        questionRequest++
        questionListRequest++
        questionReadJob?.cancel()
        questionReadJob = null
    }

    fun openQuestion(header: QuestionHeader) {
        questionReadJob?.cancel()
        val request = ++questionRequest
        val scope = requestScope()
        val args = windowArgs { put("session_id", header.sessionId); put("question_id", header.id) }
        val previous = d.question.takeIf { it.header?.key == header.key }
        edit { copy(question = (previous ?: QuestionState(header = header)).copy(visible = true, loading = true, sending = false, error = null)) }
        questionReadJob = viewModelScope.launch {
            try {
                var body = DetailBody()
                while (!body.complete) {
                    if (request != questionRequest || scope != requestScope()) return@launch
                    args.put("offset", body.nextOffset)
                    body.version?.let { args.put("version", it).put("total_bytes", body.totalBytes) }
                    val result = praxis("question_content", args, scope)
                        ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                    val chunk = parseDetailChunk(result)
                        ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                    body = body.append(chunk)
                        ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                }
                val form = withContext(Dispatchers.Default) {
                    try {
                        parseQuestionForm(JSONObject(body.chunks.joinToString("") { it.text }))
                    } catch (_: org.json.JSONException) {
                        null
                    }
                } ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (request == questionRequest && scope == requestScope()) edit { copy(question = question.copy(loading = false, form = form)) }
            } catch (error: ApiException) {
                if (request == questionRequest && scope == requestScope()) {
                    edit { copy(question = question.copy(loading = false, error = error.message ?: str(R.string.question_failed))) }
                    requestPoll()
                }
            }
        }
    }

    fun dismissQuestion() {
        questionRequest++
        questionReadJob?.cancel()
        questionReadJob = null
        edit { copy(question = question.copy(visible = false, loading = false, sending = false)) }
    }

    fun selectQuestionOption(value: String) {
        val form = d.question.form ?: return
        if (d.question.sending || form.options.none { it.value == value }) return
        edit { copy(question = question.copy(selected = if (!form.allowMultiple) setOf(value) else {
            if (value in question.selected) question.selected - value else question.selected + value
        })) }
    }

    fun editQuestionAnswer(text: String) {
        if (!d.question.sending) edit { copy(question = question.copy(freeform = text)) }
    }

    fun submitQuestion(decline: Boolean = false) {
        val state = d.question
        val header = state.header ?: return
        val form = state.form ?: return
        if (state.sending || state.loading) return
        val content = questionAnswerContent(form, state.selected, state.freeform)
        if (!decline && content == null) return
        val request = questionRequest
        val scope = requestScope()
        val args = windowArgs {
            put("session_id", header.sessionId); put("question_id", header.id)
            if (decline) put("decline", true) else put("content", content)
        }
        edit { copy(question = question.copy(sending = true, error = null)) }
        viewModelScope.launch {
            try {
                val result = praxis("question_answer", args, scope)
                if (result?.optBoolean("answered") != true) throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (request != questionRequest || scope != requestScope()) return@launch
                edit { copy(
                    question = QuestionState(), answeredQuestions = answeredQuestions + header.key,
                    questionList = questionList.copy(questions = questionList.questions.filterNot { it.key == header.key }),
                ) }
                if (d.questionList.visible) loadQuestions()
                requestPoll()
            } catch (error: ApiException) {
                if (request == questionRequest && scope == requestScope()) {
                    edit { copy(question = question.copy(sending = false, error = error.message ?: str(R.string.question_failed))) }
                    requestPoll()
                }
            }
        }
    }

    fun loadQuestions(more: Boolean = false) {
        if (d.questionList.loading) return
        val previous = d.questionList.takeIf { more }
        val offset = if (more) previous?.nextOffset ?: return else 0
        val request = ++questionListRequest
        val scope = requestScope()
        val args = windowArgs { put("offset", offset) }
        edit { copy(questionList = QuestionListState(visible = true, loading = true, questions = previous?.questions.orEmpty())) }
        viewModelScope.launch {
            try {
                val result = praxis("questions", args, scope)
                    ?: throw ApiException(ErrorKind.State, str(R.string.error_unreadable_response))
                if (request != questionListRequest || scope != requestScope()) return@launch
                val page = parseQuestionPage(result)
                edit { copy(questionList = questionList.copy(
                    loading = false, questions = (previous?.questions.orEmpty() + page.questions).distinctBy { it.key }, nextOffset = page.nextOffset,
                )) }
            } catch (error: ApiException) {
                if (request == questionListRequest && scope == requestScope()) edit { copy(questionList = questionList.copy(loading = false, error = error.message ?: str(R.string.question_failed))) }
            }
        }
    }

    fun dismissQuestionList() {
        questionListRequest++
        edit { copy(questionList = questionList.copy(visible = false, loading = false)) }
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

    /** One bounded history request at a time, including across visible step chats. */
    fun loadOlder(session: String, retry: Boolean = false): Long? {
        if (d.device == null || historyJob?.isActive == true || !d.history.canLoad(session, retry)) return null
        val history = d.history.begin(session, ++historySequence, retry)
        val request = history.request ?: return null
        val key = viewKey()
        val gen = generation
        val args = windowArgs {
            put("session_id", session)
            put("before_index", request.before)
            put("include_details", false)
        }
        edit { copy(history = history) }
        historyJob = viewModelScope.launch {
            try {
                val result = praxis("thread", args)
                if (gen != generation || key != viewKey()) return@launch
                edit {
                    copy(history = if (result == null) {
                        this.history.failed(request)
                    } else {
                        this.history.complete(request, parseThreadView(result))
                    })
                }
            } catch (error: ApiException) {
                if (gen != generation || key != viewKey()) return@launch
                edit { copy(history = this.history.failed(request, error.message)) }
            }
        }
        return request.id
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
        if (d.threads.opening != null || d.startingThread || d.device == null) return
        val scope = requestScope()
        val args = windowArgs { put("session_id", thread.sessionId) }
        edit { copy(threads = threads.copy(opening = thread.sessionId)) }
        viewModelScope.launch {
            val ok = act("open_thread", args, str(R.string.label_could_not_open_thread), scope)
            if (scope != requestScope()) return@launch
            edit { copy(threads = threads.copy(opening = null)) }
            if (!ok) return@launch
            cancelHistoryRequest()
            edit {
                copy(
                    threads = threads.copy(items = threads.items?.map { it.copy(active = it.sessionId == thread.sessionId) }),
                    watchSession = thread.sessionId,
                    history = TranscriptHistory(),
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

    /**
     * Downloads one file, piece by piece, into Downloads/Praxis, or to [chosen]
     * where Android needs the user to pick a place. One download runs at a
     * time; each piece is its own request, so chat and other actions still
     * get through while it runs.
     */
    fun download(entry: DirEntry, chosen: Uri? = null) {
        if (entry.dir || d.device == null || d.download?.active == true) return
        val window = d.windowId
        val gen = generation
        val changed = str(R.string.download_changed, entry.name)
        edit { copy(download = DownloadState(entry.path, entry.name)) }
        downloadJob = viewModelScope.launch {
            val sink = try {
                withContext(Dispatchers.IO) { DownloadSink.open(getApplication(), entry.name, chosen) }
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                failDownload(gen, str(R.string.download_cannot_save, entry.name))
                return@launch
            }
            try {
                var received = 0L
                var version: String? = null
                while (true) {
                    val args = JSONObject().put("path", entry.path).put("offset", received)
                    window?.let { args.put("window", it) }
                    val chunk = parseDownloadChunk(praxis("download", args))
                        ?: throw DownloadException(str(R.string.error_unreadable_response))
                    val bytes = checkDownloadChunk(chunk, received, version, changed)
                    version = chunk.version
                    withContext(Dispatchers.IO) { sink.stream.write(bytes) }
                    received += bytes.size
                    if (gen == generation) edit { copy(download = download?.copy(received = received, size = chunk.size)) }
                    if (received >= chunk.size) break
                }
                withContext(Dispatchers.IO) { sink.complete() }
                if (gen == generation) {
                    edit { copy(download = download?.copy(savedUri = sink.uri.toString(), inDownloads = chosen == null)) }
                }
            } catch (e: CancellationException) {
                withContext(NonCancellable + Dispatchers.IO) { sink.abandon() }
                throw e
            } catch (e: Exception) {
                withContext(NonCancellable + Dispatchers.IO) { sink.abandon() }
                val message = when (e) {
                    is ApiException -> if (e.kind == ErrorKind.Cancelled) null else e.message
                    is DownloadException -> e.message
                    is IOException -> str(R.string.download_cannot_save, entry.name)
                    else -> str(R.string.download_failed, entry.name, e.message ?: e.javaClass.simpleName)
                }
                failDownload(gen, message)
            }
        }
    }

    private fun failDownload(gen: Int, message: String?) {
        if (gen != generation) return
        edit { copy(download = if (message == null) null else download?.copy(error = message)) }
    }

    fun cancelDownload() {
        downloadJob?.cancel()
        downloadJob = null
        edit { copy(download = null) }
    }

    fun dismissDownload() {
        if (d.download?.active == true) return
        edit { copy(download = null) }
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
