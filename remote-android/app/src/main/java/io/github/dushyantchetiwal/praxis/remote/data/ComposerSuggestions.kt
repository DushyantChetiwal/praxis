package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject

enum class TriggerKind {
    Mention,
    Slash,
}

data class ActiveTrigger(
    val kind: TriggerKind,
    val query: String,
    val tokenStart: Int,
    val tokenEnd: Int,
)

data class ComposerSuggestion(
    val id: String,
    val triggerKind: TriggerKind,
    val badge: String,
    val label: String,
    val subtitle: String,
    val insertText: String,
)

data class ReplacedComposerText(
    val text: String,
    val newCursorIndex: Int,
)

data class ComposerSuggestionsState(
    val trigger: ActiveTrigger? = null,
    val loading: Boolean = false,
    val items: List<ComposerSuggestion> = emptyList(),
)

fun detectComposerTrigger(text: String, cursorIndex: Int = text.length): ActiveTrigger? {
    val end = cursorIndex.coerceIn(0, text.length)
    if (end == 0 || text[end - 1].isWhitespace()) {
        return null
    }

    var tokenStart = end
    while (tokenStart > 0 && !text[tokenStart - 1].isWhitespace()) {
        tokenStart--
    }

    val prefix = text[tokenStart]
    return when (prefix) {
        '@' -> {
            // tokenStart is 0 or immediately after whitespace, so emails like user@example.com
            // have tokenStart at 'u' and never match here.
            ActiveTrigger(
                kind = TriggerKind.Mention,
                query = text.substring(tokenStart + 1, end),
                tokenStart = tokenStart,
                tokenEnd = end,
            )
        }

        '/' -> {
            // Only trigger slash commands at the start of the first line (with optional leading spaces).
            val onlyLeadingSpaces = (0 until tokenStart).all { text[it] == ' ' }
            if (!onlyLeadingSpaces) {
                null
            } else {
                ActiveTrigger(
                    kind = TriggerKind.Slash,
                    query = text.substring(tokenStart + 1, end),
                    tokenStart = tokenStart,
                    tokenEnd = end,
                )
            }
        }

        else -> null
    }
}

fun applyComposerSuggestion(
    text: String,
    trigger: ActiveTrigger,
    suggestion: ComposerSuggestion,
): ReplacedComposerText {
    val start = trigger.tokenStart.coerceIn(0, text.length)
    val end = trigger.tokenEnd.coerceIn(start, text.length)
    val before = text.substring(0, start)
    val after = text.substring(end)

    val followedByWhitespace = after.firstOrNull()?.isWhitespace() == true
    val inserted = if (followedByWhitespace) {
        suggestion.insertText.trimEnd(' ')
    } else if (suggestion.insertText.endsWith(' ')) {
        suggestion.insertText
    } else {
        "${suggestion.insertText} "
    }

    return ReplacedComposerText(
        text = before + inserted + after,
        newCursorIndex = start + inserted.length,
    )
}

fun parseMentionSuggestions(json: JSONObject?): List<ComposerSuggestion> {
    val items = json?.optJSONArray("items") ?: return emptyList()
    val result = ArrayList<ComposerSuggestion>(items.length())
    for (index in 0 until items.length()) {
        val item = items.optJSONObject(index) ?: continue
        val kind = item.optString("kind", "").trim()
        val label = item.optString("label", "").trim()
        val path = item.optString("path", "").trim()
        val insertText = item.optString("insert_text", "").takeIf { it.isNotBlank() } ?: continue
        if (label.isEmpty() && path.isEmpty()) continue

        val badge = if (kind.equals("symbol", ignoreCase = true)) "Symbol" else "File"
        val displayLabel = label.ifEmpty { path }
        result.add(
            ComposerSuggestion(
                id = "mention:$kind:$displayLabel:$path:$index",
                triggerKind = TriggerKind.Mention,
                badge = badge,
                label = displayLabel,
                subtitle = path,
                insertText = insertText,
            ),
        )
    }
    return result
}

fun parseSkillSuggestions(json: JSONObject?): List<ComposerSuggestion> {
    if (json == null) return emptyList()
    val result = mutableListOf<ComposerSuggestion>()

    val commands = json.optJSONArray("commands")
    if (commands != null) {
        for (index in 0 until commands.length()) {
            val command = commands.optJSONObject(index) ?: continue
            val name = command.optString("name", "").trim()
            if (name.isEmpty()) continue
            val description = command.optString("description", "").trim()
            val insertText = command.optString("insert_text", "").takeIf { it.isNotBlank() } ?: "/$name "
            result.add(
                ComposerSuggestion(
                    id = "command:$name:$index",
                    triggerKind = TriggerKind.Slash,
                    badge = "Command",
                    label = "/$name",
                    subtitle = description,
                    insertText = insertText,
                ),
            )
        }
    }

    val skills = json.optJSONArray("skills")
    if (skills != null) {
        for (index in 0 until skills.length()) {
            val skill = skills.optJSONObject(index) ?: continue
            val name = skill.optString("name", "").trim()
            if (name.isEmpty()) continue
            val description = skill.optString("description", "").trim()
            val insertText = skill.optString("insert_text", "").takeIf { it.isNotBlank() } ?: "/$name "
            result.add(
                ComposerSuggestion(
                    id = "skill:$name:$index",
                    triggerKind = TriggerKind.Slash,
                    badge = "Skill",
                    label = "/$name",
                    subtitle = description,
                    insertText = insertText,
                ),
            )
        }
    }

    return result
}
