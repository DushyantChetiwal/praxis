package io.github.dushyantchetiwal.praxis.remote.data

import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import java.io.IOException
import java.io.OutputStream
import java.util.Base64
import org.json.JSONObject

/** Where downloads go on Android 10 and later, inside the shared Downloads folder. */
const val DOWNLOAD_FOLDER = "Praxis"

/** One piece of a file from the `download` op. */
data class DownloadChunk(val size: Long, val offset: Long, val version: String, val data: String)

fun parseDownloadChunk(result: JSONObject?): DownloadChunk? {
    result ?: return null
    return DownloadChunk(
        size = result.long("size") ?: return null,
        offset = result.long("offset") ?: return null,
        version = result.str("version") ?: return null,
        data = result.str("data") ?: return null,
    )
}

/** Why a piece cannot continue the download; the message is shown as is. */
class DownloadException(message: String) : Exception(message)

/**
 * The bytes of [chunk] if it continues a download that has [received] bytes
 * of [version] (null before the first piece). The computer re-reads the file
 * for every piece, so a changed version means it changed on disk.
 */
fun checkDownloadChunk(chunk: DownloadChunk, received: Long, version: String?, changed: String): ByteArray {
    if (version != null && chunk.version != version) throw DownloadException(changed)
    if (chunk.offset != received || chunk.size < received) throw DownloadException(changed)
    val bytes = try {
        Base64.getDecoder().decode(chunk.data)
    } catch (e: IllegalArgumentException) {
        throw DownloadException(changed)
    }
    val remaining = chunk.size - received
    // An empty piece before the end would repeat forever.
    if (bytes.size > remaining || (bytes.isEmpty() && remaining > 0)) throw DownloadException(changed)
    return bytes
}

fun mimeTypeFor(name: String): String {
    val extension = name.substringAfterLast('.', "").lowercase()
    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: "application/octet-stream"
}

/**
 * A file being written on the phone. [complete] makes it visible; [abandon]
 * removes what was written so a failed download leaves nothing behind.
 */
class DownloadSink private constructor(
    private val context: Context,
    val uri: Uri,
    val stream: OutputStream,
    private val pending: Boolean,
) {
    fun complete() {
        stream.close()
        if (pending && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }
            context.contentResolver.update(uri, values, null, null)
        }
    }

    fun abandon() {
        try {
            stream.close()
        } catch (_: IOException) {
            // Already broken; the file is removed below either way.
        }
        try {
            if (pending) {
                context.contentResolver.delete(uri, null, null)
            } else {
                DocumentsContract.deleteDocument(context.contentResolver, uri)
            }
        } catch (_: Exception) {
            // Nothing more can be done; the user sees that the download failed.
        }
    }

    companion object {
        /** Whether downloads can go straight to Downloads, without asking where. */
        val savesDirectly: Boolean get() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

        /** A new file in Downloads/Praxis (Android 10+), or at [chosen], a place the user picked. */
        fun open(context: Context, name: String, chosen: Uri?): DownloadSink {
            val resolver = context.contentResolver
            if (chosen != null) {
                val stream = resolver.openOutputStream(chosen, "wt") ?: throw IOException("no stream for $chosen")
                return DownloadSink(context, chosen, stream, pending = false)
            }
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
                throw IllegalStateException("Android ${Build.VERSION.SDK_INT} needs the user to choose where to save")
            }
            val values = ContentValues().apply {
                put(MediaStore.MediaColumns.DISPLAY_NAME, name)
                put(MediaStore.MediaColumns.MIME_TYPE, mimeTypeFor(name))
                put(MediaStore.MediaColumns.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/$DOWNLOAD_FOLDER")
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: throw IOException("Downloads refused a new file")
            val stream = try {
                resolver.openOutputStream(uri, "w") ?: throw IOException("no stream for $uri")
            } catch (e: Exception) {
                resolver.delete(uri, null, null)
                throw e
            }
            return DownloadSink(context, uri, stream, pending = true)
        }
    }
}
