package dev.apptrackkot

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class LedgerTest {
    // Read-only: the live synced ledger is copied into memory, never written.
    private val live = File(System.getProperty("user.home"), "syncthing/apptrack/apptrack.toml")
    private val export = "obtainium-export-2026-09-20T20-26-55.248005.json"

    @Test
    fun appendThenUpdateOnTheLiveLedgerTouchesOnlyOneRecord() {
        assumeTrue("no synced ledger on this machine", live.isFile)
        val original = live.readText()
        assertEquals(emptyMap<String, Overlay>(), Ledger.overlays(original))

        val first = Overlay(Disposition.Using, "Chromium fork; \"daily\" browser\nsecond line", "")
        val appended = Ledger.upsert(original, "io.github.jqssun.helium", "Titanium", export, "2026-09-20", first)
        assertTrue(appended.startsWith(original))
        assertEquals(first, Ledger.overlays(appended)["io.github.jqssun.helium"])

        val archived = Overlay(Disposition.Archived, "Chromium fork", "replaced by Helium \\ Vanadium")
        val updated = Ledger.upsert(appended, "io.github.jqssun.helium", "Titanium", export, "2026-09-20", archived)
        assertEquals(1, Ledger.overlays(updated).size)
        assertEquals(archived, Ledger.overlays(updated)["io.github.jqssun.helium"])
        assertTrue(updated.startsWith(original))

        val second = Overlay(Disposition.Considering, "", "")
        val two = Ledger.upsert(updated, "com.zariep.opennavbar.debug", "OpenNavbar", export, "2026-09-20", second)
        assertEquals(2, Ledger.overlays(two).size)
        assertEquals(archived, Ledger.overlays(two)["io.github.jqssun.helium"])

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
}
