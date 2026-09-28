package io.github.dushyantchetiwal.praxis.remote.data

import java.math.BigInteger
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.fail
import org.junit.Test

/** The test vectors of docs/src/ai/praxis-remote-protocol.md. */
class RemoteCryptoTest {
    private val phonePrivate = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721"
    private val desktopPrivate = "1d9a8f1f4f3fc2a7f3b0e5e2c6e3c7b1a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4"
    private val channel = "0123456789abcdef0123456789abcdef"
    private val phoneId = "fedcba9876543210fedcba9876543210"
    private val phonePublic = "BGD+1LolWp0xyWHrdMY1bWjASbiSO2H6bOZpYi5g8p+2eQP+EAi4vJmkGunpVii8ZPLxsgwtfp9Rd6PClNRGIpk="
    private val desktopPublic = "BB79lpmknsBRWuk+JOVoyVb/iHJBHpv3ntw3J4jmMZLPcet9LblSM21sa00e9K3he8jl0LJoH0KUdF0rVly6OYY="
    private val commit = "OUHlaa0veVAxCFLJFkMhNYff3Kr3bKcvpSN+zQPJeLY="
    private val shared = "e276d9ef83f4744188147d5ad3d2bc93a5bff1dbb1a079599e29b823154e85c7"
    private val transcript = "cfd62945d28a04d0bd609de1268684b94716abb7e19e9585bd8e4b73cc71748c"
    private val key = "da9dd9a4d0c328b1d923cc9b4635d7be4cba432a964aec4ca8a1300efacc6646"
    private val code = "807021"

    private val nonce = "000102030405060708090a0b"
    private val requestPlain = """{"id":"r1","op":"status","args":{},"sent_at":"2026-01-01T00:00:00Z"}"""
    private val requestBlob =
        "AAECAwQFBgcICQoLUqeO9tn6GdeJsQhhTZTZJ534qUbzV9GRWtzN83LowPpUOoA3QnYETibYcB6JwcZqAKMwPp0/eA4COaYU6ZzNZ95NG59AAfXNKN8SIyHZ302C2I4Z"

    private fun b64(text: String) = RemoteCrypto.unbase64(text)
    private fun hex(text: String) = RemoteCrypto.unhex(text)
    private val requestAad get() = RemoteCrypto.requestAad(channel, phoneId)

    @Test
    fun publicKeysFollowFromPrivateKeys() {
        assertEquals(phonePublic, RemoteCrypto.base64(P256.publicPoint(hex(phonePrivate))))
        assertEquals(desktopPublic, RemoteCrypto.base64(P256.publicPoint(hex(desktopPrivate))))
    }

    @Test
    fun pointsRoundTrip() {
        for (point in listOf(phonePublic, desktopPublic)) {
            assertEquals(point, RemoteCrypto.base64(RemoteCrypto.encodePoint(RemoteCrypto.decodePoint(b64(point)))))
        }
    }

    @Test
    fun freshKeysAreUncompressedPoints() {
        val fresh = RemoteCrypto.newKey()
        assertEquals(65, fresh.public.size)
        assertEquals(0x04.toByte(), fresh.public[0])
        RemoteCrypto.decodePoint(fresh.public)
    }

    @Test
    fun rejectsPointsOffTheCurve() {
        val bad = b64(phonePublic).also { it[64] = (it[64].toInt() xor 1).toByte() }
        assertThrows { RemoteCrypto.decodePoint(bad) }
        assertThrows { RemoteCrypto.decodePoint(b64(phonePublic).copyOf(64)) }
        val compressedPrefix = b64(phonePublic).also { it[0] = 0x02 }
        assertThrows { RemoteCrypto.decodePoint(compressedPrefix) }
    }

    @Test
    fun commitMatches() {
        assertEquals(commit, RemoteCrypto.base64(RemoteCrypto.commit(b64(phonePublic))))
    }

    @Test
    fun sharedSecretMatchesFromBothSides() {
        val fromPhone = RemoteCrypto.sharedSecret(RemoteCrypto.privateKey(hex(phonePrivate)), b64(desktopPublic))
        val fromDesktop = RemoteCrypto.sharedSecret(RemoteCrypto.privateKey(hex(desktopPrivate)), b64(phonePublic))
        assertEquals(shared, RemoteCrypto.hex(fromPhone))
        assertEquals(shared, RemoteCrypto.hex(fromDesktop))
    }

    @Test
    fun transcriptMatches() {
        val hash = RemoteCrypto.transcript(channel, phoneId, b64(phonePublic), b64(desktopPublic))
        assertEquals(transcript, RemoteCrypto.hex(hash))
    }

    @Test
    fun keyAndCodeMatch() {
        val secrets = RemoteCrypto.derive(hex(shared), hex(transcript))
        assertEquals(key, RemoteCrypto.hex(secrets.key))
        assertEquals(code, secrets.code)
        assertEquals("807 021", RemoteCrypto.formatCode(secrets.code))
    }

    @Test
    fun phoneSideDerivesTheSameSecrets() {
        val phone = RemoteCrypto.pairingKey(hex(phonePrivate), b64(phonePublic))
        val secrets = RemoteCrypto.phoneSecrets(phone, b64(desktopPublic), channel, phoneId)
        assertEquals(key, RemoteCrypto.hex(secrets.key))
        assertEquals(code, secrets.code)
    }

    @Test
    fun hkdfMatchesRfc5869TestCase1() {
        val ikm = ByteArray(22) { 0x0b }
        val salt = hex("000102030405060708090a0b0c")
        val info = hex("f0f1f2f3f4f5f6f7f8f9")
        val prk = RemoteCrypto.hkdfExtract(salt, ikm)
        assertEquals("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5", RemoteCrypto.hex(prk))
        assertEquals(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
            RemoteCrypto.hex(RemoteCrypto.hkdfExpand(prk, info, 42)),
        )
    }

    @Test
    fun decryptsTheRequestVector() {
        val plain = RemoteCrypto.open(hex(key), requestBlob, requestAad)
        assertEquals(requestPlain, String(plain, Charsets.UTF_8))
    }

    @Test
    fun encryptsTheRequestVectorWithItsNonce() {
        val blob = RemoteCrypto.seal(hex(key), requestPlain.toByteArray(Charsets.UTF_8), requestAad, hex(nonce))
        assertEquals(requestBlob, blob)
    }

    @Test
    fun randomNoncesRoundTrip() {
        val a = RemoteCrypto.seal(hex(key), requestPlain.toByteArray(), requestAad)
        val b = RemoteCrypto.seal(hex(key), requestPlain.toByteArray(), requestAad)
        assertNotEquals(a, b)
        assertArrayEquals(requestPlain.toByteArray(), RemoteCrypto.open(hex(key), a, requestAad))
    }

    @Test
    fun tamperingIsDetected() {
        val bytes = b64(requestBlob)
        for (index in listOf(0, 11, 12, 40, bytes.size - 1)) {
            val altered = bytes.copyOf().also { it[index] = (it[index].toInt() xor 0x01).toByte() }
            assertThrows { RemoteCrypto.open(hex(key), RemoteCrypto.base64(altered), requestAad) }
        }
        assertThrows { RemoteCrypto.open(hex(key), RemoteCrypto.base64(bytes.copyOf(bytes.size - 1)), requestAad) }
        assertThrows { RemoteCrypto.open(hex(key), RemoteCrypto.base64(bytes.copyOf(20)), requestAad) }
        assertThrows { RemoteCrypto.open(hex(key), "not base64!", requestAad) }
    }

    @Test
    fun wrongAadOrKeyIsRejected() {
        val other = "00112233445566778899aabbccddeeff"
        assertThrows { RemoteCrypto.open(hex(key), requestBlob, RemoteCrypto.stateAad(channel, phoneId)) }
        assertThrows { RemoteCrypto.open(hex(key), requestBlob, RemoteCrypto.requestAad(channel, other)) }
        assertThrows { RemoteCrypto.open(hex(key), requestBlob, RemoteCrypto.requestAad(other, phoneId)) }
        assertThrows { RemoteCrypto.open(hex(key), requestBlob, RemoteCrypto.responseAad(channel, phoneId, "r1")) }
        val wrongKey = hex(key).also { it[0] = (it[0].toInt() xor 1).toByte() }
        assertThrows { RemoteCrypto.open(wrongKey, requestBlob, requestAad) }
    }

    @Test
    fun aadStrings() {
        assertEquals(
            "praxis-remote/v2/state/0123456789abcdef0123456789abcdef/fedcba9876543210fedcba9876543210",
            RemoteCrypto.stateAad(channel, phoneId),
        )
        assertEquals(
            "praxis-remote/v2/request/0123456789abcdef0123456789abcdef/fedcba9876543210fedcba9876543210",
            RemoteCrypto.requestAad(channel, phoneId),
        )
        assertEquals(
            "praxis-remote/v2/response/0123456789abcdef0123456789abcdef/fedcba9876543210fedcba9876543210/r1",
            RemoteCrypto.responseAad(channel, phoneId, "r1"),
        )
    }

    @Test
    fun idsAre32HexDigits() {
        val id = RemoteCrypto.newId()
        assertEquals(32, id.length)
        assertEquals(id, id.lowercase().filter { it in "0123456789abcdef" })
    }

    private fun assertThrows(block: () -> Unit) {
        try {
            block()
        } catch (_: CryptoException) {
            return
        }
        fail("Expected a CryptoException")
    }
}

/** Textbook P-256 scalar multiplication, only to check the public keys in the vectors. */
private object P256 {
    private val p = BigInteger("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff", 16)
    private val a = p - BigInteger.valueOf(3)
    private val gx = BigInteger("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296", 16)
    private val gy = BigInteger("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5", 16)

    private data class Point(val x: BigInteger, val y: BigInteger)

    private fun add(l: Point?, r: Point?): Point? {
        if (l == null) return r
        if (r == null) return l
        val slope = if (l == r) {
            if (l.y.signum() == 0) return null
            (l.x.pow(2) * BigInteger.valueOf(3) + a) * (l.y * BigInteger.TWO).modInverse(p)
        } else {
            if (l.x == r.x) return null
            (r.y - l.y) * (r.x - l.x).modInverse(p)
        }.mod(p)
        val x = (slope.pow(2) - l.x - r.x).mod(p)
        val y = (slope * (l.x - x) - l.y).mod(p)
        return Point(x, y)
    }

    fun publicPoint(scalar: ByteArray): ByteArray {
        val k = BigInteger(1, scalar)
        var result: Point? = null
        var addend: Point? = Point(gx, gy)
        for (bit in 0 until k.bitLength()) {
            if (k.testBit(bit)) result = add(result, addend)
            addend = add(addend, addend)
        }
        val point = result!!
        fun fixed(v: BigInteger): ByteArray {
            val bytes = v.toByteArray()
            return if (bytes.size >= 32) bytes.copyOfRange(bytes.size - 32, bytes.size) else ByteArray(32 - bytes.size) + bytes
        }
        return byteArrayOf(0x04) + fixed(point.x) + fixed(point.y)
    }
}
