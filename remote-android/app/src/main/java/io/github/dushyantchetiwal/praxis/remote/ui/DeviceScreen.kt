package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.outlined.Chat
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material.icons.outlined.Folder
import androidx.compose.material.icons.outlined.History
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material.icons.outlined.Web
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.AppState
import io.github.dushyantchetiwal.praxis.remote.Banner
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.Tab
import io.github.dushyantchetiwal.praxis.remote.data.WindowInfo

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DeviceScreen(state: AppState, ui: DeviceUi, busy: Int, vm: MainViewModel, snackbar: SnackbarHostState) {
    var menu by remember { mutableStateOf(false) }
    var confirmUnpair by remember { mutableStateOf(false) }
    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    IconButton(onClick = vm::leaveDevice) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.devices_title))
                    }
                },
                title = { DeviceTitle(ui) },
                actions = {
                    Box(Modifier.size(48.dp), contentAlignment = Alignment.Center) {
                        if (busy > 0) {
                            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                        } else {
                            IconButton(onClick = vm::refresh) {
                                Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.action_refresh))
                            }
                        }
                    }
                    Box {
                        IconButton(onClick = { menu = true }) {
                            Icon(Icons.Filled.MoreVert, contentDescription = stringResource(R.string.action_more))
                        }
                        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.action_refresh)) },
                                leadingIcon = { Icon(Icons.Filled.Refresh, contentDescription = null) },
                                onClick = { menu = false; vm.refresh() },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.action_new_thread)) },
                                leadingIcon = { Icon(Icons.Outlined.Add, contentDescription = null) },
                                enabled = ui.currentWindow() != null && !ui.startingThread,
                                onClick = { menu = false; vm.startNewThread() },
                            )
                            if (ui.status?.openFolder == true) {
                                DropdownMenuItem(
                                    text = { Text(stringResource(R.string.folder_open)) },
                                    leadingIcon = { Icon(Icons.Outlined.Folder, contentDescription = null) },
                                    onClick = { menu = false; vm.browseHostFolder() },
                                )
                            }
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.action_switch_device)) },
                                leadingIcon = { Icon(Icons.Outlined.Computer, contentDescription = null) },
                                onClick = { menu = false; vm.leaveDevice() },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.action_unpair)) },
                                leadingIcon = { Icon(Icons.Outlined.LinkOff, contentDescription = null) },
                                onClick = { menu = false; confirmUnpair = true },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.settings_title)) },
                                leadingIcon = { Icon(Icons.Outlined.Settings, contentDescription = null) },
                                onClick = { menu = false; vm.openSettings() },
                            )
                        }
                    }
                },
            )
        },
        bottomBar = {
            NavigationBar {
                TabItem(ui.tab == Tab.Chat, Icons.AutoMirrored.Outlined.Chat, R.string.tab_chat) { vm.switchTab(Tab.Chat) }
                TabItem(ui.tab == Tab.Threads, Icons.Outlined.History, R.string.tab_threads) { vm.switchTab(Tab.Threads) }
                TabItem(ui.tab == Tab.Files, Icons.Outlined.Folder, R.string.tab_files) { vm.switchTab(Tab.Files) }
            }
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .consumeWindowInsets(padding)
                .imePadding(),
        ) {
            DeviceHeader(state, ui, vm)
            Box(Modifier.weight(1f)) {
                when (ui.tab) {
                    Tab.Chat -> ChatTab(ui, vm)
                    Tab.Threads -> ThreadsTab(ui, vm)
                    Tab.Files -> FilesTab(ui, vm)
                }
            }
        }
    }

    if (ui.folderBrowser.visible) HostFolderDialog(ui, vm)
    if (confirmUnpair) {
        UnpairDialog(ui.device?.name.orEmpty(), onDismiss = { confirmUnpair = false }) {
            confirmUnpair = false
            vm.unpairCurrent()
        }
    }
}

@Composable
private fun HostFolderDialog(ui: DeviceUi, vm: MainViewModel) {
    val browser = ui.folderBrowser
    val listing = browser.listing
    var path by remember(ui.device?.channel) { mutableStateOf("") }
    LaunchedEffect(listing?.path) { listing?.path?.let { path = it } }
    AlertDialog(
        onDismissRequest = vm::dismissFolderBrowser,
        title = { Text(stringResource(R.string.folder_open)) },
        confirmButton = {
            TextButton(onClick = { vm.openHostFolder(path) }, enabled = path.isNotBlank() && !browser.loading && !browser.opening) {
                Text(stringResource(R.string.folder_open_new_window))
            }
        },
        dismissButton = { TextButton(onClick = vm::dismissFolderBrowser) { Text(stringResource(if (browser.opening) R.string.action_close else R.string.action_cancel)) } },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(stringResource(R.string.folder_host_hint, ui.device?.name.orEmpty(), listing?.host.orEmpty()))
                OutlinedTextField(value = path, onValueChange = { path = it }, label = { Text(stringResource(R.string.folder_path)) }, singleLine = true)
                Row {
                    TextButton(onClick = { vm.browseHostFolder(path) }, enabled = !browser.loading && !browser.opening) { Text(stringResource(R.string.folder_browse)) }
                    listing?.parent?.let { parent ->
                        TextButton(onClick = { vm.browseHostFolder(parent) }, enabled = !browser.loading && !browser.opening) { Text(stringResource(R.string.folder_parent)) }
                    }
                }
                browser.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                if (browser.loading || browser.opening) LoadingRow(stringResource(R.string.files_loading))
                LazyColumn(Modifier.heightIn(max = 320.dp)) {
                    items(listing?.folders.orEmpty(), key = { it.path }) { folder ->
                        TextButton(onClick = { vm.browseHostFolder(folder.path) }, enabled = !browser.loading && !browser.opening) {
                            Icon(Icons.Outlined.Folder, contentDescription = null)
                            Spacer(Modifier.width(8.dp))
                            Text(folder.name)
                        }
                    }
                    if (listing?.nextOffset != null) item {
                        TextButton(onClick = { vm.browseHostFolder(listing.path, more = true) }, enabled = !browser.loading) { Text(stringResource(R.string.folder_more)) }
                    }
                }
            }
        },
    )
}

@Composable
fun UnpairDialog(deviceName: String, onDismiss: () -> Unit, onConfirm: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        icon = { Icon(Icons.Outlined.LinkOff, contentDescription = null) },
        title = { Text(stringResource(R.string.unpair_title)) },
        text = { Text(stringResource(R.string.unpair_body, deviceName)) },
        confirmButton = { TextButton(onClick = onConfirm) { Text(stringResource(R.string.unpair_confirm)) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.action_cancel)) } },
    )
}

@Composable
private fun androidx.compose.foundation.layout.RowScope.TabItem(
    selected: Boolean,
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    label: Int,
    onClick: () -> Unit,
) {
    NavigationBarItem(
        selected = selected,
        onClick = onClick,
        icon = { Icon(icon, contentDescription = null) },
        label = { Text(stringResource(label)) },
    )
}

@Composable
private fun DeviceTitle(ui: DeviceUi) {
    val context = LocalContext.current
    val now = rememberNow()
    val device = ui.device
    val online = ui.isOnline(now)
    Column {
        Text(device?.name ?: "", maxLines = 1, overflow = TextOverflow.Ellipsis)
        if (device != null) {
            val presence = when {
                online -> stringResource(R.string.presence_online)
                device.lastSeen != null -> stringResource(R.string.presence_last_seen, relativeTime(context, device.lastSeen, now))
                else -> stringResource(R.string.presence_never)
            }
            val updated = ui.snapshot?.updatedAt?.let { stringResource(R.string.presence_updated, ageText(context, it, now)) }
            Row(verticalAlignment = Alignment.CenterVertically) {
                OnlineDot(online, Modifier.size(8.dp))
                Spacer(Modifier.width(6.dp))
                Text(
                    listOfNotNull(presence, updated).joinToString(" · "),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

/** The window picker, update banner and status banners under the top bar. */
@Composable
private fun DeviceHeader(state: AppState, ui: DeviceUi, vm: MainViewModel) {
    val context = LocalContext.current
    val now = rememberNow(15_000L)
    val windows = ui.status?.windows.orEmpty()
    val device = ui.device
    val banners = buildList {
        if (device != null && !ui.isOnline(now)) {
            val seen = device.lastSeen?.let { stringResource(R.string.banner_offline_seen, relativeTime(context, it, now)) }
                ?: stringResource(R.string.banner_offline_never)
            add(Banner(stringResource(R.string.banner_offline, device.name, seen), error = false))
        } else if (device != null && ui.stateApplied && !ui.snapshotFresh(now)) {
            add(Banner(stringResource(R.string.banner_waiting), error = false))
        }
        addAll(ui.banners.values)
    }
    if (windows.size <= 1 && banners.isEmpty() && state.update == null) return
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 6.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        if (windows.size > 1) WindowPicker(windows, ui.currentWindow(), vm::selectWindow)
        state.update?.let { UpdateBanner(it, onDismiss = vm::dismissUpdate) }
        banners.forEach { BannerRow(it) }
    }
}

@Composable
private fun WindowPicker(windows: List<WindowInfo>, current: WindowInfo?, onSelect: (Long) -> Unit) {
    var open by remember { mutableStateOf(false) }
    val untitled = stringResource(R.string.window_untitled, current?.id ?: 0L)
    Box {
        AssistChip(
            onClick = { open = true },
            label = {
                Text(
                    current?.let { it.projectsLabel ?: untitled } ?: stringResource(R.string.window_choose),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            },
            leadingIcon = { Icon(Icons.Outlined.Web, contentDescription = null, modifier = Modifier.size(18.dp)) },
            trailingIcon = { Icon(Icons.Filled.ArrowDropDown, contentDescription = null) },
        )
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            windows.forEach { window ->
                DropdownMenuItem(
                    text = { Text(window.projectsLabel ?: stringResource(R.string.window_untitled, window.id)) },
                    leadingIcon = {
                        if (window.id == current?.id) Icon(Icons.Filled.Check, contentDescription = null)
                    },
                    onClick = {
                        open = false
                        onSelect(window.id)
                    },
                )
            }
        }
    }
}
