package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.outlined.Link
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.dushyantchetiwal.praxis.remote.AppState
import io.github.dushyantchetiwal.praxis.remote.Banner
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.PairPhase
import io.github.dushyantchetiwal.praxis.remote.PairingState
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.data.Device
import io.github.dushyantchetiwal.praxis.remote.data.RemoteCrypto

private const val PAIR_WINDOW_MS = 5 * 60_000L

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PairingScreen(state: AppState, vm: MainViewModel, snackbar: SnackbarHostState) {
    val pairing = state.pairing
    val device = pairing.device
    Scaffold(
        topBar = {
            TopAppBar(
                navigationIcon = {
                    IconButton(onClick = vm::cancelPairing) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
                    }
                },
                title = {
                    Text(
                        stringResource(R.string.pair_title, device?.name.orEmpty()),
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp, vertical = 16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            if (device == null) return@Column
            Column(
                Modifier.widthIn(max = 420.dp).fillMaxWidth(),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                when (val phase = pairing.phase) {
                    PairPhase.Ready -> ReadyContent(device, state.phoneName, vm)
                    PairPhase.Waiting -> WaitingContent(device, pairing, vm)
                    is PairPhase.Code -> CodeContent(device, phase.code, pairing, vm)
                    is PairPhase.Failed -> FailedContent(phase.message, vm)
                }
            }
        }
    }
}

@Composable
private fun ColumnScope.ReadyContent(device: Device, phoneName: String, vm: MainViewModel) {
    val context = LocalContext.current
    val now = rememberNow(15_000L)
    val online = device.seenRecently(now)
    Spacer(Modifier.height(8.dp))
    Icon(
        Icons.Outlined.Link,
        contentDescription = null,
        tint = MaterialTheme.colorScheme.primary,
        modifier = Modifier.size(48.dp).align(Alignment.CenterHorizontally),
    )
    Text(stringResource(R.string.pair_intro, device.name), style = MaterialTheme.typography.bodyLarge)
    Text(
        stringResource(R.string.pair_steps),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    if (phoneName.isNotBlank()) {
        Text(
            stringResource(R.string.pair_phone_name, phoneName),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    if (!online) {
        val seen = device.lastSeen?.let { stringResource(R.string.banner_offline_seen, relativeTime(context, it, now)) }
            ?: stringResource(R.string.banner_offline_never)
        BannerRow(Banner(stringResource(R.string.pair_offline, device.name, seen), error = true))
    }
    Button(onClick = vm::startPairing, modifier = Modifier.fillMaxWidth().height(52.dp)) {
        Text(
            stringResource(if (online) R.string.pair_start else R.string.pair_start_anyway),
            style = MaterialTheme.typography.titleMedium,
        )
    }
    TextButton(onClick = vm::cancelPairing, modifier = Modifier.fillMaxWidth()) {
        Text(stringResource(R.string.action_cancel))
    }
}

@Composable
private fun WaitingContent(device: Device, pairing: PairingState, vm: MainViewModel) {
    val remaining = remainingSeconds(pairing)
    Spacer(Modifier.height(8.dp))
    LoadingRow(stringResource(R.string.pair_waiting, device.name))
    LinearProgressIndicator(Modifier.fillMaxWidth())
    Text(
        stringResource(R.string.pair_waiting_help, remaining / 60, remaining % 60),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        textAlign = TextAlign.Center,
        modifier = Modifier.fillMaxWidth(),
    )
    TextButton(onClick = vm::cancelPairing, modifier = Modifier.fillMaxWidth()) {
        Text(stringResource(R.string.action_cancel))
    }
}

@Composable
private fun CodeContent(device: Device, code: String, pairing: PairingState, vm: MainViewModel) {
    val remaining = remainingSeconds(pairing)
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            Modifier.padding(20.dp).fillMaxWidth(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(stringResource(R.string.pair_code_title), style = MaterialTheme.typography.titleMedium)
            Text(
                RemoteCrypto.formatCode(code),
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold,
                fontSize = 44.sp,
                letterSpacing = 4.sp,
                color = MaterialTheme.colorScheme.primary,
            )
            Text(
                stringResource(R.string.pair_code_check, device.name),
                style = MaterialTheme.typography.bodyLarge,
                textAlign = TextAlign.Center,
            )
            Text(
                stringResource(R.string.pair_code_mismatch),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp)
                Spacer(Modifier.width(8.dp))
                Text(
                    stringResource(R.string.pair_waiting_approval, remaining / 60, remaining % 60),
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
    }
    TextButton(onClick = vm::cancelPairing, modifier = Modifier.fillMaxWidth()) {
        Text(stringResource(R.string.action_cancel))
    }
}

@Composable
private fun FailedContent(message: String, vm: MainViewModel) {
    ErrorText(message)
    Button(onClick = vm::startPairing, modifier = Modifier.fillMaxWidth().height(52.dp)) {
        Text(stringResource(R.string.action_try_again), style = MaterialTheme.typography.titleMedium)
    }
    TextButton(onClick = vm::cancelPairing, modifier = Modifier.fillMaxWidth()) {
        Text(stringResource(R.string.action_back))
    }
}

@Composable
private fun remainingSeconds(pairing: PairingState): Long {
    val now = rememberNow()
    return ((pairing.startedAt + PAIR_WINDOW_MS - now) / 1000L).coerceAtLeast(0L)
}
