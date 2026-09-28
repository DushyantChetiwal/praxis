package io.github.dushyantchetiwal.praxis.remote.data

import java.math.BigInteger
import java.security.GeneralSecurityException
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.SecureRandom
import java.security.interfaces.ECPublicKey
import java.security.spec.ECFieldFp
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECParameterSpec
import java.security.spec.ECPoint
import java.security.spec.ECPrivateKeySpec
import java.security.spec.ECPublicKeySpec
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyAgreement
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

// The cryptography of docs/src/ai/praxis-remote-protocol.md. Plain JVM code
// with no Android APIs, so it runs in local unit tests against the spec's
// test vectors.

class CryptoException(message: String, cause: Throwable? = null) : Exception(message, cause)

/** A P-256 key pair; [public] is the 65-byte uncompressed point `0x04 || X || Y`. */
class PairingKey internal constructor(internal val private: PrivateKey, val public: ByteArray)

/** What both sides derive at pairing: the 32-byte AES key and the six-digit code. */
class PairingSecrets(val key: ByteArray, val code: String)

object RemoteCrypto {
    const val KEY_BYTES = 32
    const val POINT_BYTES = 65
    private const val NONCE_BYTES = 12
    private const val TAG_BYTES = 16

    private val random = SecureRandom()

    /** The P-256 domain parameters, taken from the platform's own key generator. */
    private val params: ECParameterSpec by lazy {
        (generator().generateKeyPair().public as ECPublicKey).params
    }

    private fun generator(): KeyPairGenerator =
        KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp256r1"), random) }

    fun newKey(): PairingKey {
        val pair = generator().generateKeyPair()
        return PairingKey(pair.private, encodePoint(pair.public as ECPublicKey))
    }

    /** A private key from its 32-byte scalar (for the test vectors). */
    internal fun privateKey(scalar: ByteArray): PrivateKey =
        KeyFactory.getInstance("EC").generatePrivate(ECPrivateKeySpec(BigInteger(1, scalar), params))

    internal fun pairingKey(scalar: ByteArray, public: ByteArray): PairingKey = PairingKey(privateKey(scalar), public)

    // -----------------------------------------------------------------------
    // Points
    // -----------------------------------------------------------------------

    fun encodePoint(key: ECPublicKey): ByteArray {
        val out = ByteArray(POINT_BYTES)
        out[0] = 0x04
        unsigned(key.w.affineX).copyInto(out, 1)
        unsigned(key.w.affineY).copyInto(out, 1 + 32)
        return out
    }

    /** Imports an uncompressed point, rejecting anything that is not on P-256. */
    fun decodePoint(bytes: ByteArray): ECPublicKey {
        if (bytes.size != POINT_BYTES || bytes[0] != 0x04.toByte()) {
            throw CryptoException("A public key must be a 65-byte uncompressed P-256 point")
        }
        val x = BigInteger(1, bytes.copyOfRange(1, 33))
        val y = BigInteger(1, bytes.copyOfRange(33, 65))
        val curve = params.curve
        val p = (curve.field as ECFieldFp).p
        if (x >= p || y >= p) throw CryptoException("The public key is not on P-256")
        val left = y.multiply(y).mod(p)
        val right = x.multiply(x).multiply(x).add(curve.a.multiply(x)).add(curve.b).mod(p)
        if (left != right) throw CryptoException("The public key is not on P-256")
        return try {
            KeyFactory.getInstance("EC").generatePublic(ECPublicKeySpec(ECPoint(x, y), params)) as ECPublicKey
        } catch (e: GeneralSecurityException) {
            throw CryptoException("The public key is not usable", e)
        }
    }

    /** A coordinate as exactly 32 big-endian bytes. */
    private fun unsigned(value: BigInteger): ByteArray {
        val bytes = value.toByteArray()
        return when {
            bytes.size == 32 -> bytes
            bytes.size > 32 -> bytes.copyOfRange(bytes.size - 32, bytes.size)
            else -> ByteArray(32 - bytes.size) + bytes
        }
    }

    // -----------------------------------------------------------------------
    // Pairing
    // -----------------------------------------------------------------------

    /** SHA-256("praxis-remote/v2/commit" || 0x00 || phone_public). */
    fun commit(phonePublic: ByteArray): ByteArray =
        sha256(ascii("praxis-remote/v2/commit"), byteArrayOf(0), phonePublic)

    /** The x-coordinate of ECDH(own, peer): 32 bytes. */
    fun sharedSecret(own: PrivateKey, peerPublic: ByteArray): ByteArray {
        val peer = decodePoint(peerPublic)
        val secret = try {
            KeyAgreement.getInstance("ECDH").run {
                init(own)
                doPhase(peer, true)
                generateSecret()
            }
        } catch (e: GeneralSecurityException) {
            throw CryptoException("Key agreement failed", e)
        }
        if (secret.size != 32) throw CryptoException("Key agreement returned ${secret.size} bytes")
        return secret
    }

    fun transcript(channel: String, phoneId: String, phonePublic: ByteArray, desktopPublic: ByteArray): ByteArray {
        val zero = byteArrayOf(0)
        return sha256(
            ascii("praxis-remote/v2/pair"), zero,
            ascii(channel), zero,
            ascii(phoneId), zero,
            phonePublic, desktopPublic,
        )
    }

    fun derive(shared: ByteArray, transcript: ByteArray): PairingSecrets {
        val prk = hkdfExtract(salt = transcript, ikm = shared)
        val key = hkdfExpand(prk, ascii("praxis-remote/v2/key"), KEY_BYTES)
        val codeBytes = hkdfExpand(prk, ascii("praxis-remote/v2/code"), 4)
        var value = 0L
        for (b in codeBytes) value = (value shl 8) or (b.toLong() and 0xff)
        return PairingSecrets(key, "%06d".format(value % 1_000_000L))
    }

    /** The phone's side of pairing, once the computer's key is known. */
    fun phoneSecrets(phone: PairingKey, desktopPublic: ByteArray, channel: String, phoneId: String): PairingSecrets {
        val shared = sharedSecret(phone.private, desktopPublic)
        return derive(shared, transcript(channel, phoneId, phone.public, desktopPublic))
    }

    /** "807021" as "807 021". */
    fun formatCode(code: String): String =
        if (code.length == 6) code.substring(0, 3) + " " + code.substring(3) else code

    // -----------------------------------------------------------------------
    // HKDF-SHA-256 (RFC 5869)
    // -----------------------------------------------------------------------

    fun hkdfExtract(salt: ByteArray, ikm: ByteArray): ByteArray = hmac(salt, ikm)

    fun hkdfExpand(prk: ByteArray, info: ByteArray, length: Int): ByteArray {
        require(length in 1..255 * 32)
        val out = ByteArray(length)
        var previous = ByteArray(0)
        var written = 0
        var counter = 1
        while (written < length) {
            previous = hmac(prk, previous, info, byteArrayOf(counter.toByte()))
            val take = minOf(previous.size, length - written)
            previous.copyInto(out, written, 0, take)
            written += take
            counter++
        }
        return out
    }

    // -----------------------------------------------------------------------
    // Blobs
    // -----------------------------------------------------------------------

    fun stateAad(channel: String, phoneId: String) = "praxis-remote/v2/state/$channel/$phoneId"

    fun requestAad(channel: String, phoneId: String) = "praxis-remote/v2/request/$channel/$phoneId"

    fun responseAad(channel: String, phoneId: String, requestId: String) =
        "praxis-remote/v2/response/$channel/$phoneId/$requestId"

    /** `base64(nonce || ciphertext || tag)` with AES-256-GCM. */
    fun seal(key: ByteArray, plaintext: ByteArray, aad: String, nonce: ByteArray? = null): String {
        require(key.size == KEY_BYTES) { "The key must be 32 bytes" }
        val iv = nonce ?: ByteArray(NONCE_BYTES).also(random::nextBytes)
        require(iv.size == NONCE_BYTES) { "The nonce must be 12 bytes" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BYTES * 8, iv))
        cipher.updateAAD(aad.toByteArray(Charsets.UTF_8))
        return base64(iv + cipher.doFinal(plaintext))
    }

    /** Opens a blob; throws [CryptoException] if it was altered or belongs elsewhere. */
    fun open(key: ByteArray, blob: String, aad: String): ByteArray {
        if (key.size != KEY_BYTES) throw CryptoException("The key must be 32 bytes")
        val bytes = unbase64(blob)
        if (bytes.size < NONCE_BYTES + TAG_BYTES) throw CryptoException("The blob is too short")
        return try {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(
                Cipher.DECRYPT_MODE,
                SecretKeySpec(key, "AES"),
                GCMParameterSpec(TAG_BYTES * 8, bytes, 0, NONCE_BYTES),
            )
            cipher.updateAAD(aad.toByteArray(Charsets.UTF_8))
            cipher.doFinal(bytes, NONCE_BYTES, bytes.size - NONCE_BYTES)
        } catch (e: GeneralSecurityException) {
            throw CryptoException("The blob could not be decrypted", e)
        }
    }

    // -----------------------------------------------------------------------
    // Encodings
    // -----------------------------------------------------------------------

    fun base64(bytes: ByteArray): String = Base64.getEncoder().encodeToString(bytes)

    fun unbase64(text: String): ByteArray = try {
        Base64.getDecoder().decode(text.filterNot { it.isWhitespace() })
    } catch (e: IllegalArgumentException) {
        throw CryptoException("Invalid base64", e)
    }

    fun hex(bytes: ByteArray): String = bytes.joinToString("") { "%02x".format(it.toInt() and 0xff) }

    fun unhex(text: String): ByteArray {
        require(text.length % 2 == 0) { "Odd-length hex" }
        return ByteArray(text.length / 2) { text.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
    }

    /** 16 random bytes as 32 lowercase hex digits, for a phone id. */
    fun newId(): String = hex(ByteArray(16).also(random::nextBytes))

    private fun ascii(text: String): ByteArray = text.toByteArray(Charsets.US_ASCII)

    private fun sha256(vararg parts: ByteArray): ByteArray {
        val digest = MessageDigest.getInstance("SHA-256")
        parts.forEach(digest::update)
        return digest.digest()
    }

    private fun hmac(key: ByteArray, vararg parts: ByteArray): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        // An empty HMAC key is valid, but SecretKeySpec refuses it; HKDF's
        // salt is never empty here (it is the transcript hash).
        mac.init(SecretKeySpec(key, "HmacSHA256"))
        parts.forEach(mac::update)
        return mac.doFinal()
    }
}
