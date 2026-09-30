package io.github.dushyantchetiwal.praxis.remote.data

import java.util.Base64
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class DownloadsTest {
    private val changed = "changed"

    private fun chunk(size: Long, offset: Long, bytes: ByteArray, version: String = "v1") =
        DownloadChunk(size, offset, version, Base64.getEncoder().encodeToString(bytes))

    @Test
    fun piecesReassembleIntoTheWholeFile() {
        val file = ByteArray(10) { it.toByte() }
        var received = 0L
        var version: String? = null
        val out = mutableListOf<Byte>()
        for (piece in listOf(file.copyOfRange(0, 4), file.copyOfRange(4, 8), file.copyOfRange(8, 10))) {
            val next = chunk(file.size.toLong(), received, piece)
            val bytes = checkDownloadChunk(next, received, version, changed)
            version = next.version
            received += bytes.size
            out += bytes.toList()
        }
        assertEquals(file.size.toLong(), received)
        assertArrayEquals(file, out.toByteArray())
    }

    @Test
    fun anEmptyFileIsOnePiece() {
        val bytes = checkDownloadChunk(chunk(0, 0, ByteArray(0)), 0, null, changed)
        assertEquals(0, bytes.size)
    }

    @Test
    fun aFileThatChangesMidwayIsRefused() {
        val first = chunk(8, 0, ByteArray(4))
        checkDownloadChunk(first, 0, null, changed)
        val error = assertThrows(DownloadException::class.java) {
            checkDownloadChunk(chunk(8, 4, ByteArray(4), version = "v2"), 4, "v1", changed)
        }
        assertEquals(changed, error.message)
    }

    @Test
    fun piecesOutOfPlaceOrTooLongAreRefused() {
        assertThrows(DownloadException::class.java) { checkDownloadChunk(chunk(8, 2, ByteArray(4)), 4, "v1", changed) }
        assertThrows(DownloadException::class.java) { checkDownloadChunk(chunk(8, 4, ByteArray(6)), 4, "v1", changed) }
        assertThrows(DownloadException::class.java) { checkDownloadChunk(chunk(8, 4, ByteArray(0)), 4, "v1", changed) }
        assertThrows(DownloadException::class.java) {
            checkDownloadChunk(DownloadChunk(8, 0, "v1", "not base64!"), 0, null, changed)
        }
    }
}
