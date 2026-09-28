package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.Screen

@Composable
fun PraxisApp(vm: MainViewModel) {
    val state by vm.app.collectAsStateWithLifecycle()
    val ui by vm.ui.collectAsStateWithLifecycle()
    val busy by vm.busy.collectAsStateWithLifecycle()
    val snackbar = remember { SnackbarHostState() }

    LaunchedEffect(vm) {
        vm.messages.collect { text ->
            snackbar.currentSnackbarData?.dismiss()
            snackbar.showSnackbar(text)
        }
    }

    BackHandler(enabled = vm.canGoBack(state)) { vm.back() }

    Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
        when (state.screen) {
            Screen.Loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator()
            }
            Screen.SignIn -> SignInScreen(state, vm, snackbar)
            Screen.Repos -> RepoPickerScreen(state, vm, snackbar)
            Screen.Devices -> DevicesScreen(state, ui, vm, snackbar)
            Screen.Device -> DeviceScreen(state, ui, busy, vm, snackbar)
            Screen.Settings -> SettingsScreen(state, ui, vm, snackbar)
        }
    }
}
