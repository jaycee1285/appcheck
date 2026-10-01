package dev.apptrackkot

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.SystemClock
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import android.provider.DocumentsContract
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
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
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.SnapshotStateList
import androidx.compose.runtime.toMutableStateList
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.apptrackkot.ui.AppTrackKotTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
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

/** The export being projected plus the ledger text it was loaded against. */
data class Loaded(
    val tree: Uri,
    val export: ObtainiumExport,
    val ledgerUri: Uri,
    val ledgerText: String,
    val overlays: Map<String, Overlay>,
    val snapshots: Map<String, DisplaySnapshot>,
    /** Category edits written to obtainium-import.json but not yet imported into Obtainium. */
    val pending: Map<String, List<String>> = emptyMap(),
    val importUri: Uri? = null,
) {
    fun overlay(id: String) = overlays[id] ?: Overlay()

    /** What the screen shows: the export with pending category edits applied. */
    val display: ObtainiumExport by lazy { export.withPending(pending) }
    val desktopApps: List<DesktopApp> by lazy { Ledger.desktopApps(ledgerText) }

    fun unlistedApps(): List<ObtainiumApp> = Ledger.unlistedApps(export, snapshots)

    fun categoriesOf(id: String): List<String> =
        pending[id] ?: export.apps.firstOrNull { it.id == id }?.categories ?: emptyList()
}

@Composable
fun AppTrackKotApp() {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("apptrackkot", Context.MODE_PRIVATE) }
    val treeKey = "vault_tree_uri"

    var state by remember { mutableStateOf(VaultState.Unchecked) }
    var step by remember { mutableStateOf("0/6 · not started") }
    var detail by remember { mutableStateOf("") }
    var loaded by remember { mutableStateOf<Loaded?>(null) }
    var tree by remember {
        mutableStateOf<Uri?>(prefs.getString(treeKey, null)?.let { Uri.parse(it) })
    }
    var attempt by remember { mutableStateOf(0) }
    var selected by rememberSaveable { mutableStateOf<String?>(null) }
    var unlistedMode by rememberSaveable { mutableStateOf(false) }
    var desktopMode by rememberSaveable { mutableStateOf(false) }
    var desktopEditing by rememberSaveable { mutableStateOf<String?>(null) }

    val chooseFolder = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { picked ->
        if (picked == null) {
            state = VaultState.Error
            step = "1/6 · folder permission"
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
            step = "1/6 · folder permission"
            detail = "Grant global file access: pick the Syncthing root folder, where ObtainX or Obtainium writes its export."
        } else {
            load(context, current) { s, st, d, result ->
                state = s; step = st; detail = d
                if (result != null) loaded = result
            }
        }
    }

    AppTrackKotTheme {
        // Surface supplies the content color; bare Text defaults to black on the dark window.
        Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
            val current = loaded
            val app = selected?.let { id ->
                current?.display?.apps?.firstOrNull { it.id == id }
                    ?: current?.unlistedApps()?.firstOrNull { it.id == id }
            }
            val unlisted = app != null && current?.export?.apps?.none { it.id == app.id } == true
            BackHandler(enabled = desktopMode && desktopEditing != null) { desktopEditing = null }
            BackHandler(enabled = app != null && !desktopMode) { selected = null }
            if (desktopMode && current != null) {
                DesktopHome(
                    loaded = current,
                    editing = desktopEditing,
                    onEdit = { desktopEditing = it },
                    onBack = { desktopEditing = null; desktopMode = false },
                    onSaved = { loaded = it; desktopEditing = null },
                    onStale = { attempt++ },
                )
            } else if (app != null && current != null) {
                Detail(
                    app = app,
                    loaded = current,
                    unlisted = unlisted,
                    onBack = { selected = null },
                    onSaved = { loaded = it },
                    onStale = { attempt++ },
                )
            } else {
                Home(
                    state = state,
                    step = step,
                    detail = detail,
                    loaded = current,
                    unlistedMode = unlistedMode,
                    onUnlistedModeChange = { unlistedMode = it },
                    onRetry = { chooseFolder.launch(null) },
                    onOpen = { selected = it.id },
                    onDesktop = { desktopMode = true; selected = null },
                )
            }
        }
    }
}

private data class Child(val docId: String, val uri: Uri, val size: Long, val isDir: Boolean)

/** Children of one document in the granted tree, in a single provider query. */
private fun listChildren(context: Context, tree: Uri, parentDocId: String): Map<String, Child> {
    val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parentDocId)
    val projection = arrayOf(
        DocumentsContract.Document.COLUMN_DOCUMENT_ID,
        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_SIZE,
        DocumentsContract.Document.COLUMN_MIME_TYPE,
    )
    val found = HashMap<String, Child>()
    context.contentResolver.query(children, projection, null, null, null)?.use { c ->
        while (c.moveToNext()) {
            val name = c.getString(1) ?: continue
            val docId = c.getString(0)
            found[name] = Child(
                docId = docId,
                uri = DocumentsContract.buildDocumentUriUsingTree(tree, docId),
                size = if (c.isNull(2)) 0L else c.getLong(2),
                isDir = c.getString(3) == DocumentsContract.Document.MIME_TYPE_DIR,
            )
        }
    } ?: throw ExportException("The folder listing returned nothing. Reconnect the folder and retry.")
    return found
}

private fun readText(context: Context, uri: Uri): String =
    context.contentResolver.openInputStream(uri)?.bufferedReader()?.use { it.readText() }
        ?: throw ExportException("Could not open $uri for reading.")

private suspend fun load(
    context: Context,
    tree: Uri,
    onResult: (VaultState, String, String, Loaded?) -> Unit,
) {
    suspend fun report(s: VaultState, step: String, d: String, result: Loaded? = null) =
        withContext(Dispatchers.Main) { onResult(s, step, d, result) }
    try {
        val started = SystemClock.elapsedRealtime()
        report(VaultState.Loading, "2/6 · open folder", "Opening Syncthing folder…")
        val failure = withContext(Dispatchers.IO) {
            val root = listChildren(context, tree, DocumentsContract.getTreeDocumentId(tree))
            val name = Obtainium.newest(root.filterValues { !it.isDir }.keys.toList())
                ?: return@withContext "No obtainx-export-*.json or obtainium-export-*.json among ${root.size} entries in this folder. " +
                    "Tap Grant / retry and pick the Syncthing root (syncthing/syncthing), where ObtainX or Obtainium exports."
            val exportFile = root.getValue(name)
            val ledgerDir = root["apptrack"]?.takeIf { it.isDir }
                ?: return@withContext "No apptrack/ folder next to $name. Expected apptrack/apptrack.toml in the Syncthing root."
            val ledgerFile = listChildren(context, tree, ledgerDir.docId)["apptrack.toml"]
                ?: return@withContext "apptrack/ has no apptrack.toml."
            val located = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "3/6 · read", "Reading $name (${exportFile.size} bytes) and apptrack.toml…")
            val exportText = readText(context, exportFile.uri)
            var ledgerText = readText(context, ledgerFile.uri)
            val read = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "4/6 · parse export", "Parsing $name…")
            val export = Obtainium.parse(exportText, name, exportFile.size)
            val parsedExport = SystemClock.elapsedRealtime()
            report(VaultState.Loading, "5/6 · parse ledger", "Parsing apptrack.toml [[inbox.android]]…")
            val importChild = root[Obtainium.IMPORT_FILE]?.takeIf { !it.isDir }
            val pending = importChild?.let { Obtainium.pending(readText(context, it.uri), export) } ?: emptyMap()
            val captured = Ledger.refreshSnapshots(ledgerText, export.withPending(pending))
            if (captured != ledgerText) {
                if (readText(context, ledgerFile.uri) != ledgerText) throw StaleLedgerException()
                context.contentResolver.openOutputStream(ledgerFile.uri, "wt")?.use { it.write(captured.toByteArray()) }
                    ?: throw LedgerException("Could not capture Android row details in apptrack.toml.")
                if (readText(context, ledgerFile.uri) != captured) {
                    throw LedgerException("Android row details read back differently; check apptrack.toml.")
                }
                ledgerText = captured
            }
            val overlays = Ledger.overlays(ledgerText)
            val snapshots = Ledger.snapshots(ledgerText)
            val parsedLedger = SystemClock.elapsedRealtime()
            report(
                VaultState.Loaded,
                "6/6 · ${export.apps.size} apps · ${overlays.size} notes · ${pending.size} pending",
                "$name · locate ${located - started} ms · read ${read - located} ms · " +
                    "parse export ${parsedExport - read} ms · ledger ${parsedLedger - parsedExport} ms",
                Loaded(tree, export, ledgerFile.uri, ledgerText, overlays, snapshots, pending, importChild?.uri),
            )
            null
        }
        if (failure != null) report(VaultState.Error, "failed", failure)
    } catch (e: Throwable) {
        report(VaultState.Error, "failed", e.stackTraceToString().take(2000))
    }
}

class StaleLedgerException : Exception(
    "apptrack.toml changed on disk since it loaded (a desktop save or a Syncthing update). " +
        "Nothing was written. Reloading now; make the edit again.",
)

/** Re-check, edit one record, write, read back. Returns the new state or throws without writing. */
private suspend fun save(context: Context, loaded: Loaded, app: ObtainiumApp, overlay: Overlay): Loaded =
    withContext(Dispatchers.IO) {
        if (readText(context, loaded.ledgerUri) != loaded.ledgerText) throw StaleLedgerException()
        val next = Ledger.upsert(
            text = loaded.ledgerText,
            id = app.id,
            name = app.name,
            importedFrom = loaded.export.file,
            observedOn = Obtainium.observedOn(loaded.export.file),
            overlay = overlay,
            snapshot = DisplaySnapshot.from(app),
        )
        context.contentResolver.openOutputStream(loaded.ledgerUri, "wt")?.use { it.write(next.toByteArray()) }
            ?: throw LedgerException("Could not open apptrack.toml for writing.")
        val written = readText(context, loaded.ledgerUri)
        if (written != next) throw LedgerException("apptrack.toml read back differently after writing; check it on the desktop.")
        loaded.copy(ledgerText = written, overlays = Ledger.overlays(written), snapshots = Ledger.snapshots(written))
    }

private suspend fun saveDesktop(context: Context, loaded: Loaded, originalIdentity: String?, edit: DesktopApp): Loaded =
    withContext(Dispatchers.IO) {
        if (readText(context, loaded.ledgerUri) != loaded.ledgerText) throw StaleLedgerException()
        val next = Ledger.upsertDesktop(loaded.ledgerText, originalIdentity, edit)
        context.contentResolver.openOutputStream(loaded.ledgerUri, "wt")?.use { it.write(next.toByteArray()) }
            ?: throw LedgerException("Could not open apptrack.toml for writing.")
        val written = readText(context, loaded.ledgerUri)
        if (written != next) throw LedgerException("apptrack.toml read back differently after writing; check it on the desktop.")
        loaded.copy(ledgerText = written)
    }

/**
 * Rewrites obtainium-import.json with every pending category edit, then captures
 * the displayed categories for tracked Android rows in apptrack.toml.
 * Obtainium's import merges by id and never deletes, so one accumulating file is one import.
 */
private suspend fun saveCategories(
    context: Context,
    loaded: Loaded,
    app: ObtainiumApp,
    categories: List<String>,
): Loaded = withContext(Dispatchers.IO) {
    val source = loaded.export.apps.first { it.id == app.id }
    val edits = LinkedHashMap(loaded.pending)
    if (categories == source.categories) edits.remove(app.id) else edits[app.id] = categories
    val document = Obtainium.importDocument(
        edits = edits,
        export = loaded.export,
        exportedAt = DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH:mm:ss.SSSSSS")
            .withZone(ZoneId.systemDefault())
            .format(Instant.now()),
    )
    val uri = loaded.importUri ?: DocumentsContract.createDocument(
        context.contentResolver,
        DocumentsContract.buildDocumentUriUsingTree(loaded.tree, DocumentsContract.getTreeDocumentId(loaded.tree)),
        "application/json",
        Obtainium.IMPORT_FILE,
    ) ?: throw ExportException("Could not create ${Obtainium.IMPORT_FILE} in the Syncthing root.")
    context.contentResolver.openOutputStream(uri, "wt")?.use { it.write(document.toByteArray()) }
        ?: throw ExportException("Could not open ${Obtainium.IMPORT_FILE} for writing.")
    val written = readText(context, uri)
    if (written != document) throw ExportException("${Obtainium.IMPORT_FILE} read back differently after writing.")
    val pending = Obtainium.pending(written, loaded.export)
    val captured = Ledger.refreshSnapshots(loaded.ledgerText, loaded.export.withPending(pending))
    if (captured != loaded.ledgerText) {
        if (readText(context, loaded.ledgerUri) != loaded.ledgerText) throw StaleLedgerException()
        context.contentResolver.openOutputStream(loaded.ledgerUri, "wt")?.use { it.write(captured.toByteArray()) }
            ?: throw LedgerException("Could not save Android category snapshots in apptrack.toml.")
        if (readText(context, loaded.ledgerUri) != captured) {
            throw LedgerException("Android category snapshots read back differently; check apptrack.toml.")
        }
    }
    loaded.copy(
        pending = pending, importUri = uri, ledgerText = captured,
        snapshots = Ledger.snapshots(captured),
    )
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
    data class Category(val category: ObtainiumCategory, val count: Int) : ListRow
    data class App(val app: ObtainiumApp, val under: String) : ListRow
}

private val stringListSaver = listSaver<SnapshotStateList<String>, String>(
    save = { it.toList() },
    restore = { it.toMutableStateList() },
)

private val Muted = Color(0xFF8A8F98)

private fun ObtainiumCategory.color(): Color = argb?.let { Color(it) } ?: Muted

private fun Disposition?.color() = when (this) {
    Disposition.Using -> Color(0xFF6CC070)
    Disposition.Considering -> Color(0xFFE0B84A)
    Disposition.Archived -> Color(0xFFE06C6C)
    null -> Muted
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun DesktopHome(
    loaded: Loaded,
    editing: String?,
    onEdit: (String?) -> Unit,
    onBack: () -> Unit,
    onSaved: (Loaded) -> Unit,
    onStale: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val apps = loaded.desktopApps
    var query by rememberSaveable { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    if (editing != null) {
        val original = apps.firstOrNull { it.identity == editing }
        var name by remember(editing) { mutableStateOf(original?.name ?: "") }
        var url by remember(editing) { mutableStateOf(original?.url ?: "") }
        var description by remember(editing) { mutableStateOf(original?.description ?: "") }
        var category by remember(editing) { mutableStateOf(original?.category ?: "") }
        var tags by remember(editing) { mutableStateOf(original?.tags ?: "") }
        var disposition by remember(editing) { mutableStateOf(original?.disposition ?: Disposition.Considering) }
        LazyColumn(modifier = Modifier.fillMaxSize().systemBarsPadding().imePadding().padding(16.dp)) {
            item {
                Text(if (editing.isEmpty()) "Add desktop app" else "Edit desktop app", fontSize = 24.sp)
                Text("Record only · run c on desktop to check install routes", style = MaterialTheme.typography.bodySmall)
                OutlinedTextField(name, { name = it }, label = { Text("Name") }, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(url, { url = it }, label = { Text("Source URL") }, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(description, { description = it }, label = { Text("Description") }, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(category, { category = it }, label = { Text("Category") }, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(tags, { tags = it }, label = { Text("Tags, comma separated") }, modifier = Modifier.fillMaxWidth())
                FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Disposition.entries.forEach { choice ->
                        FilterChip(selected = disposition == choice, onClick = { disposition = choice }, label = { Text(choice.label) })
                    }
                }
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = { error = null; onEdit(null) }) { Text("Cancel") }
                    Button(onClick = {
                        scope.launch {
                            try {
                                val saved = saveDesktop(context, loaded, editing.takeIf { it.isNotEmpty() },
                                    DesktopApp(name = name, url = url, description = description,
                                        category = category, disposition = disposition, tags = tags))
                                error = null
                                onSaved(saved)
                            } catch (stale: StaleLedgerException) {
                                error = stale.message
                                onStale()
                            } catch (failure: Exception) {
                                error = failure.message ?: failure.toString()
                            }
                        }
                    }) { Text("Save") }
                }
            }
        }
        return
    }
    val visible = apps.filter { query.isBlank() || "${it.name} ${it.url} ${it.description} ${it.category} ${it.tags}".contains(query, ignoreCase = true) }
    LazyColumn(modifier = Modifier.fillMaxSize().systemBarsPadding().padding(horizontal = 16.dp)) {
        item {
            Text("Desktop Track", fontSize = 28.sp, modifier = Modifier.padding(top = 16.dp))
            Text("${apps.size} apps · synced apptrack.toml", style = MaterialTheme.typography.bodySmall)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onBack) { Text("Android Track") }
                Button(onClick = { onEdit("") }) { Text("Add app") }
            }
            OutlinedTextField(query, { query = it }, label = { Text("Search desktop apps") }, modifier = Modifier.fillMaxWidth())
        }
        for ((category, group) in visible.groupBy { it.category }.toSortedMap(String.CASE_INSENSITIVE_ORDER)) {
            item(key = "category:$category") {
                Text("$category · ${group.size}", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 14.dp))
            }
            items(group, key = { it.identity }) { app ->
                Row(modifier = Modifier.fillMaxWidth().clickable { onEdit(app.identity) }.padding(vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically) {
                    Dot(app.disposition.color())
                    Column(modifier = Modifier.padding(start = 10.dp)) {
                        Text(app.name)
                        if (app.description.isNotBlank()) Text(app.description, style = MaterialTheme.typography.bodySmall, maxLines = 2)
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun Home(
    state: VaultState,
    step: String,
    detail: String,
    loaded: Loaded?,
    unlistedMode: Boolean,
    onUnlistedModeChange: (Boolean) -> Unit,
    onRetry: () -> Unit,
    onOpen: (ObtainiumApp) -> Unit,
    onDesktop: () -> Unit,
) {
    var query by rememberSaveable { mutableStateOf("") }
    val expanded = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }
    val unlistedCollapsed = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }
    // Independent U / C / A toggles; none on shows everything, including unclassified apps.
    val filters = rememberSaveable(saver = stringListSaver) { mutableStateListOf<String>() }

    fun visible(app: ObtainiumApp) =
        unlistedMode || filters.isEmpty() || loaded?.overlay(app.id)?.disposition?.key in filters

    val export = loaded?.display
    val unlisted = loaded?.unlistedApps().orEmpty()
    val shownApps = if (unlistedMode) unlisted else export?.apps.orEmpty()
    val rows: List<ListRow> = when {
        export == null -> emptyList()
        query.isNotBlank() -> shownApps
            .filter(::visible)
            .mapNotNull { a ->
                fuzzyScore(query, "${a.name} ${a.author} ${a.id} ${a.categories.joinToString(" ")}")
                    ?.let { it to a }
            }
            .sortedBy { it.first }
            .map { ListRow.App(it.second, "search") }
        unlistedMode -> buildList {
            val names = unlisted.flatMap { app ->
                app.categories.ifEmpty { listOf(ObtainiumExport.UNCATEGORIZED) }
            }.distinct().sortedWith(String.CASE_INSENSITIVE_ORDER)
            for (name in names) {
                val apps = unlisted.filter { app ->
                    if (name == ObtainiumExport.UNCATEGORIZED) app.categories.isEmpty()
                    else name in app.categories
                }
                add(ListRow.Category(ObtainiumCategory(name, export.colors[name]), apps.size))
                if (name !in unlistedCollapsed) apps.forEach { add(ListRow.App(it, name)) }
            }
        }
        else -> buildList {
            for (category in export.categories) {
                val apps = export.appsIn(category.name).filter(::visible)
                if (apps.isEmpty()) continue
                add(ListRow.Category(category, apps.size))
                if (category.name in expanded) apps.forEach { add(ListRow.App(it, category.name)) }
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
            OutlinedButton(onClick = onDesktop) { Text("Desktop Track · ${loaded?.desktopApps?.size ?: 0}") }
            Text("smoke 5 · Obtainium projection + notes", style = MaterialTheme.typography.bodyMedium)
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
        if (loaded != null && export != null) {
            item {
                val unclassified = export.apps.count { loaded.overlay(it.id).disposition == null }
                Text(
                    if (unlistedMode) "${unlisted.size} unlisted · saved in apptrack.toml"
                    else "${export.apps.size} apps · ${export.installed} installed · $unclassified unclassified",
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(top = 8.dp),
                )
                FlowRow(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                    modifier = Modifier.padding(top = 8.dp),
                ) {
                    for (d in Disposition.entries) {
                        val on = d.key in filters
                        FilterChip(
                            selected = on && !unlistedMode,
                            onClick = {
                                if (unlistedMode) {
                                    onUnlistedModeChange(false)
                                    filters.clear()
                                    filters.add(d.key)
                                } else if (!filters.remove(d.key)) filters.add(d.key)
                            },
                            label = {
                                Text(
                                    "${d.mark} ${export.apps.count { loaded.overlay(it.id).disposition == d }}",
                                    color = d.color(),
                                )
                            },
                            modifier = Modifier.heightIn(min = 48.dp),
                        )
                    }
                    FilterChip(
                        selected = unlistedMode,
                        onClick = { onUnlistedModeChange(!unlistedMode) },
                        label = { Text("Unlisted ${unlisted.size}") },
                        modifier = Modifier.heightIn(min = 48.dp),
                    )
                }
                OutlinedTextField(
                    value = query,
                    onValueChange = { query = it },
                    singleLine = true,
                    placeholder = { Text("Search name, author, package id, category") },
                    modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp),
                )
                if (query.isNotBlank() || filters.isNotEmpty()) {
                    val shown = if (query.isNotBlank()) rows.size
                        else if (unlistedMode) unlisted.size else export.apps.count(::visible)
                    Text("$shown shown", style = MaterialTheme.typography.bodySmall)
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
                        count = row.count,
                        open = if (unlistedMode) row.category.name !in unlistedCollapsed else row.category.name in expanded,
                        onToggle = {
                            val state = if (unlistedMode) unlistedCollapsed else expanded
                            if (!state.remove(row.category.name)) state.add(row.category.name)
                        },
                    )
                    is ListRow.App -> AppRow(
                        row.app,
                        loaded.overlay(row.app.id).disposition,
                        unlisted = unlistedMode,
                        onClick = { onOpen(row.app) },
                    )
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
private fun DispositionDot(disposition: Disposition?) {
    val modifier = Modifier
        .size(10.dp)
        .semantics { contentDescription = disposition?.label ?: "Untagged" }
    Box(
        if (disposition == null) modifier.border(1.dp, MaterialTheme.colorScheme.onBackground, CircleShape)
        else modifier.background(disposition.color(), CircleShape),
    )
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
private fun AppRow(app: ObtainiumApp, disposition: Disposition?, unlisted: Boolean, onClick: () -> Unit) {
    val versions = when (val installed = app.installedVersion) {
        null -> "not installed · latest ${app.latestVersion ?: "unknown"}"
        else -> "installed $installed · latest ${app.latestVersion ?: "unknown"}"
    }
    val subtitle = if (unlisted) {
        listOf(
            app.categories.joinToString(", ").ifEmpty { ObtainiumExport.UNCATEGORIZED },
            app.author,
            versions,
        ).filter { it.isNotEmpty() }.joinToString(" · ")
    } else "${app.author} · $versions"
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 56.dp)
            .clickable(onClick = onClick)
            .padding(start = 28.dp, top = 6.dp, bottom = 6.dp),
    ) {
        DispositionDot(disposition)
        Column(modifier = Modifier.weight(1f).padding(start = 10.dp)) {
            Text(
                app.name + if (app.pinned) "  · pinned" else "",
                style = MaterialTheme.typography.bodyLarge,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.7f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@Composable
private fun Detail(
    app: ObtainiumApp,
    loaded: Loaded,
    unlisted: Boolean,
    onBack: () -> Unit,
    onSaved: (Loaded) -> Unit,
    onStale: () -> Unit,
) {
    val overlay = loaded.overlay(app.id)
    var editing by rememberSaveable { mutableStateOf(false) }
    val fields = listOf(
        "package id" to app.id,
        "author" to app.author.ifEmpty { "none" },
        "installed version" to (app.installedVersion ?: "not installed"),
        "latest version" to (app.latestVersion ?: "unknown"),
        "pinned" to app.pinned.toString(),
    ) + if (unlisted) listOf(
        "last known categories" to app.categories.joinToString(", ").ifEmpty { ObtainiumExport.UNCATEGORIZED },
        "listing" to "No longer in the selected export; the note remains in apptrack.toml",
    ) else listOf(
        "source" to app.url.ifEmpty { "none" },
        "release date" to Obtainium.formatMicros(app.releaseDateMicros),
        "last update check" to Obtainium.formatMicros(app.lastUpdateCheckMicros),
        "APK assets" to app.apkNames.joinToString("\n").ifEmpty { "none" },
        "from export" to loaded.export.file,
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
        Text(
            overlay.disposition?.label ?: "Unclassified",
            color = overlay.disposition.color(),
            style = MaterialTheme.typography.titleMedium,
        )
        HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
        Label("description")
        Text(
            overlay.description.ifEmpty { "none" },
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier.padding(bottom = 8.dp),
        )
        if (overlay.archivedBecause.isNotEmpty()) {
            Label("archived because")
            Text(overlay.archivedBecause, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(bottom = 8.dp))
        }
        OutlinedButton(onClick = { editing = true }, modifier = Modifier.heightIn(min = 48.dp)) {
            Text("Edit description / disposition")
        }
        HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
        if (!unlisted) {
            Categories(app, loaded, onSaved)
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
        }
        for ((label, value) in fields) {
            Label(label)
            Text(value, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(bottom = 8.dp))
        }
        if (app.settings.isNotEmpty()) {
            Collapsible("additional settings (${app.settings.size})") {
                Text(
                    app.settings.joinToString("\n") { (k, v) -> "$k = $v" },
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                )
            }
        }
        app.changeLog?.let {
            Collapsible("changelog") { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
        if (!unlisted) Collapsible("complete export record") {
            Text(app.raw, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        }
        Box(Modifier.heightIn(min = 24.dp))
    }
    if (editing) {
        EditDialog(
            app = app,
            loaded = loaded,
            onDismiss = { editing = false },
            onSaved = { editing = false; onSaved(it) },
            onStale = onStale,
        )
    }
}

/**
 * Obtainium's categories for this app. Saving rewrites obtainium-import.json with every
 * pending edit; Obtainium applies them when John imports that file.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun Categories(app: ObtainiumApp, loaded: Loaded, onSaved: (Loaded) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val source = loaded.export.apps.first { it.id == app.id }.categories
    var chosen by remember(app.id, loaded.pending) { mutableStateOf(loaded.categoriesOf(app.id)) }
    var saving by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val changed = chosen.sorted() != source.sorted()

    Label("categories (Obtainium)")
    if (loaded.export.assignable.isEmpty()) {
        Text("Obtainium has no categories yet.", style = MaterialTheme.typography.bodyMedium)
        return
    }
    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        for (name in loaded.export.assignable) {
            val on = name in chosen
            FilterChip(
                selected = on,
                onClick = { chosen = if (on) chosen - name else chosen + name },
                label = { Text(name) },
                modifier = Modifier.heightIn(min = 48.dp),
            )
        }
    }
    if (chosen.isEmpty()) {
        Text(ObtainiumExport.UNCATEGORIZED, style = MaterialTheme.typography.bodySmall, color = Muted)
    }
    if (loaded.pending.containsKey(app.id)) {
        Text(
            "pending import: ${Obtainium.IMPORT_FILE} holds ${loaded.pending.size} edit(s). " +
                "Import that file in Obtainium (Import/Export → Import), then export again.",
            style = MaterialTheme.typography.bodySmall,
            color = Muted,
            modifier = Modifier.padding(top = 8.dp),
        )
    }
    if (changed) {
        Button(
            enabled = !saving,
            onClick = {
                saving = true
                error = null
                scope.launch {
                    try {
                        onSaved(saveCategories(context, loaded, app, chosen))
                    } catch (e: Throwable) {
                        error = e.stackTraceToString().take(1500)
                    }
                    saving = false
                }
            },
            modifier = Modifier.padding(top = 8.dp).heightIn(min = 48.dp),
        ) { Text("Save to ${Obtainium.IMPORT_FILE}") }
    }
    if (saving) LinearProgressIndicator(modifier = Modifier.fillMaxWidth().padding(top = 8.dp))
    error?.let {
        Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
    }
}

@Composable
private fun Collapsible(title: String, content: @Composable () -> Unit) {
    var open by rememberSaveable(title) { mutableStateOf(false) }
    HorizontalDivider(modifier = Modifier.padding(top = 4.dp))
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable { open = !open },
    ) {
        Text(if (open) "▾ " else "▸ ", style = MaterialTheme.typography.titleMedium)
        Text(title, style = MaterialTheme.typography.labelLarge)
    }
    if (open) Box(Modifier.padding(bottom = 12.dp)) { content() }
}

@Composable
private fun EditDialog(
    app: ObtainiumApp,
    loaded: Loaded,
    onDismiss: () -> Unit,
    onSaved: (Loaded) -> Unit,
    onStale: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val current = loaded.overlay(app.id)
    var description by rememberSaveable { mutableStateOf(current.description) }
    var dispositionKey by rememberSaveable { mutableStateOf(current.disposition?.key) }
    var reason by rememberSaveable { mutableStateOf(current.archivedBecause) }
    var menu by remember { mutableStateOf(false) }
    var saving by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val disposition = Disposition.entries.firstOrNull { it.key == dispositionKey }
    val needsReason = disposition == Disposition.Archived && reason.isBlank()

    AlertDialog(
        onDismissRequest = { if (!saving) onDismiss() },
        title = { Text(app.name) },
        text = {
            Column(modifier = Modifier.verticalScroll(rememberScrollState())) {
                OutlinedTextField(
                    value = description,
                    onValueChange = { description = it },
                    label = { Text("Description") },
                    minLines = 3,
                    modifier = Modifier.fillMaxWidth(),
                )
                Box(modifier = Modifier.padding(top = 12.dp)) {
                    OutlinedButton(onClick = { menu = true }, modifier = Modifier.heightIn(min = 48.dp)) {
                        Text(disposition?.label ?: "Unclassified", color = disposition.color())
                        Text("  ▾")
                    }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        for (d in Disposition.entries) {
                            DropdownMenuItem(
                                text = { Text(d.label, color = d.color()) },
                                onClick = { dispositionKey = d.key; menu = false },
                                modifier = Modifier.heightIn(min = 48.dp),
                            )
                        }
                    }
                }
                if (disposition == Disposition.Archived) {
                    OutlinedTextField(
                        value = reason,
                        onValueChange = { reason = it },
                        label = { Text("Archived because (required)") },
                        isError = needsReason,
                        modifier = Modifier.fillMaxWidth().padding(top = 12.dp),
                    )
                }
                if (saving) LinearProgressIndicator(modifier = Modifier.fillMaxWidth().padding(top = 12.dp))
                error?.let {
                    Text(
                        it,
                        color = MaterialTheme.colorScheme.error,
                        style = MaterialTheme.typography.bodySmall,
                        modifier = Modifier.padding(top = 12.dp),
                    )
                }
                Text(
                    "Saves to apptrack.toml as [[inbox.android]] android:${app.id}",
                    style = MaterialTheme.typography.bodySmall,
                    color = Muted,
                    modifier = Modifier.padding(top = 12.dp),
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = !saving && !needsReason,
                onClick = {
                    saving = true
                    error = null
                    scope.launch {
                        try {
                            onSaved(save(context, loaded, app, Overlay(disposition, description.trim(), reason.trim())))
                        } catch (e: StaleLedgerException) {
                            // Keep the edits; the reload swaps in fresh ledger text, then Save again.
                            error = e.message
                            saving = false
                            onStale()
                        } catch (e: Throwable) {
                            error = e.stackTraceToString().take(1500)
                            saving = false
                        }
                    }
                },
            ) { Text("Save") }
        },
        dismissButton = {
            TextButton(enabled = !saving, onClick = onDismiss) { Text("Cancel") }
        },
    )
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
