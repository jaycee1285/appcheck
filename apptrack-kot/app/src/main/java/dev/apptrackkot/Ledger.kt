package dev.apptrackkot

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

class LedgerException(message: String) : Exception(message)

/**
 * Reads and surgically edits `[[inbox.android]]` records keyed `identity = "android:<id>"`.
 * Every byte outside the edited record is preserved (comments, order, unknown fields),
 * and each edit is re-parsed and checked before it is returned for writing.
 */
object Ledger {
    const val HEADER = "[[inbox.android]]"

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

    /** Returns the new file text with this app's record created or updated; throws rather than write a bad file. */
    fun upsert(
        text: String,
        id: String,
        name: String,
        importedFrom: String,
        observedOn: String,
        overlay: Overlay,
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
