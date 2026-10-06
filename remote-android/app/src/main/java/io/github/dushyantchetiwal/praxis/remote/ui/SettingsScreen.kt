package io.github.dushyantchetiwal.praxis.remote.ui

import android.text.format.DateFormat
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Logout
import androidx.compose.material.icons.filled.Person
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.PhoneAndroid
import androidx.compose.material.icons.outlined.SystemUpdate
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
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
import androidx.compose.material3.TextButton
import androidx.compose.material3.Switch
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.AppState
import io.github.dushyantchetiwal.praxis.remote.BuildConfig
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import java.util.Date

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(state: AppState, ui: DeviceUi, vm: MainViewModel, snackbar: SnackbarHostState) {
    val context = LocalContext.current
    var confirmSignOut by remember { mutableStateOf(false) }
    var confirmUnpair by remember { mutableStateOf(false) }

    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    IconButton(onClick = { vm.back() }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                    }
                },
                title = { Text(stringResource(R.string.settings_title)) },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Section(stringResource(R.string.settings_account)) {
                ListItem(
                    leadingContent = {
                        val avatar = state.avatar
                        if (avatar != null) {
                            Image(
                                avatar,
                                contentDescription = null,
                                contentScale = ContentScale.Crop,
                                modifier = Modifier.size(40.dp).clip(CircleShape),
                            )
                        } else {
                            Icon(Icons.Filled.Person, contentDescription = null, modifier = Modifier.size(40.dp))
                        }
                    },
                    headlineContent = { Text(state.login ?: stringResource(R.string.settings_unknown_user)) },
                    supportingContent = { Text(stringResource(R.string.settings_signed_in_with_github)) },
                    colors = transparentItem(),
                )
                state.tokenExpiresAt?.let { expiresAt ->
                    ListItem(
                        leadingContent = { Icon(Icons.Outlined.Info, contentDescription = null) },
                        headlineContent = {
                            Text(
                                stringResource(
                                    R.string.settings_token_expires,
                                    DateFormat.getMediumDateFormat(context).format(Date(expiresAt)) + " " +
                                        DateFormat.getTimeFormat(context).format(Date(expiresAt)),
                                ),
                            )
                        },
                        supportingContent = { Text(stringResource(R.string.settings_token_expires_help)) },
                        colors = transparentItem(),
                    )
                }
                Box(Modifier.padding(horizontal = 16.dp, vertical = 8.dp)) {
                    OutlinedButton(onClick = { confirmSignOut = true }, modifier = Modifier.fillMaxWidth()) {
                        Icon(Icons.AutoMirrored.Filled.Logout, contentDescription = null, modifier = Modifier.size(18.dp))
                        Spacer(Modifier.width(8.dp))
                        Text(stringResource(R.string.settings_sign_out))
                    }
                }
            }

            Section(stringResource(R.string.settings_connection)) {
                val relayLabel = stringResource(R.string.settings_live_relay)
                ListItem(
                    headlineContent = { Text(relayLabel) },
                    supportingContent = { Text(stringResource(R.string.settings_live_relay_help)) },
                    trailingContent = {
                        Switch(checked = vm.liveRelayEnabled, onCheckedChange = vm::changeLiveRelayEnabled,
                            modifier = Modifier.semantics { contentDescription = relayLabel })
                    },
                    colors = transparentItem(),
                )
            }

            Section(stringResource(R.string.settings_pairing)) {
                ListItem(
                    leadingContent = { Icon(Icons.Outlined.PhoneAndroid, contentDescription = null) },
                    headlineContent = { Text(state.phoneName) },
                    supportingContent = { Text(stringResource(R.string.settings_phone_help)) },
                    colors = transparentItem(),
                )
                ui.device?.let { device ->
                    ListItem(
                        leadingContent = { Icon(Icons.Outlined.Computer, contentDescription = null) },
                        headlineContent = { Text(device.name) },
                        supportingContent = { Text(stringResource(R.string.settings_selected_computer)) },
                        colors = transparentItem(),
                    )
                    Box(Modifier.padding(horizontal = 16.dp, vertical = 8.dp)) {
                        OutlinedButton(onClick = { confirmUnpair = true }, modifier = Modifier.fillMaxWidth()) {
                            Icon(Icons.Outlined.LinkOff, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(8.dp))
                            Text(stringResource(R.string.action_unpair))
                        }
                    }
                }
            }

            Section(stringResource(R.string.settings_app)) {
                ListItem(
                    leadingContent = { AppLogo(40.dp) },
                    headlineContent = { Text(stringResource(R.string.app_name)) },
                    supportingContent = {
                        Text(stringResource(R.string.settings_version, BuildConfig.VERSION_NAME, BuildConfig.VERSION_CODE))
                    },
                    colors = transparentItem(),
                )
                state.latestUpdate?.let { update ->
                    ListItem(
                        leadingContent = { Icon(Icons.Outlined.SystemUpdate, contentDescription = null) },
                        headlineContent = { Text(stringResource(R.string.update_available)) },
                        supportingContent = { Text(update.name) },
                        trailingContent = {
                            TextButton(onClick = { openUrl(context, update.url) }) {
                                Text(stringResource(R.string.update_download))
                            }
                        },
                        colors = transparentItem(),
                    )
                }
                Row(
                    Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    FilledTonalButton(
                        onClick = vm::checkForUpdates,
                        enabled = !state.checkingUpdate,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        if (state.checkingUpdate) {
                            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                            Spacer(Modifier.width(8.dp))
                        }
                        Text(stringResource(R.string.settings_check_updates))
                    }
                }
                HorizontalDivider(Modifier.padding(horizontal = 16.dp))
                Text(
                    stringResource(
                        R.string.settings_client_id,
                        vm.effectiveClientId().ifEmpty { stringResource(R.string.settings_none) },
                    ),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(16.dp),
                )
            }

            Text(
                stringResource(R.string.settings_privacy),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = 4.dp),
            )
        }
    }

    if (confirmUnpair) {
        UnpairDialog(ui.device?.name.orEmpty(), onDismiss = { confirmUnpair = false }) {
            confirmUnpair = false
            vm.unpairCurrent()
        }
    }

    if (confirmSignOut) {
        AlertDialog(
            onDismissRequest = { confirmSignOut = false },
            title = { Text(stringResource(R.string.settings_sign_out_title)) },
            text = { Text(stringResource(R.string.settings_sign_out_body)) },
            confirmButton = {
                TextButton(onClick = {
                    confirmSignOut = false
                    vm.signOut()
                }) { Text(stringResource(R.string.settings_sign_out)) }
            },
            dismissButton = {
                TextButton(onClick = { confirmSignOut = false }) { Text(stringResource(R.string.action_cancel)) }
            },
        )
    }
}

@Composable
private fun Section(title: String, content: @Composable () -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Text(
            title,
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(horizontal = 4.dp),
        )
        Card(
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Column(Modifier.padding(vertical = 4.dp)) { content() }
        }
    }
}

@Composable
private fun transparentItem() = ListItemDefaults.colors(containerColor = androidx.compose.ui.graphics.Color.Transparent)
