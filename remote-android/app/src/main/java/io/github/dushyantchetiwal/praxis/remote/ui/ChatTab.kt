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
import androidx.compose.foundation.selection.selectable
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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.RadioButton
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
import androidx.compose.material3.PlainTooltip
import androidx.compose.material3.TooltipBox
import androidx.compose.material3.TooltipDefaults
import androidx.compose.material3.rememberTooltipState
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
import androidx.compose.ui.semantics.Role
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
import io.github.dushyantchetiwal.praxis.remote.StopTarget
import io.github.dushyantchetiwal.praxis.remote.data.Architect
import io.github.dushyantchetiwal.praxis.remote.data.Entry
import io.github.dushyantchetiwal.praxis.remote.data.ApiException
import io.github.dushyantchetiwal.praxis.remote.data.DetailRequest
import io.github.dushyantchetiwal.praxis.remote.data.DetailBody
import io.github.dushyantchetiwal.praxis.remote.data.ErrorKind
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
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
    val generating = ui.isGenerating()

    Column(Modifier.fillMaxSize()) {
        val permissions = ui.visiblePermissions()
        if (permissions.isNotEmpty() || (summary?.questionCount ?: 0) > 0) {
            Column(
                Modifier
                    .heightIn(max = 340.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(horizontal = 12.dp, vertical = 4.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                QuestionCards(ui, vm)
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
    if (ui.queue.visible) QueueDialog(ui, vm)
    QuestionDialogs(ui, vm)
}

@Composable
internal fun ConversationControls(ui: DeviceUi, vm: MainViewModel, onOpenQueue: () -> Unit) {
    val window = ui.currentWindow()
    val summary = window?.thread
    val now = rememberNow(5_000L)
    ThreadHeader(ui, ui.isGenerating(), onOpenQueue)
    if (ui.isGenerating() && window?.architect?.running == true) {
        TextButton(onClick = vm::stopGenerating, enabled = !ui.stopping, modifier = Modifier.padding(horizontal = 8.dp)) {
            Text(stringResource(R.string.action_stop_agent))
        }
    }
    if (summary?.modelSelection == true) ModelPicker(ui, vm)
    summary?.mode?.takeIf { it.available.isNotEmpty() }?.let { mode ->
        Text(stringResource(R.string.agent_mode), style = MaterialTheme.typography.labelLarge, modifier = Modifier.padding(horizontal = 12.dp))
        ModeSelector(mode, ui.currentModeId(now), vm::setMode)
    }
    window?.architect?.takeIf { it.steps > 0 }?.let { ArchitectCard(it, vm) }
}

@Composable
private fun ModelPicker(ui: DeviceUi, vm: MainViewModel) {
    val session = ui.currentWindow()?.thread?.sessionId
    var open by remember(session) { mutableStateOf(false) }
    var query by remember(session) { mutableStateOf("") }
    val models = ui.models.takeIf { it.session == session }
    val info = models?.info
    val current = info?.available?.find { it.id == info?.current }?.name
        ?: ui.currentWindow()?.thread?.modelName
        ?: ui.currentWindow()?.thread?.model
    TextButton(onClick = { open = true }, modifier = Modifier.padding(horizontal = 8.dp)) {
        Text(stringResource(R.string.model_current, current ?: stringResource(R.string.model_select)), maxLines = 1, overflow = TextOverflow.Ellipsis)
        Icon(Icons.Filled.ExpandMore, contentDescription = null)
    }
    if (open) {
        LaunchedEffect(session) { vm.loadModels() }
        AlertDialog(
            onDismissRequest = { open = false },
            title = { Text(stringResource(R.string.model_select)) },
            confirmButton = { TextButton(onClick = { open = false }) { Text(stringResource(R.string.action_close)) } },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = query, onValueChange = { query = it }, singleLine = true,
                        label = { Text(stringResource(R.string.model_search)) },
                    )
                    if (models?.loading == true || models?.changing == true) {
                        LoadingRow(stringResource(if (models?.changing == true) R.string.model_changing else R.string.model_loading))
                    }
                    models?.error?.let { error ->
                        Text(error, color = MaterialTheme.colorScheme.error)
                        TextButton(onClick = { vm.loadModels() }) { Text(stringResource(R.string.action_retry)) }
                    }
                    if (info?.nextOffset != null) {
                        TextButton(onClick = { vm.loadModels(more = true) }, enabled = models?.loading != true && models?.changing != true) {
                            Text(stringResource(R.string.model_load_more))
                        }
                    }
                    LazyColumn(Modifier.heightIn(max = 360.dp)) {
                        items(info?.available.orEmpty().filter {
                            it.name.contains(query, ignoreCase = true) || it.group.orEmpty().contains(query, ignoreCase = true)
                        }, key = { it.id }) { model ->
                            Row(
                                Modifier.fillMaxWidth().selectable(
                                    selected = info?.current == model.id,
                                    enabled = !model.disabled && models?.changing != true && models?.loading != true,
                                    role = Role.RadioButton,
                                    onClick = { vm.setModel(model) },
                                ).padding(vertical = 4.dp),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                RadioButton(selected = info?.current == model.id, onClick = null, enabled = !model.disabled)
                                Spacer(Modifier.width(8.dp))
                                Column {
                                    Text(model.name, color = if (model.disabled) MaterialTheme.colorScheme.outline else MaterialTheme.colorScheme.onSurface)
                                    model.group?.let { Text(it, style = MaterialTheme.typography.labelSmall) }
                                }
                            }
                        }
                    }
                }
            },
        )
    }
}

@Composable
internal fun chatTitle(ui: DeviceUi): String = when {
    ui.device == null -> stringResource(R.string.chat_no_device)
    ui.status == null -> stringResource(R.string.chat_connecting)
    ui.currentWindow() == null -> stringResource(R.string.chat_no_window)
    else -> ui.conversationTitle()
        ?: stringResource(if (ui.currentWindow()?.thread != null) R.string.chat_untitled else R.string.chat_no_thread)
}

@Composable
private fun ThreadHeader(ui: DeviceUi, generating: Boolean, onOpenQueue: () -> Unit) {
    val summary = ui.currentWindow()?.thread
    val title = chatTitle(ui)
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
            if (summary?.queueManagement == true && summary.queued > 0) {
                TextButton(onClick = onOpenQueue) {
                    Text(pluralStringResource(R.plurals.status_queued, summary.queued, summary.queued))
                }
            }
            StatusChip(generating, if (summary?.queueManagement == true) 0 else summary?.queued ?: 0)
        }
    }
}

@Composable
private fun QueueDialog(ui: DeviceUi, vm: MainViewModel) {
    val queue = ui.queue
    val session = queue.session ?: return
    val expanded = remember(session) { mutableStateMapOf<String, Boolean>() }
    AlertDialog(
        onDismissRequest = vm::dismissQueue,
        title = { Text(stringResource(R.string.queue_title)) },
        confirmButton = { TextButton(onClick = vm::dismissQueue) { Text(stringResource(R.string.action_close)) } },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(stringResource(R.string.queue_help), style = MaterialTheme.typography.bodySmall)
                TextButton(onClick = { vm.loadQueue() }, enabled = !queue.loading) { Text(stringResource(R.string.action_refresh)) }
                if (queue.loading) LoadingRow(stringResource(R.string.queue_loading))
                queue.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                if (!queue.loading && queue.entries.isEmpty() && queue.error == null) Text(stringResource(R.string.queue_empty))
                LazyColumn(Modifier.heightIn(max = 380.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    items(queue.entries, key = { it.id }) { message ->
                        Column {
                            TextButton(onClick = { expanded[message.id] = expanded[message.id] != true }) {
                                Text(message.text, maxLines = 2, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                Icon(if (expanded[message.id] == true) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                                    contentDescription = stringResource(if (expanded[message.id] == true) R.string.action_collapse else R.string.action_expand))
                            }
                            Row {
                                if (ui.currentWindow()?.thread?.steering == true) TextButton(
                                    onClick = { vm.queueAction(message.id, session, !message.steer) },
                                    enabled = !queue.loading && message.id !in queue.busy,
                                ) { Text(stringResource(if (message.steer) R.string.action_wait_turn else R.string.action_steer)) }
                                TextButton(onClick = { vm.queueAction(message.id, session) }, enabled = !queue.loading && message.id !in queue.busy) {
                                    Text(stringResource(R.string.action_send_now))
                                }
                            }
                            if (expanded[message.id] == true) EntryDetails(message.text, true, DetailRequest(session, queueId = message.id), vm)
                        }
                    }
                    if (queue.nextOffset != null) item {
                        TextButton(onClick = { vm.loadQueue(more = true) }, enabled = !queue.loading) { Text(stringResource(R.string.queue_more)) }
                    }
                }
            }
        },
    )
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
                    pluralStringResource(R.plurals.architect_step, architect.steps, step, architect.steps) +
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
            TranscriptEntry(entry, session, expanded, vm)
        }
        stepThreads.forEach { step ->
            item(key = stepItemKey(step.sessionId)) {
                Text(
                    stringResource(
                        if (ui.history.records[step.sessionId]?.live == true) R.string.chat_active_step else R.string.chat_step_history,
                        step.title ?: stringResource(R.string.chat_untitled),
                    ),
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            item(key = historyItemKey(step.sessionId)) {
                HistoryHeader(ui.history, step.sessionId, loadHistory)
            }
            items(step.entries, key = { transcriptEntryKey(step.sessionId, it.index) }) { entry ->
                TranscriptEntry(entry, step.sessionId, expanded, vm)
            }
        }
        items(ui.outbox.filter { it.session == null || it.session == session }, key = { "o-${it.id}" }) { item ->
            OutboxBubble(item, ui.currentWindow()?.thread?.sendNow == true && item.session == ui.currentWindow()?.thread?.sessionId, ui.currentWindow()?.thread?.steering == true, vm)
        }
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
private fun TranscriptEntry(entry: Entry, session: String?, expanded: MutableMap<String, Boolean>, vm: MainViewModel) {
    val entryKey = transcriptEntryKey(session, entry.index)
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (entry.parts.isEmpty()) {
            EntryView(entry, expanded[entryKey] == true, { expanded[entryKey] = expanded[entryKey] != true }) {
                EntryDetails(entry.text, entry.detailsPending, session?.let { DetailRequest(it, entry.index) }, vm)
            }
        } else {
            entry.parts.forEach { part ->
                val partKey = "$entryKey:part:${part.index}"
                EntryView(
                    Entry(entry.index, part.role, part.text, entry.status, detailsPending = part.detailsPending),
                    expanded[partKey] == true,
                    { expanded[partKey] = expanded[partKey] != true },
                ) {
                    EntryDetails(part.text, part.detailsPending, session?.let { DetailRequest(it, entry.index, part.index) }, vm)
                }
            }
        }
        if (entry.truncated) {
            Text(stringResource(R.string.chat_truncated), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun EntryView(entry: Entry, expanded: Boolean, onToggle: () -> Unit, details: @Composable () -> Unit) {
    when (entry.role) {
        "user", "assistant" -> MessageRow(entry, expanded, onToggle, details)
        "tool" -> ToolRow(entry, expanded, onToggle, details)
        "reasoning" -> ThinkingRow(expanded, onToggle, details)
        else -> if (entry.detailsPending) MessageRow(entry, expanded, onToggle, details) else Text(
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
private fun MessageRow(entry: Entry, expanded: Boolean, onToggle: () -> Unit, details: @Composable () -> Unit) {
    val preview = remember(entry.text) { firstLine(entry.text).take(160) }
    Surface(color = MaterialTheme.colorScheme.surfaceContainerLow, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column {
            TextButton(onClick = onToggle, modifier = Modifier.fillMaxWidth()) {
                Column(Modifier.weight(1f), horizontalAlignment = Alignment.Start) {
                    Text(stringResource(when (entry.role) {
                        "user" -> R.string.chat_user_header
                        "assistant" -> R.string.chat_agent_header
                        else -> R.string.chat_notice_header
                    }))
                    if (preview.isNotBlank()) Text(preview, maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodySmall)
                }
                Icon(
                    if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = stringResource(if (expanded) R.string.action_collapse else R.string.action_expand),
                )
            }
            if (expanded) Box(Modifier.padding(12.dp)) { details() }
        }
    }
}

@Composable
private fun ThinkingRow(expanded: Boolean, onToggle: () -> Unit, details: @Composable () -> Unit) {
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
                Box(Modifier.padding(start = 12.dp, end = 12.dp, bottom = 12.dp)) { details() }
            }
        }
    }
}

@Composable
private fun EntryDetails(text: String, pending: Boolean, request: DetailRequest?, vm: MainViewModel) {
    if (!pending || request == null) {
        SelectionContainer { Markdown(text, style = MaterialTheme.typography.bodySmall) }
        return
    }
    var body by remember(request) { mutableStateOf(DetailBody()) }
    var blocks by remember(request) { mutableStateOf<List<MdBlock>>(emptyList()) }
    var error by remember(request) { mutableStateOf<String?>(null) }
    var attempt by remember(request) { mutableStateOf(0) }
    var loading by remember(request) { mutableStateOf(true) }
    val failure = stringResource(R.string.details_failed)
    LaunchedEffect(request, attempt) {
        loading = true
        error = null
        try {
            val chunk = vm.loadDetail(request, body)
            val next = body.append(chunk) ?: throw ApiException(ErrorKind.State, failure)
            val parsed = withContext(Dispatchers.Default) { parseMarkdown(next.chunks.joinToString("") { it.text }) }
            body = next
            blocks = parsed
        } catch (exception: ApiException) {
            error = exception.message ?: failure
        } finally {
            loading = false
        }
    }
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        if (loading) LoadingRow(stringResource(R.string.details_loading))
        error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        LazyColumn(Modifier.fillMaxWidth().heightIn(max = 420.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            items(blocks) { block -> SelectionContainer { MarkdownBlock(block, style = MaterialTheme.typography.bodySmall) } }
        }
        body.totalBytes?.let {
            Text(stringResource(R.string.details_progress, body.nextOffset, it), style = MaterialTheme.typography.labelSmall)
            Text(stringResource(R.string.details_snapshot), style = MaterialTheme.typography.labelSmall)
        }
        if (body.complete && blocks.isEmpty()) Text(stringResource(R.string.details_empty))
        Row {
            if (!body.complete) TextButton(onClick = { loading = true; attempt++ }, enabled = !loading) {
                Text(stringResource(if (error != null) R.string.action_retry else R.string.details_more))
            }
            TextButton(onClick = { body = DetailBody(); blocks = emptyList(); loading = true; attempt++ }, enabled = !loading) {
                Text(stringResource(R.string.details_refresh))
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
private fun OutboxBubble(item: OutboxItem, canSendNow: Boolean, canSteer: Boolean, vm: MainViewModel) {
    val label = stringResource(
        when (item.state) {
            OutboxState.Sending -> R.string.outbox_sending
            OutboxState.Queued -> if (item.steer) R.string.outbox_steering else R.string.outbox_queued
            OutboxState.Sent -> R.string.outbox_sent
            OutboxState.Unconfirmed -> R.string.outbox_unconfirmed
        },
    )
    Column(horizontalAlignment = Alignment.End, modifier = Modifier.fillMaxWidth()) {
        UserBubble(item.text, Modifier.alpha(0.7f), label)
        if (item.state == OutboxState.Unconfirmed) {
            TextButton(onClick = { vm.restorePromptDraft(item) }, enabled = vm.canRestorePromptDraft(item)) {
                Text(stringResource(R.string.action_restore_draft))
            }
        }
        if (canSendNow && item.state == OutboxState.Queued && item.queueId != null) {
            Row {
                if (canSteer) TextButton(onClick = { vm.steerQueuedMessage(item) }, enabled = !item.sendingNow) {
                    Text(stringResource(if (item.steer) R.string.action_wait_turn else R.string.action_steer))
                }
                TextButton(onClick = { vm.sendQueuedNow(item) }, enabled = !item.sendingNow) {
                    Text(stringResource(if (item.sendingNow) R.string.outbox_sending else R.string.action_send_now))
                }
            }
        }
    }
}

private fun firstLine(text: String): String =
    text.lineSequence().firstOrNull { it.isNotBlank() }.orEmpty().trim().replace(Regex("^#{1,6}\\s+"), "")

@Composable
private fun ToolRow(entry: Entry, expanded: Boolean, onToggle: () -> Unit, details: @Composable () -> Unit) {
    val first = remember(entry.text) { firstLine(entry.text) }
    val summary = first.ifEmpty { stringResource(R.string.permission_tool_call) }
    val hasMore = entry.detailsPending || '\n' in entry.text.trim() || summary.length > 60
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
                Box(Modifier.padding(start = 10.dp, end = 10.dp, bottom = 10.dp)) { details() }
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
        Column {
            if (generating && ui.currentWindow()?.thread?.sendNow == true && vm.composer.isNotBlank()) {
                Row(Modifier.align(Alignment.End)) {
                    if (ui.currentWindow()?.thread?.steering == true) TextButton(onClick = { vm.sendPrompt(steer = true) }) {
                        Text(stringResource(R.string.action_steer))
                    }
                    TextButton(onClick = { vm.sendPrompt(sendNow = true) }) {
                        Text(stringResource(R.string.action_send_now))
                    }
                }
            }
            ComposerInput(ui, vm, generating)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ComposerInput(ui: DeviceUi, vm: MainViewModel, generating: Boolean) {
    val stopTarget = ui.stopTarget()
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
        if (stopTarget != null) {
            Spacer(Modifier.width(6.dp))
            val stopLabel = stringResource(if (stopTarget == StopTarget.Plan) R.string.action_stop_plan else R.string.action_stop)
            TooltipBox(
                positionProvider = TooltipDefaults.rememberPlainTooltipPositionProvider(),
                tooltip = { PlainTooltip { Text(stopLabel) } },
                state = rememberTooltipState(),
            ) {
                FilledTonalIconButton(
                    onClick = { if (stopTarget == StopTarget.Plan) vm.architectAction(run = false) else vm.stopGenerating() },
                    enabled = stopTarget == StopTarget.Plan || !ui.stopping,
                    colors = IconButtonDefaults.filledTonalIconButtonColors(
                        containerColor = MaterialTheme.colorScheme.errorContainer,
                        contentColor = MaterialTheme.colorScheme.onErrorContainer,
                    ),
                    modifier = Modifier.size(52.dp),
                ) {
                    Icon(Icons.Filled.Stop, contentDescription = stopLabel)
                }
            }
        }
        Spacer(Modifier.width(6.dp))
        FilledIconButton(
            onClick = { vm.sendPrompt() },
            enabled = ui.device != null && vm.composer.isNotBlank(),
            modifier = Modifier.size(52.dp),
        ) {
            Icon(Icons.AutoMirrored.Filled.Send, contentDescription = stringResource(if (generating) R.string.action_queue else R.string.action_send))
        }
    }
}
