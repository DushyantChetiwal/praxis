package io.github.dushyantchetiwal.praxis.remote.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row

import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.outlined.InsertDriveFile
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.outlined.FileDownload
import androidx.compose.material.icons.outlined.Folder
import androidx.compose.material.icons.outlined.FolderOff
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.PlainTooltip
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TooltipBox
import androidx.compose.material3.TooltipDefaults
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.material3.rememberTooltipState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
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
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.DownloadState
import io.github.dushyantchetiwal.praxis.remote.FilesState
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.data.DirEntry
import io.github.dushyantchetiwal.praxis.remote.data.DownloadSink
import io.github.dushyantchetiwal.praxis.remote.data.FileContent
import io.github.dushyantchetiwal.praxis.remote.data.mimeTypeFor

@Composable
fun FilesTab(ui: DeviceUi, vm: MainViewModel) {
    val files = ui.files
    val file = files.file
    // Before Android 10 a download needs the user to pick where it goes.
    var picking by rememberSaveable { mutableStateOf<String?>(null) }
    val chooser = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) { uri ->
        val path = picking
        picking = null
        if (uri != null && path != null) vm.download(DirEntry(path.substringAfterLast('/'), path, dir = false), uri)
    }
    val onDownload: (DirEntry) -> Unit = { entry ->
        if (DownloadSink.savesDirectly) {
            vm.download(entry)
        } else {
            picking = entry.path
            chooser.launch(entry.name)
        }
    }
    val downloading = ui.download?.active == true

    Column(Modifier.fillMaxSize()) {
        ui.download?.let { download ->
            DownloadBar(download, vm, Modifier.padding(horizontal = 12.dp, vertical = 6.dp))
        }
        Box(Modifier.weight(1f)) {
            if (file != null) {
                BackHandler(onBack = vm::closeFile)
                val entry = DirEntry(file.path.substringAfterLast('/').ifEmpty { file.path }, file.path, dir = false)
                FileViewer(file, vm::closeFile) {
                    DownloadButton(entry, ui.download, enabled = !downloading, onDownload = onDownload)
                }
            } else {
                DirectoryListing(files, vm) { entry ->
                    DownloadButton(entry, ui.download, enabled = !downloading && !files.loading, onDownload = onDownload)
                }
            }
        }
    }
}

/** Progress of the one running download, or how the last one ended. */
@Composable
private fun DownloadBar(download: DownloadState, vm: MainViewModel, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val scheme = MaterialTheme.colorScheme
    val error = download.error
    Surface(
        color = if (error != null) scheme.errorContainer else scheme.secondaryContainer,
        contentColor = if (error != null) scheme.onErrorContainer else scheme.onSecondaryContainer,
        shape = MaterialTheme.shapes.medium,
        modifier = modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(start = 12.dp, end = 4.dp, top = 4.dp, bottom = 4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                val title = when {
                    error != null -> error
                    download.savedUri != null && download.inDownloads -> stringResource(R.string.download_saved_downloads, download.name)
                    download.savedUri != null -> stringResource(R.string.download_saved, download.name)
                    else -> stringResource(R.string.download_running, download.name)
                }
                Column(Modifier.weight(1f).padding(vertical = 6.dp)) {
                    Text(title, style = MaterialTheme.typography.bodyMedium, maxLines = 3, overflow = TextOverflow.Ellipsis)
                    if (download.active) {
                        val size = download.size
                        Text(
                            if (size == null) {
                                stringResource(R.string.download_starting)
                            } else {
                                stringResource(R.string.download_progress, formatBytes(download.received), formatBytes(size))
                            },
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
                when {
                    download.active -> TextButton(onClick = vm::cancelDownload) {
                        Text(stringResource(R.string.action_cancel))
                    }
                    else -> {
                        download.savedUri?.let { uri ->
                            TextButton(onClick = { openDownload(context, Uri.parse(uri), download.name) }) {
                                Text(stringResource(R.string.download_open))
                            }
                        }
                        IconButton(onClick = vm::dismissDownload) {
                            Icon(Icons.Filled.Close, contentDescription = stringResource(R.string.action_dismiss))
                        }
                    }
                }
            }
            if (download.active) {
                val fraction = download.fraction
                val barModifier = Modifier.fillMaxWidth().padding(end = 8.dp, bottom = 6.dp)
                if (fraction == null) {
                    LinearProgressIndicator(barModifier)
                } else {
                    LinearProgressIndicator(progress = { fraction }, modifier = barModifier)
                }
            }
        }
    }
}

private fun openDownload(context: Context, uri: Uri, name: String) {
    val intent = Intent(Intent.ACTION_VIEW)
        .setDataAndType(uri, mimeTypeFor(name))
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
    try {
        context.startActivity(intent)
    } catch (e: ActivityNotFoundException) {
        Toast.makeText(context, context.getString(R.string.download_no_app), Toast.LENGTH_LONG).show()
    }
}

/** A file's download button, which shows progress while that file downloads. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DownloadButton(entry: DirEntry, download: DownloadState?, enabled: Boolean, onDownload: (DirEntry) -> Unit) {
    if (download?.active == true && download.path == entry.path) {
        Box(Modifier.size(48.dp), contentAlignment = Alignment.Center) {
            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
        }
        return
    }
    val label = stringResource(R.string.download_action, entry.name)
    TooltipBox(
        positionProvider = TooltipDefaults.rememberPlainTooltipPositionProvider(),
        tooltip = { PlainTooltip { Text(label) } },
        state = rememberTooltipState(),
    ) {
        IconButton(onClick = { onDownload(entry) }, enabled = enabled) {
            Icon(Icons.Outlined.FileDownload, contentDescription = label)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DirectoryListing(files: FilesState, vm: MainViewModel, fileAction: @Composable (DirEntry) -> Unit) {
    val segments = files.path.split('/').filter { it.isNotEmpty() }
    if (segments.isNotEmpty()) {
        BackHandler { vm.listDir(segments.dropLast(1).joinToString("/")) }
    }
    Column(Modifier.fillMaxSize()) {
        Breadcrumbs(segments, vm::listDir)
        if (files.loading) LinearProgressIndicator(Modifier.fillMaxWidth()) else HorizontalDivider()
        PullToRefreshBox(
            isRefreshing = false,
            onRefresh = { vm.listDir(files.path) },
            modifier = Modifier.weight(1f),
        ) {
            LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(vertical = 4.dp)) {
                files.error?.let { error ->
                    item(key = "error") {
                        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            ErrorText(error)
                            if (files.entries == null) {
                                OutlinedButton(onClick = { vm.listDir(files.path) }, modifier = Modifier.fillMaxWidth()) {
                                    Text(stringResource(R.string.action_try_again))
                                }
                            }
                        }
                    }
                }
                val entries = files.entries
                when {
                    entries == null -> if (files.loading) item(key = "loading") { LoadingRow(stringResource(R.string.files_loading)) }
                    entries.isEmpty() -> item(key = "empty") {
                        EmptyState(
                            title = stringResource(if (files.path.isEmpty()) R.string.files_no_projects else R.string.files_empty),
                            icon = Icons.Outlined.FolderOff,
                        )
                    }
                    else -> {
                        items(entries, key = { it.path }) { entry ->
                            ListItem(
                                leadingContent = {
                                    Icon(
                                        if (entry.dir) Icons.Outlined.Folder else Icons.AutoMirrored.Outlined.InsertDriveFile,
                                        contentDescription = null,
                                        tint = if (entry.dir) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
                                    )
                                },
                                headlineContent = { Text(entry.name, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                                trailingContent = if (entry.dir) {
                                    { Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null) }
                                } else {
                                    { fileAction(entry) }
                                },
                                modifier = Modifier.clickable(enabled = !files.loading) {
                                    if (entry.dir) vm.listDir(entry.path) else vm.openFile(entry)
                                },
                            )
                        }
                        if (files.truncated) {
                            item(key = "truncated") {
                                Text(
                                    stringResource(R.string.files_truncated_listing),
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    textAlign = TextAlign.Center,
                                    modifier = Modifier.fillMaxWidth().padding(16.dp),
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun Breadcrumbs(segments: List<String>, onNavigate: (String) -> Unit) {
    LazyRow(
        contentPadding = PaddingValues(horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.fillMaxWidth(),
    ) {
        item(key = "root") {
            TextButton(onClick = { onNavigate("") }, enabled = segments.isNotEmpty()) {
                Text(stringResource(R.string.files_projects), fontWeight = if (segments.isEmpty()) FontWeight.Bold else null)
            }
        }
        itemsIndexed(segments) { index, segment ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("/", color = MaterialTheme.colorScheme.outline)
                val last = index == segments.lastIndex
                TextButton(onClick = { onNavigate(segments.take(index + 1).joinToString("/")) }, enabled = !last) {
                    Text(segment, fontWeight = if (last) FontWeight.Bold else null)
                }
            }
        }
    }
}

@Composable
private fun FileViewer(file: FileContent, onBack: () -> Unit, action: @Composable () -> Unit) {
    val name = file.path.substringAfterLast('/').ifEmpty { file.path }
    val meta = listOfNotNull(file.path, file.size?.let(::formatBytes)).joinToString(" · ")
    val lines = remember(file.content) {
        file.content.replace("\r\n", "\n").replace('\r', '\n').split('\n').let {
            if (it.size > 1 && it.last().isEmpty()) it.dropLast(1) else it
        }
    }
    val numbers = remember(lines) { (1..lines.size).joinToString("\n") }
    val code = remember(lines) { lines.joinToString("\n") }

    Column(Modifier.fillMaxSize()) {
        Row(Modifier.padding(end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.action_back))
            }
            Column(Modifier.weight(1f)) {
                Text(name, style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    meta,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            action()
        }
        if (file.truncated) {
            BannerRow(
                io.github.dushyantchetiwal.praxis.remote.Banner(stringResource(R.string.files_truncated), error = false),
                Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
            )
        }
        HorizontalDivider()
        val style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 18.sp)
        Row(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(vertical = 8.dp),
        ) {
            Text(
                numbers,
                style = style,
                color = MaterialTheme.colorScheme.outline,
                textAlign = TextAlign.End,
                softWrap = false,
                modifier = Modifier.padding(start = 8.dp, end = 10.dp),
            )
            Box(
                Modifier
                    .weight(1f)
                    .horizontalScroll(rememberScrollState()),
            ) {
                SelectionContainer {
                    Text(code, style = style, softWrap = false, modifier = Modifier.padding(end = 16.dp))
                }
            }
        }
    }
}
