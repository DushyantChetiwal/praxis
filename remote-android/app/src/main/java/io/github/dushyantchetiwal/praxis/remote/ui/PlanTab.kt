package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material.icons.outlined.AccountTree
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.CallMerge
import androidx.compose.material.icons.outlined.ChatBubbleOutline
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.ExpandLess
import androidx.compose.material.icons.outlined.ExpandMore
import androidx.compose.material.icons.outlined.FolderOpen
import androidx.compose.material.icons.outlined.Lock
import androidx.compose.material.icons.outlined.RestartAlt
import androidx.compose.material.icons.outlined.SkipNext
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.data.PlanEdge
import io.github.dushyantchetiwal.praxis.remote.data.PlanInspection
import io.github.dushyantchetiwal.praxis.remote.data.PlanNode
import io.github.dushyantchetiwal.praxis.remote.data.PlanUiState
import io.github.dushyantchetiwal.praxis.remote.data.SubagentSummary
import io.github.dushyantchetiwal.praxis.remote.data.WatchdogInfo
import io.github.dushyantchetiwal.praxis.remote.data.WorktreeInfo
import io.github.dushyantchetiwal.praxis.remote.ui.theme.OnlineGreen

data class PlanCallbacks(
    val onRefresh: () -> Unit,
    val onPlanControl: (String) -> Unit,
    val onRetryStep: (List<String>) -> Unit,
    val onSkipStep: (List<String>) -> Unit,
    val onEditStep: (path: List<String>, intent: String, rules: List<String>, capture: String) -> Unit,
    val onOpenSession: (String) -> Unit,
    val onCancelSubagent: (String) -> Unit,
    val onCreateWorktree: (String) -> Unit,
    val onMergeWorktree: (WorktreeInfo) -> Unit,
    val onRestartWatchdog: () -> Unit,
)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PlanTab(
    state: PlanUiState,
    callbacks: PlanCallbacks,
    modifier: Modifier = Modifier,
) {
    val inspection = state.inspection
    var editingNode by remember { mutableStateOf<PlanNode?>(null) }
    var showNewWorktreeDialog by remember { mutableStateOf(false) }
    var showRestartWatchdogDialog by remember { mutableStateOf(false) }

    val incomingByPath = remember(inspection?.edges) {
        inspection?.edges.orEmpty().groupBy { it.to.joinToString("/") }
    }
    val outgoingByPath = remember(inspection?.edges) {
        inspection?.edges.orEmpty().groupBy { it.from.joinToString("/") }
    }

    val hasAnyContent = (inspection?.hasPlan == true) ||
        (inspection?.subagents?.isNotEmpty() == true) ||
        state.worktrees.isNotEmpty() ||
        (state.watchdog?.supervised == true)

    PullToRefreshBox(
        isRefreshing = state.loading && hasAnyContent,
        onRefresh = callbacks.onRefresh,
        modifier = modifier.fillMaxSize(),
    ) {
        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(12.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            state.error?.let { error ->
                item(key = "error") {
                    ErrorText(error)
                }
            }

            if (state.loading && !hasAnyContent) {
                item(key = "loading") {
                    LoadingRow("Loading plan and workspace...")
                }
            }

            if (inspection != null && inspection.hasPlan) {
                item(key = "plan_run_bar") {
                    PlanRunBar(
                        inspection = inspection,
                        busy = state.busy,
                        onPlanControl = callbacks.onPlanControl,
                    )
                }
            }

            val subagents = inspection?.subagents.orEmpty()
            if (subagents.isNotEmpty()) {
                item(key = "subagents_header") {
                    SectionHeader("Sub-Agents (${subagents.size})")
                }
                items(subagents, key = { "subagent:${it.sessionId}" }) { subagent ->
                    SubagentCard(
                        subagent = subagent,
                        busy = state.busy,
                        onOpen = { callbacks.onOpenSession(subagent.sessionId) },
                        onStop = { callbacks.onCancelSubagent(subagent.sessionId) },
                    )
                }
            }

            val nodes = inspection?.nodes.orEmpty()
            if (inspection?.hasPlan == true && nodes.isNotEmpty()) {
                item(key = "nodes_header") {
                    SectionHeader("Plan Steps (${nodes.size})")
                }
                items(nodes, key = { "node:${it.pathKey}" }) { node ->
                    PlanNodeCard(
                        node = node,
                        incoming = incomingByPath[node.pathKey].orEmpty(),
                        outgoing = outgoingByPath[node.pathKey].orEmpty(),
                        busy = state.busy,
                        onOpenSession = callbacks.onOpenSession,
                        onRetry = { callbacks.onRetryStep(node.path) },
                        onEdit = { editingNode = node },
                        onSkip = { callbacks.onSkipStep(node.path) },
                    )
                }
            } else if (!state.loading && inspection?.hasPlan != true && subagents.isEmpty()) {
                item(key = "plan_empty") {
                    EmptyState(
                        title = "No active plan",
                        body = "Draft or run a plan in Architect mode on Desktop to inspect steps and sub-agents here.",
                        icon = Icons.Outlined.AccountTree,
                    )
                }
            }

            item(key = "worktrees_header") {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    SectionHeader("Git Worktrees (${state.worktrees.size})")
                    FilledTonalButton(
                        onClick = { showNewWorktreeDialog = true },
                        enabled = !state.busy,
                        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                    ) {
                        Icon(Icons.Outlined.Add, contentDescription = null, modifier = Modifier.size(16.dp))
                        Spacer(Modifier.width(6.dp))
                        Text("New Worktree")
                    }
                }
            }

            if (state.worktrees.isEmpty() && !state.loading) {
                item(key = "worktrees_empty") {
                    Surface(
                        color = MaterialTheme.colorScheme.surfaceContainerLow,
                        shape = RoundedCornerShape(14.dp),
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Text(
                            text = "No git worktrees found in this workspace.",
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(14.dp),
                        )
                    }
                }
            } else {
                items(state.worktrees, key = { "wt:${it.path}" }) { worktree ->
                    WorktreeCard(
                        worktree = worktree,
                        busy = state.busy,
                        onOpenSession = callbacks.onOpenSession,
                        onMerge = { callbacks.onMergeWorktree(worktree) },
                    )
                }
            }

            val watchdog = state.watchdog
            if (watchdog != null && watchdog.supervised) {
                item(key = "watchdog_header") {
                    SectionHeader("Desktop Watchdog")
                }
                item(key = "watchdog_card") {
                    WatchdogCard(
                        watchdog = watchdog,
                        busy = state.busy,
                        onRestartClick = { showRestartWatchdogDialog = true },
                    )
                }
            }
        }
    }

    editingNode?.let { node ->
        EditStepDialog(
            node = node,
            onDismiss = { editingNode = null },
            onSave = { intent, rules, capture ->
                editingNode = null
                callbacks.onEditStep(node.path, intent, rules, capture)
            },
        )
    }

    if (showNewWorktreeDialog) {
        NewWorktreeDialog(
            onDismiss = { showNewWorktreeDialog = false },
            onCreate = { branch ->
                showNewWorktreeDialog = false
                callbacks.onCreateWorktree(branch)
            },
        )
    }

    if (showRestartWatchdogDialog) {
        AlertDialog(
            onDismissRequest = { showRestartWatchdogDialog = false },
            title = { Text("Restart Desktop Praxis?") },
            text = {
                Text("This asks the watchdog supervisor to restart the Desktop Praxis process. Active local turns may be interrupted.")
            },
            confirmButton = {
                Button(
                    onClick = {
                        showRestartWatchdogDialog = false
                        callbacks.onRestartWatchdog()
                    },
                    colors = ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.error,
                        contentColor = MaterialTheme.colorScheme.onError,
                    ),
                ) {
                    Text("Restart")
                }
            },
            dismissButton = {
                TextButton(onClick = { showRestartWatchdogDialog = false }) {
                    Text("Cancel")
                }
            },
        )
    }
}

@Composable
private fun SectionHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(horizontal = 2.dp, vertical = 2.dp),
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PlanRunBar(
    inspection: PlanInspection,
    busy: Boolean,
    onPlanControl: (String) -> Unit,
) {
    val statusLabel = when {
        inspection.running -> "Running"
        inspection.paused -> "Paused"
        !inspection.outcome.isNullOrBlank() -> inspection.outcome
        else -> "Ready"
    }
    val (badgeContainer, badgeContent) = statusColors(
        when {
            inspection.running -> "in_progress"
            inspection.paused -> "waiting"
            else -> inspection.outcome?.lowercase() ?: "ready"
        },
    )

    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(14.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Text(
                        text = "${inspection.nodes.size} steps",
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold,
                    )
                    if (inspection.revision > 0L) {
                        Text(
                            text = "r${inspection.revision}",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                StatusPill(
                    text = statusLabel,
                    containerColor = badgeContainer,
                    contentColor = badgeContent,
                )
            }

            FlowRow(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                when {
                    inspection.running -> {
                        FilledTonalButton(
                            onClick = { onPlanControl("pause") },
                            enabled = !busy,
                        ) {
                            Icon(Icons.Filled.Pause, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Pause")
                        }
                        OutlinedButton(
                            onClick = { onPlanControl("stop") },
                            enabled = !busy,
                            colors = ButtonDefaults.outlinedButtonColors(
                                contentColor = MaterialTheme.colorScheme.error,
                            ),
                        ) {
                            Icon(Icons.Filled.Stop, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Stop")
                        }
                    }

                    inspection.paused || inspection.canResume -> {
                        Button(
                            onClick = { onPlanControl("resume") },
                            enabled = !busy,
                        ) {
                            Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Resume")
                        }
                        OutlinedButton(
                            onClick = { onPlanControl("stop") },
                            enabled = !busy,
                            colors = ButtonDefaults.outlinedButtonColors(
                                contentColor = MaterialTheme.colorScheme.error,
                            ),
                        ) {
                            Icon(Icons.Filled.Stop, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Stop")
                        }
                    }

                    else -> {
                        Button(
                            onClick = { onPlanControl("run") },
                            enabled = !busy,
                        ) {
                            Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Run")
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun SubagentCard(
    subagent: SubagentSummary,
    busy: Boolean,
    onOpen: () -> Unit,
    onStop: () -> Unit,
) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(14.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (subagent.isRunning) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(14.dp),
                        strokeWidth = 2.dp,
                    )
                } else {
                    Box(
                        modifier = Modifier
                            .size(10.dp)
                            .clip(CircleShape)
                            .background(MaterialTheme.colorScheme.outline),
                    )
                }
                Text(
                    text = subagent.title,
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Text(
                    text = "${subagent.totalEntries} entries",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            if (subagent.preview.isNotBlank()) {
                Text(
                    text = subagent.preview,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Row(
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                FilledTonalButton(
                    onClick = onOpen,
                    enabled = !busy,
                    contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                ) {
                    Icon(Icons.Outlined.ChatBubbleOutline, contentDescription = null, modifier = Modifier.size(16.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Open")
                }
                if (subagent.isRunning && subagent.canCancel) {
                    OutlinedButton(
                        onClick = onStop,
                        enabled = !busy,
                        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        colors = ButtonDefaults.outlinedButtonColors(
                            contentColor = MaterialTheme.colorScheme.error,
                        ),
                    ) {
                        Icon(Icons.Filled.Stop, contentDescription = null, modifier = Modifier.size(16.dp))
                        Spacer(Modifier.width(6.dp))
                        Text("Stop")
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PlanNodeCard(
    node: PlanNode,
    incoming: List<PlanEdge>,
    outgoing: List<PlanEdge>,
    busy: Boolean,
    onOpenSession: (String) -> Unit,
    onRetry: () -> Unit,
    onEdit: () -> Unit,
    onSkip: () -> Unit,
) {
    var expanded by remember(node.pathKey) { mutableStateOf(false) }
    val (badgeContainer, badgeContent) = statusColors(node.status)
    val hasDetails = node.intent.isNotBlank() ||
        node.rules.isNotEmpty() ||
        node.capture.isNotBlank() ||
        !node.summary.isNullOrBlank()

    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(14.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (node.path.size > 1) {
                    Text(
                        text = node.path.dropLast(1).joinToString(" / "),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Text(
                    text = node.title,
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (node.locked) {
                    Icon(
                        imageVector = Icons.Outlined.Lock,
                        contentDescription = "Locked",
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(16.dp),
                    )
                }
                StatusPill(
                    text = node.status.replace('_', ' '),
                    containerColor = badgeContainer,
                    contentColor = badgeContent,
                )
            }

            if (node.responsibility.isNotBlank() || !node.model.isNullOrBlank() || node.attempt > 0) {
                FlowRow(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    if (node.responsibility.isNotBlank()) {
                        MetaChip(node.responsibility)
                    }
                    node.model?.let { model ->
                        MetaChip(model)
                    }
                    if (node.attempt > 0) {
                        MetaChip("Attempt ${node.attempt}")
                    }
                }
            }

            node.reason?.let { reason ->
                Text(
                    text = reason,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            if (incoming.isNotEmpty() || outgoing.isNotEmpty()) {
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    if (incoming.isNotEmpty()) {
                        Text(
                            text = "From: " + incoming.joinToString(", ") { it.from.lastOrNull() ?: it.from.joinToString("/") },
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    if (outgoing.isNotEmpty()) {
                        val targets = outgoing.joinToString(", ") { edge ->
                            val target = edge.to.lastOrNull() ?: edge.to.joinToString("/")
                            if (!edge.condition.isNullOrBlank()) "$target (${edge.condition})" else target
                        }
                        Text(
                            text = "To: $targets",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }

            if (hasDetails) {
                TextButton(
                    onClick = { expanded = !expanded },
                    contentPadding = PaddingValues(horizontal = 4.dp, vertical = 0.dp),
                ) {
                    Icon(
                        imageVector = if (expanded) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore,
                        contentDescription = null,
                        modifier = Modifier.size(18.dp),
                    )
                    Spacer(Modifier.width(4.dp))
                    Text(if (expanded) "Hide brief & result" else "Show brief & result")
                }

                if (expanded) {
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .clip(RoundedCornerShape(10.dp))
                            .background(MaterialTheme.colorScheme.surfaceContainer)
                            .padding(10.dp),
                        verticalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        if (node.intent.isNotBlank()) {
                            DetailBlock("Intent", node.intent)
                        }
                        if (node.rules.isNotEmpty()) {
                            DetailBlock("Rules", node.rules.joinToString("\n") { "• $it" })
                        }
                        if (node.capture.isNotBlank()) {
                            DetailBlock("Capture", node.capture)
                        }
                        node.summary?.let { summary ->
                            DetailBlock("Result", summary)
                        }
                    }
                }
            }

            val showAnyAction = node.sessionId != null || node.canRetry || node.canEditOrSkip
            if (showAnyAction) {
                FlowRow(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    node.sessionId?.let { sessionId ->
                        FilledTonalButton(
                            onClick = { onOpenSession(sessionId) },
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Outlined.ChatBubbleOutline, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Open Chat")
                        }
                    }
                    if (node.canRetry) {
                        OutlinedButton(
                            onClick = onRetry,
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Filled.Refresh, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Retry")
                        }
                    }
                    if (node.canEditOrSkip) {
                        OutlinedButton(
                            onClick = onEdit,
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Outlined.Edit, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Edit")
                        }
                        OutlinedButton(
                            onClick = onSkip,
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Outlined.SkipNext, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Skip")
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun DetailBlock(label: String, value: String) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(
            text = label,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            fontWeight = FontWeight.SemiBold,
        )
        Text(
            text = value,
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun WorktreeCard(
    worktree: WorktreeInfo,
    busy: Boolean,
    onOpenSession: (String) -> Unit,
    onMerge: () -> Unit,
) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(14.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Icon(
                    imageVector = Icons.Outlined.FolderOpen,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.size(18.dp),
                )
                Text(
                    text = worktree.name,
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (worktree.isMain) {
                    MetaChip("Main")
                }
            }

            FlowRow(
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                MetaChip(worktree.branch ?: "detached")
                if (worktree.changedFiles > 0) {
                    MetaChip("${worktree.changedFiles} changed")
                }
            }

            Text(
                text = worktree.path,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )

            val canMerge = !worktree.isMain && worktree.branch != null
            if (worktree.sessionId != null || canMerge) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    worktree.sessionId?.let { sessionId ->
                        FilledTonalButton(
                            onClick = { onOpenSession(sessionId) },
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Outlined.ChatBubbleOutline, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Open Chat")
                        }
                    }
                    if (canMerge) {
                        OutlinedButton(
                            onClick = onMerge,
                            enabled = !busy,
                            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                        ) {
                            Icon(Icons.Outlined.CallMerge, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Merge")
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun WatchdogCard(
    watchdog: WatchdogInfo,
    busy: Boolean,
    onRestartClick: () -> Unit,
) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(14.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    OnlineDot(online = watchdog.childPid != null)
                    Text(
                        text = "Supervisor active",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
                Text(
                    text = "Restarts: ${watchdog.restarts}",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            val heartbeatText = watchdog.heartbeatAgeMs?.let { ageMs ->
                if (ageMs < 1_000L) "${ageMs}ms ago" else "${ageMs / 1_000L}s ago"
            } ?: "Unknown"

            Text(
                text = buildString {
                    append("Heartbeat: $heartbeatText")
                    watchdog.childPid?.let { append(" • PID $it") }
                    watchdog.supervisorPid?.let { append(" • Sup $it") }
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            watchdog.lastExitReason?.let { reason ->
                Text(
                    text = "Last exit: $reason",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            OutlinedButton(
                onClick = onRestartClick,
                enabled = !busy,
                colors = ButtonDefaults.outlinedButtonColors(
                    contentColor = MaterialTheme.colorScheme.error,
                ),
            ) {
                Icon(Icons.Outlined.RestartAlt, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(6.dp))
                Text("Restart Praxis")
            }
        }
    }
}

@Composable
private fun EditStepDialog(
    node: PlanNode,
    onDismiss: () -> Unit,
    onSave: (intent: String, rules: List<String>, capture: String) -> Unit,
) {
    var intent by remember(node.pathKey) { mutableStateOf(node.intent) }
    var rulesText by remember(node.pathKey) { mutableStateOf(node.rules.joinToString("\n")) }
    var capture by remember(node.pathKey) { mutableStateOf(node.capture) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Edit ${node.title}") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                OutlinedTextField(
                    value = intent,
                    onValueChange = { intent = it },
                    label = { Text("Intent") },
                    minLines = 2,
                    maxLines = 4,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = rulesText,
                    onValueChange = { rulesText = it },
                    label = { Text("Rules (one per line)") },
                    minLines = 2,
                    maxLines = 5,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = capture,
                    onValueChange = { capture = it },
                    label = { Text("Capture") },
                    minLines = 1,
                    maxLines = 3,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            Button(
                onClick = {
                    val rules = rulesText
                        .lineSequence()
                        .map { it.trim() }
                        .filter { it.isNotEmpty() }
                        .toList()
                    onSave(intent.trim(), rules, capture.trim())
                },
            ) {
                Text("Save")
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancel")
            }
        },
    )
}

@Composable
private fun NewWorktreeDialog(
    onDismiss: () -> Unit,
    onCreate: (String) -> Unit,
) {
    var branch by remember { mutableStateOf("") }
    val trimmed = branch.trim()
    val valid = trimmed.isNotEmpty() && !trimmed.startsWith("-") && !trimmed.contains("..") && trimmed.none { it.isWhitespace() }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New Git Worktree") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    text = "Creates a new branch and worktree directory on Desktop.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(2.dp))
                OutlinedTextField(
                    value = branch,
                    onValueChange = { branch = it },
                    label = { Text("Branch name") },
                    placeholder = { Text("feature/my-task") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            Button(
                onClick = { onCreate(trimmed) },
                enabled = valid,
            ) {
                Text("Create")
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancel")
            }
        },
    )
}

@Composable
private fun StatusPill(
    text: String,
    containerColor: Color,
    contentColor: Color,
) {
    Surface(
        color = containerColor,
        contentColor = contentColor,
        shape = RoundedCornerShape(999.dp),
    ) {
        Text(
            text = text,
            style = MaterialTheme.typography.labelSmall,
            fontWeight = FontWeight.Medium,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 3.dp),
        )
    }
}

@Composable
private fun MetaChip(text: String) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        contentColor = MaterialTheme.colorScheme.onSurfaceVariant,
        shape = RoundedCornerShape(8.dp),
    ) {
        Text(
            text = text,
            style = MaterialTheme.typography.labelSmall,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 3.dp),
        )
    }
}

@Composable
private fun statusColors(status: String): Pair<Color, Color> {
    val scheme = MaterialTheme.colorScheme
    return when (status.lowercase()) {
        "completed", "succeeded" -> OnlineGreen.copy(alpha = 0.16f) to OnlineGreen
        "in_progress", "running" -> scheme.primaryContainer to scheme.onPrimaryContainer
        "failed", "error" -> scheme.errorContainer to scheme.onErrorContainer
        "ready" -> scheme.secondaryContainer to scheme.onSecondaryContainer
        else -> scheme.surfaceContainerHigh to scheme.onSurfaceVariant
    }
}
