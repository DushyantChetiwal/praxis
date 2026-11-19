package io.github.dushyantchetiwal.praxis.remote.data

import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.currentTime
import kotlinx.coroutines.test.runTest
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class GistDiscoveryTest {
    private class MemoryMetaCache : GistMetaCache {
        val entries = LinkedHashMap<String, Pair<String, String>>()

        override fun get(gistId: String, rawUrl: String): String? =
            entries[gistId]?.takeIf { it.first == rawUrl }?.second

        override fun put(gistId: String, rawUrl: String, metaJson: String) {
            entries[gistId] = rawUrl to metaJson
        }

        override fun retainOnly(gistIds: Set<String>) {
            entries.keys.retainAll(gistIds)
        }
    }

    private fun metaJson(
        channel: String,
        name: String,
        lastSeen: String,
        nostr: Int = 1,
    ): String = JSONObject()
        .put("protocol", PROTOCOL)
        .put("channel", channel)
        .put("device", name)
        .put("started_at", "2026-11-19T10:00:00Z")
        .put("last_seen", lastSeen)
        .put("phones", JSONArray().put("a".repeat(32)))
        .put("nostr", nostr)
        .toString()

    private fun listedGist(id: String, rawUrl: String?): JSONObject {
        val metaFile = JSONObject().put("filename", META_FILE).apply {
            if (rawUrl != null) put("raw_url", rawUrl)
        }
        return JSONObject()
            .put("id", id)
            .put("files", JSONObject().put(META_FILE, metaFile))
    }

    @Test
    fun fetchesMultipleGistsInParallelAndCachesByRawUrlRevision() = runTest {
        val ch1 = "1".repeat(32)
        val ch2 = "2".repeat(32)
        val ch3 = "3".repeat(32)
        val cache = MemoryMetaCache()
        val rawCalls = AtomicInteger(0)
        val readCalls = AtomicInteger(0)

        var rawUrl1 = "https://gist.githubusercontent.com/u/g1/raw/rev1/praxis-remote.json"
        val rawUrl2 = "https://gist.githubusercontent.com/u/g2/raw/rev1/praxis-remote.json"
        val rawUrl3 = "https://gist.githubusercontent.com/u/g3/raw/rev1/praxis-remote.json"

        val payloads = mutableMapOf(
            rawUrl1 to metaJson(ch1, "Laptop", "2026-11-19T22:00:00Z"),
            rawUrl2 to metaJson(ch2, "Debian VM", "2026-11-19T22:01:00Z"),
            rawUrl3 to metaJson(ch3, "Old Box", "2026-10-28T20:00:00Z"),
        )

        val first = discoverDevices(
            metaCache = cache,
            listPage = {
                JSONArray()
                    .put(listedGist("g1", rawUrl1))
                    .put(listedGist("g2", rawUrl2))
                    .put(listedGist("g3", rawUrl3))
            },
            fetchRaw = { url ->
                rawCalls.incrementAndGet()
                delay(500L)
                payloads.getValue(url)
            },
            readGist = {
                readCalls.incrementAndGet()
                null
            },
        )

        // All 3 500ms raw_url fetches ran concurrently in a single 500ms window, with zero GET /gists/{id} calls.
        assertEquals(500L, currentTime)
        assertEquals(3, rawCalls.get())
        assertEquals(0, readCalls.get())
        assertEquals(setOf("Laptop", "Debian VM", "Old Box"), first.map { it.name }.toSet())

        // Second refresh: g2 and g3 have unchanged raw_url hashes (0 network calls); only g1 rotated to rev2.
        rawUrl1 = "https://gist.githubusercontent.com/u/g1/raw/rev2/praxis-remote.json"
        payloads[rawUrl1] = metaJson(ch1, "Laptop", "2026-11-19T22:05:00Z")

        val second = discoverDevices(
            metaCache = cache,
            listPage = {
                JSONArray()
                    .put(listedGist("g1", rawUrl1))
                    .put(listedGist("g2", rawUrl2))
                    .put(listedGist("g3", rawUrl3))
            },
            fetchRaw = { url ->
                rawCalls.incrementAndGet()
                delay(500L)
                payloads.getValue(url)
            },
            readGist = {
                readCalls.incrementAndGet()
                null
            },
        )

        assertEquals(4, rawCalls.get())
        assertEquals(0, readCalls.get())
        assertEquals(parseTime("2026-11-19T22:05:00Z"), second.first { it.channel == ch1 }.lastSeen)
    }

    @Test
    fun fallsBackToReadGistForKnownOffPageIdsAndRawCdnFailuresAndPrunesStaleCache() = runTest {
        val ch1 = "a".repeat(32)
        val ch2 = "b".repeat(32)
        val cache = MemoryMetaCache().apply {
            put("deleted-gist", "https://gist.githubusercontent.com/u/deleted/raw/r1/praxis-remote.json", "{}")
        }
        val readIds = mutableListOf<String>()

        val devices = discoverDevices(
            known = listOf("known-off-page", "missing-gist"),
            metaCache = cache,
            listPage = {
                JSONArray().put(listedGist("listed-cdn-blocked", "https://gist.githubusercontent.com/u/g1/raw/r1/praxis-remote.json"))
            },
            fetchRaw = {
                throw ApiException(ErrorKind.Network, "CDN blocked")
            },
            readGist = { id ->
                synchronized(readIds) { readIds.add(id) }
                when (id) {
                    "listed-cdn-blocked" -> parseMeta(metaJson(ch1, "Corp Laptop", "2026-11-19T22:00:00Z"), id)
                    "known-off-page" -> parseMeta(metaJson(ch2, "Archive Host", "2026-11-19T21:00:00Z"), id)
                    "missing-gist" -> throw ApiException(ErrorKind.NotFound, "Not found", 404)
                    else -> null
                }
            },
        )

        assertEquals(setOf("Corp Laptop", "Archive Host"), devices.map { it.name }.toSet())
        assertTrue("deleted-gist" !in cache.entries)
        assertEquals(setOf("listed-cdn-blocked", "known-off-page", "missing-gist"), readIds.toSet())
    }

    @Test
    fun deduplicatesCompetingGistsForSameChannelByNewestLastSeen() = runTest {
        val ch = "f".repeat(32)
        val devices = discoverDevices(
            listPage = {
                JSONArray()
                    .put(listedGist("old-gist", "https://example.com/old"))
                    .put(listedGist("new-gist", "https://example.com/new"))
            },
            fetchRaw = { url ->
                if (url.endsWith("/old")) {
                    metaJson(ch, "Laptop (old)", "2026-10-28T20:40:30Z")
                } else {
                    metaJson(ch, "Laptop", "2026-11-19T22:19:01Z")
                }
            },
            readGist = { null },
        )

        assertEquals(1, devices.size)
        assertEquals("new-gist", devices.single().gistId)
        assertEquals("Laptop", devices.single().name)
    }
}
