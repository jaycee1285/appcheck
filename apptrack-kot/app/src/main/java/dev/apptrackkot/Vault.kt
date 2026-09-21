package dev.apptrackkot

import java.io.File

data class VaultSnapshot(
    val path: String,
    val bytes: Long,
    val tracked: Int,
    val android: Int,
    val nixInbox: Int,
)

enum class VaultState {
    Unchecked,
    RequestingPermission,
    Loading,
    Loaded,
    Error,
}

object Vault {
    val candidatePaths: List<String>
        get() {
            val roots = ArrayList<String>()
            roots.add("/storage/emulated/0")
            File("/storage").listFiles()?.forEach { roots.add(it.absolutePath) }
            val names = listOf(
                "syncthing/apptrack/apptrack.toml",
                "syncthing/syncthing/apptrack/apptrack.toml",
            )
            return roots.flatMap { root -> names.map { "$root/$it" } }
        }

    fun locate(): Pair<VaultState, String> {
        for (path in candidatePaths) {
            val file = File(path)
            if (file.isFile && file.canRead()) {
                return VaultState.Loaded to path
            }
        }
        return VaultState.Error to candidatePaths.joinToString("\n")
    }

    fun parse(text: String): VaultSnapshot {
        val tracked = Regex("""(?m)^\[\[apps\]\]\s*$""").findAll(text).count()
        val android = Regex("""(?m)^\[\[inbox\.android\]\]\s*$""").findAll(text).count()
        val nix = Regex("""(?m)^\[\[inbox\.nix\]\]\s*$""").findAll(text).count()
        return VaultSnapshot(
            path = "",
            bytes = text.length.toLong(),
            tracked = tracked,
            android = android,
            nixInbox = nix,
        )
    }
}