package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import java.nio.ByteBuffer
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull
import org.webrtc.DataChannel
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription

/** A peer-to-peer link to Praxis. Packets on it are sealed exactly as on the relays. */
interface DirectLink {
    val isOpen: Boolean
    /** Returns false when the link cannot take the message, so the caller uses the relays. */
    fun send(message: ByteArray): Boolean
    fun close()
}

fun interface DirectLinks {
    /** Offers a link, exchanges the session description through [signal], and returns it once negotiated. */
    suspend fun open(onMessage: (ByteArray) -> Unit, signal: suspend (String) -> String): DirectLink
}

// Free public servers that tell each side its public address. Networks that
// block direct links keep using the relays.
private val STUN_SERVERS = listOf("stun:stun.l.google.com:19302", "stun:stun.cloudflare.com:3478")
// The offer carries every address found by then, so no further exchange is needed.
private const val GATHERING_TIMEOUT_MS = 3_000L

class WebRtcLinks(context: Context) : DirectLinks {
    private val appContext = context.applicationContext

    override suspend fun open(onMessage: (ByteArray) -> Unit, signal: suspend (String) -> String): DirectLink {
        val link = WebRtcLink(sharedFactory(appContext), stunServers(), onMessage)
        try {
            link.accept(signal(link.offer()))
            return link
        } catch (error: Throwable) {
            link.close()
            throw error
        }
    }

    companion object {
        private var factory: PeerConnectionFactory? = null

        // The native library may only be initialized once per process.
        @Synchronized internal fun sharedFactory(context: Context): PeerConnectionFactory = factory ?: run {
            PeerConnectionFactory.initialize(PeerConnectionFactory.InitializationOptions.builder(context.applicationContext).createInitializationOptions())
            PeerConnectionFactory.builder().createPeerConnectionFactory().also { factory = it }
        }

        internal fun stunServers(): List<PeerConnection.IceServer> =
            listOf(PeerConnection.IceServer.builder(STUN_SERVERS).createIceServer())
    }
}

internal fun rtcConfiguration(iceServers: List<PeerConnection.IceServer>) = PeerConnection.RTCConfiguration(iceServers).apply {
    sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
    continualGatheringPolicy = PeerConnection.ContinualGatheringPolicy.GATHER_ONCE
}

internal suspend fun describe(create: (SdpObserver) -> Unit): SessionDescription = suspendCancellableCoroutine { continuation ->
    create(object : SdpObserver {
        override fun onCreateSuccess(description: SessionDescription?) {
            if (description != null) continuation.resume(description)
            else continuation.resumeWithException(ApiException(ErrorKind.Network, "The direct connection produced no session description."))
        }
        override fun onSetSuccess() {}
        override fun onCreateFailure(error: String?) {
            continuation.resumeWithException(ApiException(ErrorKind.Network, "The direct connection could not start: $error"))
        }
        override fun onSetFailure(error: String?) {}
    })
}

internal suspend fun settle(apply: (SdpObserver) -> Unit): Unit = suspendCancellableCoroutine { continuation ->
    apply(object : SdpObserver {
        override fun onCreateSuccess(description: SessionDescription?) {}
        override fun onSetSuccess() { continuation.resume(Unit) }
        override fun onCreateFailure(error: String?) {}
        override fun onSetFailure(error: String?) {
            continuation.resumeWithException(ApiException(ErrorKind.Network, "The direct connection was refused: $error"))
        }
    })
}

/** Observer methods a data-channel-only peer never needs. */
internal open class DataOnlyObserver : PeerConnection.Observer {
    override fun onSignalingChange(state: PeerConnection.SignalingState?) {}
    override fun onIceConnectionChange(state: PeerConnection.IceConnectionState?) {}
    override fun onIceConnectionReceivingChange(receiving: Boolean) {}
    override fun onIceGatheringChange(state: PeerConnection.IceGatheringState?) {}
    override fun onIceCandidate(candidate: IceCandidate?) {}
    override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>?) {}
    override fun onAddStream(stream: MediaStream?) {}
    override fun onRemoveStream(stream: MediaStream?) {}
    override fun onDataChannel(channel: DataChannel?) {}
    override fun onRenegotiationNeeded() {}
}

internal class WebRtcLink(
    factory: PeerConnectionFactory,
    iceServers: List<PeerConnection.IceServer>,
    private val onMessage: (ByteArray) -> Unit,
) : DirectLink {
    private val gathered = CompletableDeferred<Unit>()
    @Volatile private var connected = false
    private var closed = false
    private val peer: PeerConnection = factory.createPeerConnection(rtcConfiguration(iceServers), object : DataOnlyObserver() {
        override fun onIceGatheringChange(state: PeerConnection.IceGatheringState?) {
            if (state == PeerConnection.IceGatheringState.COMPLETE) gathered.complete(Unit)
        }
        override fun onIceConnectionChange(state: PeerConnection.IceConnectionState?) {
            connected = state == PeerConnection.IceConnectionState.CONNECTED || state == PeerConnection.IceConnectionState.COMPLETED
        }
    }) ?: throw ApiException(ErrorKind.Network, "This phone cannot open direct connections.")
    // The phone creates the channel so the offer already describes it.
    private val channel: DataChannel = peer.createDataChannel("praxis", DataChannel.Init().apply { ordered = true })

    init {
        channel.registerObserver(object : DataChannel.Observer {
            override fun onBufferedAmountChange(previousAmount: Long) {}
            override fun onStateChange() {}
            override fun onMessage(buffer: DataChannel.Buffer) {
                val bytes = ByteArray(buffer.data.remaining())
                buffer.data.get(bytes)
                onMessage(bytes)
            }
        })
    }

    suspend fun offer(): String {
        val offer = describe { peer.createOffer(it, MediaConstraints()) }
        settle { peer.setLocalDescription(it, offer) }
        withTimeoutOrNull(GATHERING_TIMEOUT_MS) { gathered.await() }
        return synchronized(this) { if (closed) null else peer.localDescription?.description }
            ?: throw ApiException(ErrorKind.Network, "The direct connection produced no session offer.")
    }

    suspend fun accept(answer: String) {
        settle { peer.setRemoteDescription(it, SessionDescription(SessionDescription.Type.ANSWER, answer)) }
    }

    override val isOpen: Boolean
        @Synchronized get() = !closed && connected && channel.state() == DataChannel.State.OPEN

    @Synchronized override fun send(message: ByteArray): Boolean =
        isOpen && channel.send(DataChannel.Buffer(ByteBuffer.wrap(message), true))

    @Synchronized override fun close() {
        if (closed) return
        closed = true
        channel.unregisterObserver()
        channel.close()
        channel.dispose()
        peer.dispose()
    }
}
