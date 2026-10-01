package dev.apptrackkot

import org.json.JSONArray
import org.tomlj.Toml
import org.tomlj.TomlParseResult

/** The desktop's three dispositions (ledger.rs); an app with none is unclassified. */
enum class Disposition(val key: String, val label: String, val mark: String) {
    Using("using", "Using", "U"),
    Considering("considering", "Considering", "C"),
    Archived("archived", "Archived", "A"),
}

/** What John has recorded about one Android app in apptrack.toml's [[inbox.android]]. */
data class Overlay(
    val disposition: Disposition? = null,
    val description: String = "",
    val archivedBecause: String = "",
)

data class DesktopApp(
    val identity: String = "",
    val name: String = "",
    val url: String = "",
    val description: String = "",
    val category: String = "",
    val disposition: Disposition = Disposition.Considering,
    val tags: String = "",
)

/** The last row data observed while an Android app was still in the export. */
data class DisplaySnapshot(
    val name: String,
    val author: String = "",
    val categories: List<String> = emptyList(),
    val installedVersion: String? = null,
    val latestVersion: String? = null,
    val pinned: Boolean = false,
) {
    companion object {
        fun from(app: ObtainiumApp) = DisplaySnapshot(
            app.name, app.author, app.categories, app.installedVersion, app.latestVersion, app.pinned,
        )
    }

    fun asApp(id: String) = ObtainiumApp(
        id, name, author, "", installedVersion, latestVersion, null, null, pinned, categories,
        emptyList(), emptyList(), null, "",
    )
}

class LedgerException(message: String) : Exception(message)

/**
 * Reads and surgically edits `[[inbox.android]]` records keyed `identity = "android:<id>"`.
 * Every byte outside the edited record is preserved (comments, order, unknown fields),
 * and each edit is re-parsed and checked before it is returned for writing.
 */
object Ledger {
    const val HEADER = "[[inbox.android]]"

    fun desktopApps(text: String): List<DesktopApp> {
        val apps = parse(text).getArray("apps") ?: return emptyList()
        return (0 until apps.size()).map { index ->
            val row = apps.getTable(index)
            val identity = row.getString("identity") ?: throw LedgerException("[[apps]] row $index has no identity")
            val disposition = row.getString("disposition")?.let { value ->
                Disposition.entries.firstOrNull { it.key == value }
                    ?: throw LedgerException("[[apps]] $identity has unknown disposition $value")
            } ?: Disposition.Considering
            DesktopApp(
                identity = identity,
                name = row.getString("name") ?: "",
                url = identity.takeUnless { it.startsWith("name:") } ?: "",
                description = row.getString("description") ?: "",
                category = row.getString("category") ?: "",
                disposition = disposition,
                tags = row.getArray("tags")?.let { values ->
                    (0 until values.size()).mapNotNull { values.getString(it) }.joinToString(", ")
                } ?: "",
            )
        }
    }

    /** Edit only top-level [[apps]] fields; nested recipe and receipts remain byte-identical. */
    fun upsertDesktop(text: String, originalIdentity: String?, edit: DesktopApp): String {
        val name = edit.name.trim()
        val category = edit.category.trim()
        if (name.isEmpty() || category.isEmpty()) throw LedgerException("Name and category are required.")
        val url = edit.url.trim().trimEnd('/').removeSuffix(".git")
        val identity = if (url.isEmpty()) "name:" + name.lowercase().split(Regex("\\s+")).joinToString("-")
            else if (url.startsWith("https://github.com/")) "https://github.com/" + url.removePrefix("https://github.com/").lowercase()
            else url
        val before = desktopApps(text)
        val old = originalIdentity?.let { key -> before.singleOrNull { it.identity == key }
            ?: throw LedgerException("Desktop record changed or disappeared; reload first.") }
        if (before.any { it.identity == identity && it.identity != originalIdentity })
            throw LedgerException("Already tracked: $identity")
        if (old != null && old.identity != identity) {
            val table = parse(text).getArray("apps")!!.let { apps ->
                apps.getTable(before.indexOfFirst { it.identity == originalIdentity })
            }
            val provenance = table.getTable("provenance")
            if (table.getTable("recipe") != null || table.getTable("nix_migration") != null ||
                provenance?.getBoolean("managed_by_apptrack") == true ||
                provenance?.getString("source") !in listOf(null, "unknown", "manual"))
                throw LedgerException("This URL owns reviewed install evidence. Change it on desktop after reviewing its route.")
        }
        val lines = text.split("\n").toMutableList()
        val sections = lines.indices.filter { lines[it].trim() == "[[apps]]" }
        val recordIndex = before.indexOfFirst { it.identity == originalIdentity }
        val start = if (old == null) lines.size else sections.getOrNull(recordIndex)
            ?: throw LedgerException("Could not find the desktop record block.")
        val end = if (old == null) start else (start + 1 until lines.size).firstOrNull {
            tableHeader.matches(lines[it].trim())
        } ?: lines.size
        val keys = listOf(
            "identity" to identity, "name" to name, "category" to category,
            "description" to edit.description.trim(), "disposition" to edit.disposition.key,
        )
        val tags = edit.tags.split(',').map(String::trim).filter(String::isNotEmpty)
        val tagLine = "tags = [" + tags.joinToString(", ") { "\"${escape(it)}\"" } + "]"
        if (old == null) {
            if (lines.lastOrNull() == "") lines.removeAt(lines.lastIndex)
            lines += ""
            lines += "[[apps]]"
            lines += keys.map { (key, value) -> line(key, value) }
            lines += "outcome = \"unknown\""
            lines += tagLine
            lines += ""
        } else {
            var finish = end
            for ((key, value) in keys) {
                val at = (start + 1 until finish).firstOrNull { lines[it].trimStart().startsWith("$key =") ||
                    Regex("^${Regex.escape(key)}\\s*=").containsMatchIn(lines[it].trimStart()) }
                if (at != null) lines[at] = line(key, value)
                else { lines.add(finish, line(key, value)); finish++ }
            }
            val at = (start + 1 until finish).firstOrNull { Regex("^tags\\s*=").containsMatchIn(lines[it].trimStart()) }
            if (at != null) lines[at] = tagLine else lines.add(finish, tagLine)
        }
        if (old != null && old.identity != identity) {
            val headers = lines.indices.filter { lines[it].trim() == "[[apps]]" }
            val own = headers.getOrNull(recordIndex) ?: throw LedgerException("Desktop record moved unexpectedly.")
            val next = headers.getOrNull(recordIndex + 1) ?: lines.size
            val provenance = (own + 1 until next).firstOrNull { lines[it].trim() == "[apps.provenance]" }
            if (provenance != null) {
                val finish = (provenance + 1 until next).firstOrNull { tableHeader.matches(lines[it].trim()) } ?: next
                val repoLine = (provenance + 1 until finish).firstOrNull { Regex("^repo\\s*=").containsMatchIn(lines[it].trimStart()) }
                if (identity.startsWith("https://")) {
                    if (repoLine != null) lines[repoLine] = line("repo", identity)
                    else lines.add(finish, line("repo", identity))
                } else if (repoLine != null) lines.removeAt(repoLine)
            }
        }
        val result = lines.joinToString("\n")
        val after = desktopApps(result)
        if (after.size != before.size + if (old == null) 1 else 0) throw LedgerException("Desktop record count changed unexpectedly.")
        val saved = after.singleOrNull { it.identity == identity } ?: throw LedgerException("Desktop record did not read back.")
        if (saved.name != name || saved.category != category || saved.description != edit.description.trim() ||
            saved.disposition != edit.disposition || saved.tags != tags.joinToString(", "))
            throw LedgerException("Desktop record did not read back as entered.")
        return result
    }

    fun parse(text: String): TomlParseResult {
        val toml = Toml.parse(text)
        if (toml.hasErrors()) {
            throw LedgerException("apptrack.toml does not parse:\n" + toml.errors().take(5).joinToString("\n"))
        }
        return toml
    }

    fun overlays(text: String): Map<String, Overlay> {
        val records = parse(text).getArray(listOf("inbox", "android")) ?: return emptyMap()
        val found = HashMap<String, Overlay>()
        for (i in 0 until records.size()) {
            val t = records.getTable(i)
            val identity = t.getString("identity") ?: continue
            if (!identity.startsWith("android:")) continue
            val key = t.getString("disposition")
            found[identity.removePrefix("android:")] = Overlay(
                disposition = key?.let { k ->
                    Disposition.entries.firstOrNull { it.key == k }
                        ?: throw LedgerException("[[inbox.android]] $identity has unknown disposition \"$k\".")
                },
                description = t.getString("description") ?: "",
                archivedBecause = t.getString("archived_because") ?: "",
            )
        }
        return found
    }

    fun snapshots(text: String): Map<String, DisplaySnapshot> {
        val records = parse(text).getArray(listOf("inbox", "android")) ?: return emptyMap()
        val found = HashMap<String, DisplaySnapshot>()
        for (i in 0 until records.size()) {
            val t = records.getTable(i)
            val identity = t.getString("identity") ?: continue
            if (!identity.startsWith("android:")) continue
            val categories = t.getString("display_categories")?.let { encoded ->
                val array = JSONArray(encoded)
                (0 until array.length()).map { array.getString(it) }
            } ?: emptyList()
            found[identity.removePrefix("android:")] = DisplaySnapshot(
                name = t.getString("display_name") ?: t.getString("name") ?: identity.removePrefix("android:"),
                author = t.getString("display_author") ?: "",
                categories = categories,
                installedVersion = t.getString("display_installed_version")?.ifEmpty { null },
                latestVersion = t.getString("display_latest_version")?.ifEmpty { null },
                pinned = t.getString("display_pinned") == "true",
            )
        }
        return found
    }

    fun unlistedApps(export: ObtainiumExport, snapshots: Map<String, DisplaySnapshot>): List<ObtainiumApp> {
        val listed = export.apps.mapTo(HashSet()) { it.id }
        return snapshots.filterKeys { it !in listed }
            .map { (id, snapshot) -> snapshot.asApp(id) }
            .sortedWith(compareBy(String.CASE_INSENSITIVE_ORDER) { it.name })
    }

    /** Keep the last displayed row current while it is listed, before a later export can omit it. */
    fun refreshSnapshots(text: String, export: ObtainiumExport): String {
        val records = parse(text).getArray(listOf("inbox", "android")) ?: return text
        val byId = export.apps.associateBy { it.id }
        val existing = overlays(text)
        val oldSnapshots = snapshots(text)
        var result = text
        for (i in 0 until records.size()) {
            val t = records.getTable(i)
            val identity = t.getString("identity") ?: continue
            if (!identity.startsWith("android:")) continue
            val id = identity.removePrefix("android:")
            val app = byId[id] ?: continue
            val snapshot = DisplaySnapshot.from(app)
            if (t.getString("display_categories") != null && oldSnapshots[id] == snapshot) continue
            result = upsert(result, id, app.name, export.file, Obtainium.observedOn(export.file),
                existing.getValue(id), snapshot)
        }
        return result
    }

    /** Returns the new file text with this app's record created or updated; throws rather than write a bad file. */
    fun upsert(
        text: String,
        id: String,
        name: String,
        importedFrom: String,
        observedOn: String,
        overlay: Overlay,
        snapshot: DisplaySnapshot? = null,
    ): String {
        val lines = text.split("\n").toMutableList()
        val identityLine = Regex("""^identity\s*=\s*"${Regex.escape(escape("android:$id"))}"\s*$""")
        val block = findBlock(lines) { identityLine.matches(it.trim()) }

        val edited = mutableListOf<Pair<String, String>>()
        overlay.disposition?.let { edited += "disposition" to it.key }
        edited += "description" to overlay.description
        if (overlay.disposition == Disposition.Archived || overlay.archivedBecause.isNotEmpty()) {
            edited += "archived_because" to overlay.archivedBecause
        }
        snapshot?.let {
            edited += "display_name" to it.name
            edited += "display_author" to it.author
            edited += "display_categories" to JSONArray(it.categories).toString()
            edited += "display_installed_version" to (it.installedVersion ?: "")
            edited += "display_latest_version" to (it.latestVersion ?: "")
            edited += "display_pinned" to it.pinned.toString()
        }

        val start: Int
        val oldEnd: Int
        val newEnd: Int
        if (block == null) {
            // Append a new record at the end, keeping the file's trailing newline.
            val trailing = if (lines.last().isEmpty()) lines.removeAt(lines.lastIndex) else null
            start = lines.size
            oldEnd = start
            val record = listOf(
                "",
                HEADER,
                line("identity", "android:$id"),
                line("name", name),
                line("imported_from", importedFrom),
                line("observed_on", observedOn),
            ) + edited.map { (k, v) -> line(k, v) }
            lines.addAll(record)
            newEnd = lines.size
            if (trailing != null) lines.add(trailing)
        } else {
            start = block.first
            oldEnd = block.last + 1
            var end = oldEnd
            for ((key, value) in edited) {
                val keyLine = Regex("""^${Regex.escape(key)}\s*=""")
                val at = (start until end).firstOrNull { keyLine.containsMatchIn(lines[it].trim()) }
                if (at != null) {
                    lines[at] = line(key, value)
                } else {
                    // After the record's last key line, before any trailing blanks/comments.
                    val last = (start until end).last { lines[it].isNotBlank() && !lines[it].trimStart().startsWith("#") }
                    lines.add(last + 1, line(key, value))
                    end++
                }
            }
            newEnd = end
        }
        val result = lines.joinToString("\n")
        verify(text, result, id, overlay, start, oldEnd, newEnd)
        return result
    }

    private fun line(key: String, value: String) = "$key = \"${escape(value)}\""

    /** TOML basic-string escaping. */
    fun escape(value: String): String = buildString {
        for (c in value) {
            when {
                c == '\\' -> append("\\\\")
                c == '"' -> append("\\\"")
                c == '\n' -> append("\\n")
                c == '\t' -> append("\\t")
                c == '\r' -> append("\\r")
                c.code < 0x20 || c.code == 0x7F -> append("\\u%04X".format(c.code))
                else -> append(c)
            }
        }
    }

    private val tableHeader = Regex("""^\[\[?[^\[\]]+]]?\s*(#.*)?$""")

    /** Line range of the [[inbox.android]] block whose lines satisfy [matches], if any. */
    private fun findBlock(lines: List<String>, matches: (String) -> Boolean): IntRange? {
        var i = 0
        while (i < lines.size) {
            if (lines[i].trim() == HEADER) {
                var end = i + 1
                while (end < lines.size && !tableHeader.matches(lines[end].trim())) end++
                if ((i + 1 until end).any { matches(lines[it]) }) return i until end
                i = end
            } else {
                i++
            }
        }
        return null
    }

    private fun verify(old: String, new: String, id: String, overlay: Overlay, start: Int, oldEnd: Int, newEnd: Int) {
        val before = parse(old)
        val after = parse(new)
        fun count(t: TomlParseResult, vararg path: String) = t.getArray(path.toList())?.size() ?: 0
        check(count(after, "apps") == count(before, "apps")) { "[[apps]] count changed" }
        check(count(after, "inbox", "nix") == count(before, "inbox", "nix")) { "[[inbox.nix]] count changed" }
        val androidDelta = count(after, "inbox", "android") - count(before, "inbox", "android")
        check(androidDelta == 0 || androidDelta == 1) { "[[inbox.android]] count changed by $androidDelta" }
        val readBack = overlays(new)[id]
        check(readBack == overlay) { "record reads back as $readBack, expected $overlay" }
        val o = old.split("\n")
        val n = new.split("\n")
        check(o.subList(0, start) == n.subList(0, start)) { "lines before the record changed" }
        check(o.subList(oldEnd, o.size) == n.subList(newEnd, n.size)) { "lines after the record changed" }
    }

    private fun check(ok: Boolean, what: () -> String) {
        if (!ok) throw LedgerException("Refusing to write apptrack.toml: ${what()}.")
    }
}
