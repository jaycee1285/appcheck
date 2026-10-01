package dev.apptrackkot

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class LedgerTest {
    @Test
    fun desktopEditKeepsManagedTablesAndNewRecordReadsOnDesktopContract() {
        val original = """schema_version = 1
[[apps]]
identity = "https://github.com/owner/tool"
name = "Tool"
category = "Utilities"
description = "old"
disposition = "considering"
[apps.recipe]
source = "github"
asset = "tool-linux.tar.gz"
[apps.provenance]
source = "github"
managed_by_apptrack = true
"""
        val edited = Ledger.upsertDesktop(original, "https://github.com/owner/tool",
            DesktopApp(name = "Tool two", url = "https://github.com/owner/tool", category = "Tools",
                description = "edited", disposition = Disposition.Using, tags = "cli, rust"))
        assertTrue("[apps.recipe]\nsource = \"github\"\nasset = \"tool-linux.tar.gz\"" in edited)
        assertTrue("[apps.provenance]\nsource = \"github\"\nmanaged_by_apptrack = true" in edited)
        assertEquals("Tool two", Ledger.desktopApps(edited).single().name)
        val added = Ledger.upsertDesktop(edited, null, DesktopApp(name = "Phone find", url = "https://github.com/o/new",
            category = "Tools", description = "found on phone"))
        assertEquals(2, Ledger.desktopApps(added).size)
        assertTrue("outcome = \"unknown\"" in added)
    }
    // Read-only: the live synced ledger is copied into memory, never written.
    private val live = File(System.getProperty("user.home"), "syncthing/apptrack/apptrack.toml")
    private val export = "obtainium-export-2026-09-20T20-26-55.248005.json"

    @Test
    fun appendThenUpdateOnTheLiveLedgerTouchesOnlyOneRecord() {
        assumeTrue("no synced ledger on this machine", live.isFile)
        val original = live.readText()
        val existing = Ledger.overlays(original)
        val firstId = "dev.apptrackkot.fixture.first"
        val secondId = "dev.apptrackkot.fixture.second"
        assertTrue(firstId !in existing && secondId !in existing)

        val first = Overlay(Disposition.Using, "Chromium fork; \"daily\" browser\nsecond line", "")
        val appended = Ledger.upsert(original, firstId, "Titanium", export, "2026-09-20", first)
        assertTrue(appended.startsWith(original))
        assertEquals(existing + (firstId to first), Ledger.overlays(appended))

        val archived = Overlay(Disposition.Archived, "Chromium fork", "replaced by Helium \\ Vanadium")
        val updated = Ledger.upsert(appended, firstId, "Titanium", export, "2026-09-20", archived)
        assertEquals(existing.size + 1, Ledger.overlays(updated).size)
        assertEquals(existing + (firstId to archived), Ledger.overlays(updated))
        assertTrue(updated.startsWith(original))

        val second = Overlay(Disposition.Considering, "", "")
        val two = Ledger.upsert(updated, secondId, "OpenNavbar", export, "2026-09-20", second)
        assertEquals(existing.size + 2, Ledger.overlays(two).size)
        assertEquals(existing + (firstId to archived) + (secondId to second), Ledger.overlays(two))

        // For the shell: the desktop binary must still load these and list the same apps.
        File("build/ledger-appended.toml").writeText(appended)
        File("build/ledger-two.toml").writeText(two)
        File("build/ledger-two-tail.txt").writeText(two.removePrefix(original))
    }

    @Test
    fun updatePreservesCommentsAndUnknownKeysInsideTheRecord() {
        val text = """
            schema_version = 1

            [[inbox.android]]
            identity = "android:a.b"
            # John's own note
            name = "B"
            future_field = "keep me"
            disposition = "considering"

            [[inbox.nix]]
            name = "x"
        """.trimIndent() + "\n"
        val out = Ledger.upsert(text, "a.b", "B", export, "2026-09-20", Overlay(Disposition.Archived, "d", "why"))
        assertTrue("# John's own note" in out)
        assertTrue("future_field = \"keep me\"" in out)
        assertTrue(out.endsWith("[[inbox.nix]]\nname = \"x\"\n"))
        assertEquals(Overlay(Disposition.Archived, "d", "why"), Ledger.overlays(out)["a.b"])
    }

    @Test
    fun escapeRoundTripsThroughAParser() {
        val nasty = "quote \" backslash \\ tab \t newline \n bell \u0007 é"
        val out = Ledger.upsert("", "x", "X", export, "2026-09-20", Overlay(null, nasty, ""))
        assertEquals(nasty, Ledger.overlays(out)["x"]?.description)
    }

    @Test
    fun savedAndroidRecordSurvivesWhenAppLeavesTheExport() {
        val saved = Overlay(Disposition.Archived, "Tried it on the phone", "Removed from ObtainX")
        val ledger = Ledger.upsert("schema_version = 1\n", "gone.app", "Gone", export, "2026-09-20", saved)
        val laterExport = Obtainium.parse(
            """{"apps":[{"id":"still.app","name":"Still"}]}""", "later.json", 0,
        )

        assertTrue(laterExport.apps.none { it.id == "gone.app" })
        assertEquals(saved, Ledger.overlays(ledger)["gone.app"])

        val afterAnotherSave = Ledger.upsert(
            ledger, "still.app", "Still", "later.json", "2026-09-30",
            Overlay(Disposition.Using, "Still listed", ""),
        )
        assertEquals(saved, Ledger.overlays(afterAnotherSave)["gone.app"])
    }

    @Test
    fun missingListingKeepsTheCapturedRowAndCategory() {
        val original = Ledger.upsert(
            "schema_version = 1\n", "gone.app", "Old name", export, "2026-09-20",
            Overlay(Disposition.Using, "My note", ""),
        )
        val listed = Obtainium.parse(
            """{"apps":[{"id":"gone.app","name":"Current name","author":"Maker","installedVersion":"1","latestVersion":"2","pinned":true,"categories":["Tools","Daily"]}]}""",
            "current.json", 0,
        )
        val captured = Ledger.refreshSnapshots(original, listed)
        assertEquals(Ledger.overlays(original), Ledger.overlays(captured))
        assertEquals("Current name", Ledger.snapshots(captured).getValue("gone.app").name)
        assertEquals(listOf("Tools", "Daily"), Ledger.snapshots(captured).getValue("gone.app").categories)

        val later = Obtainium.parse("""{"apps":[]}""", "later.json", 0)
        val unlisted = Ledger.unlistedApps(later, Ledger.snapshots(captured)).single()
        assertEquals("Current name", unlisted.name)
        assertEquals("Maker", unlisted.author)
        assertEquals("1", unlisted.installedVersion)
        assertEquals("2", unlisted.latestVersion)
        assertEquals(listOf("Tools", "Daily"), unlisted.categories)
        assertTrue(unlisted.pinned)
        assertTrue(Ledger.unlistedApps(listed, Ledger.snapshots(captured)).isEmpty())

        val recategorized = listed.copy(apps = listed.apps.map { it.copy(categories = listOf("Later")) })
        val refreshed = Ledger.refreshSnapshots(captured, recategorized)
        assertEquals(listOf("Later"), Ledger.snapshots(refreshed).getValue("gone.app").categories)
        assertEquals(Ledger.overlays(original), Ledger.overlays(refreshed))
    }

    @Test
    fun currentAndroidLedgerCanCaptureRowsWithoutChangingNotes() {
        val source = File(System.getProperty("user.home"), "syncthing/obtainx-export-2026-09-28T14-31-47.675840.json")
        assumeTrue("no synced ledger or ObtainX export on this machine", live.isFile && source.isFile)
        val original = live.readText()
        val listed = Obtainium.parse(source.readText(), source.name, source.length())
        val captured = Ledger.refreshSnapshots(original, listed)

        assertEquals(Ledger.overlays(original), Ledger.overlays(captured))
        assertEquals(Ledger.overlays(original).keys, Ledger.snapshots(captured).keys)
        assertEquals(captured, Ledger.refreshSnapshots(captured, listed))
    }
}
