package dev.apptrackkot

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.SystemClock
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Divider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.SnapshotStateList
import androidx.compose.runtime.toMutableStateList
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.documentfile.provider.DocumentFile
import dev.apptrackkot.ui.AppTrackKotTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Always-dark app: force light system-bar icons regardless of the phone's theme.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        setContent { AppTrackKotApp() }
    }
}

@Composable
fun AppTrackKotApp() {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("apptrackkot", Context.MODE_PRIVATE) }
    val treeKey = "vault_tree_uri"

    var state by remember { mutableStateOf(VaultState.Unchecked) }
    var step by remember { mutableStateOf("0/5 · not started") }
    var detail by remember { mutableStateOf("") }
    var snapshot by remember { mutableStateOf<VaultSnapshot?>(null) }
    var tree by remember {
        mutableStateOf<Uri?>(prefs.getString(treeKey, null)?.let { Uri.parse(it) })
    }
    var attempt by remember { mutableStateOf(0) }
    var selected by rememberSaveable { mutableStateOf<String?>(null) }

    val chooseFolder = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { picked ->
        if (picked == null) {
            state = VaultState.Error
            step = "1/5 · folder permission"
            detail = "Folder selection cancelled. Tap Grant / retry to pick the Syncthing folder again."
            return@rememberLauncherForActivityResult
        }
        try {
            context.contentResolver.takePersistableUriPermission(
                picked,
                Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
            )
            prefs.edit().putString(treeKey, picked.toString()).apply()
            tree = picked
            attempt++
        } catch (e: Exception) {
            state = VaultState.Error
            detail = "Could not keep the folder permission:\n${e.stackTraceToString().take(1500)}"
        }
    }

    LaunchedEffect(tree, attempt) {
        val current = tree
        if (current == null) {
            state = VaultState.RequestingPermission
            step = "1/5 · folder permission"
            detail = "Grant global file access: pick the Syncthing folder that holds apptrack.toml."
        } else {
            load(context, current) { s, st, d, snap ->
                state = s; step = st; detail = d
                if (snap != null) snapshot = snap
            }
        }
    }

    AppTrackKotTheme {
        // Surface supplies the content color; bare Text defaults to black on the dark window.
        Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
            val snap = snapshot
            val app = selected?.let { id -> snap?.apps?.firstOrNull { it.identity == id } }
            BackHandler(enabled = app != null) { selected = null }
            if (app != null) {
                Detail(app, onBack = { selected = null })
            } else {
                Home(
                    state = state,
                    step = step,
                    detail = detail,
                    snapshot = snap,
                    onRetry = { chooseFolder.launch(null) },
                    onOpen = { selected = it.identity },
                )
            }
        }
    }
}

private suspend fun load(
    context: Context,
    tree: Uri,
    onResult: (VaultState, String, String, VaultSnapshot?) -> Unit,
) {
    suspend fun report(s: VaultState, step: String, d: String, snap: VaultSnapshot? = null) =
        withContext(Dispatchers.Main) { onResult(s, step, d, snap) }
    try {
        val started = SystemClock.elapsedRealtime()
        report(VaultState.Loading, "2/5 · open folder", "Opening Syncthing folder…")
        val result = withContext(Dispatchers.IO) {
            val root = DocumentFile.fromTreeUri(context, tree)
                ?: return@withContext "Folder unavailable. Reconnect the vault, then tap Grant / retry."
            val file = root.findFile("apptrack.toml")
                ?: return@withContext "apptrack.toml is not in this folder. Expected it at the root of the picked Syncthing folder."
            val located = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "3/5 · read", "Reading ${file.length()} bytes…")
            val text = context.contentResolver.openInputStream(file.uri)?.bufferedReader()?.use { it.readText() }
                ?: return@withContext "Could not read apptrack.toml. Reconnect the vault and retry."
            val read = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "4/5 · parse", "Parsing ${text.length} chars…")
            val snap = Vault.parse(text, tree.toString(), file.length())
            val parsed = SystemClock.elapsedRealtime()
            val timings = "locate ${located - started} ms · read ${read - located} ms · parse ${parsed - read} ms"
            report(VaultState.Loaded, "5/5 · ${snap.tracked} apps indexed", "$timings · ${snap.bytes} bytes", snap)
            null
        }
        if (result != null) report(VaultState.Error, "failed", result)
    } catch (e: Throwable) {
        report(VaultState.Error, "failed", e.stackTraceToString().take(2000))
    }
}

private val UsingColor = Color(0xFF6CC070)
private val ConsideringColor = Color(0xFFE0B84A)
private val ArchivedColor = Color(0xFFE06C6C)

private fun Disposition.color() = when (this) {
    Disposition.Using -> UsingColor
    Disposition.Considering -> ConsideringColor
    Disposition.Archived -> ArchivedColor
}

/** ui.rs fuzzy_score: substring position, else in-order character match scored past 1000. */
private fun fuzzyScore(query: String, text: String): Int? {
    val q = query.lowercase()
    val t = text.lowercase()
    val pos = t.indexOf(q)
    if (pos >= 0) return pos
    var score = 1000
    var from = 0
    for (c in q) {
        val i = t.indexOf(c, from)
        if (i < 0) return null
        score += i
        from = i + 1
    }
    return score
}

private sealed interface MapRow {
    data class Category(val name: String) : MapRow
    data class Group(val category: String, val disposition: Disposition) : MapRow
    data class App(val app: AppRecord) : MapRow
}

private val stringListSaver = listSaver<SnapshotStateList<String>, String>(
    save = { it.toList() },
    restore = { it.toMutableStateList() },
)

@Composable
fun Home(
    state: VaultState,
    step: String,
    detail: String,
    snapshot: VaultSnapshot?,
    onRetry: () -> Unit,
    onOpen: (AppRecord) -> Unit,
) {
    var query by rememberSaveable { mutableStateOf("") }
    val expanded = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }
    val archives = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }

    val rows: List<MapRow> = when {
        snapshot == null -> emptyList()
        query.isNotBlank() -> snapshot.apps
            .mapNotNull { a ->
                fuzzyScore(query, "${a.name} ${a.description} ${a.category} ${a.tags.joinToString(" ")}")
                    ?.let { it to a }
            }
            .sortedBy { it.first }
            .map { MapRow.App(it.second) }
        else -> buildList {
            for (category in snapshot.categories) {
                add(MapRow.Category(category))
                if (category !in expanded) continue
                for (d in Disposition.entries) {
                    add(MapRow.Group(category, d))
                    if (d == Disposition.Archived && category !in archives) continue
                    snapshot.apps.filter { it.category == category && it.disposition == d }
                        .forEach { add(MapRow.App(it)) }
                }
            }
        }
    }

    LazyColumn(
        modifier = Modifier
            .fillMaxSize()
            .systemBarsPadding()
            .imePadding()
            .padding(horizontal = 16.dp),
    ) {
        item {
            Text("AppTrack-KOT", fontSize = 28.sp, modifier = Modifier.padding(top = 16.dp))
            Text("smoke 3 · read-only viewer", style = MaterialTheme.typography.bodyMedium)
            Text(
                "state: ${state.label} · step $step",
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(top = 8.dp),
            )
            Text(detail, style = MaterialTheme.typography.bodySmall, maxLines = 30)
            if (state == VaultState.Loading) {
                LinearProgressIndicator(modifier = Modifier.fillMaxWidth().padding(top = 8.dp))
            }
            if (state == VaultState.Error || state == VaultState.RequestingPermission) {
                Button(onClick = onRetry, modifier = Modifier.padding(top = 12.dp)) { Text("Grant / retry") }
            }
        }
        if (snapshot != null) {
            item {
                Text(
                    "tracked ${snapshot.tracked} · U ${snapshot.count(null, Disposition.Using)} " +
                        "· C ${snapshot.count(null, Disposition.Considering)} " +
                        "· A ${snapshot.count(null, Disposition.Archived)} " +
                        "· android inbox ${snapshot.android} · nix inbox ${snapshot.nixInbox}",
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(top = 8.dp),
                )
                OutlinedTextField(
                    value = query,
                    onValueChange = { query = it },
                    singleLine = true,
                    placeholder = { Text("Search name, description, category, tags") },
                    modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
                )
                if (query.isNotBlank()) {
                    Text("${rows.size} matches", style = MaterialTheme.typography.bodySmall)
                }
            }
            items(rows, key = {
                when (it) {
                    is MapRow.Category -> "c:${it.name}"
                    is MapRow.Group -> "g:${it.category}:${it.disposition.key}"
                    is MapRow.App -> "a:${it.app.identity}"
                }
            }) { row ->
                when (row) {
                    is MapRow.Category -> CategoryRow(
                        name = row.name,
                        snapshot = snapshot,
                        open = row.name in expanded,
                        onToggle = { if (!expanded.remove(row.name)) expanded.add(row.name) },
                    )
                    is MapRow.Group -> GroupRow(
                        disposition = row.disposition,
                        count = snapshot.count(row.category, row.disposition),
                        foldable = row.disposition == Disposition.Archived,
                        open = row.category in archives,
                        onToggle = { if (!archives.remove(row.category)) archives.add(row.category) },
                    )
                    is MapRow.App -> AppRow(row.app, onClick = { onOpen(row.app) })
                }
            }
        }
    }
}

@Composable
private fun CategoryRow(name: String, snapshot: VaultSnapshot, open: Boolean, onToggle: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable(onClick = onToggle),
    ) {
        Text(if (open) "▾ " else "▸ ", style = MaterialTheme.typography.titleMedium)
        Text(name, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
        for (d in Disposition.entries) {
            Text(
                "${d.mark} ${snapshot.count(name, d)}",
                color = d.color(),
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.width(48.dp),
            )
        }
    }
}

@Composable
private fun GroupRow(disposition: Disposition, count: Int, foldable: Boolean, open: Boolean, onToggle: () -> Unit) {
    val label = "${disposition.label} ($count)" + if (foldable) (if (open) " ▾" else " ▸") else ""
    Text(
        label,
        color = disposition.color(),
        style = MaterialTheme.typography.labelLarge,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = if (foldable) 48.dp else 32.dp)
            .then(if (foldable) Modifier.clickable(onClick = onToggle) else Modifier)
            .padding(start = 20.dp, top = 8.dp),
    )
}

@Composable
private fun AppRow(app: AppRecord, onClick: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 56.dp)
            .clickable(onClick = onClick)
            .padding(start = 20.dp, top = 4.dp, bottom = 4.dp),
    ) {
        Text(
            app.disposition.mark,
            color = app.disposition.color(),
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.width(24.dp),
        )
        Column(modifier = Modifier.weight(1f)) {
            Text(app.name, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            if (app.description.isNotBlank()) {
                Text(
                    app.description,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.7f),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

@Composable
private fun Detail(app: AppRecord, onBack: () -> Unit) {
    val archivedBecause = when {
        app.archivedBecause.isNotEmpty() -> app.archivedBecause
        app.disposition == Disposition.Archived && app.review.isNotEmpty() -> app.review
        else -> "not recorded"
    }
    val fields = listOf(
        "disposition" to app.disposition.label,
        "category" to app.category,
        "tags" to app.tags.joinToString(", ").ifEmpty { "none" },
        "description" to app.description.ifEmpty { "none" },
        "ledger installed" to (app.installed?.toString() ?: "unknown"),
        "effective version" to app.version,
        "outcome" to app.outcome,
        "review" to app.review.ifEmpty { "not recorded" },
        "archived because" to archivedBecause,
        "source" to app.source,
        "repository" to (app.repo ?: "unknown"),
        "package" to (app.pkg ?: "unknown"),
        "installer" to (app.installer ?: "unknown"),
        "identity" to app.identity,
    )
    Column(
        modifier = Modifier
            .fillMaxSize()
            .systemBarsPadding()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp),
    ) {
        TextButton(onClick = onBack, modifier = Modifier.heightIn(min = 48.dp)) { Text("← Back") }
        Text(app.name, fontSize = 26.sp)
        Text(app.disposition.label, color = app.disposition.color(), style = MaterialTheme.typography.titleMedium)
        Divider(modifier = Modifier.padding(vertical = 12.dp))
        for ((label, value) in fields) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.6f))
            Text(value, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(bottom = 8.dp))
        }
        Divider(modifier = Modifier.padding(vertical = 12.dp))
        Text("complete ledger record (including evidence/history)", style = MaterialTheme.typography.labelMedium)
        Text(
            app.raw,
            fontFamily = FontFamily.Monospace,
            fontSize = 12.sp,
            modifier = Modifier.padding(top = 8.dp, bottom = 24.dp),
        )
    }
}

private val VaultState.label: String get() = when (this) {
    VaultState.Unchecked -> "unchecked"
    VaultState.RequestingPermission -> "asking"
    VaultState.Loading -> "loading"
    VaultState.Loaded -> "loaded"
    VaultState.Error -> "error"
}
