package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.background
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
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.net.URI

// A small, safe markdown renderer: text is never interpreted as markup beyond
// the constructs below, and only http(s) links are made clickable.

sealed interface MdBlock {
    data class Paragraph(val text: String) : MdBlock
    data class Heading(val level: Int, val text: String) : MdBlock
    data class Code(val language: String?, val code: String) : MdBlock
    data class Item(val marker: String, val level: Int, val text: String) : MdBlock
    data class Quote(val text: String) : MdBlock
    data object Rule : MdBlock
}

private val FENCE_RE = Regex("""^\s*(`{3,}|~{3,})(.*)$""")
private val HEADING_RE = Regex("""^\s{0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$""")
private val RULE_RE = Regex("""^\s{0,3}([-*_])(\s*\1){2,}\s*$""")
private val QUOTE_RE = Regex("""^\s{0,3}>\s?(.*)$""")
private val BULLET_RE = Regex("""^(\s*)[-*+]\s+(.*)$""")
private val ORDERED_RE = Regex("""^(\s*)(\d{1,9})[.)]\s+(.*)$""")
private val CONTINUATION_RE = Regex("""^\s+\S""")

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
            out += MdBlock.Item("•", (bullet.groupValues[1].length / 2).coerceAtMost(3), bullet.groupValues[2])
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
        "|(https?://[^\\s<>\"'`]+[^\\s<>\"'`.,:;!?)\\]}])", // 8: bare URL
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
    val colors = mdColors()
    Column(modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (block in blocks) {
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
                    },
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
                MdBlock.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp))
            }
        }
    }
}

@Composable
fun CodeBlock(code: String, language: String?, modifier: Modifier = Modifier) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainerHighest,
        shape = RoundedCornerShape(8.dp),
        modifier = modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            if (language != null) {
                Text(
                    language,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(bottom = 4.dp),
                )
            }
            Box(Modifier.horizontalScroll(rememberScrollState())) {
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
