package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.exifinterface.media.ExifInterface
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.util.UUID
import android.util.Log

import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

const val MAX_PROMPT_IMAGES = 4
const val MAX_RETAINED_IMAGES = 8
private const val MAX_SOURCE_BYTES = 20 * 1024 * 1024
const val MAX_IMAGE_BYTES = 2 * 1024 * 1024
private const val MAX_EDGE = 1600

class PromptImage(val id: String, val bytes: ByteArray, val preview: ImageBitmap) {
    internal val encodedHash: ByteArray = java.security.MessageDigest.getInstance("SHA-256")
        .digest(RemoteCrypto.base64(bytes).toByteArray(Charsets.US_ASCII))
}

fun promptFingerprint(text: String, images: List<PromptImage>): String {
    if (images.isEmpty()) return transcriptFingerprint(text)
    val digest = java.security.MessageDigest.getInstance("SHA-256")
    digest.update("praxis-remote/image-message/v1".toByteArray())
    digest.update(0.toByte())
    digest.update(text.trim().toByteArray(Charsets.UTF_8))
    digest.update(0.toByte())
    for (image in images) {
        digest.update("image/jpeg".toByteArray())
        digest.update(0.toByte())
        digest.update(image.encodedHash)
    }
    return "image-v1:" + RemoteCrypto.base64(digest.digest())
}

internal fun imageSampleSize(width: Int, height: Int): Int {
    require(width > 0 && height > 0 && width.toLong() * height <= 100_000_000) { "This image is too large to decode. Choose a smaller image." }
    var sample = 1
    while (maxOf(width, height) / (sample * 2) >= MAX_EDGE) sample *= 2
    return sample
}

suspend fun prepareImage(context: Context, uri: Uri): PromptImage = withContext(Dispatchers.IO) {
    require(uri.scheme == "content") { "Choose an image using the photo picker." }
    val source = context.contentResolver.openInputStream(uri)?.use { input ->
        val output = ByteArrayOutputStream()
        val buffer = ByteArray(16 * 1024)
        while (true) {
            val count = input.read(buffer)
            if (count < 0) break
            require(output.size() + count <= MAX_SOURCE_BYTES) { "Choose an image smaller than 20 MiB." }
            output.write(buffer, 0, count)
        }
        output.toByteArray()
    } ?: error("The selected image could not be opened. Choose it again.")
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeByteArray(source, 0, source.size, bounds)
    val options = BitmapFactory.Options().apply {
        inSampleSize = imageSampleSize(bounds.outWidth, bounds.outHeight)
        inPreferredConfig = Bitmap.Config.ARGB_8888
    }
    var bitmap = BitmapFactory.decodeByteArray(source, 0, source.size, options)
        ?: error("The selected image could not be decoded. Choose another image.")
    try {
        val scale = minOf(1f, MAX_EDGE.toFloat() / maxOf(bitmap.width, bitmap.height))
        if (scale < 1f) {
            val scaled = Bitmap.createScaledBitmap(bitmap, (bitmap.width * scale).toInt().coerceAtLeast(1), (bitmap.height * scale).toInt().coerceAtLeast(1), true)
            if (scaled !== bitmap) { bitmap.recycle(); bitmap = scaled }
        }
        val orientation = ByteArrayInputStream(source).use { ExifInterface(it).getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL) }
        val matrix = Matrix().apply {
            when (orientation) {
                ExifInterface.ORIENTATION_FLIP_HORIZONTAL -> setScale(-1f, 1f)
                ExifInterface.ORIENTATION_ROTATE_180 -> setRotate(180f)
                ExifInterface.ORIENTATION_FLIP_VERTICAL -> setScale(1f, -1f)
                ExifInterface.ORIENTATION_TRANSPOSE -> { setRotate(90f); postScale(-1f, 1f) }
                ExifInterface.ORIENTATION_ROTATE_90 -> setRotate(90f)
                ExifInterface.ORIENTATION_TRANSVERSE -> { setRotate(-90f); postScale(-1f, 1f) }
                ExifInterface.ORIENTATION_ROTATE_270 -> setRotate(-90f)
            }
        }
        if (!matrix.isIdentity) {
            val oriented = Bitmap.createBitmap(bitmap, 0, 0, bitmap.width, bitmap.height, matrix, true)
            if (oriented !== bitmap) { bitmap.recycle(); bitmap = oriented }
        }
        if (bitmap.hasAlpha()) {
            val opaque = Bitmap.createBitmap(bitmap.width, bitmap.height, Bitmap.Config.ARGB_8888)
            Canvas(opaque).apply { drawColor(Color.WHITE); drawBitmap(bitmap, 0f, 0f, null) }
            bitmap.recycle()
            bitmap = opaque
        }
        val output = ByteArrayOutputStream()
        check(bitmap.compress(Bitmap.CompressFormat.JPEG, 85, output)) { "The image could not be prepared." }
        if (output.size() > MAX_IMAGE_BYTES) {
            output.reset()
            check(bitmap.compress(Bitmap.CompressFormat.JPEG, 65, output)) { "The image could not be prepared." }
        }
        require(output.size() in 1..MAX_IMAGE_BYTES) { "This image is still too large. Crop it or choose a smaller image." }
        val previewScale = minOf(1f, 192f / maxOf(bitmap.width, bitmap.height))
        val preview = Bitmap.createScaledBitmap(bitmap, (bitmap.width * previewScale).toInt().coerceAtLeast(1), (bitmap.height * previewScale).toInt().coerceAtLeast(1), true)
        // ImageBitmap retains its Bitmap; do not recycle the preview's backing pixels.
        val retained = if (preview === bitmap) checkNotNull(bitmap.copy(Bitmap.Config.ARGB_8888, false)) else preview
        PromptImage(UUID.randomUUID().toString(), output.toByteArray(), retained.asImageBitmap())
    } finally { bitmap.recycle() }
}

internal fun nextImageOffset(reply: JSONObject, expected: Int): Int {
    val offset = reply.index("next_offset")
    require(offset == expected) { "Praxis returned an inconsistent image offset. Attach the image again." }
    return offset
}

suspend fun uploadImage(
    image: PromptImage,
    target: JSONObject,
    preferredChunkBytes: Int = LIVE_UPLOAD_CHUNK_BYTES,
    maxParallel: Int = MAX_PARALLEL_UPLOAD_CHUNKS,
    request: suspend (String, JSONObject) -> JSONObject?,
): String {
    fun args() = JSONObject(target.toString())
    val begin = request(
        "image_begin",
        args().put("client_id", image.id)
            .put("size", image.bytes.size)
            .put("mime_type", "image/jpeg")
            .put("preferred_chunk_bytes", preferredChunkBytes.coerceIn(1, MAX_UPLOAD_CHUNK_BYTES)),
    ) ?: error("Praxis did not acknowledge the image upload.")
    val id = begin.str("upload_id")?.takeIf(::isHexId) ?: error("Praxis returned an invalid image upload ID.")
    val chunkSize = begin.index("chunk_bytes")?.takeIf { it in 1..MAX_UPLOAD_CHUNK_BYTES }
        ?: error("Praxis returned an invalid image chunk size.")
    if (begin.bool("ready")) return id
    val serverParallel = begin.index("max_parallel")?.coerceIn(1, MAX_PARALLEL_UPLOAD_CHUNKS) ?: 1
    val parallelism = minOf(maxParallel.coerceAtLeast(1), serverParallel)
    try {
        val startOffset = begin.index("next_offset") ?: 0
        require(startOffset in 0..image.bytes.size) { "Praxis returned an invalid image resume offset." }
        val chunks = (startOffset until image.bytes.size step chunkSize).toList()
        if (parallelism <= 1 || chunks.size <= 1) {
            var offset = startOffset
            while (offset < image.bytes.size) {
                val end = minOf(image.bytes.size, offset + chunkSize)
                val reply = request(
                    "image_chunk",
                    args().put("upload_id", id).put("offset", offset)
                        .put("data", RemoteCrypto.base64(image.bytes.copyOfRange(offset, end))),
                ) ?: error("Praxis did not acknowledge the image chunk.")
                offset = if (reply.has("next_offset")) {
                    nextImageOffset(reply, end)
                } else {
                    val received = reply.index("received_bytes")
                    if (received != null) {
                        require(received >= end) {
                            "Praxis returned an inconsistent image offset. Attach the image again."
                        }
                        end
                    } else {
                        nextImageOffset(reply, end)
                    }
                }
            }
        } else {
            val semaphore = Semaphore(parallelism)
            coroutineScope {
                for (offset in chunks) {
                    val end = minOf(image.bytes.size, offset + chunkSize)
                    launch {
                        semaphore.withPermit {
                            val reply = request(
                                "image_chunk",
                                args().put("upload_id", id).put("offset", offset)
                                    .put("data", RemoteCrypto.base64(image.bytes.copyOfRange(offset, end))),
                            ) ?: error("Praxis did not acknowledge the image chunk.")
                            reply.index("received_bytes")?.let { received ->
                                require(received > 0) {
                                    "Praxis returned an inconsistent image offset. Attach the image again."
                                }
                            }
                        }
                    }
                }
            }
        }
        val finish = request("image_finish", args().put("upload_id", id))
        check(finish?.bool("ready") == true && finish.str("upload_id") == id) { "Praxis could not finish the image upload." }
        return id
    } catch (error: Exception) {
        withContext(NonCancellable) {
            try { withTimeoutOrNull(5_000) { request("image_discard", args().put("upload_id", id)) } }
            catch (cleanup: Exception) { Log.d("PraxisRemote", "Incomplete image will expire on the desktop", cleanup) }
        }
        throw error
    }
}
