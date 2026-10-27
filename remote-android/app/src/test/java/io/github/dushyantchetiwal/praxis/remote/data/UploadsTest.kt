package io.github.dushyantchetiwal.praxis.remote.data

import java.io.ByteArrayInputStream
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class UploadsTest {
    @Test
    fun readsUpToTheLimitAndNoMore() {
        assertArrayEquals(byteArrayOf(1, 2, 3), readLimited(ByteArrayInputStream(byteArrayOf(1, 2, 3)), 3))
        assertThrows(UploadTooLargeException::class.java) {
            readLimited(ByteArrayInputStream(ByteArray(4)), 3)
        }
    }

    @Test
    fun statusAdvertisesTheUploadLimit() {
        val status = parseStatus(
            JSONObject("""{"device":"pc","windows":[],"capabilities":{"open_folder":true,"file_upload":{"max_bytes":26214400}}}"""),
        )
        assertEquals(26214400L, status.uploadLimit)
        assertNull(parseStatus(JSONObject("""{"device":"pc","windows":[]}""")).uploadLimit)
    }

    @Test
    fun uploadsInPiecesAndReturnsTheSavedPath() = runTest {
        val id = "0123456789abcdef0123456789abcdef"
        val file = PickedFile("00000000-0000-4000-8000-000000000001", "notes.txt", ByteArray(50_000) { it.toByte() })
        val received = java.io.ByteArrayOutputStream()
        val operations = mutableListOf<String>()
        val progress = mutableListOf<Long>()
        val path = uploadFile(file, JSONObject().put("attachment", true), { progress += it }) { op, args ->
            operations += op
            when (op) {
                "file_begin" -> {
                    assertEquals("notes.txt", args.getString("name"))
                    assertEquals(true, args.getBoolean("attachment"))
                    JSONObject().put("upload_id", id).put("chunk_bytes", 24 * 1024).put("next_offset", 0)
                }
                "file_chunk" -> {
                    assertEquals(received.size(), args.getInt("offset"))
                    received.write(RemoteCrypto.unbase64(args.getString("data")))
                    JSONObject().put("next_offset", received.size())
                }
                "file_finish" -> JSONObject().put("upload_id", id).put("path", "project/.praxis/uploads/notes.txt")
                else -> error("unexpected $op")
            }
        }
        assertEquals("project/.praxis/uploads/notes.txt", path)
        assertEquals(listOf("file_begin", "file_chunk", "file_chunk", "file_chunk", "file_finish"), operations)
        assertArrayEquals(file.bytes, received.toByteArray())
        assertEquals(50_000L, progress.last())
    }

    @Test
    fun aFinishedUploadIsNotSentAgain() = runTest {
        val file = PickedFile("00000000-0000-4000-8000-000000000002", "a.bin", byteArrayOf(1))
        val operations = mutableListOf<String>()
        val path = uploadFile(file, JSONObject(), {}) { op, _ ->
            operations += op
            JSONObject().put("upload_id", "0123456789abcdef0123456789abcdef").put("chunk_bytes", 1).put("path", "project/a.bin")
        }
        assertEquals("project/a.bin", path)
        assertEquals(listOf("file_begin"), operations)
    }
}
