package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TranscriptTest {
    @Test
    fun legacySnapshotsKeepTheirEntriesWithoutInventingThinking() {
        val thread = parseThreadView(JSONObject("""{
            "session_id":"root", "total":7,
            "entries":[
                {"index":5,"role":"assistant","text":"Answer"},
                {"index":6,"role":"tool","text":"Read file","status":"completed"}
            ]
        }"""))
        assertEquals(7, thread.total)
        assertEquals(listOf(5, 6), thread.entries.map { it.index })
        assertTrue(thread.entries.all { it.parts.isEmpty() })
        assertTrue(thread.stepThreads.isEmpty())
        assertEquals("completed", thread.entries.last().status)
    }

    @Test
    fun reasoningPartsStayOrderedAndSeparateFromTheAnswer() {
        val thread = parseThreadView(JSONObject("""{
            "total":2, "entries":[
                {"index":0,"role":"assistant","text":"Answer","parts":[
                    {"index":0,"role":"reasoning","text":"Provider thought"},
                    {"index":1,"role":"assistant","text":"Answer"},
                    {"index":2,"role":"reasoning","text":"More thought"}
                ]},
                {"index":1,"role":"assistant","text":"","parts":[
                    {"index":0,"role":"reasoning","text":"Still thinking"},
                    {"index":1,"role":"reasoning","text":"  "}
                ]}
            ]
        }"""))
        assertEquals(2, thread.entries.size)
        assertEquals(2, thread.total)
        assertEquals(listOf("reasoning", "assistant", "reasoning"), thread.entries.first().parts.map { it.role })
        assertEquals(listOf(0, 1, 2), thread.entries.first().parts.map { it.index })
        assertEquals("Answer", thread.entries.first().text)
        assertEquals(listOf(EntryPart(0, "reasoning", "Still thinking")), thread.entries.last().parts)
    }

    @Test
    fun streamingUpdatesKeepPartIdentityEvenWhenEarlierPartsAreBudgetedOut() {
        fun update(text: String) = parseThreadView(JSONObject("""{
            "session_id":"root", "entries":[
                {"index":12,"role":"assistant","text":"","parts":[
                    {"index":4,"role":"reasoning","text":"$text"}
                ]}
            ]
        }"""))
        val first = update("Checking")
        val next = update("Checking the provider output")
        assertEquals(first.sessionId, next.sessionId)
        assertEquals(first.entries.single().index, next.entries.single().index)
        assertEquals(first.entries.single().parts.single().index, next.entries.single().parts.single().index)
        assertEquals("Checking the provider output", next.entries.single().parts.single().text)
    }

    @Test
    fun liveParallelStepsDoNotReplaceTheRootOrItsEntryCount() {
        val snapshot = parseSnapshot("""{
            "watch":{"session_id":"root","window":1},
            "status":{"windows":[{"window":1,"architect":{
                "steps":5,"running":true,"step_number":2,"current_step":"Leaf"
            }}]},
            "thread":{"session_id":"root","total":10,"entries":[
                {"index":9,"role":"user","text":"Run this plan"}
            ], "step_threads":[
                {"session_id":"leaf","title":"Leaf","status":"generating","entries":[
                    {"index":0,"role":"assistant","text":"","parts":[
                        {"index":0,"role":"reasoning","text":"Live leaf thought"}
                    ]}
                ]},
                {"session_id":"parallel","title":"Parallel","entries":[
                    {"index":0,"role":"assistant","text":"Parallel answer"}
                ]}
            ]}
        }""")!!
        val thread = snapshot.thread!!
        assertEquals("root", thread.sessionId)
        assertEquals(10, thread.total)
        assertEquals(1, thread.entries.size)
        assertEquals(listOf("leaf", "parallel"), thread.stepThreads.map { it.sessionId })
        assertEquals("Live leaf thought", thread.stepThreads.first().entries.single().parts.single().text)
        assertEquals("generating", thread.stepThreads.first().status)
        assertEquals(5, snapshot.status!!.windows.single().architect!!.steps)
    }

    @Test
    fun unknownPartsRemainReadableAndNullExtensionsFallBackToLegacyText() {
        val thread = parseThreadView(JSONObject("""{"entries":[
            {"index":0,"role":"assistant","text":"Legacy","parts":null},
            {"index":1,"role":"assistant","text":"","parts":[
                {"index":0,"role":"future-kind","text":"Future content"}
            ]}
        ],"step_threads":null}"""))
        assertTrue(thread.entries.first().parts.isEmpty())
        assertEquals("Legacy", thread.entries.first().text)
        assertFalse(thread.entries.last().parts.isEmpty())
        assertEquals("future-kind", thread.entries.last().parts.single().role)
        assertTrue(thread.stepThreads.isEmpty())
    }
}
