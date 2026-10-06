package io.github.dushyantchetiwal.praxis.remote.data

import android.util.Log
import java.time.Duration
import java.time.Instant
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject
import org.nostrdevkit.sdk.Client
import org.nostrdevkit.sdk.AckPolicy
import org.nostrdevkit.sdk.ClientNotification
import org.nostrdevkit.sdk.EventBuilder
import org.nostrdevkit.sdk.Filter
import org.nostrdevkit.sdk.Keys
import org.nostrdevkit.sdk.Kind
import org.nostrdevkit.sdk.RelayUrl
import org.nostrdevkit.sdk.ReqTarget
import org.nostrdevkit.sdk.Tag

internal const val RELAY_KIND: UShort = 21761u
internal const val RELAY_PACKET_LIMIT = 96_000
private val RELAYS = listOf("wss://relay.damus.io", "wss://relay.primal.net")

internal fun relayAad(link: Link, direction: String): String =
    "$PROTOCOL/nostr/${link.channel}/${link.phoneId}/$direction"

internal fun relaySecret(link: Link, role: String): ByteArray = RemoteCrypto.hkdfExpand(
    RemoteCrypto.hkdfExtract("praxis-remote/nostr/v1".toByteArray(), link.key),
    relayAad(link, role).toByteArray(), 32,
)

internal fun sameRelayPeer(first: Link, second: Link): Boolean =
    first.channel == second.channel && first.phoneId == second.phoneId && first.key.contentEquals(second.key)

internal class RelayCursor {
    var epoch: String? = null
        private set
    private var sequence = 0L

    @Synchronized fun handshake(value: String): Boolean {
        if (!isHexId(value)) return false
        if (epoch != value) { epoch = value; sequence = 0 }
        return true
    }

    @Synchronized fun accept(packet: JSONObject): Boolean {
        val incoming = packet.long("sequence") ?: return false
        if (packet.str("epoch") != epoch || incoming <= sequence) return false
        sequence = incoming
        return true
    }
}

data class LiveSnapshot(val channel: String, val phoneId: String, val snapshot: Snapshot)
data class LiveConnection(val channel: String, val connected: Boolean, val epoch: String? = null)

/** Relays only carry the existing paired channel's authenticated ciphertext. */
class NostrChannel {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    @Volatile private var session: Session? = null
    private val _snapshots = MutableSharedFlow<LiveSnapshot>(extraBufferCapacity = 1)
    val snapshots = _snapshots.asSharedFlow()
    private val _connection = MutableStateFlow<LiveConnection?>(null)
    val connection = _connection.asStateFlow()

    fun start(link: Link) {
        val previous = session
        if (previous?.job?.isActive == true && sameRelayPeer(previous.link, link)) return
        stop()
        val next = Session(link)
        session = next
        next.job = scope.launch { next.run() }
    }

    fun stop() {
        session?.job?.cancel()
        session = null
        _connection.value = null
    }

    fun close() { stop(); scope.cancel() }

    fun ready(link: Link): Boolean = session?.let { sameRelayPeer(it.link, link) && it.ready() } == true

    // Null means no command was submitted. Once submitted, every failure is surfaced,
    // never converted into a second dispatch over GitHub.
    suspend fun exchange(link: Link, id: String, op: String, args: JSONObject): JSONObject? {
        if (op == "unpair" || op == "batch") return null
        // Check the captured session, not a second read that may refer to a
        // different computer after a concurrent selection change.
        val active = session?.takeIf { sameRelayPeer(it.link, link) && it.ready() } ?: return null
        return active.exchange(id, op, args)
    }

    private inner class Session(val link: Link) {
        @Volatile var job: Job? = null
        private var client: Client? = null
        private var keys: Keys? = null
        private var desktop: Keys? = null
        private val pending = ConcurrentHashMap<String, CompletableDeferred<JSONObject>>()
        private val cursor = RelayCursor()
        @Volatile private var epoch: String? = null
        @Volatile private var lastHello = 0L

        fun ready(): Boolean {
            val last = lastHello
            return epoch != null && last != 0L && System.nanoTime() - last in 0 until 30_000_000_000L && job?.isActive == true
        }

        suspend fun run() {
            var receiver: Job? = null
            try {
                val own = Keys.parse(relaySecret(link, "phone").joinToString("") { "%02x".format(it) })
                keys = own
                val peer = Keys.parse(relaySecret(link, "desktop").joinToString("") { "%02x".format(it) })
                desktop = peer
                val transport = Client()
                client = transport
                val notifications = transport.notifications()
                RELAYS.forEach { transport.addRelay(RelayUrl.parse(it)) }
                receiver = scope.launch {
                    try {
                        while (isActive) {
                            val notification = notifications.next() ?: break
                            if (notification !is ClientNotification.NewEvent) continue
                            val event = notification.event
                            try {
                                if (event.content().length > RELAY_PACKET_LIMIT || !event.verify() || event.author().toHex() != peer.publicKey().toHex()) continue
                                if (event.kind().use { it.asU16() } != RELAY_KIND) continue
                                if (event.tags().none { it.toVec() == listOf("p", own.publicKey().toHex()) }) continue
                                val plain = RemoteCrypto.open(link.key, event.content(), relayAad(link, "desktop"))
                                val packet = JSONObject(String(plain, Charsets.UTF_8))
                                when (packet.str("type")) {
                                    "response" -> packet.str("id")?.let { pending[it]?.complete(packet) }
                                    "snapshot" -> if (cursor.accept(packet)) {
                                        parseSnapshot(packet.obj("snapshot")?.toString())?.takeIf { session === this@Session }?.let {
                                            _snapshots.emit(LiveSnapshot(link.channel, link.phoneId, it))
                                        }
                                    }
                                }
                            } catch (error: CancellationException) { throw error }
                            catch (error: Exception) { Log.w("PraxisRemote", "Ignored an invalid live packet", error) }
                            finally { event.close() }
                        }
                    } finally { notifications.close() }
                }
                transport.connect(Duration.ofSeconds(8))
                transport.subscribe(ReqTarget.auto(listOf(Filter().author(peer.publicKey()).pubkey(own.publicKey()).kind(Kind(RELAY_KIND)).limit(0uL))), id = "praxis-live")
                var failures = 0
                while (kotlinx.coroutines.currentCoroutineContext().isActive) {
                    val answer = try {
                        withTimeoutOrNull(6_000) { exchange(UUID.randomUUID().toString(), "hello", JSONObject(), attempts = 1) }
                    } catch (error: CancellationException) { throw error }
                    catch (error: Exception) { Log.d("PraxisRemote", "Live handshake will retry", error); null }
                    val newEpoch = answer?.takeIf { it.bool("ok") }?.obj("result")?.str("epoch")
                    if (newEpoch != null && cursor.handshake(newEpoch)) {
                        epoch = newEpoch
                        lastHello = System.nanoTime()
                        failures = 0
                        if (session === this) _connection.value = LiveConnection(link.channel, true, newEpoch)
                        delay(15_000)
                    } else {
                        lastHello = 0
                        if (session === this) _connection.value = LiveConnection(link.channel, false)
                        failures++
                        delay(minOf(30_000L, 2_000L shl failures.coerceAtMost(4)))
                    }
                }
            } catch (error: CancellationException) { throw error }
            catch (error: LinkageError) { Log.w("PraxisRemote", "Live transport is unavailable on this device; keeping GitHub fallback", error) }
            catch (error: Exception) { Log.w("PraxisRemote", "Live transport failed; keeping GitHub fallback", error) }
            finally {
                lastHello = 0
                receiver?.cancel()
                pending.values.forEach { it.completeExceptionally(ApiException(ErrorKind.Network, "The live connection closed. Check the conversation before resending.")) }
                pending.clear()
                withContext(NonCancellable) {
                    receiver?.cancelAndJoin()
                    withTimeoutOrNull(5_000) { client?.shutdown() }
                }
                client?.close()
                keys?.close()
                desktop?.close()
                if (session === this) _connection.value = LiveConnection(link.channel, false)
            }
        }

        suspend fun exchange(id: String, op: String, args: JSONObject, attempts: Int = 3): JSONObject {
            val transport = client ?: throw ApiException(ErrorKind.Network, "The live connection is not ready.")
            val own = keys ?: throw ApiException(ErrorKind.Network, "The live connection is not ready.")
            val peer = desktop ?: throw ApiException(ErrorKind.Network, "The live connection is not ready.")
            val requestEpoch = epoch
            val payload = JSONObject().put("id", id).put("op", op).put("args", args)
                .put("sent_at", Instant.now().toString()).put("epoch", requestEpoch)
            val plain = payload.toString().toByteArray(Charsets.UTF_8)
            if (plain.size > 64_000) throw ApiException(ErrorKind.State, "The live request is too large. Send smaller chunks.")
            val waiting = CompletableDeferred<JSONObject>()
            pending[id] = waiting
            try {
                repeat(attempts) {
                    val blob = RemoteCrypto.seal(link.key, plain, relayAad(link, "request"))
                    val event = EventBuilder(Kind(RELAY_KIND), blob).tags(listOf(
                        Tag.publicKey(peer.publicKey()),
                        Tag.parse(listOf("salt", UUID.randomUUID().toString())),
                    )).finalize(own)
                    try {
                        withTimeoutOrNull(3_000) { transport.sendEvent(event, ackPolicy = AckPolicy.none(), okTimeout = Duration.ofSeconds(2)) }
                    } catch (error: CancellationException) { throw error }
                    catch (error: Exception) { Log.d("PraxisRemote", "Waiting for the desktop receipt after relay publication failure", error) }
                    finally { event.close() }
                    val answer = withTimeoutOrNull(if (op == "hello") 2_000 else 8_000) { waiting.await() }
                    if (answer != null) {
                        if (op != "hello" && answer.str("epoch") != requestEpoch) {
                            throw ApiException(ErrorKind.Timeout, "Praxis restarted. Check whether the message arrived before sending it again.")
                        }
                        return answer
                    }
                }
            } finally { pending.remove(id) }
            if (op == "hello") return JSONObject().put("ok", false)
            throw ApiException(ErrorKind.Timeout, "No desktop receipt yet. The request may have executed; check the conversation before resending. It was not resent through GitHub.")
        }
    }
}
