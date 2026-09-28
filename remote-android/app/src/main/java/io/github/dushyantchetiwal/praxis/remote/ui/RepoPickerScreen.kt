package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.OpenInNew
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Public
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.outlined.Inventory2
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.AppState
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R

private const val INSTALLATIONS_URL = "https://github.com/settings/installations"
private const val APPS_URL = "https://github.com/apps"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RepoPickerScreen(state: AppState, vm: MainViewModel, snackbar: SnackbarHostState) {
    val repos = state.repos
    val context = LocalContext.current
    var manual by rememberSaveable { mutableStateOf("") }

    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    if (vm.canGoBack(state)) {
                        IconButton(onClick = { vm.back() }) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                        }
                    }
                },
                title = { Text(stringResource(R.string.repos_title)) },
                actions = {
                    IconButton(onClick = { vm.discoverRepos() }, enabled = !repos.loading) {
                        Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.action_refresh))
                    }
                    TextButton(onClick = { vm.signOut() }) { Text(stringResource(R.string.settings_sign_out)) }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .imePadding(),
            contentPadding = PaddingValues(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                Text(
                    stringResource(R.string.repos_intro),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            repos.error?.let { error -> item { ErrorText(error) } }
            if (repos.loading && repos.repos.isEmpty()) {
                item { LoadingRow(stringResource(R.string.repos_loading)) }
            } else if (repos.loaded && repos.repos.isEmpty() && repos.error == null) {
                item {
                    Card(Modifier.fillMaxWidth()) {
                        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                Icon(Icons.Outlined.Inventory2, contentDescription = null)
                                Spacer(Modifier.width(12.dp))
                                Text(stringResource(R.string.repos_none_title), style = MaterialTheme.typography.titleMedium)
                            }
                            Text(
                                stringResource(
                                    if (repos.noInstallations) R.string.repos_none_installations else R.string.repos_none_repos,
                                ),
                                style = MaterialTheme.typography.bodyMedium,
                            )
                            Button(onClick = { openUrl(context, INSTALLATIONS_URL) }, modifier = Modifier.fillMaxWidth()) {
                                Icon(Icons.AutoMirrored.Filled.OpenInNew, contentDescription = null, modifier = Modifier.size(18.dp))
                                Spacer(Modifier.width(8.dp))
                                Text(stringResource(R.string.repos_open_installations))
                            }
                            OutlinedButton(onClick = { openUrl(context, APPS_URL) }, modifier = Modifier.fillMaxWidth()) {
                                Text(stringResource(R.string.repos_browse_apps))
                            }
                            TextButton(onClick = { vm.discoverRepos(autoSelect = true) }, modifier = Modifier.fillMaxWidth()) {
                                Text(stringResource(R.string.action_try_again))
                            }
                        }
                    }
                }
            }
            if (repos.repos.isNotEmpty()) {
                item {
                    Text(stringResource(R.string.repos_available), style = MaterialTheme.typography.titleSmall)
                }
                items(repos.repos, key = { it.fullName }) { repo ->
                    Card(Modifier.fillMaxWidth()) {
                        ListItem(
                            headlineContent = { Text(repo.fullName) },
                            supportingContent = {
                                Text(stringResource(if (repo.private) R.string.repos_private else R.string.repos_public_warning))
                            },
                            leadingContent = {
                                Icon(
                                    if (repo.private) Icons.Filled.Lock else Icons.Filled.Public,
                                    contentDescription = null,
                                    tint = if (repo.private) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.error,
                                )
                            },
                            trailingContent = {
                                if (state.repo.equals(repo.fullName, ignoreCase = true)) {
                                    Text(stringResource(R.string.label_current), style = MaterialTheme.typography.labelMedium)
                                }
                            },
                            colors = ListItemDefaults.colors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
                            modifier = Modifier.clickable { vm.pickRepo(repo) },
                        )
                    }
                }
            }
            item {
                Column(Modifier.padding(top = 8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(stringResource(R.string.repos_manual_title), style = MaterialTheme.typography.titleSmall)
                    OutlinedTextField(
                        value = manual,
                        onValueChange = { manual = it },
                        label = { Text(stringResource(R.string.repos_manual_label)) },
                        placeholder = { Text("owner/praxis-remote") },
                        singleLine = true,
                        isError = repos.manualError != null,
                        supportingText = repos.manualError?.let { error -> { Text(error) } },
                        keyboardOptions = KeyboardOptions(
                            keyboardType = KeyboardType.Uri,
                            imeAction = ImeAction.Go,
                            autoCorrectEnabled = false,
                        ),
                        keyboardActions = KeyboardActions(onGo = { vm.useManualRepo(manual) }),
                        modifier = Modifier.fillMaxWidth(),
                    )
                    FilledTonalButton(
                        onClick = { vm.useManualRepo(manual) },
                        enabled = manual.isNotBlank() && !repos.checking,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        if (repos.checking) {
                            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                            Spacer(Modifier.width(8.dp))
                        }
                        Text(stringResource(R.string.repos_manual_use))
                    }
                }
            }
        }
    }

    repos.confirmPublic?.let { repo ->
        AlertDialog(
            onDismissRequest = vm::dismissPublicRepo,
            icon = { Icon(Icons.Filled.Public, contentDescription = null) },
            title = { Text(stringResource(R.string.repos_public_title)) },
            text = { Text(stringResource(R.string.repos_public_body, repo.fullName)) },
            confirmButton = {
                TextButton(onClick = vm::confirmPublicRepo) { Text(stringResource(R.string.repos_public_confirm)) }
            },
            dismissButton = {
                TextButton(onClick = vm::dismissPublicRepo) { Text(stringResource(R.string.action_cancel)) }
            },
        )
    }
}
