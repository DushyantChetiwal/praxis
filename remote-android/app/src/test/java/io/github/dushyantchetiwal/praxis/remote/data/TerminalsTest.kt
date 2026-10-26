package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalsTest {
    @Test
    fun aThreadSummaryReportsTheTurnErrorRetriesAndBackgroundCommands() {
        val status = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{
            "session_id":"s","queued":0,"pending":[],
            "error":{"kind":"stream_error","message":"Connection to Anthropic's API was interrupted.","retryable":true},
            "retrying":{"attempt":3,"max_attempts":18,"message":"Can't reach api.anthropic.com."},
            "dismiss_error":true,"compact":true,"terminals":true,"background_commands":2}}]}"""))
        val thread = status.windows.single().thread!!
        assertEquals(TurnError("stream_error", "Connection to Anthropic's API was interrupted.", true), thread.error)
        assertEquals(RetryInfo(3, 18, "Can't reach api.anthropic.com."), thread.retrying)
        assertTrue(thread.dismissError && thread.compact && thread.terminals)
        assertEquals(2, thread.backgroundCommands)

        // An older Praxis sends none of these and the phone hides the controls.
        val older = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{"session_id":"s","queued":0,"pending":[]}}]}"""))
        val plain = older.windows.single().thread!!
        assertNull(plain.error)
        assertNull(plain.retrying)
        assertFalse(plain.dismissError || plain.compact || plain.terminals)
        assertEquals(0, plain.backgroundCommands)
    }

    @Test
    fun aThreadSummaryCarriesTheAgentChecklist() {
        val status = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{
            "session_id":"s","queued":0,"pending":[],"plan":[
            {"step":"Read the loader","status":"completed"},
            {"step":"Add the setting","status":"in_progress"},
            {"status":"pending"},
            {"step":"Run the tests"}]}}]}"""))
        val plan = status.windows.single().thread!!.plan
        assertEquals(
            listOf(
                PlanStep("Read the loader", "completed"),
                PlanStep("Add the setting", "in_progress"),
                PlanStep("Run the tests", "pending"),
            ),
            plan,
        )

        // An older Praxis sends no checklist and the phone shows none.
        val older = parseStatus(JSONObject("""{"windows":[{"window":1,"projects":[],"thread":{"session_id":"s","queued":0,"pending":[]}}]}"""))
        assertTrue(older.windows.single().thread!!.plan.isEmpty())
    }

    @Test
    fun terminalsParseAndSkipEntriesWithoutAnId() {
        val tasks = parseTerminals(JSONObject("""{"terminals":[
            {"id":"a","command":"cargo build","running":true,"in_background":true,"elapsed_ms":61000},
            {"command":"no id"},
            {"id":"b","command":"ls","running":false,"in_background":false,"elapsed_ms":20}]}"""))
        assertEquals(listOf("a", "b"), tasks.map { it.id })
        assertTrue(tasks[0].running && tasks[0].inBackground)
        assertEquals(61_000L, tasks[0].elapsedMillis)
        assertTrue(parseTerminals(JSONObject("{}")).isEmpty())
    }

    @Test
    fun earlierOutputPagesArePrependedOnceAndAMovedOutputStartsOver() {
        val latest = parseTerminalOutput(JSONObject("""{"text":"world","start":6,"end":11,"total":11,"running":false}"""))!!
        val loaded = TerminalOutput().with(latest)
        assertEquals("world", loaded.text)
        assertTrue(loaded.hasEarlier)

        val earlier = parseTerminalOutput(JSONObject("""{"text":"hello ","start":0,"end":6,"total":11,"running":false}"""))!!
        val full = loaded.with(earlier)
        assertEquals("hello world", full.text)
        assertFalse(full.hasEarlier)

        val moved = parseTerminalOutput(JSONObject("""{"text":"new","start":0,"end":3,"total":3,"running":true}"""))!!
        assertEquals("new", full.with(moved).text)
        assertNull(parseTerminalOutput(JSONObject("""{"text":"bad","start":5,"end":2}""")))
    }
}
