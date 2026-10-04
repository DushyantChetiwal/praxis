package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TranscriptTest {
    @Test
    fun questionsPreserveChoiceValuesAndFreeformTakesPrecedence() {
        val form = parseQuestionForm(JSONObject("""{
            "question":"Which database?", "allow_multiple":true, "auto_answer_paused":true,
            "options":[{"value":"postgres","label":"PostgreSQL","description":"Shared"},
                       {"value":"sqlite","label":"SQLite"}]
        }"""))!!
        assertTrue(form.autoAnswerPaused)
        assertEquals("Shared", form.options.first().description)
        assertEquals(listOf("postgres", "sqlite"), questionAnswerContent(form, setOf("sqlite", "postgres"), "")!!.getJSONArray("answer").strings())
        val freeform = questionAnswerContent(form, setOf("postgres"), "  Another choice  ")!!
        assertEquals("Another choice", freeform.getString("freeform_answer"))
        assertFalse(freeform.has("answer"))
        assertEquals(null, questionAnswerContent(form, emptySet(), " "))
        assertEquals(null, questionAnswerContent(form, setOf("unknown"), ""))
        assertEquals(null, questionAnswerContent(form.copy(allowMultiple = false), setOf("postgres", "sqlite"), ""))
        val plain = form.copy(options = emptyList(), allowMultiple = false)
        assertEquals("Custom", questionAnswerContent(plain, emptySet(), "Custom")!!.getString("answer"))
    }

    @Test
    fun questionsKeepSessionIdentitySeparateFromPermissionRequests() {
        val page = parseQuestionPage(JSONObject("""{
            "questions":[{"id":"request","session_id":"step-one","title":"Question", "session_title":"Build"},
                         {"id":"request","session_id":"step-two","title":"Question"}], "next_offset":null
        }"""))
        assertEquals(2, page.questions.map { it.key }.toSet().size)
        assertEquals("Build", page.questions.first().sessionTitle)
        assertEquals(null, page.nextOffset)
    }

    @Test
    fun messageFingerprintsIdentifyLongPromptsWithoutDownloadingTheirBodies() {
        assertEquals("LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=", transcriptFingerprint("hello"))
        assertEquals(transcriptFingerprint("hello"), transcriptFingerprint("  hello\n"))
        assertFalse(transcriptFingerprint("same prefix, first") == transcriptFingerprint("same prefix, second"))
    }

    @Test
    fun detailChunksReassembleUnicodeAndWhitespaceWithoutLoss() {
        val texts = listOf("  First\\n\n", "🦀 café\t", "last line  \n")
        val total = texts.sumOf { it.toByteArray(Charsets.UTF_8).size }.toLong()
        var body = DetailBody()
        for (text in texts) {
            val next = body.nextOffset + text.toByteArray(Charsets.UTF_8).size
            body = body.append(DetailChunk(body.nextOffset, next, total, "version", text))!!
        }
        assertTrue(body.complete)
        assertEquals(texts.joinToString(""), body.chunks.joinToString("") { it.text })
        assertEquals(total, body.nextOffset)
    }

    @Test
    fun bodyChunksRejectGapsDuplicatesChangedVersionsAndNoProgress() {
        val first = DetailChunk(0, 3, 6, "first", "abc")
        val body = DetailBody().append(first)!!
        for (chunk in listOf(
            first,
            DetailChunk(4, 6, 6, "first", "ef"),
            DetailChunk(3, 6, 6, "changed", "def"),
            DetailChunk(3, 6, 9, "first", "def"),
            DetailChunk(3, 3, 6, "first", ""),
            DetailChunk(3, 6, 6, "first", "🦀"),
        )) assertEquals(null, body.append(chunk))
        assertTrue(body.append(DetailChunk(3, 6, 6, "first", "def"))!!.complete)
        assertTrue(DetailBody().append(DetailChunk(0, 0, 0, "empty", ""))!!.complete)
    }

    @Test
    fun bodyOffsetsAreExactNonNegativeIntegers() {
        val base = """{"offset":0,"next_offset":3,"total_bytes":3,"version":"v","text":"abc","done":true}"""
        assertEquals(3L, parseDetailChunk(JSONObject(base))!!.totalBytes)
        for (invalid in listOf(-1, 0.5, "0", true, "9223372036854775808")) {
            assertEquals(null, parseDetailChunk(JSONObject(base).put("offset", invalid)))
        }
        assertEquals(null, parseDetailChunk(JSONObject(base).put("done", false)))
    }

    @Test
    fun collapsedDetailsKeepTheirIdentityWithoutTransportingTheirText() {
        val thread = parseThreadView(JSONObject("""{
            "session_id":"step", "entries":[
                {"index":4,"role":"tool","text":"Read file","status":"running","details_pending":true},
                {"index":5,"role":"assistant","text":"Partial answer","parts":[
                    {"index":2,"role":"reasoning","text":"","details_pending":true},
                    {"index":3,"role":"assistant","text":"Partial answer"}
                ]}
            ]
        }"""))
        assertTrue(thread.entries.first().detailsPending)
        assertEquals("Read file", thread.entries.first().text)
        val parts = thread.entries.last().parts
        assertEquals(listOf(2, 3), parts.map { it.index })
        assertTrue(parts.first().detailsPending)
        assertEquals("", parts.first().text)
        assertEquals("Partial answer", parts.last().text)
        assertFalse(parts.last().detailsPending)
        val request = DetailRequest("step", 5, 2).arguments()
        assertEquals("step", request.getString("session_id"))
        assertEquals(5, request.getInt("entry_index"))
        assertEquals(2, request.getInt("part_index"))
        assertFalse(DetailRequest("root", 4).arguments().has("part_index"))
    }

    @Test
    fun modelPagesPreserveOpaqueIdsDisabledChoicesAndSelection() {
        val first = parseModels(JSONObject("""{
            "current":"provider/model/one", "next_offset":1,
            "available":[{"id":"provider/model/one","name":"First","group":"Provider","disabled":false}]
        }"""))
        val second = parseModels(JSONObject("""{
            "current":"provider/model/two", "next_offset":null,
            "available":[{"id":"provider/model/two","name":"Second","disabled":true}]
        }"""))
        val merged = first.append(second)
        assertEquals("provider/model/two", merged.current)
        assertEquals(listOf("provider/model/one", "provider/model/two"), merged.available.map { it.id })
        assertEquals("Provider", merged.available.first().group)
        assertTrue(merged.available.last().disabled)
        assertEquals(null, merged.nextOffset)
        assertEquals(merged, merged.append(second))
    }

    @Test
    fun newControlsAreHiddenForOlderDesktops() {
        fun summary(fields: String) = parseStatus(JSONObject("""{
            "windows":[{"window":1,"thread":{"session_id":"root"$fields}}]
        }""")).windows.single().thread!!
        val legacy = summary("")
        assertFalse(legacy.modelSelection)
        assertFalse(legacy.sendNow)
        val current = summary(""", "model_selection":true, "send_now":true, "model":"provider/model" """)
        assertTrue(current.modelSelection)
        assertTrue(current.sendNow)
        assertEquals("provider/model", current.model)
    }

    @Test
    fun executionVisitsCanExceedTheRootPlanStepCount() {
        val status = parseStatus(JSONObject("""{
            "windows":[{"window":1,"architect":{
                "steps":5,"running":true,"step_number":9,"current_step":"Retrying leaf"
            }}]
        }"""))
        val architect = status.windows.single().architect!!
        assertEquals(5, architect.steps)
        assertEquals(9, architect.stepNumber)
        assertTrue(architect.stepNumber > architect.steps)
        assertTrue(architect.running)
    }

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
