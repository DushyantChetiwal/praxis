package io.github.dushyantchetiwal.praxis.remote.data

import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.StopTarget
import io.github.dushyantchetiwal.praxis.remote.Tab
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ChatNavigationTest {
    @Test
    fun secondaryPagesReturnToChatBeforeLeavingTheComputer() {
        assertNull(DeviceUi(tab = Tab.Chat).parentTab())
        assertEquals(Tab.Chat, DeviceUi(tab = Tab.Threads).parentTab())
        assertEquals(Tab.Chat, DeviceUi(tab = Tab.Files).parentTab())
    }

    @Test
    fun stopRemainsAvailableWithoutOpeningPlanDetails() {
        val summary = ThreadSummary("current", "Current chat", "generating", 0, null, emptyList())
        val plan = Architect(3, true, "Build", 2, null)
        assertEquals(StopTarget.Agent, device(summary).stopTarget())
        assertEquals(StopTarget.Plan, device(summary, plan).stopTarget())
        assertEquals(StopTarget.Plan, device(summary.copy(status = "idle"), plan).stopTarget())
        assertNull(device(summary.copy(status = "idle"), plan.copy(running = false)).stopTarget())
        assertNull(DeviceUi().stopTarget())
    }

    @Test
    fun compactHeaderNeverLabelsANewConversationWithAnOldSnapshot() {
        val summary = ThreadSummary("current", "Current chat", "idle", 0, null, emptyList())
        val previous = ThreadView("previous", "Old chat", "generating", 0, emptyList())
        val ui = device(summary).copy(history = TranscriptHistory().live(previous))
        assertEquals("Current chat", ui.conversationTitle())
        assertFalse(ui.isGenerating())
        assertNull(ui.stopTarget())
        val missingWindow = ui.copy(windowId = 2)
        assertNull(missingWindow.conversationTitle())
        assertNull(missingWindow.stopTarget())
    }

    @Test
    fun matchingSnapshotsStillSupplyLiveTitlesAndActivity() {
        val summary = ThreadSummary("current", "Earlier title", "idle", 0, null, emptyList())
        val current = ThreadView("current", "Live title", "generating", 0, emptyList())
        val ui = device(summary).copy(history = TranscriptHistory().live(current))
        assertEquals("Live title", ui.conversationTitle())
        assertTrue(ui.isGenerating())
        assertEquals(StopTarget.Agent, ui.stopTarget())
        assertEquals("Earlier title", ui.copy(history = TranscriptHistory().live(current.copy(title = " "))).conversationTitle())
    }

    private fun device(summary: ThreadSummary, plan: Architect? = null): DeviceUi = DeviceUi(
        windowId = 1,
        status = Status("Computer", listOf(WindowInfo(1, listOf("Project"), true, summary, plan))),
    )
}
