package dev.apptrackkot

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class ImportDocumentTest {
    private val root = File(System.getProperty("user.home"), "syncthing")
    private val newest = "obtainium-export-2026-09-22T15-42-18.294658.json"

    private fun liveExport(): ObtainiumExport {
        val file = File(root, newest)
        assumeTrue("no synced export on this machine", file.isFile)
        return Obtainium.parse(file.readText(), file.name, file.length())
    }

    @Test
    fun newestPrefersTheSeptember22Export() {
        assertEquals(
            newest,
            Obtainium.newest(
                listOf(
                    "obtainium-export-2026-06-03T16-43-20.892830.json",
                    "obtainium-export-2026-09-20T20-26-55.248005.json",
                    newest,
                    Obtainium.IMPORT_FILE,
                ),
            ),
        )
        // The file we write must never be mistaken for an export.
        assertNull(Obtainium.newest(listOf(Obtainium.IMPORT_FILE)))
    }

    @Test
    fun importDocumentCarriesOnlyEditedAppsAndChangesOnlyCategories() {
        val export = liveExport()
        val id = "io.github.jqssun.helium"
        val edits = mapOf(id to listOf("internet", "AI"))
        val document = Obtainium.importDocument(edits, export, "2026-09-22T16:00:00.000000")

        val root = JSONObject(document)
        assertEquals(2, root.getInt("schemaVersion"))
        assertTrue(root.has("exportedAt"))
        // No settings block: Obtainium's import would write every settings key back into prefs.
        assertTrue(!root.has("settings"))
        val apps = root.getJSONArray("apps")
        assertEquals(1, apps.length())

        val written = apps.getJSONObject(0)
        val source = JSONObject(export.apps.first { it.id == id }.raw)
        assertEquals(listOf("internet", "AI"), written.getJSONArray("categories").let { a -> (0 until a.length()).map { a.getString(it) } })
        for (key in source.keys().asSequence().toSet() - "categories") {
            assertEquals("field $key", source.get(key).toString(), written.get(key).toString())
        }

        File("build/obtainium-import-sample.json").writeText(document)
    }

    @Test
    fun pendingDropsEditsTheExportHasCaughtUpWith() {
        val export = liveExport()
        val id = "io.github.jqssun.helium"
        val current = export.apps.first { it.id == id }.categories

        val stillPending = Obtainium.importDocument(mapOf(id to listOf("editors")), export, "t")
        assertEquals(mapOf(id to listOf("editors")), Obtainium.pending(stillPending, export))

        // Same categories as the export: nothing left to import.
        val applied = Obtainium.importDocument(mapOf(id to current), export, "t")
        assertEquals(emptyMap<String, List<String>>(), Obtainium.pending(applied, export))
    }

    @Test
    fun displayShowsPendingCategories() {
        val export = liveExport()
        val id = "io.github.jqssun.helium"
        val display = export.withPending(mapOf(id to listOf("games")))
        assertEquals(listOf("games"), display.apps.first { it.id == id }.categories)
        assertTrue(display.appsIn("games").any { it.id == id })
        assertEquals(export.apps.size, display.apps.size)
    }

    @Test(expected = ExportException::class)
    fun refusesAnIdThatIsNotInTheExport() {
        Obtainium.importDocument(mapOf("not.a.real.app" to listOf("games")), liveExport(), "t")
    }
}
