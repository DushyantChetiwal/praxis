package io.github.dushyantchetiwal.praxis.remote.data

import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.ModelOverride
import io.github.dushyantchetiwal.praxis.remote.ModelsState
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

class ModelSelectionTest {
    @Test
    fun parseModelsDeduplicatesRecommendedAndProviderEntriesWithoutLosingProviderLabel() {
        val payload = JSONObject()
            .put("current", "google/gemini-4-neon")
            .put("next_offset", 100)
            .put(
                "available",
                JSONArray()
                    .put(
                        JSONObject()
                            .put("id", "google/gemini-4-neon")
                            .put("name", "Gemini 4 Neon")
                            .put("group", "Recommended")
                            .put("disabled", false),
                    )
                    .put(
                        JSONObject()
                            .put("id", "google/gemini-4-neon")
                            .put("name", "Gemini 4 Neon")
                            .put("group", "Google")
                            .put("disabled", false),
                    )
                    .put(
                        JSONObject()
                            .put("id", "google/gemini-4-hydrogen")
                            .put("name", "Gemini 4 Hydrogen")
                            .put("group", "Google")
                            .put("disabled", false),
                    ),
            )

        val parsed = parseModels(payload)
        assertEquals(2, parsed.available.size)
        assertEquals("google/gemini-4-neon", parsed.available[0].id)
        assertEquals("Recommended · Google", parsed.available[0].group)
        assertEquals("google/gemini-4-hydrogen", parsed.available[1].id)
        assertEquals("Google", parsed.available[1].group)

        // Appending a later page that repeats a Recommended model also merges its provider group cleanly.
        val page1 = parseModels(
            JSONObject()
                .put("current", "openrouter/qwen/qwen3-coder")
                .put("next_offset", 1)
                .put(
                    "available",
                    JSONArray().put(
                        JSONObject()
                            .put("id", "openrouter/qwen/qwen3-coder")
                            .put("name", "Qwen3 Coder")
                            .put("group", "Recommended"),
                    ),
                ),
        )
        val page2 = parseModels(
            JSONObject()
                .put("current", "openrouter/qwen/qwen3-coder")
                .put(
                    "available",
                    JSONArray().put(
                        JSONObject()
                            .put("id", "openrouter/qwen/qwen3-coder")
                            .put("name", "Qwen3 Coder")
                            .put("group", "OpenRouter"),
                    ),
                ),
        )
        val combined = page1.append(page2)
        assertEquals(1, combined.available.size)
        assertEquals("Recommended · OpenRouter", combined.available.single().group)
    }

    @Test
    fun modelOverrideKeepsConfirmedSelectionVisibleEvenBeforeDesktopStatusCatchesUp() {
        val summary = ThreadSummary(
            sessionId = "session-1",
            title = "Task",
            status = "idle",
            queued = 0,
            mode = null,
            pending = emptyList(),
            model = "google/gemini-4-carbon",
            modelName = "Gemini 4 Carbon",
            modelSelection = true,
        )
        val status = Status(
            device = "Laptop",
            windows = listOf(
                WindowInfo(
                    id = 1L,
                    projects = listOf("GUI ACP"),
                    active = true,
                    thread = summary,
                    architect = null,
                ),
            ),
        )
        val ui = DeviceUi(
            status = status,
            windowId = 1L,
            modelOverride = ModelOverride(
                session = "session-1",
                id = "google/gemini-4-hydrogen",
                name = "Gemini 4 Hydrogen",
            ),
            models = ModelsState(
                session = "session-1",
                info = ModelInfo(
                    current = "google/gemini-4-hydrogen",
                    available = listOf(
                        ModelOption("google/gemini-4-carbon", "Gemini 4 Carbon", "Google", false),
                        ModelOption("google/gemini-4-hydrogen", "Gemini 4 Hydrogen", "Google", false),
                    ),
                ),
            ),
        )

        assertEquals("google/gemini-4-hydrogen", ui.currentModelId())
        assertEquals("Gemini 4 Hydrogen", ui.currentModelName())
    }
}
