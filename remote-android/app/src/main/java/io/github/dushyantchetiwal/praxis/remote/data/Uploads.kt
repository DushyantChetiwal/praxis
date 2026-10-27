package io.github.dushyantchetiwal.praxis.remote.data

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.util.UUID
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject

/** How many files one message can attach, as the computer allows. */
const val MAX_ATTACHED_FILES = 8
private const val MAX_CHUNK_BYTES = 24 * 1024

/** A file picked on the phone, read into memory to be sent in pieces. */
class PickedFile(val id: String, val name: String, val bytes: ByteArray)

/** A file already saved on the computer and attached to the next message. */
data class FileAttachment(val id: String, val name: String, val path: String)

class UploadTooLargeException(val limit: Long) : IllegalArgumentException()

/** Reads at most [maxBytes], failing as soon as the stream has more. */
internal fun readLimited(input: InputStream, maxBytes: Long): ByteArray {
    val output = ByteArrayOutputStream()
    val buffer = ByteArray(64 * 1024)
    while (true) {
        val read = input.read(buffer)
        if (read < 0) break
        if (output.size().toLong() + read > maxBytes) throw UploadTooLargeException(maxBytes)
        output.write(buffer, 0, read)
    }
    return output.toByteArray()
}

suspend fun readPickedFile(context: Context, uri: Uri, maxBytes: Long): PickedFile = withContext(Dispatchers.IO) {
    val resolver = context.contentResolver
    var name: String? = null
    var size: Long? = null
    resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { cursor ->
        if (cursor.moveToFirst()) {
            name = cursor.getString(0)
            if (!cursor.isNull(1)) size = cursor.getLong(1)
        }
    }
    size?.let { if (it > maxBytes) throw UploadTooLargeException(maxBytes) }
    val bytes = resolver.openInputStream(uri)?.use { readLimited(it, maxBytes) }
        ?: error("The file could not be read.")
    require(bytes.isNotEmpty()) { "The file is empty." }
    PickedFile(UUID.randomUUID().toString(), name?.takeIf { it.isNotBlank() } ?: "upload", bytes)
}

internal fun nextUploadOffset(reply: JSONObject, expected: Int): Int {
    require(reply.index("next_offset") == expected) {
        "Praxis returned an inconsistent upload offset. Upload the file again."
    }
    return expected
}

/**
 * Sends [file] to the computer in pieces and returns where it was saved, as a
 * project path. [target] says where: a folder (`path`) or a chat attachment
 * (`attachment`). A retry with the same file resumes rather than restarting.
 */
suspend fun uploadFile(
    file: PickedFile,
    target: JSONObject,
    onProgress: (Long) -> Unit,
    request: suspend (String, JSONObject) -> JSONObject?,
): String {
    fun args() = JSONObject(target.toString())
    val begin = request(
        "file_begin",
        args().put("client_id", file.id).put("name", file.name).put("size", file.bytes.size),
    ) ?: error("Praxis did not acknowledge the upload.")
    val id = begin.str("upload_id")?.takeIf(::isHexId) ?: error("Praxis returned an invalid upload ID.")
    begin.str("path")?.let { return it }
    val chunkSize = begin.index("chunk_bytes")?.takeIf { it in 1..MAX_CHUNK_BYTES }
        ?: error("Praxis returned an invalid upload chunk size.")
    try {
        var offset = begin.index("next_offset") ?: 0
        require(offset in 0..file.bytes.size) { "Praxis returned an invalid upload resume offset." }
        onProgress(offset.toLong())
        while (offset < file.bytes.size) {
            val end = minOf(file.bytes.size, offset + chunkSize)
            val reply = request(
                "file_chunk",
                args().put("upload_id", id).put("offset", offset)
                    .put("data", RemoteCrypto.base64(file.bytes.copyOfRange(offset, end))),
            ) ?: error("Praxis did not acknowledge part of the upload.")
            offset = nextUploadOffset(reply, end)
            onProgress(offset.toLong())
        }
        val finish = request("file_finish", args().put("upload_id", id))
        check(finish?.str("upload_id") == id) { "Praxis could not save the file." }
        return finish?.str("path") ?: error("Praxis could not save the file.")
    } catch (error: Exception) {
        withContext(NonCancellable) {
            try { withTimeoutOrNull(5_000) { request("file_discard", args().put("upload_id", id)) } }
            catch (cleanup: Exception) { Log.d("PraxisRemote", "An unfinished upload will expire on the desktop", cleanup) }
        }
        throw error
    }
}
