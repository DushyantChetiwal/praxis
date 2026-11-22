package io.github.dushyantchetiwal.praxis.remote.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class HostFoldersTest {
    @Test
    fun parseHostFoldersParsesTargetsAndRemainsBackwardCompatible() {
        val withTargets = parseHostFolders(
            JSONObject(
                """{
                    "host": "Debian",
                    "target": "remote",
                    "targets": [
                        {"id": "local", "label": "This PC"},
                        {"id": "remote", "label": "Debian"}
                    ],
                    "path": "/home/me",
                    "parent": "/home",
                    "folders": [{"name": "project", "path": "/home/me/project"}],
                    "next_offset": null
                }"""
            )
        )
        assertEquals("Debian", withTargets.host)
        assertEquals("remote", withTargets.target)
        assertEquals(
            listOf(HostTarget("local", "This PC"), HostTarget("remote", "Debian")),
            withTargets.targets,
        )
        assertEquals(listOf(HostFolder("project", "/home/me/project")), withTargets.folders)

        val legacy = parseHostFolders(
            JSONObject(
                """{
                    "host": "windows",
                    "path": "C:\\Users\\me",
                    "parent": "C:\\Users",
                    "folders": [],
                    "next_offset": null
                }"""
            )
        )
        assertEquals("", legacy.target)
        assertEquals(emptyList<HostTarget>(), legacy.targets)
    }

    @Test
    fun resolveFolderTargetSwitchesBetweenLocalAndRemoteCleanly() {
        val targets = listOf(HostTarget("local", "This PC"), HostTarget("remote", "Debian"))
        val remoteListing = HostFolders(
            host = "Debian",
            path = "/home/me",
            parent = "/home",
            folders = emptyList(),
            nextOffset = null,
            target = "remote",
            targets = targets,
        )
        val localListing = HostFolders(
            host = "windows",
            path = """C:\Users\me""",
            parent = """C:\Users""",
            folders = emptyList(),
            nextOffset = null,
            target = "local",
            targets = targets,
        )

        // Explicit chip switch wins over current state.
        assertEquals("local", resolveFolderTarget("local", "remote", "", remoteListing))
        assertEquals("remote", resolveFolderTarget("remote", "local", "", localListing))

        // Typing a Windows drive path while browsing WSL auto-targets local PC.
        assertEquals(
            "local",
            resolveFolderTarget(null, "remote", """C:\Users\me\Projects""", remoteListing),
        )
        // Normal WSL subfolder preserves remote target.
        assertEquals(
            "remote",
            resolveFolderTarget(null, "remote", "/home/me/project", remoteListing),
        )
        // Typing a Unix path while on local Windows with a WSL target switches to remote.
        assertEquals(
            "remote",
            resolveFolderTarget(null, "local", "/home/me/project", localListing),
        )
        // Fresh dialog open omits target so desktop picks based on the selected window.
        assertNull(resolveFolderTarget(null, null, "", null))
    }
}
