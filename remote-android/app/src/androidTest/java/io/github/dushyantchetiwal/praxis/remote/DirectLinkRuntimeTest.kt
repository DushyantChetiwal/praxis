package io.github.dushyantchetiwal.praxis.remote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.dushyantchetiwal.praxis.remote.data.DataOnlyObserver
import io.github.dushyantchetiwal.praxis.remote.data.WebRtcLink
import io.github.dushyantchetiwal.praxis.remote.data.WebRtcLinks
import io.github.dushyantchetiwal.praxis.remote.data.describe
import io.github.dushyantchetiwal.praxis.remote.data.rtcConfiguration
import io.github.dushyantchetiwal.praxis.remote.data.settle
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.webrtc.DataChannel
import org.webrtc.MediaConstraints
import org.webrtc.PeerConnection
import org.webrtc.SessionDescription

@RunWith(AndroidJUnit4::class)
class DirectLinkRuntimeTest {
    private fun DataChannel.Buffer.text(): String {
        val bytes = ByteArray(data.remaining())
        data.get(bytes)
        return String(bytes)
    }

    /** Praxis's side is played by a second peer that answers, as the desktop does. */
    @Test fun directLinkCarriesMessagesBothWays() = runBlocking {
        val factory = WebRtcLinks.sharedFactory(InstrumentationRegistry.getInstrumentation().targetContext)
        val desktopInbox = LinkedBlockingQueue<String>()
        val desktopChannel = AtomicReference<DataChannel?>()
        val gathered = CompletableDeferred<Unit>()
        val desktop = factory.createPeerConnection(rtcConfiguration(emptyList()), object : DataOnlyObserver() {
            override fun onIceGatheringChange(state: PeerConnection.IceGatheringState?) {
                if (state == PeerConnection.IceGatheringState.COMPLETE) gathered.complete(Unit)
            }
            override fun onDataChannel(channel: DataChannel?) {
                channel ?: return
                channel.registerObserver(object : DataChannel.Observer {
                    override fun onBufferedAmountChange(previousAmount: Long) {}
                    override fun onStateChange() {}
                    override fun onMessage(buffer: DataChannel.Buffer) { desktopInbox.add(buffer.text()) }
                })
                desktopChannel.set(channel)
            }
        })!!
        val phoneInbox = LinkedBlockingQueue<String>()
        val link = WebRtcLink(factory, emptyList()) { phoneInbox.add(String(it)) }
        try {
            val offer = link.offer()
            settle { desktop.setRemoteDescription(it, SessionDescription(SessionDescription.Type.OFFER, offer)) }
            val answer = describe { desktop.createAnswer(it, MediaConstraints()) }
            settle { desktop.setLocalDescription(it, answer) }
            withTimeout(10_000) { gathered.await() }
            link.accept(desktop.localDescription.description)
            withTimeout(20_000) {
                while (!link.isOpen || desktopChannel.get()?.state() != DataChannel.State.OPEN) delay(50)
            }

            assertTrue(link.send("from the phone".toByteArray()))
            assertEquals("from the phone", desktopInbox.poll(10, TimeUnit.SECONDS))
            val channel = desktopChannel.get()!!
            assertTrue(channel.send(DataChannel.Buffer(ByteBuffer.wrap("from Praxis".toByteArray()), true)))
            assertEquals("from Praxis", phoneInbox.poll(10, TimeUnit.SECONDS))
        } finally {
            link.close()
            assertFalse(link.isOpen)
            assertFalse("a closed link leaves packets to the relays", link.send("after closing".toByteArray()))
            desktopChannel.get()?.let { it.unregisterObserver(); it.dispose() }
            desktop.dispose()
        }
    }
}
