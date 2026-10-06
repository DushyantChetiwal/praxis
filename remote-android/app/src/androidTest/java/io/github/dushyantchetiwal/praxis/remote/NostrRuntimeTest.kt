package io.github.dushyantchetiwal.praxis.remote

import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.dushyantchetiwal.praxis.remote.data.Link
import io.github.dushyantchetiwal.praxis.remote.data.RELAY_KIND
import io.github.dushyantchetiwal.praxis.remote.data.RemoteCrypto
import io.github.dushyantchetiwal.praxis.remote.data.relayAad
import io.github.dushyantchetiwal.praxis.remote.data.relaySecret
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.nostrdevkit.sdk.Client
import org.nostrdevkit.sdk.EventBuilder
import org.nostrdevkit.sdk.Keys
import org.nostrdevkit.sdk.Kind
import org.nostrdevkit.sdk.Tag

@RunWith(AndroidJUnit4::class)
class NostrRuntimeTest {
    @Test fun nativeSigningAndEncryptionLoadOnAndroid() = runBlocking {
        Keys.parse("0".repeat(63) + "1").use { known ->
            assertEquals("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798", known.publicKey().use { it.toHex() })
        }
        val link = Link("fixture", "a".repeat(32), "b".repeat(32), ByteArray(32) { 7 }, "Synthetic fixture")
        val secret = relaySecret(link, "phone").joinToString("") { "%02x".format(it) }
        val peerSecret = relaySecret(link, "desktop").joinToString("") { "%02x".format(it) }
        assertNotEquals(secret, peerSecret)
        Keys.parse(secret).use { sender ->
            Keys.parse(peerSecret).use { peer ->
                val plaintext = "Synthetic Android interoperability check".toByteArray()
                val blob = RemoteCrypto.seal(link.key, plaintext, relayAad(link, "request"))
                EventBuilder(Kind(RELAY_KIND), blob).tags(listOf(Tag.publicKey(peer.publicKey()))).finalize(sender).use { event ->
                    assertTrue(event.verify())
                    assertArrayEquals(plaintext, RemoteCrypto.open(link.key, event.content(), relayAad(link, "request")))
                }
            }
        }
        // Exercise UniFFI's coroutine bridge without contacting public relays.
        Client().use { it.shutdown() }
    }
}
