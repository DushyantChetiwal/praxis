package io.github.dushyantchetiwal.praxis.remote.ui

import android.os.Build
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.dushyantchetiwal.praxis.remote.R
import java.net.URI

// A small, safe markdown renderer: text is never interpreted as markup beyond
// the constructs below, and only http(s) links are made clickable.

sealed interface MdBlock {
    data class Paragraph(val text: String) : MdBlock
    data class Heading(val level: Int, val text: String) : MdBlock
    data class Code(val language: String?, val code: String) : MdBlock
    data class Item(val marker: String, val level: Int, val text: String) : MdBlock
    data class Quote(val text: String) : MdBlock
    data class Table(val alignments: List<TextAlign>, val header: List<String>, val rows: List<List<String>>) : MdBlock
    data object Rule : MdBlock
}

private val FENCE_RE = Regex("""^\s*(`{3,}|~{3,})(.*)$""")
private val HEADING_RE = Regex("""^\s{0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$""")
private val RULE_RE = Regex("""^\s{0,3}([-*_])(\s*\1){2,}\s*$""")
private val QUOTE_RE = Regex("""^\s{0,3}>\s?(.*)$""")
private val BULLET_RE = Regex("""^(\s*)[-*+]\s+(.*)$""")
private val ORDERED_RE = Regex("""^(\s*)(\d{1,9})[.)]\s+(.*)$""")
private val CONTINUATION_RE = Regex("""^\s+\S""")
private val TASK_RE = Regex("""^\[([ xX])]\s+(.*)$""")
private val TABLE_SEPARATOR_RE = Regex("""^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$""")
private const val MAX_TABLE_COLUMNS = 16

// Splits one table row on unescaped pipes, ignoring the optional outer ones.
fun tableCells(line: String): List<String> {
    var row = line.trim()
    if (row.startsWith("|")) row = row.substring(1)
    if (row.endsWith("|") && !row.endsWith("\\|")) row = row.dropLast(1)
    val cells = mutableListOf<String>()
    val cell = StringBuilder()
    var index = 0
    while (index < row.length) {
        val character = row[index]
        if (character == '\\' && index + 1 < row.length && row[index + 1] == '|') {
            cell.append('|')
            index += 2
            continue
        }
        if (character == '|') {
            cells += cell.toString().trim()
            cell.clear()
        } else {
            cell.append(character)
        }
        index++
    }
    cells += cell.toString().trim()
    return cells
}

private fun tableAlignment(cell: String): TextAlign = when {
    cell.startsWith(":") && cell.endsWith(":") -> TextAlign.Center
    cell.endsWith(":") -> TextAlign.End
    else -> TextAlign.Start
}

fun parseMarkdown(source: String): List<MdBlock> {
    val lines = source.replace("\r\n", "\n").replace('\r', '\n').split('\n')
    val out = mutableListOf<MdBlock>()
    val paragraph = mutableListOf<String>()
    val quote = mutableListOf<String>()
    var inList = false

    fun flushParagraph() {
        if (paragraph.isNotEmpty()) out += MdBlock.Paragraph(paragraph.joinToString("\n"))
        paragraph.clear()
    }
    fun flushQuote() {
        if (quote.isNotEmpty()) out += MdBlock.Quote(quote.joinToString("\n"))
        quote.clear()
    }
    fun flushAll() {
        flushParagraph()
        flushQuote()
        inList = false
    }

    var i = 0
    while (i < lines.size) {
        val line = lines[i]
        val fence = FENCE_RE.find(line)
        if (fence != null) {
            flushAll()
            val marker = fence.groupValues[1]
            val closer = Regex("^\\s*" + marker[0] + "{" + marker.length + ",}\\s*$")
            val language = fence.groupValues[2].trim().split(Regex("\\s+")).firstOrNull()?.takeIf { it.isNotEmpty() }
            val body = mutableListOf<String>()
            i++
            while (i < lines.size && !closer.matches(lines[i])) {
                body += lines[i]
                i++
            }
            out += MdBlock.Code(language, body.joinToString("\n"))
            i++
            continue
        }
        if (line.isBlank()) {
            flushAll()
            i++
            continue
        }
        val heading = HEADING_RE.find(line)
        if (heading != null) {
            flushAll()
            out += MdBlock.Heading(heading.groupValues[1].length, heading.groupValues[2])
            i++
            continue
        }
        if (RULE_RE.matches(line)) {
            flushAll()
            out += MdBlock.Rule
            i++
            continue
        }
        if ('|' in line && i + 1 < lines.size && TABLE_SEPARATOR_RE.matches(lines[i + 1])) {
            val header = tableCells(line).take(MAX_TABLE_COLUMNS)
            val alignments = tableCells(lines[i + 1]).take(header.size).map(::tableAlignment)
            if (header.size > 1 || line.trim().startsWith("|")) {
                if (alignments.size == header.size) {
                    flushAll()
                    val rows = mutableListOf<List<String>>()
                    i += 2
                    while (i < lines.size && lines[i].isNotBlank() && '|' in lines[i]) {
                        val cells = tableCells(lines[i]).take(header.size)
                        rows += cells + List(header.size - cells.size) { "" }
                        i++
                    }
                    out += MdBlock.Table(alignments, header, rows)
                    continue
                }
            }
        }
        val quoteLine = QUOTE_RE.find(line)
        if (quoteLine != null) {
            flushParagraph()
            inList = false
            quote += quoteLine.groupValues[1]
            i++
            continue
        }
        flushQuote()
        val bullet = BULLET_RE.find(line)
        if (bullet != null) {
            flushParagraph()
            inList = true
            val level = (bullet.groupValues[1].length / 2).coerceAtMost(3)
            val task = TASK_RE.find(bullet.groupValues[2])
            out += if (task != null) {
                MdBlock.Item(if (task.groupValues[1] == " ") "☐" else "☑", level, task.groupValues[2])
            } else {
                MdBlock.Item("•", level, bullet.groupValues[2])
            }
            i++
            continue
        }
        val ordered = ORDERED_RE.find(line)
        if (ordered != null) {
            flushParagraph()
            inList = true
            val level = (ordered.groupValues[1].length / 2).coerceAtMost(3)
            out += MdBlock.Item("${ordered.groupValues[2]}.", level, ordered.groupValues[3])
            i++
            continue
        }
        val last = out.lastOrNull()
        if (inList && last is MdBlock.Item && CONTINUATION_RE.containsMatchIn(line)) {
            // An indented continuation of the previous list item.
            out[out.lastIndex] = last.copy(text = last.text + "\n" + line.trim())
            i++
            continue
        }
        inList = false
        paragraph += line
        i++
    }
    flushAll()
    return out
}

private val INLINE_RE = Regex(
    "`([^`\\n]+)`" + // 1: code
        "|\\[([^\\]\\n]+)]\\(([^()\\s]+)\\)" + // 2, 3: link
        "|\\*\\*([^*\\n]+)\\*\\*" + // 4: bold
        "|__([^_\\n]+)__" + // 5: bold
        "|(?<![\\w*])\\*([^*\\s][^*\\n]*?)\\*(?![\\w*])" + // 6: italic
        "|(?<!\\w)_([^_\\s][^_\\n]*?)_(?!\\w)" + // 7: italic
        "|(https?://[^\\s<>\"'`]+[^\\s<>\"'`.,:;!?)\\]}])" + // 8: bare URL
        "|~~([^~\\n]+)~~", // 9: strikethrough
)

fun safeUrl(raw: String): String? = runCatching {
    val uri = URI(raw)
    val scheme = uri.scheme?.lowercase()
    if ((scheme == "http" || scheme == "https") && !uri.host.isNullOrEmpty()) uri.toString() else null
}.getOrNull()

class MdColors(val code: Color, val link: Color)

fun AnnotatedString.Builder.appendInline(text: String, colors: MdColors) {
    var last = 0
    for (match in INLINE_RE.findAll(text)) {
        append(text.substring(last, match.range.first))
        last = match.range.last + 1
        val g = match.groups
        when {
            g[1] != null -> withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = colors.code)) {
                append(g[1]!!.value)
            }
            g[2] != null -> {
                val url = safeUrl(g[3]!!.value)
                if (url == null) {
                    append(match.value)
                } else {
                    withLink(link(url, colors)) { appendInline(g[2]!!.value, colors) }
                }
            }
            g[4] != null -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { appendInline(g[4]!!.value, colors) }
            g[5] != null -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { appendInline(g[5]!!.value, colors) }
            g[6] != null -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { appendInline(g[6]!!.value, colors) }
            g[7] != null -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { appendInline(g[7]!!.value, colors) }
            g[8] != null -> {
                val url = safeUrl(g[8]!!.value)
                if (url == null) append(match.value) else withLink(link(url, colors)) { append(match.value) }
            }
            g[9] != null -> withStyle(SpanStyle(textDecoration = TextDecoration.LineThrough)) {
                appendInline(g[9]!!.value, colors)
            }
        }
    }
    append(text.substring(last))
}

private fun link(url: String, colors: MdColors) = LinkAnnotation.Url(
    url,
    TextLinkStyles(SpanStyle(color = colors.link, textDecoration = TextDecoration.Underline)),
)

@Composable
fun mdColors(): MdColors {
    val scheme = MaterialTheme.colorScheme
    return MdColors(code = scheme.onSurface.copy(alpha = 0.08f), link = scheme.primary)
}

@Composable
fun inlineMarkdown(text: String): AnnotatedString {
    val colors = mdColors()
    return remember(text, colors.code, colors.link) { buildAnnotatedString { appendInline(text, colors) } }
}

@Composable
fun Markdown(
    text: String,
    modifier: Modifier = Modifier,
    style: TextStyle = MaterialTheme.typography.bodyMedium,
    color: Color = LocalContentColor.current,
) {
    val blocks = remember(text) { parseMarkdown(text) }
    Column(modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (block in blocks) MarkdownBlock(block, style, color)
    }
}

@Composable
fun MarkdownBlock(
    block: MdBlock,
    style: TextStyle = MaterialTheme.typography.bodyMedium,
    color: Color = LocalContentColor.current,
) {
    val colors = mdColors()
    when (block) {
                is MdBlock.Paragraph -> Text(
                    remember(block, colors.link) { buildAnnotatedString { appendInline(block.text, colors) } },
                    style = style,
                    color = color,
                )
                is MdBlock.Heading -> Text(
                    remember(block, colors.link) { buildAnnotatedString { appendInline(block.text, colors) } },
                    style = when (block.level) {
                        1 -> MaterialTheme.typography.titleLarge
                        2 -> MaterialTheme.typography.titleMedium
                        else -> MaterialTheme.typography.titleSmall
                    }.copy(fontWeight = FontWeight.SemiBold),
                    color = color,
                    modifier = Modifier.padding(top = 4.dp),
                )
                is MdBlock.Item -> Row(Modifier.padding(start = (block.level * 16).dp)) {
                    Text(
                        block.marker,
                        style = style,
                        color = color,
                        modifier = Modifier.widthIn(min = 18.dp).padding(end = 6.dp),
                    )
                    Text(
                        remember(block, colors.link) { buildAnnotatedString { appendInline(block.text, colors) } },
                        style = style,
                        color = color,
                    )
                }
                is MdBlock.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
                    Box(
                        Modifier
                            .width(3.dp)
                            .fillMaxHeight()
                            .background(MaterialTheme.colorScheme.outlineVariant, RoundedCornerShape(2.dp)),
                    )
                    Text(
                        remember(block, colors.link) { buildAnnotatedString { appendInline(block.text, colors) } },
                        style = style,
                        color = color.copy(alpha = 0.8f),
                        modifier = Modifier.padding(start = 10.dp),
                    )
                }
                is MdBlock.Code -> CodeBlock(block.code, block.language)
                is MdBlock.Table -> MarkdownTable(block, style, color)
                MdBlock.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp))
    }
}

// Copies text and confirms it; Android 13 and later show their own confirmation.
@Composable
fun rememberCopyText(): (String) -> Unit {
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    val copied = stringResource(R.string.copied)
    return remember(clipboard, context, copied) {
        { text ->
            clipboard.setText(AnnotatedString(text))
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                Toast.makeText(context, copied, Toast.LENGTH_SHORT).show()
            }
        }
    }
}

@Composable
fun CodeBlock(code: String, language: String?, modifier: Modifier = Modifier) {
    val copyText = rememberCopyText()
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerHighest,
        shape = RoundedCornerShape(8.dp),
        modifier = modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(start = 10.dp, end = 2.dp, bottom = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    language.orEmpty(),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.weight(1f),
                )
                IconButton(onClick = { copyText(code) }, modifier = Modifier.size(36.dp)) {
                    Icon(
                        Icons.Outlined.ContentCopy,
                        contentDescription = stringResource(R.string.action_copy_code),
                        modifier = Modifier.size(16.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            Box(Modifier.padding(end = 8.dp).horizontalScroll(rememberScrollState())) {
                Text(
                    code,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                    lineHeight = 17.sp,
                    softWrap = false,
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }
        }
    }
}

@Composable
private fun MarkdownTable(block: MdBlock.Table, style: TextStyle, color: Color) {
    val colors = mdColors()
    val scheme = MaterialTheme.colorScheme
    val maxCellWidth = with(LocalDensity.current) { 260.dp.roundToPx() }
    val dividerHeight = with(LocalDensity.current) { 1.dp.roundToPx().coerceAtLeast(1) }
    val columns = block.header.size
    val rows = listOf(block.header) + block.rows
    Box(
        Modifier
            .horizontalScroll(rememberScrollState())
            .border(1.dp, scheme.outlineVariant, RoundedCornerShape(6.dp)),
    ) {
        Layout(
            content = {
                Box(Modifier.background(scheme.surfaceContainerHighest))
                repeat(rows.size - 1) { Box(Modifier.background(scheme.outlineVariant)) }
                rows.forEachIndexed { rowIndex, row ->
                    row.forEachIndexed { column, cell ->
                        Text(
                            remember(cell, colors.link) { buildAnnotatedString { appendInline(cell, colors) } },
                            style = if (rowIndex == 0) style.copy(fontWeight = FontWeight.SemiBold) else style,
                            color = color,
                            textAlign = block.alignments.getOrElse(column) { TextAlign.Start },
                            modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
                        )
                    }
                }
            },
        ) { measurables, _ ->
            val cells = measurables.drop(rows.size)
            val widths = IntArray(columns) { column ->
                rows.indices.maxOf { row -> cells[row * columns + column].maxIntrinsicWidth(Constraints.Infinity) }
                    .coerceAtMost(maxCellWidth)
            }
            val placeables = cells.mapIndexed { index, cell ->
                val width = widths[index % columns]
                cell.measure(Constraints(minWidth = width, maxWidth = width))
            }
            val heights = IntArray(rows.size) { row ->
                (0 until columns).maxOf { column -> placeables[row * columns + column].height }
            }
            val totalWidth = widths.sum()
            val totalHeight = heights.sum() + dividerHeight * (rows.size - 1)
            val header = measurables[0].measure(Constraints.fixed(totalWidth, heights[0]))
            val dividers = (1 until rows.size).map { measurables[it].measure(Constraints.fixed(totalWidth, dividerHeight)) }
            layout(totalWidth, totalHeight) {
                header.place(0, 0)
                var y = 0
                for (row in rows.indices) {
                    if (row > 0) {
                        dividers[row - 1].place(0, y)
                        y += dividerHeight
                    }
                    var x = 0
                    for (column in 0 until columns) {
                        placeables[row * columns + column].place(x, y)
                        x += widths[column]
                    }
                    y += heights[row]
                }
            }
        }
    }
}
