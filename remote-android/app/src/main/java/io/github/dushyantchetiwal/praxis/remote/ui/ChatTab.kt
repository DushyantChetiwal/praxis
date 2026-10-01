package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.interaction.collectIsDraggedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Block
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.ErrorOutline
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.HourglassTop
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.RemoveCircleOutline
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material.icons.outlined.AccountTree
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.ChatBubbleOutline
import androidx.compose.material.icons.outlined.Shield
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.key
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.OutboxItem
import io.github.dushyantchetiwal.praxis.remote.OutboxState
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.Tab
import io.github.dushyantchetiwal.praxis.remote.data.Architect
import io.github.dushyantchetiwal.praxis.remote.data.Entry
import io.github.dushyantchetiwal.praxis.remote.data.HistoryScrollAnchor
import io.github.dushyantchetiwal.praxis.remote.data.HistoryScrollGate
import io.github.dushyantchetiwal.praxis.remote.data.TranscriptHistory
import io.github.dushyantchetiwal.praxis.remote.data.followLatestAfterScroll
import io.github.dushyantchetiwal.praxis.remote.data.historyItemKey
import io.github.dushyantchetiwal.praxis.remote.data.stepItemKey
import io.github.dushyantchetiwal.praxis.remote.data.transcriptEntryKey
import io.github.dushyantchetiwal.praxis.remote.data.transcriptItemKeys
import kotlinx.coroutines.flow.first
import io.github.dushyantchetiwal.praxis.remote.data.ModeInfo
import io.github.dushyantchetiwal.praxis.remote.data.Permission
import io.github.dushyantchetiwal.praxis.remote.ui.theme.OnlineGreen

@Composable
fun ChatTab(ui: DeviceUi, vm: MainViewModel) {
    val window = ui.currentWindow()
    val summary = window?.thread
    val now = rememberNow(5_000L)
    val generating = ui.isGenerating()

    Column(Modifier.fillMaxSize()) {
        ThreadHeader(ui, generating)
        summary?.mode?.takeIf { it.available.isNotEmpty() }?.let { mode ->
            ModeSelector(mode, ui.currentModeId(now), vm::setMode)
        }
        window?.architect?.takeIf { it.steps > 0 }?.let { ArchitectCard(it, vm) }
        val permissions = ui.visiblePermissions()
        if (permissions.isNotEmpty()) {
            Column(
                Modifier
                    .heightIn(max = 340.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(horizontal = 12.dp, vertical = 4.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                permissions.forEach { permission ->
                    PermissionCard(permission, busy = permission.key in ui.busyPermissions, vm = vm)
                }
            }
        }
        Box(Modifier.weight(1f)) {
            Transcript(ui, vm)
        }
        Composer(ui, vm, generating)
    }
}

@Composable
private fun ThreadHeader(ui: DeviceUi, generating: Boolean) {
    val window = ui.currentWindow()
    val summary = window?.thread
    val title = when {
        ui.device == null -> stringResource(R.string.chat_no_device)
        ui.status == null -> stringResource(R.string.chat_connecting)
        window == null -> stringResource(R.string.chat_no_window)
        else -> ui.thread?.title?.takeIf { it.isNotBlank() }
            ?: summary?.title?.takeIf { it.isNotBlank() }
            ?: stringResource(if (summary != null) R.string.chat_untitled else R.string.chat_no_thread)
    }
    val status = ui.thread?.status ?: summary?.status
    Row(
        Modifier
            .fillMaxWidth()
            .padding(start = 16.dp, end = 12.dp, top = 8.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            title,
            style = MaterialTheme.typography.titleMedium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        if (status != null) {
            Spacer(Modifier.width(8.dp))
            StatusChip(generating, summary?.queued ?: 0)
        }
    }
}

@Composable
private fun StatusChip(generating: Boolean, queued: Int) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        shape = RoundedCornerShape(50),
        color = if (generating) scheme.primaryContainer else scheme.surfaceContainerHigh,
        contentColor = if (generating) scheme.onPrimaryContainer else scheme.onSurfaceVariant,
    ) {
        Row(Modifier.padding(horizontal = 10.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            if (generating) {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = scheme.onPrimaryContainer)
                Spacer(Modifier.width(6.dp))
            }
            val text = stringResource(if (generating) R.string.status_working else R.string.status_idle)
            val queuedText = if (queued > 0) " · " + pluralStringResource(R.plurals.status_queued, queued, queued) else ""
            Text(text + queuedText, style = MaterialTheme.typography.labelMedium)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ModeSelector(mode: ModeInfo, current: String?, onSelect: (String) -> Unit) {
    val modes = mode.available
    if (modes.size <= 4) {
        SingleChoiceSegmentedButtonRow(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 4.dp),
        ) {
            modes.forEachIndexed { index, option ->
                SegmentedButton(
                    selected = option.id == current,
                    onClick = { onSelect(option.id) },
                    shape = SegmentedButtonDefaults.itemShape(index, modes.size),
                    label = { Text(option.name, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                )
            }
        }
    } else {
        Row(
            Modifier
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 12.dp, vertical = 2.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            modes.forEach { option ->
                FilterChip(
                    selected = option.id == current,
                    onClick = { onSelect(option.id) },
                    label = { Text(option.name) },
                )
            }
        }
    }
}

@Composable
private fun ArchitectCard(architect: Architect, vm: MainViewModel) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 4.dp),
    ) {
        Row(Modifier.padding(start = 14.dp, end = 10.dp, top = 10.dp, bottom = 10.dp), verticalAlignment = Alignment.CenterVertically) {
            Icon(Icons.Outlined.AccountTree, contentDescription = null)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.architect_title), style = MaterialTheme.typography.labelLarge)
                val line = if (architect.running) {
                    val step = if (architect.stepNumber > 0) architect.stepNumber.toString() else "?"
                    stringResource(R.string.architect_step, step, architect.steps) +
                        (architect.currentStep?.let { " · $it" } ?: "")
                } else {
                    pluralStringResource(R.plurals.architect_planned, architect.steps, architect.steps)
                }
                Text(line, style = MaterialTheme.typography.bodyMedium, maxLines = 2, overflow = TextOverflow.Ellipsis)
                if (!architect.running && architect.outcome != null) {
                    Text(
                        architect.outcome,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSecondaryContainer.copy(alpha = 0.8f),
                    )
                }
            }
            Spacer(Modifier.width(8.dp))
            if (architect.running) {
                OutlinedButton(
                    onClick = { vm.architectAction(run = false) },
                    colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
                ) {
                    Icon(Icons.Filled.Stop, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(4.dp))
                    Text(stringResource(R.string.action_stop))
                }
            } else {
                Button(onClick = { vm.architectAction(run = true) }) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(4.dp))
                    Text(stringResource(R.string.architect_run))
                }
            }
        }
    }
}

@OptIn(androidx.compose.foundation.layout.ExperimentalLayoutApi::class)
@Composable
private fun PermissionCard(permission: Permission, busy: Boolean, vm: MainViewModel) {
    var expanded by remember(permission.key) { mutableStateOf(false) }
    val scheme = MaterialTheme.colorScheme
    Card(
        colors = CardDefaults.cardColors(containerColor = scheme.tertiaryContainer, contentColor = scheme.onTertiaryContainer),
        modifier = Modifier
            .fillMaxWidth()
            .alpha(if (busy) 0.6f else 1f),
    ) {
        Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Shield, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(stringResource(R.string.permission_needed), style = MaterialTheme.typography.labelLarge, modifier = Modifier.weight(1f))
                if (busy) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
            }
            Text(
                inlineMarkdown(permission.title?.takeIf { it.isNotBlank() } ?: stringResource(R.string.permission_tool_call)),
                style = MaterialTheme.typography.titleSmall,
            )
            if (!permission.detail.isNullOrBlank()) {
                if (expanded) {
                    Surface(color = scheme.surface.copy(alpha = 0.6f), shape = RoundedCornerShape(8.dp)) {
                        Markdown(permission.detail, Modifier.padding(10.dp), style = MaterialTheme.typography.bodySmall)
                    }
                }
                TextButton(onClick = { expanded = !expanded }, contentPadding = PaddingValues(horizontal = 4.dp)) {
                    Text(stringResource(if (expanded) R.string.permission_hide_details else R.string.permission_show_details))
                    Icon(if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore, contentDescription = null)
                }
            }
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                permission.options.forEach { option ->
                    val onClick = { vm.answerPermission(permission, option) }
                    when {
                        option.isAllow -> Button(onClick = onClick, enabled = !busy) { Text(option.name) }
                        option.isReject -> OutlinedButton(
                            onClick = onClick,
                            enabled = !busy,
                            colors = ButtonDefaults.outlinedButtonColors(contentColor = scheme.error),
                        ) { Text(option.name) }
                        else -> OutlinedButton(onClick = onClick, enabled = !busy) { Text(option.name) }
                    }
                }
            }
        }
    }
}

@Composable
private fun Transcript(ui: DeviceUi, vm: MainViewModel) {
    key(ui.device?.channel, ui.windowId, ui.thread?.sessionId) {
        TranscriptContent(ui, vm)
    }
}

@Composable
private fun TranscriptContent(ui: DeviceUi, vm: MainViewModel) {
    val thread = ui.thread
    val entries = thread?.entries.orEmpty()
    val session = thread?.sessionId
    val listState = rememberLazyListState()
    val expanded = remember(session) { mutableStateMapOf<String, Boolean>() }
    val stepThreads = thread?.stepThreads.orEmpty()
    var stick by remember { mutableStateOf(true) }
    var anchor by remember { mutableStateOf<HistoryScrollAnchor?>(null) }
    val dragging by listState.interactionSource.collectIsDraggedAsState()
    val gate = remember { HistoryScrollGate() }
    val history by rememberUpdatedState(ui.history)
    val itemKeys by rememberUpdatedState(transcriptItemKeys(thread))

    fun visibleAnchor(requestId: Long): HistoryScrollAnchor? =
        listState.layoutInfo.visibleItemsInfo.firstOrNull { (it.key as? String)?.startsWith("entry:") == true }
            ?.let { HistoryScrollAnchor(requestId, it.key as String, it.offset) }

    val loadHistory: (String, Boolean) -> Unit = { target, retry ->
        val requestId = vm.loadOlder(target, retry)
        if (requestId != null) {
            stick = false
            anchor = visibleAnchor(requestId)
        }
    }
    val latestLoadHistory by rememberUpdatedState(loadHistory)

    val atBottom by remember {
        derivedStateOf {
            val info = listState.layoutInfo
            val last = info.visibleItemsInfo.lastOrNull()
            last == null ||
                (last.index >= info.totalItemsCount - 1 && last.offset + last.size <= info.viewportEndOffset + 48)
        }
    }
    LaunchedEffect(listState) {
        snapshotFlow {
            (dragging to listState.isScrollInProgress) to (listState.firstVisibleItemIndex to listState.firstVisibleItemScrollOffset)
        }.collect { (interaction, position) ->
            val (dragging, scrolling) = interaction
            val (index, offset) = position
            val visibleKeys = listState.layoutInfo.visibleItemsInfo.map { it.key }.toSet()
            val candidate = history.records.keys.firstOrNull {
                history.canLoad(it) && historyItemKey(it) in visibleKeys
            }
            val fetch = gate.update(dragging, index, offset, candidate != null, scrolling)
            stick = followLatestAfterScroll(
                stick, gate.userScrolling, gate.movingUp, atBottom, history.request != null || anchor != null,
            )
            if (gate.userScrolling) anchor?.let { anchor = visibleAnchor(it.requestId) ?: it }
            if (fetch && candidate != null) latestLoadHistory(candidate, false)
        }
    }
    LaunchedEffect(ui.history.request?.id, anchor) {
        val pending = anchor ?: return@LaunchedEffect
        if (ui.history.request?.id == pending.requestId) return@LaunchedEffect
        snapshotFlow { listState.isScrollInProgress }.first { !it }
        pending.indexIn(itemKeys)?.let { listState.scrollToItem(it, -pending.offset) }
        if (anchor == pending) anchor = null
    }

    val hasItems by remember { derivedStateOf { listState.layoutInfo.totalItemsCount > 0 } }
    LaunchedEffect(session, entries, stepThreads, ui.outbox.size, ui.threadKnown) {
        if (stick) scrollToEnd(listState)
    }
    LaunchedEffect(ui.outbox.size) {
        // Sending always brings the new message into view.
        if (ui.outbox.isNotEmpty()) {
            anchor = null
            stick = true
            scrollToEnd(listState)
        }
    }

    LazyColumn(
        state = listState,
        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
        modifier = Modifier.fillMaxSize(),
    ) {
        if (thread == null && ui.outbox.isEmpty()) {
            item(key = "empty") { TranscriptEmpty(ui, vm) }
        }
        if (thread != null) {
            item(key = historyItemKey(session)) {
                HistoryHeader(ui.history, session, loadHistory)
            }
        }
        items(entries, key = { transcriptEntryKey(session, it.index) }) { entry ->
            TranscriptEntry(entry, transcriptEntryKey(session, entry.index), expanded)
        }
        stepThreads.forEach { step ->
            item(key = stepItemKey(step.sessionId)) {
                Text(
                    stringResource(R.string.chat_active_step, step.title ?: stringResource(R.string.chat_untitled)),
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            item(key = historyItemKey(step.sessionId)) {
                HistoryHeader(ui.history, step.sessionId, loadHistory)
            }
            items(step.entries, key = { transcriptEntryKey(step.sessionId, it.index) }) { entry ->
                TranscriptEntry(entry, transcriptEntryKey(step.sessionId, entry.index), expanded)
            }
        }
        items(ui.outbox, key = { "o-${it.id}" }) { item -> OutboxBubble(item) }
    }

    AnimatedVisibility(
        visible = !stick && hasItems,
        enter = fadeIn(),
        exit = fadeOut(),
        modifier = Modifier.fillMaxSize(),
    ) {
        Box(Modifier.fillMaxSize().padding(12.dp), contentAlignment = Alignment.BottomEnd) {
            SmallFloatingActionButton(onClick = { anchor = null; stick = true }) {
                Icon(Icons.Filled.KeyboardArrowDown, contentDescription = stringResource(R.string.chat_jump_to_bottom))
            }
        }
    }
    LaunchedEffect(stick) { if (stick) scrollToEnd(listState) }
}

@Composable
private fun HistoryHeader(history: TranscriptHistory, session: String?, onLoad: (String, Boolean) -> Unit) {
    val record = history.records[session] ?: return
    val loading = history.request?.session == session
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        when {
            loading -> LoadingRow(stringResource(R.string.history_loading))
            record.failure != null -> {
                Text(
                    record.failure.message ?: stringResource(R.string.history_no_progress),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
                TextButton(onClick = { if (session != null) onLoad(session, true) }, enabled = history.request == null) {
                    Text(stringResource(R.string.history_retry))
                }
            }
            (record.nextBefore ?: 0) > 0 -> TextButton(
                onClick = { if (session != null) onLoad(session, false) },
                enabled = history.request == null,
            ) { Text(stringResource(R.string.history_load)) }
            else -> Text(
                stringResource(if (record.nextBefore == null && record.view.total > record.view.entries.size) {
                    R.string.history_upgrade
                } else {
                    R.string.history_start
                }),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

private suspend fun scrollToEnd(state: LazyListState) {
    val count = state.layoutInfo.totalItemsCount
    if (count > 0) state.scrollToItem(count - 1, Int.MAX_VALUE)
}

@Composable
private fun TranscriptEmpty(ui: DeviceUi, vm: MainViewModel) {
    val now = rememberNow(15_000L)
    when {
        ui.status == null || !ui.threadKnown -> {
            val text = when {
                !ui.isOnline(now) -> stringResource(R.string.chat_waiting_online)
                ui.status == null -> stringResource(R.string.chat_waiting)
                else -> stringResource(R.string.chat_loading_thread)
            }
            LoadingRow(text)
        }
        ui.threadError != null -> EmptyState(
            title = stringResource(R.string.chat_thread_error),
            body = ui.threadError,
            icon = Icons.Filled.ErrorOutline,
        )
        else -> EmptyState(
            title = stringResource(R.string.chat_no_thread),
            body = stringResource(R.string.chat_no_thread_body),
            icon = Icons.Outlined.ChatBubbleOutline,
        ) {
            OutlinedButton(onClick = { vm.switchTab(Tab.Threads) }) { Text(stringResource(R.string.tab_threads)) }
        }
    }
}

@Composable
private fun TranscriptEntry(entry: Entry, entryKey: String, expanded: MutableMap<String, Boolean>) {
    if (entry.parts.isEmpty()) {
        EntryView(entry, expanded[entryKey] == true) { expanded[entryKey] = expanded[entryKey] != true }
    } else {
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            entry.parts.forEach { part ->
                val partKey = "$entryKey:part:${part.index}"
                EntryView(
                    Entry(entry.index, part.role, part.text, entry.status),
                    expanded[partKey] == true,
                ) { expanded[partKey] = expanded[partKey] != true }
            }
        }
    }
}

@Composable
private fun EntryView(entry: Entry, expanded: Boolean, onToggle: () -> Unit) {
    when (entry.role) {
        "user" -> UserBubble(entry.text)
        "assistant" -> SelectionContainer { Markdown(entry.text, Modifier.fillMaxWidth().padding(horizontal = 4.dp)) }
        "tool" -> ToolRow(entry, expanded, onToggle)
        "reasoning" -> ThinkingRow(entry.text, expanded, onToggle)
        else -> Text(
            entry.text,
            style = MaterialTheme.typography.bodySmall,
            fontStyle = FontStyle.Italic,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp),
        )
    }
}

@Composable
private fun ThinkingRow(text: String, expanded: Boolean, onToggle: () -> Unit) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column {
            TextButton(onClick = onToggle, modifier = Modifier.fillMaxWidth()) {
                Text(stringResource(R.string.chat_thinking), modifier = Modifier.weight(1f), textAlign = TextAlign.Start)
                Icon(
                    if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = stringResource(if (expanded) R.string.action_collapse else R.string.action_expand),
                )
            }
            if (expanded) {
                SelectionContainer {
                    Markdown(text, Modifier.padding(start = 12.dp, end = 12.dp, bottom = 12.dp), style = MaterialTheme.typography.bodySmall)
                }
            }
        }
    }
}

@Composable
private fun UserBubble(text: String, modifier: Modifier = Modifier, label: String? = null) {
    Row(modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
        Spacer(Modifier.width(48.dp))
        Column(horizontalAlignment = Alignment.End) {
            Surface(
                color = MaterialTheme.colorScheme.primaryContainer,
                contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
                shape = RoundedCornerShape(18.dp, 18.dp, 4.dp, 18.dp),
            ) {
                SelectionContainer {
                    Text(text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp))
                }
            }
            if (label != null) {
                Text(
                    label,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(top = 2.dp, end = 4.dp),
                )
            }
        }
    }
}

@Composable
private fun OutboxBubble(item: OutboxItem) {
    val label = stringResource(
        when (item.state) {
            OutboxState.Sending -> R.string.outbox_sending
            OutboxState.Queued -> R.string.outbox_queued
            OutboxState.Sent -> R.string.outbox_sent
        },
    )
    UserBubble(item.text, Modifier.alpha(0.7f), label)
}

private fun firstLine(text: String): String =
    text.lineSequence().firstOrNull { it.isNotBlank() }.orEmpty().trim().replace(Regex("^#{1,6}\\s+"), "")

@Composable
private fun ToolRow(entry: Entry, expanded: Boolean, onToggle: () -> Unit) {
    val summary = firstLine(entry.text).ifEmpty { stringResource(R.string.permission_tool_call) }
    val hasMore = entry.text.trim().lines().size > 1 || summary.length > 60
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column {
            Row(
                Modifier
                    .then(if (hasMore) Modifier.clickable(onClick = onToggle) else Modifier)
                    .padding(horizontal = 10.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                ToolStatusIcon(entry.status)
                Spacer(Modifier.width(8.dp))
                Text(
                    inlineMarkdown(summary),
                    style = MaterialTheme.typography.bodySmall,
                    maxLines = if (expanded) 3 else 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (hasMore) {
                    Icon(
                        if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                        contentDescription = stringResource(if (expanded) R.string.action_collapse else R.string.action_expand),
                        modifier = Modifier.size(18.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (expanded && hasMore) {
                SelectionContainer {
                    Markdown(
                        entry.text,
                        Modifier.padding(start = 10.dp, end = 10.dp, bottom = 10.dp),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
        }
    }
}

@Composable
private fun ToolStatusIcon(status: String?) {
    val scheme = MaterialTheme.colorScheme
    val modifier = Modifier.size(16.dp)
    when (status) {
        "running" -> CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 1.5.dp)
        "completed" -> Icon(Icons.Filled.CheckCircle, stringResource(R.string.tool_completed), modifier, tint = OnlineGreen)
        "failed" -> Icon(Icons.Filled.ErrorOutline, stringResource(R.string.tool_failed), modifier, tint = scheme.error)
        "waiting" -> Icon(Icons.Filled.HourglassTop, stringResource(R.string.tool_waiting), modifier, tint = scheme.tertiary)
        "rejected" -> Icon(Icons.Filled.Block, stringResource(R.string.tool_rejected), modifier, tint = scheme.error)
        "canceled" -> Icon(Icons.Filled.RemoveCircleOutline, stringResource(R.string.tool_canceled), modifier, tint = scheme.outline)
        "pending" -> Icon(Icons.Filled.Schedule, stringResource(R.string.tool_pending), modifier, tint = scheme.outline)
        else -> Icon(Icons.Outlined.Build, stringResource(R.string.permission_tool_call), modifier, tint = scheme.outline)
    }
}

@Composable
private fun Composer(ui: DeviceUi, vm: MainViewModel, generating: Boolean) {
    Surface(tonalElevation = 2.dp, modifier = Modifier.fillMaxWidth()) {
        Row(
            Modifier.padding(horizontal = 8.dp, vertical = 8.dp),
            verticalAlignment = Alignment.Bottom,
        ) {
            OutlinedTextField(
                value = vm.composer,
                onValueChange = { vm.composer = it },
                placeholder = { Text(stringResource(R.string.composer_placeholder)) },
                enabled = ui.device != null,
                maxLines = 6,
                shape = RoundedCornerShape(24.dp),
                modifier = Modifier.weight(1f),
            )
            if (generating) {
                Spacer(Modifier.width(6.dp))
                FilledTonalIconButton(
                    onClick = vm::stopGenerating,
                    enabled = !ui.stopping,
                    colors = IconButtonDefaults.filledTonalIconButtonColors(
                        containerColor = MaterialTheme.colorScheme.errorContainer,
                        contentColor = MaterialTheme.colorScheme.onErrorContainer,
                    ),
                    modifier = Modifier.size(52.dp),
                ) {
                    Icon(Icons.Filled.Stop, contentDescription = stringResource(R.string.action_stop))
                }
            }
            Spacer(Modifier.width(6.dp))
            FilledIconButton(
                onClick = vm::sendPrompt,
                enabled = ui.device != null && vm.composer.isNotBlank(),
                modifier = Modifier.size(52.dp),
            ) {
                Icon(Icons.AutoMirrored.Filled.Send, contentDescription = stringResource(R.string.action_send))
            }
        }
    }
}
