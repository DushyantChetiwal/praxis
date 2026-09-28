package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.outlined.History
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.SuggestionChip
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.Tab

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ThreadsTab(ui: DeviceUi, vm: MainViewModel) {
    val context = LocalContext.current
    val threads = ui.threads
    val now = rememberNow(15_000L)
    PullToRefreshBox(
        isRefreshing = threads.loading && threads.items != null,
        onRefresh = vm::loadThreads,
        modifier = Modifier.fillMaxSize(),
    ) {
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = PaddingValues(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            threads.error?.let { error -> item(key = "error") { ErrorText(error) } }
            val items = threads.items
            when {
                items == null -> if (threads.loading) item(key = "loading") { LoadingRow(stringResource(R.string.threads_loading)) }
                else if (threads.error != null) {
                    item(key = "retry") {
                        OutlinedButton(onClick = vm::loadThreads, modifier = Modifier.fillMaxWidth()) {
                            Text(stringResource(R.string.action_try_again))
                        }
                    }
                }
                items.isEmpty() -> item(key = "empty") {
                    EmptyState(
                        title = stringResource(R.string.threads_empty_title),
                        body = stringResource(R.string.threads_empty_body),
                        icon = Icons.Outlined.History,
                    ) {
                        OutlinedButton(onClick = { vm.switchTab(Tab.Chat) }) { Text(stringResource(R.string.tab_chat)) }
                    }
                }
                else -> items(items, key = { it.sessionId }) { thread ->
                    Surface(
                        color = MaterialTheme.colorScheme.surfaceContainerLow,
                        shape = RoundedCornerShape(12.dp),
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        ListItem(
                            headlineContent = {
                                Text(
                                    thread.title?.takeIf { it.isNotBlank() } ?: stringResource(R.string.chat_untitled),
                                    maxLines = 2,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            },
                            supportingContent = thread.updatedAt?.let { { Text(relativeTime(context, it, now)) } },
                            trailingContent = {
                                when {
                                    threads.opening == thread.sessionId ->
                                        CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                                    thread.active -> SuggestionChip(
                                        onClick = { vm.openThread(thread) },
                                        label = { Text(stringResource(R.string.threads_active)) },
                                    )
                                    else -> Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null)
                                }
                            },
                            colors = ListItemDefaults.colors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
                            modifier = Modifier
                                .clip(RoundedCornerShape(12.dp))
                                .clickable(enabled = threads.opening == null) { vm.openThread(thread) },
                        )
                    }
                }
            }
        }
    }
}
