package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONArray
import org.json.JSONObject

data class PlanInspection(
    val hasPlan: Boolean = false,
    val revision: Long = 0L,
    val running: Boolean = false,
    val paused: Boolean = false,
    val canResume: Boolean = false,
    val outcome: String? = null,
    val nodes: List<PlanNode> = emptyList(),
    val edges: List<PlanEdge> = emptyList(),
    val subagents: List<SubagentSummary> = emptyList(),
)

data class PlanNode(
    val path: List<String>,
    val id: String,
    val title: String,
    val responsibility: String = "",
    val intent: String = "",
    val rules: List<String> = emptyList(),
    val capture: String = "",
    val locked: Boolean = false,
    val status: String = "pending",
    val reason: String? = null,
    val model: String? = null,
    val sessionId: String? = null,
    val summary: String? = null,
    val attempt: Int = 0,
) {
    val pathKey: String get() = path.joinToString("/")
    val canEditOrSkip: Boolean get() = !locked && status !in setOf("in_progress", "completed")
    val canRetry: Boolean get() = status in setOf("failed", "ready", "pending", "waiting", "skipped")
}

data class PlanEdge(
    val from: List<String>,
    val to: List<String>,
    val condition: String? = null,
)

data class SubagentSummary(
    val sessionId: String,
    val title: String,
    val status: String,
    val totalEntries: Int = 0,
    val canCancel: Boolean = false,
    val preview: String = "",
) {
    val isRunning: Boolean get() = status == "generating"
}

data class WorktreeInfo(
    val path: String,
    val name: String,
    val branch: String? = null,
    val isMain: Boolean = false,
    val sessionId: String? = null,
    val changedFiles: Int = 0,
)

data class WatchdogInfo(
    val supervised: Boolean = false,
    val supervisorPid: Long? = null,
    val childPid: Long? = null,
    val restarts: Int = 0,
    val heartbeatAgeMs: Long? = null,
    val lastExitReason: String? = null,
)

data class PlanUiState(
    val loading: Boolean = false,
    val busy: Boolean = false,
    val error: String? = null,
    val inspection: PlanInspection? = null,
    val worktrees: List<WorktreeInfo> = emptyList(),
    val watchdog: WatchdogInfo? = null,
)

fun buildPlanRunActionJson(action: String): JSONObject =
    JSONObject().put("action", action)

fun buildRetryStepJson(path: List<String>): JSONObject =
    JSONObject()
        .put("action", "retry_step")
        .put("path", JSONArray(path))

fun buildSkipStepJson(path: List<String>): JSONObject =
    JSONObject()
        .put("action", "skip_step")
        .put("path", JSONArray(path))

fun buildEditStepJson(
    path: List<String>,
    intent: String?,
    rules: List<String>?,
    capture: String?,
): JSONObject = JSONObject().apply {
    put("action", "edit_step")
    put("path", JSONArray(path))
    if (intent != null) put("intent", intent)
    if (rules != null) put("rules", JSONArray(rules))
    if (capture != null) put("capture", capture)
}

fun buildCancelSubagentJson(sessionId: String): JSONObject =
    JSONObject()
        .put("action", "cancel_subagent")
        .put("session_id", sessionId)

fun buildWorktreeCreateJson(branch: String): JSONObject =
    JSONObject().put("branch", branch)

fun buildWorktreeMergeJson(branch: String, path: String? = null): JSONObject = JSONObject().apply {
    put("branch", branch)
    if (!path.isNullOrBlank()) put("path", path)
}

fun parsePlanInspection(json: JSONObject?): PlanInspection {
    if (json == null) return PlanInspection()
    val nodes = json.arr("nodes")?.objects().orEmpty().mapNotNull(::parsePlanNode)
    val edges = json.arr("edges")?.objects().orEmpty().mapNotNull(::parsePlanEdge)
    val subagents = json.arr("subagents")?.objects().orEmpty().mapNotNull(::parseSubagentSummary)
    return PlanInspection(
        hasPlan = json.bool("has_plan"),
        revision = json.long("revision") ?: 0L,
        running = json.bool("running"),
        paused = json.bool("paused"),
        canResume = json.bool("can_resume"),
        outcome = json.str("outcome")?.takeIf { it.isNotBlank() },
        nodes = nodes,
        edges = edges,
        subagents = subagents,
    )
}

private fun parsePlanNode(obj: JSONObject): PlanNode? {
    val rawPath = obj.arr("path")?.strings()?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
    val rawId = obj.str("id")?.trim()?.takeIf { it.isNotEmpty() }
    val path = rawPath.ifEmpty { listOfNotNull(rawId) }
    val id = rawId ?: path.lastOrNull() ?: return null
    if (path.isEmpty()) return null

    val modelObj = obj.obj("model")
    val model = if (modelObj != null) {
        val provider = modelObj.str("provider")?.trim()?.takeIf { it.isNotEmpty() }
        val modelId = modelObj.str("model")?.trim()?.takeIf { it.isNotEmpty() }
        when {
            provider != null && modelId != null -> "$provider/$modelId"
            modelId != null -> modelId
            else -> provider
        }
    } else {
        obj.str("model")?.trim()?.takeIf { it.isNotEmpty() }
    }

    return PlanNode(
        path = path,
        id = id,
        title = obj.str("title")?.takeIf { it.isNotBlank() } ?: id,
        responsibility = obj.str("responsibility") ?: "",
        intent = obj.str("intent") ?: "",
        rules = obj.arr("rules")?.strings()?.filter { it.isNotBlank() }.orEmpty(),
        capture = obj.str("capture") ?: "",
        locked = obj.bool("locked"),
        status = obj.str("status")?.takeIf { it.isNotBlank() } ?: "pending",
        reason = obj.str("reason")?.takeIf { it.isNotBlank() },
        model = model,
        sessionId = obj.str("session_id")?.takeIf { it.isNotBlank() },
        summary = obj.str("summary")?.takeIf { it.isNotBlank() },
        attempt = (obj.int("attempt") ?: 0).coerceAtLeast(0),
    )
}

private fun parsePlanEdge(obj: JSONObject): PlanEdge? {
    val from = obj.arr("from")?.strings()?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
    val to = obj.arr("to")?.strings()?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
    if (from.isEmpty() || to.isEmpty()) return null
    return PlanEdge(
        from = from,
        to = to,
        condition = obj.str("condition")?.takeIf { it.isNotBlank() },
    )
}

private fun parseSubagentSummary(obj: JSONObject): SubagentSummary? {
    val sessionId = obj.str("session_id")?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    val previewObj = obj.obj("preview")
    val preview = if (previewObj != null) {
        previewObj.str("text")?.trim()?.takeIf { it.isNotEmpty() }
            ?: previewObj.arr("parts")?.objects().orEmpty()
                .firstNotNullOfOrNull { it.str("text")?.trim()?.takeIf { text -> text.isNotEmpty() } }
            ?: ""
    } else {
        obj.str("preview")?.trim() ?: ""
    }
    return SubagentSummary(
        sessionId = sessionId,
        title = obj.str("title")?.takeIf { it.isNotBlank() } ?: "Sub-agent",
        status = obj.str("status")?.takeIf { it.isNotBlank() } ?: "idle",
        totalEntries = (obj.int("total_entries") ?: 0).coerceAtLeast(0),
        canCancel = obj.bool("can_cancel"),
        preview = preview,
    )
}

fun parseWorktrees(json: JSONObject?): List<WorktreeInfo> {
    if (json == null) return emptyList()
    return json.arr("worktrees")?.objects().orEmpty().mapNotNull { obj ->
        val path = obj.str("path")?.trim()?.takeIf { it.isNotEmpty() } ?: return@mapNotNull null
        val name = obj.str("name")?.takeIf { it.isNotBlank() }
            ?: path.substringAfterLast('/').substringAfterLast('\\').ifBlank { path }
        WorktreeInfo(
            path = path,
            name = name,
            branch = obj.str("branch")?.takeIf { it.isNotBlank() },
            isMain = obj.bool("is_main"),
            sessionId = obj.str("session_id")?.takeIf { it.isNotBlank() },
            changedFiles = (obj.int("changed_files") ?: 0).coerceAtLeast(0),
        )
    }
}

fun parseWatchdogInfo(json: JSONObject?): WatchdogInfo {
    if (json == null) return WatchdogInfo()
    return WatchdogInfo(
        supervised = json.bool("supervised"),
        supervisorPid = json.long("supervisor_pid"),
        childPid = json.long("child_pid"),
        restarts = (json.int("restarts") ?: 0).coerceAtLeast(0),
        heartbeatAgeMs = json.long("heartbeat_age_ms"),
        lastExitReason = json.str("last_exit_reason")?.takeIf { it.isNotBlank() },
    )
}
