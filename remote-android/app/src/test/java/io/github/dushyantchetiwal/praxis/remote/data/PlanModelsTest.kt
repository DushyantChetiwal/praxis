package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PlanModelsTest {
    @Test
    fun parsePlanInspectionRoundTripsFullDesktopPayload() {
        val json = JSONObject(
            """
            {
              "has_plan": true,
              "revision": 7,
              "running": true,
              "paused": false,
              "can_resume": false,
              "outcome": null,
              "nodes": [
                {
                  "path": ["build"],
                  "id": "build",
                  "title": "Build project",
                  "responsibility": "Compilation",
                  "intent": "Compile workspace cleanly",
                  "rules": ["No warnings", "Use clippy"],
                  "capture": "Binary path",
                  "locked": true,
                  "status": "completed",
                  "reason": "",
                  "model": {"provider": "anthropic", "model": "claude-sonnet-4-5"},
                  "session_id": "sess-build",
                  "summary": "Compiled with 0 warnings",
                  "attempt": 1
                },
                {
                  "path": ["verify", "unit"],
                  "id": "unit",
                  "title": "Run unit tests",
                  "responsibility": "Testing",
                  "intent": "Pass all unit tests",
                  "rules": [],
                  "capture": "Failing test names",
                  "locked": false,
                  "status": "failed",
                  "reason": "1 test failed",
                  "model": null,
                  "session_id": null,
                  "summary": null,
                  "attempt": 2
                }
              ],
              "edges": [
                {
                  "from": ["build"],
                  "to": ["verify", "unit"],
                  "condition": "Build succeeded"
                }
              ],
              "subagents": [
                {
                  "session_id": "sub-1",
                  "title": "Investigate parser",
                  "status": "generating",
                  "total_entries": 14,
                  "can_cancel": true,
                  "preview": {
                    "index": 13,
                    "role": "assistant",
                    "text": "Checking edge cases in parser.rs",
                    "status": "in_progress"
                  }
                }
              ]
            }
            """.trimIndent(),
        )

        val inspection = parsePlanInspection(json)
        assertTrue(inspection.hasPlan)
        assertEquals(7L, inspection.revision)
        assertTrue(inspection.running)
        assertFalse(inspection.paused)
        assertFalse(inspection.canResume)
        assertNull(inspection.outcome)

        assertEquals(2, inspection.nodes.size)
        val build = inspection.nodes[0]
        assertEquals(listOf("build"), build.path)
        assertEquals("build", build.pathKey)
        assertEquals("Build project", build.title)
        assertEquals("Compilation", build.responsibility)
        assertEquals(listOf("No warnings", "Use clippy"), build.rules)
        assertTrue(build.locked)
        assertEquals("completed", build.status)
        assertNull(build.reason)
        assertEquals("anthropic/claude-sonnet-4-5", build.model)
        assertEquals("sess-build", build.sessionId)
        assertEquals("Compiled with 0 warnings", build.summary)
        assertEquals(1, build.attempt)
        assertFalse(build.canEditOrSkip)
        assertFalse(build.canRetry)

        val unit = inspection.nodes[1]
        assertEquals(listOf("verify", "unit"), unit.path)
        assertEquals("verify/unit", unit.pathKey)
        assertEquals("1 test failed", unit.reason)
        assertNull(unit.model)
        assertNull(unit.sessionId)
        assertEquals(2, unit.attempt)
        assertTrue(unit.canEditOrSkip)
        assertTrue(unit.canRetry)

        assertEquals(1, inspection.edges.size)
        assertEquals(
            PlanEdge(
                from = listOf("build"),
                to = listOf("verify", "unit"),
                condition = "Build succeeded",
            ),
            inspection.edges[0],
        )

        assertEquals(1, inspection.subagents.size)
        val sub = inspection.subagents[0]
        assertEquals("sub-1", sub.sessionId)
        assertEquals("Investigate parser", sub.title)
        assertEquals("generating", sub.status)
        assertEquals(14, sub.totalEntries)
        assertTrue(sub.canCancel)
        assertTrue(sub.isRunning)
        assertEquals("Checking edge cases in parser.rs", sub.preview)
    }

    @Test
    fun parseWorktreesAndWatchdogRoundTrip() {
        val worktreesJson = JSONObject(
            """
            {
              "worktrees": [
                {
                  "path": "/repo/main",
                  "name": "main",
                  "branch": "Enhanced_Agents",
                  "is_main": true,
                  "session_id": "sess-main",
                  "changed_files": 3
                },
                {
                  "path": "/repo/wt-fix",
                  "name": "wt-fix",
                  "branch": "fix/plan-ui",
                  "is_main": false,
                  "session_id": null
                }
              ]
            }
            """.trimIndent(),
        )
        val worktrees = parseWorktrees(worktreesJson)
        assertEquals(2, worktrees.size)
        assertEquals(
            WorktreeInfo(
                path = "/repo/main",
                name = "main",
                branch = "Enhanced_Agents",
                isMain = true,
                sessionId = "sess-main",
                changedFiles = 3,
            ),
            worktrees[0],
        )
        assertEquals(
            WorktreeInfo(
                path = "/repo/wt-fix",
                name = "wt-fix",
                branch = "fix/plan-ui",
                isMain = false,
                sessionId = null,
                changedFiles = 0,
            ),
            worktrees[1],
        )

        val watchdogJson = JSONObject(
            """
            {
              "supervised": true,
              "supervisor_pid": 4120,
              "child_pid": 8832,
              "restarts": 1,
              "heartbeat_age_ms": 1450,
              "last_exit_reason": "signal 15"
            }
            """.trimIndent(),
        )
        val watchdog = parseWatchdogInfo(watchdogJson)
        assertEquals(
            WatchdogInfo(
                supervised = true,
                supervisorPid = 4120L,
                childPid = 8832L,
                restarts = 1,
                heartbeatAgeMs = 1450L,
                lastExitReason = "signal 15",
            ),
            watchdog,
        )
    }

    @Test
    fun parsersHandleNullAndMalformedInputsGracefully() {
        assertEquals(PlanInspection(), parsePlanInspection(null))
        assertEquals(emptyList<WorktreeInfo>(), parseWorktrees(null))
        assertEquals(WatchdogInfo(), parseWatchdogInfo(null))

        val malformedPlan = JSONObject(
            """
            {
              "has_plan": true,
              "nodes": [
                "not-an-object",
                {"path": [], "id": ""},
                {"id": "solo", "status": "in_progress", "attempt": -5},
                {"path": ["a", "b"], "model": "gpt-5"}
              ],
              "edges": [
                123,
                {"from": [], "to": ["a"]},
                {"from": ["a"], "to": ["b"], "condition": ""}
              ],
              "subagents": [
                null,
                {"title": "missing id"},
                {"session_id": "s1", "status": "idle", "preview": "plain string"}
              ]
            }
            """.trimIndent(),
        )
        val parsed = parsePlanInspection(malformedPlan)
        assertEquals(2, parsed.nodes.size)
        assertEquals(listOf("solo"), parsed.nodes[0].path)
        assertEquals(0, parsed.nodes[0].attempt)
        assertFalse(parsed.nodes[0].canEditOrSkip)
        assertEquals("b", parsed.nodes[1].id)
        assertEquals("gpt-5", parsed.nodes[1].model)

        assertEquals(1, parsed.edges.size)
        assertNull(parsed.edges[0].condition)

        assertEquals(1, parsed.subagents.size)
        assertEquals("plain string", parsed.subagents[0].preview)
        assertFalse(parsed.subagents[0].isRunning)

        val malformedWorktrees = JSONObject(
            """
            {
              "worktrees": [
                "bad",
                {"name": "no-path"},
                {"path": "/tmp/wt-1", "branch": ""}
              ]
            }
            """.trimIndent(),
        )
        val worktrees = parseWorktrees(malformedWorktrees)
        assertEquals(1, worktrees.size)
        assertEquals("wt-1", worktrees[0].name)
        assertNull(worktrees[0].branch)
    }

    @Test
    fun nodeAndSubagentStateHelpersBehaveAsExpected() {
        val pending = PlanNode(path = listOf("a"), id = "a", title = "A", status = "pending", locked = false)
        assertTrue(pending.canEditOrSkip)
        assertTrue(pending.canRetry)

        val locked = pending.copy(locked = true)
        assertFalse(locked.canEditOrSkip)
        assertTrue(locked.canRetry)

        val inProgress = pending.copy(status = "in_progress")
        assertFalse(inProgress.canEditOrSkip)
        assertFalse(inProgress.canRetry)

        val completed = pending.copy(status = "completed")
        assertFalse(completed.canEditOrSkip)
        assertFalse(completed.canRetry)

        val skipped = pending.copy(status = "skipped")
        assertTrue(skipped.canEditOrSkip)
        assertTrue(skipped.canRetry)

        val runningSub = SubagentSummary(sessionId = "s1", title = "Sub", status = "generating")
        assertTrue(runningSub.isRunning)
        val idleSub = runningSub.copy(status = "idle")
        assertFalse(idleSub.isRunning)
    }

    @Test
    fun requestPayloadBuildersProduceExactRpcSchemas() {
        val runJson = buildPlanRunActionJson("pause")
        assertEquals("pause", runJson.getString("action"))

        val retryJson = buildRetryStepJson(listOf("parent", "child"))
        assertEquals("retry_step", retryJson.getString("action"))
        assertEquals(JSONArray(listOf("parent", "child")).toString(), retryJson.getJSONArray("path").toString())

        val skipJson = buildSkipStepJson(listOf("step-1"))
        assertEquals("skip_step", skipJson.getString("action"))
        assertEquals(JSONArray(listOf("step-1")).toString(), skipJson.getJSONArray("path").toString())

        val editJson = buildEditStepJson(
            path = listOf("step-1"),
            intent = "New intent",
            rules = listOf("Rule 1", "Rule 2"),
            capture = "Output",
        )
        assertEquals("edit_step", editJson.getString("action"))
        assertEquals("New intent", editJson.getString("intent"))
        assertEquals(JSONArray(listOf("Rule 1", "Rule 2")).toString(), editJson.getJSONArray("rules").toString())
        assertEquals("Output", editJson.getString("capture"))

        val cancelSubJson = buildCancelSubagentJson("sub-42")
        assertEquals("cancel_subagent", cancelSubJson.getString("action"))
        assertEquals("sub-42", cancelSubJson.getString("session_id"))

        val createWtJson = buildWorktreeCreateJson("feature/x")
        assertEquals("feature/x", createWtJson.getString("branch"))

        val mergeWtNoPath = buildWorktreeMergeJson("feature/x")
        assertEquals("feature/x", mergeWtNoPath.getString("branch"))
        assertFalse(mergeWtNoPath.has("path"))

        val mergeWtWithPath = buildWorktreeMergeJson("feature/x", "/repo/wt-x")
        assertEquals("feature/x", mergeWtWithPath.getString("branch"))
        assertEquals("/repo/wt-x", mergeWtWithPath.getString("path"))
    }
}
