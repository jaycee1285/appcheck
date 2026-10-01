package dev.apptrackkot

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assume.assumeTrue
import org.junit.Test

class ObtainiumTest {
    // Machine-bound: John's synced Syncthing root, where Obtainium exports.
    private val root = File(System.getProperty("user.home"), "syncthing")

    @Test
    fun newestPicksTheSeptemberExportAndIgnoresOtherJson() {
        assertEquals(
            "obtainium-export-2026-09-20T20-26-55.248005.json",
            Obtainium.newest(
                listOf(
                    "obtainium-export-2026-06-03T16-43-20.892830.json",
                    "notes.json",
                    "obtainium-export-2026-09-20T20-26-55.248005.json",
                    "obtainium-export-2026-09-20T20-26-55.248005.json.tmp",
                ),
            ),
        )
        assertNull(Obtainium.newest(listOf("apptrack.toml")))
    }

    @Test
    fun newestPrefersObtainxAndExtractsItsDate() {
        val obtainx = "obtainx-export-2026-09-28T14-31-47.675840.json"
        assertEquals(
            obtainx,
            Obtainium.newest(listOf(
                "obtainium-export-2026-09-28T18-12-36.836929-auto.json",
                "obtainx-export-2026-09-20T20-26-55.248005.json",
                obtainx,
                "$obtainx.tmp",
            )),
        )
        assertEquals("2026-09-28", Obtainium.observedOn(obtainx))
        assertEquals("2026-09-28", Obtainium.observedOn("obtainium-export-2026-09-28T18-12-36.836929-auto.json"))
    }

    @Test
    fun obtainxExportParsesToThePhoneProjection() {
        val file = File(root, "obtainx-export-2026-09-28T14-31-47.675840.json")
        assumeTrue("no synced ObtainX export on this machine", file.isFile)
        val export = Obtainium.parse(file.readText(), file.name, file.length())
        assertEquals(104, export.apps.size)
        assertEquals(49, export.installed)
        assertEquals(14, export.assignable.size)
    }

    @Test
    fun liveExportParsesToThePhoneProjection() {
        val file = File(root, "obtainium-export-2026-09-20T20-26-55.248005.json")
        assumeTrue("no synced export on this machine", file.isFile)
        val export = Obtainium.parse(file.readText(), file.name, file.length())

        // Written for the shell to diff against jq over the same file.
        val report = buildString {
            appendLine("apps ${export.apps.size}")
            appendLine("installed ${export.installed}")
            for (c in export.categories) appendLine("${c.name}|${export.appsIn(c.name).size}|${c.argb ?: "-"}")
            for (a in export.apps) {
                appendLine("${a.id}|${a.installedVersion ?: "null"}|${a.latestVersion ?: "null"}|${a.apkNames.size}|${a.settings.size}|${Obtainium.formatMicros(a.releaseDateMicros)}")
            }
        }
        File("build/obtainium-report.txt").writeText(report)
    }

    @Test
    fun jsonNullStaysNullNotTheWordNull() {
        val export = Obtainium.parse(
            """{"apps":[{"id":"a.b","name":"B","author":"x","url":"u","installedVersion":null,
               "latestVersion":"1","apkUrls":"[]","additionalSettings":"{}","categories":[],
               "releaseDate":null,"lastUpdateCheck":1789939383627321,"pinned":false,"changeLog":null}],
               "settings":{}}""",
            "t.json",
            0,
        )
        val app = export.apps.single()
        assertNull(app.installedVersion)
        assertNull(app.changeLog)
        assertEquals("not recorded", Obtainium.formatMicros(app.releaseDateMicros))
        assertEquals("2026-09-20 21:23 UTC", Obtainium.formatMicros(app.lastUpdateCheckMicros))
        assertEquals(listOf("Uncategorized"), export.categories.map { it.name })
    }

    @Test(expected = ExportException::class)
    fun missingIdFailsLoudly() {
        Obtainium.parse("""{"apps":[{"name":"no id"}],"settings":{}}""", "t.json", 0)
    }
}
