package dev.apptrackkot

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class VaultTest {
    // Machine-bound like the desktop's live-config test: reads John's synced ledger.
    private val live = File(System.getProperty("user.home"), "syncthing/apptrack/apptrack.toml")

    @Test
    fun liveLedgerParsesAndMatchesDesktopListShape() {
        assumeTrue("no synced ledger on this machine", live.isFile)
        val text = live.readText()
        val snap = Vault.parse(text, live.path, live.length())

        // Same headers the regex counter used to count.
        assertEquals(Regex("""(?m)^\[\[apps]]\s*$""").findAll(text).count(), snap.tracked)
        assertEquals(Regex("""(?m)^\[\[inbox\.nix]]\s*$""").findAll(text).count(), snap.nixInbox)
        assertTrue(snap.apps.all { it.raw.startsWith("[[apps]]") && "identity = \"${it.identity}\"" in it.raw })

        // Written in `apptrack list` layout so the shell can diff it against the desktop binary.
        val report = snap.categories.joinToString("\n") { c ->
            "%-28s U %3d   C %3d   A %3d".format(
                c,
                snap.count(c, Disposition.Using),
                snap.count(c, Disposition.Considering),
                snap.count(c, Disposition.Archived),
            )
        }
        File("build/vault-list.txt").writeText(report + "\n")
    }

    @Test
    fun multiInstallTakesHighestNumericVersion() {
        val snap = Vault.parse(
            """
            [[apps]]
            identity = "name:x"
            name = "x"
            category = "AI Agents"
            disposition = "using"
            tags = ["multi-install"]
            version = "0.9.0"

            [[apps.installations]]
            version = "0.154.0"

            [[apps.installations]]
            version = "0.84.2"
            """.trimIndent(),
            "",
            0,
        )
        assertEquals("0.154.0", snap.apps.single().version)
    }

    @Test(expected = LedgerException::class)
    fun missingDispositionFailsLoudly() {
        Vault.parse("[[apps]]\nidentity = \"name:y\"\nname = \"y\"\ncategory = \"Git\"\n", "", 0)
    }
}
