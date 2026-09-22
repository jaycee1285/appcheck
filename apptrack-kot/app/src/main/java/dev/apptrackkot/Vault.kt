package dev.apptrackkot

import org.tomlj.Toml
import org.tomlj.TomlTable

enum class VaultState {
    Unchecked,
    RequestingPermission,
    Loading,
    Loaded,
    Error,
}

/** The desktop's three tracked dispositions (ledger.rs), in map order. */
enum class Disposition(val key: String, val label: String, val mark: String) {
    Using("using", "Using", "U"),
    Considering("considering", "Considering", "C"),
    Archived("archived", "Archived", "A"),
}

data class AppRecord(
    val identity: String,
    val name: String,
    val category: String,
    val description: String,
    val tags: List<String>,
    val disposition: Disposition,
    val installed: Boolean?,
    val outcome: String,
    val version: String,
    val review: String,
    val archivedBecause: String,
    val source: String,
    val repo: String?,
    val pkg: String?,
    val installer: String?,
    /** The record's exact text in apptrack.toml, including evidence and history. */
    val raw: String,
)

data class VaultSnapshot(
    val path: String,
    val bytes: Long,
    val apps: List<AppRecord>,
    /** First-seen order, as the desktop map shows them. */
    val categories: List<String>,
    val android: Int,
    val nixInbox: Int,
) {
    val tracked: Int get() = apps.size
    fun count(category: String?, disposition: Disposition) =
        apps.count { (category == null || it.category == category) && it.disposition == disposition }
}

class LedgerException(message: String) : Exception(message)

object Vault {
    fun parse(text: String, path: String, bytes: Long): VaultSnapshot {
        val toml = Toml.parse(text)
        if (toml.hasErrors()) {
            throw LedgerException(
                "apptrack.toml does not parse:\n" + toml.errors().take(5).joinToString("\n") { it.toString() },
            )
        }
        val tables = toml.getArray("apps")
        val raws = rawRecords(text)
        val count = tables?.size() ?: 0
        if (raws.size != count) {
            throw LedgerException("Found $count [[apps]] tables but ${raws.size} [[apps]] headers; refusing to guess.")
        }
        val apps = (0 until count).map { i -> record(tables!!.getTable(i), raws[i], i) }
        return VaultSnapshot(
            path = path,
            bytes = bytes,
            apps = apps,
            categories = apps.map { it.category }.distinct(),
            android = toml.getArray(listOf("inbox", "android"))?.size() ?: 0,
            nixInbox = toml.getArray(listOf("inbox", "nix"))?.size() ?: 0,
        )
    }

    private fun record(t: TomlTable, raw: String, index: Int): AppRecord {
        fun required(key: String) = t.getString(key)
            ?: throw LedgerException("[[apps]] #${index + 1} (${t.getString("name") ?: "unnamed"}) has no $key.")
        val tags = t.getArray("tags")?.toList()?.map { it.toString() } ?: emptyList()
        val dispositionKey = required("disposition")
        val provenance = t.getTable("provenance")
        return AppRecord(
            identity = required("identity"),
            name = required("name"),
            category = required("category"),
            description = t.getString("description") ?: "",
            tags = tags,
            disposition = Disposition.entries.firstOrNull { it.key == dispositionKey }
                ?: throw LedgerException("[[apps]] #${index + 1} has unknown disposition \"$dispositionKey\"."),
            installed = t.getBoolean("installed"),
            outcome = t.getString("outcome") ?: "unknown",
            version = effectiveVersion(t, tags),
            review = t.getString("review") ?: "",
            archivedBecause = t.getString("archived_because") ?: "",
            source = provenance?.getString("source") ?: "unknown",
            repo = provenance?.getString("repo"),
            pkg = provenance?.getString("package"),
            installer = provenance?.getString("installer"),
            raw = raw,
        )
    }

    /** doctor.rs effective_version: multi-install takes the highest numeric version. */
    private fun effectiveVersion(t: TomlTable, tags: List<String>): String {
        val own = t.getString("version")
        if ("multi-install" !in tags) return own ?: "unknown"
        val installations = t.getArray("installations")
        val versions = listOfNotNull(own) + (0 until (installations?.size() ?: 0))
            .mapNotNull { installations!!.getTable(it).getString("version") }
        return versions.sortedWith { a, b ->
            val left = numericVersion(a)
            val right = numericVersion(b)
            if (left != null && right != null) compareVersions(left, right) else 0
        }.lastOrNull() ?: "unknown"
    }

    private fun numericVersion(value: String): List<ULong>? {
        val v = value.removePrefix("v")
        if (v.isEmpty()) return null
        return v.split('.').map { it.toULongOrNull() ?: return null }
    }

    private fun compareVersions(a: List<ULong>, b: List<ULong>): Int {
        for (i in 0 until minOf(a.size, b.size)) {
            val c = a[i].compareTo(b[i])
            if (c != 0) return c
        }
        return a.size.compareTo(b.size)
    }

    private val header = Regex("""^\s*\[\[?\s*([A-Za-z0-9_.\-]+)\s*]]?""")

    /** Split the file text into each [[apps]] record's own lines (nested apps.* tables included). */
    private fun rawRecords(text: String): List<String> {
        val records = ArrayList<String>()
        var current: StringBuilder? = null
        for (line in text.lines()) {
            val name = header.find(line)?.groupValues?.get(1)
            if (name != null && line.trimStart().startsWith("[[") && name == "apps") {
                current?.let { records.add(it.toString().trimEnd()) }
                current = StringBuilder()
            } else if (name != null && name != "apps" && !name.startsWith("apps.")) {
                current?.let { records.add(it.toString().trimEnd()) }
                current = null
            }
            current?.append(line)?.append('\n')
        }
        current?.let { records.add(it.toString().trimEnd()) }
        return records
    }
}
