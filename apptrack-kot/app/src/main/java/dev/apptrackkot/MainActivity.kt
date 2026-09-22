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
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
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
import android.provider.DocumentsContract
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
    var export by remember { mutableStateOf<ObtainiumExport?>(null) }
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
            detail = "Grant global file access: pick the Syncthing root folder, where Obtainium writes obtainium-export-*.json."
        } else {
            load(context, current) { s, st, d, loaded ->
                state = s; step = st; detail = d
                if (loaded != null) export = loaded
            }
        }
    }

    AppTrackKotTheme {
        // Surface supplies the content color; bare Text defaults to black on the dark window.
        Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
            val current = export
            val app = selected?.let { id -> current?.apps?.firstOrNull { it.id == id } }
            BackHandler(enabled = app != null) { selected = null }
            if (app != null && current != null) {
                Detail(app, current, onBack = { selected = null })
            } else {
                Home(
                    state = state,
                    step = step,
                    detail = detail,
                    export = current,
                    onRetry = { chooseFolder.launch(null) },
                    onOpen = { selected = it.id },
                )
            }
        }
    }
}

private suspend fun load(
    context: Context,
    tree: Uri,
    onResult: (VaultState, String, String, ObtainiumExport?) -> Unit,
) {
    suspend fun report(s: VaultState, step: String, d: String, loaded: ObtainiumExport? = null) =
        withContext(Dispatchers.Main) { onResult(s, step, d, loaded) }
    try {
        val started = SystemClock.elapsedRealtime()
        report(VaultState.Loading, "2/5 · open folder", "Opening Syncthing folder…")
        val failure = withContext(Dispatchers.IO) {
            // One children query (name, id, size, type) instead of a provider round-trip per file.
            val files = listChildren(context, tree)
            val name = Obtainium.newest(files.keys.toList())
                ?: return@withContext "No obtainium-export-*.json among ${files.size} files in this folder. " +
                    "Tap Grant / retry and pick the Syncthing root (syncthing/syncthing), where Obtainium exports."
            val (uri, size) = files.getValue(name)
            val located = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "3/5 · read", "Reading $name ($size bytes)…")
            val text = context.contentResolver.openInputStream(uri)?.bufferedReader()?.use { it.readText() }
                ?: return@withContext "Could not read $name. Reconnect the folder and retry."
            val read = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "4/5 · parse", "Parsing $name…")
            val loaded = Obtainium.parse(text, name, size)
            val parsed = SystemClock.elapsedRealtime()
            report(
                VaultState.Loaded,
                "5/5 · ${loaded.apps.size} apps indexed",
                "$name · ${loaded.bytes} bytes · locate ${located - started} ms · read ${read - located} ms · parse ${parsed - read} ms",
                loaded,
            )
            null
        }
        if (failure != null) report(VaultState.Error, "failed", failure)
    } catch (e: Throwable) {
        report(VaultState.Error, "failed", e.stackTraceToString().take(2000))
    }
}

/** Regular files directly inside the granted folder: display name → (document uri, size). */
private fun listChildren(context: Context, tree: Uri): Map<String, Pair<Uri, Long>> {
    val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
    val projection = arrayOf(
        DocumentsContract.Document.COLUMN_DOCUMENT_ID,
        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_SIZE,
        DocumentsContract.Document.COLUMN_MIME_TYPE,
    )
    val found = HashMap<String, Pair<Uri, Long>>()
    context.contentResolver.query(children, projection, null, null, null)?.use { c ->
        while (c.moveToNext()) {
            if (c.getString(3) == DocumentsContract.Document.MIME_TYPE_DIR) continue
            val name = c.getString(1) ?: continue
            found[name] = DocumentsContract.buildDocumentUriUsingTree(tree, c.getString(0)) to
                (if (c.isNull(2)) 0L else c.getLong(2))
        }
    } ?: throw ExportException("The folder listing returned nothing. Reconnect the folder and retry.")
    return found
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

private sealed interface ListRow {
    data class Category(val category: ObtainiumCategory) : ListRow
    data class App(val app: ObtainiumApp, val under: String) : ListRow
}

private val stringListSaver = listSaver<SnapshotStateList<String>, String>(
    save = { it.toList() },
    restore = { it.toMutableStateList() },
)

private val Muted = Color(0xFF8A8F98)

private fun ObtainiumCategory.color(): Color = argb?.let { Color(it) } ?: Muted

@Composable
fun Home(
    state: VaultState,
    step: String,
    detail: String,
    export: ObtainiumExport?,
    onRetry: () -> Unit,
    onOpen: (ObtainiumApp) -> Unit,
) {
    var query by rememberSaveable { mutableStateOf("") }
    val expanded = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }

    val rows: List<ListRow> = when {
        export == null -> emptyList()
        query.isNotBlank() -> export.apps
            .mapNotNull { a ->
                fuzzyScore(query, "${a.name} ${a.author} ${a.id} ${a.categories.joinToString(" ")}")
                    ?.let { it to a }
            }
            .sortedBy { it.first }
            .map { ListRow.App(it.second, "search") }
        else -> buildList {
            for (category in export.categories) {
                add(ListRow.Category(category))
                if (category.name in expanded) {
                    export.appsIn(category.name).forEach { add(ListRow.App(it, category.name)) }
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
            Text("smoke 4 · Obtainium projection", style = MaterialTheme.typography.bodyMedium)
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
        if (export != null) {
            item {
                Text(
                    "${export.apps.size} apps · ${export.installed} installed · ${export.categories.size} categories",
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(top = 8.dp),
                )
                OutlinedTextField(
                    value = query,
                    onValueChange = { query = it },
                    singleLine = true,
                    placeholder = { Text("Search name, author, package id, category") },
                    modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
                )
                if (query.isNotBlank()) {
                    Text("${rows.size} matches", style = MaterialTheme.typography.bodySmall)
                }
            }
            items(rows, key = {
                when (it) {
                    is ListRow.Category -> "c:${it.category.name}"
                    is ListRow.App -> "a:${it.under}:${it.app.id}"
                }
            }) { row ->
                when (row) {
                    is ListRow.Category -> CategoryRow(
                        category = row.category,
                        count = export.appsIn(row.category.name).size,
                        open = row.category.name in expanded,
                        onToggle = { if (!expanded.remove(row.category.name)) expanded.add(row.category.name) },
                    )
                    is ListRow.App -> AppRow(row.app, onClick = { onOpen(row.app) })
                }
            }
        }
    }
}

@Composable
private fun Dot(color: Color) {
    Box(Modifier.size(10.dp).background(color, CircleShape))
}

@Composable
private fun CategoryRow(category: ObtainiumCategory, count: Int, open: Boolean, onToggle: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable(onClick = onToggle),
    ) {
        Text(if (open) "▾ " else "▸ ", style = MaterialTheme.typography.titleMedium)
        Dot(category.color())
        Text(
            category.name,
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.weight(1f).padding(start = 10.dp),
        )
        Text("$count", style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun AppRow(app: ObtainiumApp, onClick: () -> Unit) {
    val versions = when (val installed = app.installedVersion) {
        null -> "not installed · latest ${app.latestVersion ?: "unknown"}"
        else -> "installed $installed · latest ${app.latestVersion ?: "unknown"}"
    }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 56.dp)
            .clickable(onClick = onClick)
            .padding(start = 28.dp, top = 6.dp, bottom = 6.dp),
    ) {
        Text(
            app.name + if (app.pinned) "  · pinned" else "",
            style = MaterialTheme.typography.bodyLarge,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Text(
            "${app.author} · $versions",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.7f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun Detail(app: ObtainiumApp, export: ObtainiumExport, onBack: () -> Unit) {
    val fields = listOf(
        "package id" to app.id,
        "author" to app.author.ifEmpty { "none" },
        "source" to app.url.ifEmpty { "none" },
        "categories" to app.categories.joinToString(", ").ifEmpty { ObtainiumExport.UNCATEGORIZED },
        "installed version" to (app.installedVersion ?: "not installed"),
        "latest version" to (app.latestVersion ?: "unknown"),
        "release date" to Obtainium.formatMicros(app.releaseDateMicros),
        "last update check" to Obtainium.formatMicros(app.lastUpdateCheckMicros),
        "pinned" to app.pinned.toString(),
        "APK assets" to app.apkNames.joinToString("\n").ifEmpty { "none" },
        "from export" to export.file,
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
        HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
        for ((label, value) in fields) {
            Label(label)
            Text(value, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(bottom = 8.dp))
        }
        if (app.settings.isNotEmpty()) {
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
            Label("additional settings")
            Text(
                app.settings.joinToString("\n") { (k, v) -> "$k = $v" },
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
        app.changeLog?.let {
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
            Label("changelog")
            Text(it, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(top = 4.dp))
        }
        HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
        Label("complete export record")
        Text(
            app.raw,
            fontFamily = FontFamily.Monospace,
            fontSize = 12.sp,
            modifier = Modifier.padding(top = 8.dp, bottom = 24.dp),
        )
    }
}

@Composable
private fun Label(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.6f),
    )
}

private val VaultState.label: String get() = when (this) {
    VaultState.Unchecked -> "unchecked"
    VaultState.RequestingPermission -> "asking"
    VaultState.Loading -> "loading"
    VaultState.Loaded -> "loaded"
    VaultState.Error -> "error"
}
