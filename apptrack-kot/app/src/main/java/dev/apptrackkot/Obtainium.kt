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
    /** Alphabetical, Uncategorized last (when any app has no category). */
    val categories: List<ObtainiumCategory>,
) {
    val installed: Int get() = apps.count { it.installedVersion != null }
    fun appsIn(category: String) = if (category == UNCATEGORIZED) {
        apps.filter { it.categories.isEmpty() }
    } else {
        apps.filter { category in it.categories }
    }

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
            // settings.categories is itself a JSON string: {"name": ARGB, …}
            nullableString(settings, "categories")?.let { encoded ->
                val table = JSONObject(encoded)
                for (key in table.keys()) colors[key] = table.getLong(key)
            }
        }
        val names = (colors.keys + apps.flatMap { it.categories })
            .distinct()
            .filter { name -> apps.any { name in it.categories } }
            .sortedWith(String.CASE_INSENSITIVE_ORDER)
        val categories = names.map { ObtainiumCategory(it, colors[it]) } +
            if (apps.any { it.categories.isEmpty() }) listOf(ObtainiumCategory(ObtainiumExport.UNCATEGORIZED, null)) else emptyList()
        return ObtainiumExport(file, bytes, apps, categories)
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

    /** optString turns JSON null into the text "null"; this doesn't. */
    private fun nullableString(o: JSONObject, key: String): String? =
        if (!o.has(key) || o.isNull(key)) null else o.getString(key)

    private val day = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm 'UTC'").withZone(ZoneOffset.UTC)

    fun formatMicros(micros: Long?): String =
        micros?.let { day.format(Instant.ofEpochMilli(it / 1000)) } ?: "not recorded"
}
