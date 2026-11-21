package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ComposerSuggestionsTest {
    @Test
    fun detectMentionTriggerAtStartAfterWhitespaceAndWithPaths() {
        assertEquals(
            ActiveTrigger(TriggerKind.Mention, "", 0, 1),
            detectComposerTrigger("@"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Mention, "rem", 0, 4),
            detectComposerTrigger("@rem"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Mention, "src/remote.rs", 6, 20),
            detectComposerTrigger("check @src/remote.rs"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Mention, "TranscriptHistory", 7, 25),
            detectComposerTrigger("line 1\n@TranscriptHistory"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Mention, "src", 5, 9),
            detectComposerTrigger("open @src please", cursorIndex = 9),
        )
    }

    @Test
    fun detectMentionTriggerIgnoresEmailsAndCompletedTokens() {
        assertNull(detectComposerTrigger(""))
        assertNull(detectComposerTrigger("   "))
        assertNull(detectComposerTrigger("user@domain.com"))
        assertNull(detectComposerTrigger("send to user@example.com now", cursorIndex = 24))
        assertNull(detectComposerTrigger("@foo bar"))
        assertNull(detectComposerTrigger("@foo "))
    }

    @Test
    fun detectSlashTriggerOnlyAtStartOfPrompt() {
        assertEquals(
            ActiveTrigger(TriggerKind.Slash, "", 0, 1),
            detectComposerTrigger("/"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Slash, "compact", 0, 8),
            detectComposerTrigger("/compact"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Slash, "gpui-test", 2, 12),
            detectComposerTrigger("  /gpui-test"),
        )
        assertEquals(
            ActiveTrigger(TriggerKind.Slash, "comp", 0, 5),
            detectComposerTrigger("/comp now", cursorIndex = 5),
        )

        assertNull(detectComposerTrigger("fix crates/agent/src"))
        assertNull(detectComposerTrigger("run /compact"))
        assertNull(detectComposerTrigger("first line\n/compact"))
        assertNull(detectComposerTrigger("/compact now"))
    }

    @Test
    fun applyComposerSuggestionReplacesTokenAndAvoidsDoubleSpaces() {
        val mention = ComposerSuggestion(
            id = "m1",
            triggerKind = TriggerKind.Mention,
            badge = "File",
            label = "remote.rs",
            subtitle = "crates/agent_ui/src/remote.rs",
            insertText = "@crates/agent_ui/src/remote.rs ",
        )

        val atEnd = applyComposerSuggestion(
            text = "check @rem",
            trigger = ActiveTrigger(TriggerKind.Mention, "rem", 6, 10),
            suggestion = mention,
        )
        assertEquals("check @crates/agent_ui/src/remote.rs ", atEnd.text)
        assertEquals(atEnd.text.length, atEnd.newCursorIndex)

        val midSentence = applyComposerSuggestion(
            text = "check @rem and fix",
            trigger = ActiveTrigger(TriggerKind.Mention, "rem", 6, 10),
            suggestion = mention,
        )
        assertEquals("check @crates/agent_ui/src/remote.rs and fix", midSentence.text)
        assertEquals("check @crates/agent_ui/src/remote.rs".length, midSentence.newCursorIndex)

        val withoutTrailingSpace = mention.copy(insertText = "@crates/agent_ui/src/remote.rs")
        val addedSpace = applyComposerSuggestion(
            text = "@rem",
            trigger = ActiveTrigger(TriggerKind.Mention, "rem", 0, 4),
            suggestion = withoutTrailingSpace,
        )
        assertEquals("@crates/agent_ui/src/remote.rs ", addedSpace.text)
        assertEquals(addedSpace.text.length, addedSpace.newCursorIndex)

        val slash = ComposerSuggestion(
            id = "s1",
            triggerKind = TriggerKind.Slash,
            badge = "Command",
            label = "/compact",
            subtitle = "Compact conversation history",
            insertText = "/compact ",
        )
        val slashApplied = applyComposerSuggestion(
            text = "/comp",
            trigger = ActiveTrigger(TriggerKind.Slash, "comp", 0, 5),
            suggestion = slash,
        )
        assertEquals("/compact ", slashApplied.text)
        assertEquals(9, slashApplied.newCursorIndex)
    }

    @Test
    fun parseMentionSuggestionsHandlesValidEmptyNullAndMalformedPayloads() {
        assertTrue(parseMentionSuggestions(null).isEmpty())
        assertTrue(parseMentionSuggestions(JSONObject("{}")).isEmpty())

        val payload = JSONObject(
            """
            {
              "items": [
                {
                  "kind": "file",
                  "label": "mentions.rs",
                  "path": "crates/agent_ui/src/remote/mentions.rs",
                  "insert_text": "@crates/agent_ui/src/remote/mentions.rs "
                },
                {
                  "kind": "symbol",
                  "label": "TranscriptHistory",
                  "path": "remote-android/app/src/main/java/TranscriptHistory.kt",
                  "insert_text": "@TranscriptHistory "
                },
                {
                  "kind": "file",
                  "label": "missing-insert-text",
                  "path": "src/broken.rs"
                },
                {
                  "kind": "file",
                  "insert_text": "@broken "
                }
              ]
            }
            """.trimIndent(),
        )

        val parsed = parseMentionSuggestions(payload)
        assertEquals(2, parsed.size)

        assertEquals(TriggerKind.Mention, parsed[0].triggerKind)
        assertEquals("File", parsed[0].badge)
        assertEquals("mentions.rs", parsed[0].label)
        assertEquals("crates/agent_ui/src/remote/mentions.rs", parsed[0].subtitle)
        assertEquals("@crates/agent_ui/src/remote/mentions.rs ", parsed[0].insertText)

        assertEquals(TriggerKind.Mention, parsed[1].triggerKind)
        assertEquals("Symbol", parsed[1].badge)
        assertEquals("TranscriptHistory", parsed[1].label)
        assertEquals("remote-android/app/src/main/java/TranscriptHistory.kt", parsed[1].subtitle)
        assertEquals("@TranscriptHistory ", parsed[1].insertText)
    }

    @Test
    fun parseSkillSuggestionsHandlesValidEmptyNullAndMalformedPayloads() {
        assertTrue(parseSkillSuggestions(null).isEmpty())
        assertTrue(parseSkillSuggestions(JSONObject("{}")).isEmpty())

        val payload = JSONObject(
            """
            {
              "commands": [
                {
                  "name": "compact",
                  "description": "Compact conversation history",
                  "insert_text": "/compact "
                },
                {
                  "name": "",
                  "description": "Invalid blank command"
                }
              ],
              "skills": [
                {
                  "name": "gpui-test",
                  "description": "Write GPUI tests in Zed",
                  "insert_text": "/gpui-test "
                },
                {
                  "name": "fallback-skill",
                  "description": "Missing insert_text falls back"
                }
              ]
            }
            """.trimIndent(),
        )

        val parsed = parseSkillSuggestions(payload)
        assertEquals(3, parsed.size)

        assertEquals(TriggerKind.Slash, parsed[0].triggerKind)
        assertEquals("Command", parsed[0].badge)
        assertEquals("/compact", parsed[0].label)
        assertEquals("Compact conversation history", parsed[0].subtitle)
        assertEquals("/compact ", parsed[0].insertText)

        assertEquals(TriggerKind.Slash, parsed[1].triggerKind)
        assertEquals("Skill", parsed[1].badge)
        assertEquals("/gpui-test", parsed[1].label)
        assertEquals("Write GPUI tests in Zed", parsed[1].subtitle)
        assertEquals("/gpui-test ", parsed[1].insertText)

        assertEquals(TriggerKind.Slash, parsed[2].triggerKind)
        assertEquals("Skill", parsed[2].badge)
        assertEquals("/fallback-skill", parsed[2].label)
        assertEquals("/fallback-skill ", parsed[2].insertText)
    }

    @Test
    fun parseSuggestionsIgnoresJsonNullValues() {
        val mentionPayload = JSONObject(
            """
            {
              "items": [
                {
                  "kind": null,
                  "label": null,
                  "path": "src/main.rs",
                  "insert_text": "@src/main.rs "
                },
                {
                  "kind": "file",
                  "label": "null-insert",
                  "path": "src/ignored.rs",
                  "insert_text": null
                }
              ]
            }
            """.trimIndent(),
        )
        val mentions = parseMentionSuggestions(mentionPayload)
        assertEquals(1, mentions.size)
        assertEquals("File", mentions[0].badge)
        assertEquals("src/main.rs", mentions[0].label)
        assertEquals("src/main.rs", mentions[0].subtitle)
        assertEquals("@src/main.rs ", mentions[0].insertText)

        val skillPayload = JSONObject(
            """
            {
              "commands": [
                {
                  "name": null,
                  "description": "ignored",
                  "insert_text": null
                },
                {
                  "name": "compact",
                  "description": null,
                  "insert_text": null
                }
              ],
              "skills": [
                {
                  "name": "gpui-test",
                  "description": null,
                  "insert_text": null
                }
              ]
            }
            """.trimIndent(),
        )
        val skills = parseSkillSuggestions(skillPayload)
        assertEquals(2, skills.size)
        assertEquals("/compact", skills[0].label)
        assertEquals("", skills[0].subtitle)
        assertEquals("/compact ", skills[0].insertText)
        assertEquals("/gpui-test", skills[1].label)
        assertEquals("", skills[1].subtitle)
        assertEquals("/gpui-test ", skills[1].insertText)
    }
}
