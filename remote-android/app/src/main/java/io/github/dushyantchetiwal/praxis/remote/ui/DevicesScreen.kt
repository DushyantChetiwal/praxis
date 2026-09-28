package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.AppState
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.data.DEVICE_TITLE_PREFIX
import io.github.dushyantchetiwal.praxis.remote.data.Device

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DevicesScreen(state: AppState, ui: DeviceUi, vm: MainViewModel, snackbar: SnackbarHostState) {
    val devices = state.devices
    val now = rememberNow(15_000L)
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.devices_title))
                        state.repo?.let {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                },
                actions = {
                    IconButton(onClick = vm::openSettings) {
                        Icon(Icons.Filled.Settings, contentDescription = stringResource(R.string.settings_title))
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        PullToRefreshBox(
            isRefreshing = devices.loading && devices.loaded,
            onRefresh = { vm.loadDevices() },
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = PaddingValues(16.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                state.update?.let { update ->
                    item(key = "update") { UpdateBanner(update, onDismiss = vm::dismissUpdate) }
                }
                devices.error?.let { error -> item(key = "error") { ErrorText(error) } }
                when {
                    !devices.loaded -> item(key = "loading") { LoadingRow(stringResource(R.string.devices_loading)) }
                    devices.devices.isEmpty() -> item(key = "empty") {
                        EmptyState(
                            title = stringResource(R.string.devices_empty_title),
                            body = stringResource(
                                R.string.devices_empty_body,
                                "$DEVICE_TITLE_PREFIX…",
                                state.repo ?: "",
                            ),
                            icon = Icons.Outlined.Computer,
                        ) {
                            OutlinedButton(onClick = { vm.loadDevices() }) { Text(stringResource(R.string.action_refresh)) }
                        }
                    }
                    else -> items(devices.devices, key = { it.number }) { device ->
                        DeviceCard(device, ui.isOnline(device, now), now) { vm.openDevice(device) }
                    }
                }
            }
        }
    }
}

@Composable
private fun DeviceCard(device: Device, online: Boolean, now: Long, onClick: () -> Unit) {
    val context = LocalContext.current
    val presence = when {
        online -> stringResource(R.string.presence_online)
        device.lastSeen != null -> stringResource(R.string.presence_last_seen, relativeTime(context, device.lastSeen, now))
        else -> stringResource(R.string.presence_never)
    }
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
        modifier = Modifier.fillMaxWidth(),
    ) {
        ListItem(
            leadingContent = { OnlineDot(online) },
            headlineContent = { Text(device.name, style = MaterialTheme.typography.titleMedium) },
            supportingContent = { Text(stringResource(R.string.devices_subtitle, presence, device.number)) },
            trailingContent = { Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null) },
            colors = ListItemDefaults.colors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
            modifier = Modifier.clickable(onClick = onClick),
        )
    }
}
