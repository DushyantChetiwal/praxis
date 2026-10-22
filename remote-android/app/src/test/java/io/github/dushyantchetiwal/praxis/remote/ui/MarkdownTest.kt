package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.ui.text.style.TextAlign
import org.junit.Assert.assertEquals
import org.junit.Test

class MarkdownTest {
    @Test
    fun pipeTablesKeepAlignmentAndPadShortRows() {
        val blocks = parseMarkdown(
            """
            Results:

            | Name | Count | Note |
            |:-----|------:|:----:|
            | `a\|b` | 2 | ok |
            | c | 3 |
            After
            """.trimIndent(),
        )
        assertEquals(MdBlock.Paragraph("Results:"), blocks[0])
        val table = blocks[1] as MdBlock.Table
        assertEquals(listOf("Name", "Count", "Note"), table.header)
        assertEquals(listOf(TextAlign.Start, TextAlign.End, TextAlign.Center), table.alignments)
        assertEquals(listOf(listOf("`a|b`", "2", "ok"), listOf("c", "3", "")), table.rows)
        assertEquals(MdBlock.Paragraph("After"), blocks[2])
    }

    @Test
    fun aParagraphWithAPipeIsNotATable() {
        val blocks = parseMarkdown("use a | b here\nand more")
        assertEquals(listOf(MdBlock.Paragraph("use a | b here\nand more")), blocks)
    }

    @Test
    fun taskItemsShowTheirState() {
        val blocks = parseMarkdown("- [ ] todo\n- [x] done\n- plain")
        assertEquals(
            listOf(MdBlock.Item("☐", 0, "todo"), MdBlock.Item("☑", 0, "done"), MdBlock.Item("•", 0, "plain")),
            blocks,
        )
    }
}
