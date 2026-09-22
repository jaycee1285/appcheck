package dev.apptrackkot

import java.time.Instant
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter
import org.json.JSONArray
import org.json.JSONObject

enum class VaultState {
    Unchecked,
    RequestingPermission,
    Loading,
    Loaded,
    Error,
}

/** One entry of an Obtainium export's `apps` array, as the 2026-09-20 export writes it. */
data class ObtainiumApp(
    val id: String,
    val name: String,
    val author: String,
    val url: String,
    val installedVersion: String?,
    val latestVersion: String?,
    /** Microseconds since the epoch in the export, not milliseconds. */
    val releaseDateMicros: Long?,
    val lastUpdateCheckMicros: Long?,
    val pinned: Boolean,
    val categories: List<String>,
    /** File names from the `apkUrls` JSON string ([[name, url], …]). */
    val apkNames: List<String>,
    /** `additionalSettings` JSON string, flattened to key = value. */
    val settings: List<Pair<String, String>>,
    val changeLog: String?,
    /** The app's own export object, pretty-printed. */
    val raw: String,
)

data class ObtainiumCategory(val name: String, val argb: Long?)

data class ObtainiumExport(
    val file: String,
    val bytes: Long,
    val apps: List<ObtainiumApp>,
    /** Every category Obtainium knows with its colour, whether or not an app uses it. */
    val colors: Map<String, Long>,
) {
    val installed: Int get() = apps.count { it.installedVersion != null }

    /** Alphabetical (Obtainium's own order, pages/apps.dart), Uncategorized last. */
    val categories: List<ObtainiumCategory> by lazy {
        (colors.keys + apps.flatMap { it.categories })
            .distinct()
            .filter { name -> apps.any { name in it.categories } }
            .sortedWith(String.CASE_INSENSITIVE_ORDER)
            .map { ObtainiumCategory(it, colors[it]) } +
            if (apps.any { it.categories.isEmpty() }) listOf(ObtainiumCategory(UNCATEGORIZED, null)) else emptyList()
    }

    /** Every category name that can be assigned (Obtainium's own list plus any in use). */
    val assignable: List<String> by lazy {
        (colors.keys + apps.flatMap { it.categories }).distinct().sortedWith(String.CASE_INSENSITIVE_ORDER)
    }

    fun appsIn(category: String) = if (category == UNCATEGORIZED) {
        apps.filter { it.categories.isEmpty() }
    } else {
        apps.filter { category in it.categories }
    }

    /** The same export with pending category edits applied, for display. */
    fun withPending(pending: Map<String, List<String>>): ObtainiumExport =
        if (pending.isEmpty()) this
        else copy(apps = apps.map { a -> pending[a.id]?.let { a.copy(categories = it) } ?: a })

    companion object {
        const val UNCATEGORIZED = "Uncategorized"
    }
}

class ExportException(message: String) : Exception(message)

object Obtainium {
    private val exportName = Regex("""^obtainium-export-.*\.json$""")

    /** Export names embed an ISO timestamp, so the lexically greatest is the newest. */
    fun newest(names: List<String>): String? = names.filter { exportName.matches(it) }.maxOrNull()

    fun parse(text: String, file: String, bytes: Long): ObtainiumExport {
        val root = JSONObject(text)
        val array = root.optJSONArray("apps") ?: throw ExportException("$file has no \"apps\" array.")
        val apps = (0 until array.length()).map { i -> app(array.getJSONObject(i), i, file) }

        val colors = HashMap<String, Long>()
        root.optJSONObject("settings")?.let { settings ->
            // settings.categories is a JSON string in schema v1 and v2; tolerate a plain object.
            val table = settings.optJSONObject("categories")
                ?: nullableString(settings, "categories")?.let { JSONObject(it) }
            if (table != null) for (key in table.keys()) colors[key] = table.getLong(key)
        }
        return ObtainiumExport(file, bytes, apps, colors)
    }

    private fun app(o: JSONObject, index: Int, file: String): ObtainiumApp {
        fun required(key: String) = nullableString(o, key)
            ?: throw ExportException("$file apps[$index] has no $key; refusing to invent one.")
        val categories = o.optJSONArray("categories")?.let { a -> (0 until a.length()).map { a.getString(it) } } ?: emptyList()
        val apkNames = nullableString(o, "apkUrls")?.let { encoded ->
            val pairs = JSONArray(encoded)
            (0 until pairs.length()).map { pairs.getJSONArray(it).getString(0) }
        } ?: emptyList()
        val settings = nullableString(o, "additionalSettings")?.let { encoded ->
            val table = JSONObject(encoded)
            table.keys().asSequence().map { it to table.get(it).toString() }.toList()
        } ?: emptyList()
        return ObtainiumApp(
            id = required("id"),
            name = required("name"),
            author = nullableString(o, "author") ?: "",
            url = nullableString(o, "url") ?: "",
            installedVersion = nullableString(o, "installedVersion"),
            latestVersion = nullableString(o, "latestVersion"),
            releaseDateMicros = if (o.isNull("releaseDate")) null else o.getLong("releaseDate"),
            lastUpdateCheckMicros = if (o.isNull("lastUpdateCheck")) null else o.getLong("lastUpdateCheck"),
            pinned = o.optBoolean("pinned", false),
            categories = categories,
            apkNames = apkNames,
            settings = settings,
            changeLog = nullableString(o, "changeLog"),
            raw = o.toString(2),
        )
    }

    /** The file the phone writes for John to import back into Obtainium. */
    const val IMPORT_FILE = "obtainium-import.json"

    /** Schema the installed Obtainium writes and accepts (apps_provider_import_export.dart). */
    private const val SCHEMA_VERSION = 2

    /**
     * An apps-only import document: each app's own export record re-emitted with
     * `categories` replaced. No `settings` block, so importing cannot overwrite
     * Obtainium's preferences (_applyImportedSettings writes back every key it finds).
     */
    fun importDocument(edits: Map<String, List<String>>, export: ObtainiumExport, exportedAt: String): String {
        val byId = export.apps.associateBy { it.id }
        val apps = JSONArray()
        for ((id, categories) in edits.entries.sortedBy { it.key }) {
            val app = byId[id] ?: throw ExportException("$id is not in ${export.file}; refusing to invent a record.")
            val record = JSONObject(app.raw)
            record.put("categories", JSONArray(categories))
            apps.put(record)
        }
        val document = JSONObject()
            .put("schemaVersion", SCHEMA_VERSION)
            .put("exportedAt", exportedAt)
            .put("apps", apps)
            .toString(2)
        verifyImport(document, edits, export)
        return document
    }

    /** Every field except `categories` must survive byte-identically. */
    private fun verifyImport(document: String, edits: Map<String, List<String>>, export: ObtainiumExport) {
        val written = JSONObject(document).optJSONArray("apps")
            ?: throw ExportException("Refusing to write: the import document has no apps array.")
        if (written.length() != edits.size) {
            throw ExportException("Refusing to write: ${written.length()} records for ${edits.size} edits.")
        }
        val byId = export.apps.associateBy { it.id }
        for (i in 0 until written.length()) {
            val record = written.getJSONObject(i)
            val id = record.getString("id")
            val source = JSONObject(byId.getValue(id).raw)
            val wantCategories = edits.getValue(id)
            val got = record.getJSONArray("categories").let { a -> (0 until a.length()).map { a.getString(it) } }
            if (got != wantCategories) {
                throw ExportException("Refusing to write: $id categories came out as $got, expected $wantCategories.")
            }
            val keys = source.keys().asSequence().toSet()
            if (keys != record.keys().asSequence().toSet()) {
                throw ExportException("Refusing to write: $id field set changed.")
            }
            for (key in keys - "categories") {
                if (record.get(key).toString() != source.get(key).toString()) {
                    throw ExportException("Refusing to write: $id field \"$key\" changed.")
                }
            }
        }
    }

    /** Reads back the import file, dropping edits the export has already caught up with. */
    fun pending(text: String, export: ObtainiumExport): Map<String, List<String>> {
        val apps = JSONObject(text).optJSONArray("apps") ?: return emptyMap()
        val byId = export.apps.associateBy { it.id }
        val found = LinkedHashMap<String, List<String>>()
        for (i in 0 until apps.length()) {
            val record = apps.getJSONObject(i)
            val id = nullableString(record, "id") ?: continue
            val categories = record.optJSONArray("categories")
                ?.let { a -> (0 until a.length()).map { a.getString(it) } } ?: emptyList()
            val current = byId[id] ?: continue
            if (categories != current.categories) found[id] = categories
        }
        return found
    }

    /** optString turns JSON null into the text "null"; this doesn't. */
    private fun nullableString(o: JSONObject, key: String): String? =
        if (!o.has(key) || o.isNull(key)) null else o.getString(key)

    private val day = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm 'UTC'").withZone(ZoneOffset.UTC)

    fun formatMicros(micros: Long?): String =
        micros?.let { day.format(Instant.ofEpochMilli(it / 1000)) } ?: "not recorded"
}
