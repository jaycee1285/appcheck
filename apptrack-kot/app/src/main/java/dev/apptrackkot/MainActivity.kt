package dev.apptrackkot

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Button
import androidx.compose.material3.Divider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.documentfile.provider.DocumentFile
import dev.apptrackkot.ui.AppTrackKotTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
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
    var detail by remember { mutableStateOf("") }
    var snapshot by remember { mutableStateOf<VaultSnapshot?>(null) }
    var tree by remember {
        mutableStateOf<Uri?>(prefs.getString(treeKey, null)?.let { Uri.parse(it) })
    }

    val chooseFolder = rememberLauncherForActivityResult(androidx.activity.result.contract.ActivityResultContracts.OpenDocumentTree()) { picked ->
        if (picked == null) {
            state = VaultState.Error
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
            run(context, picked) { s, d, snap -> state = s; detail = d; snapshot = snap }
        } catch (e: Exception) {
            state = VaultState.Error
            detail = "Could not keep the folder permission: ${e.message}"
        }
    }

    LaunchedEffect(tree) {
        val current = tree
        if (current == null) {
            state = VaultState.RequestingPermission
            detail = "Grant global file access: pick the Syncthing folder that holds apptrack.toml."
        } else {
            run(context, current) { s, d, snap -> state = s; detail = d; snapshot = snap }
        }
    }

    AppTrackKotTheme {
        // Surface supplies the content color; bare Text defaults to black on the dark window.
        Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
            Home(
                state = state,
                detail = detail,
                snapshot = snapshot,
                onOpen = { chooseFolder.launch(null) },
                onRetry = { chooseFolder.launch(null) },
            )
        }
    }
}

private fun run(
    context: Context,
    tree: Uri,
    onResult: (VaultState, String, VaultSnapshot?) -> Unit,
) {
    onResult(VaultState.Loading, "Opening Syncthing folder…", null)
    val root = DocumentFile.fromTreeUri(context, tree)
        ?: return onResult(
            VaultState.Error,
            "Folder unavailable. Reconnect the vault, then tap Grant / retry.",
            null,
        )
    onResult(VaultState.Loading, "Locating apptrack.toml…", null)
    val file = root.findFile("apptrack.toml")
        ?: return onResult(
            VaultState.Error,
            "apptrack.toml is not in this folder. Expected it at the root of the picked Syncthing folder.",
            null,
        )
    onResult(VaultState.Loading, "Reading ${file.length()} bytes…", null)
    val text = context.contentResolver.openInputStream(file.uri)?.bufferedReader()?.use { it.readText() }
        ?: return onResult(
            VaultState.Error,
            "Could not read apptrack.toml. Reconnect the vault and retry.",
            null,
        )
    val snap = Vault.parse(text).copy(path = tree.toString(), bytes = file.length())
    onResult(VaultState.Loaded, "Loaded ${tree} · ${snap.bytes} bytes", snap)
}

@Composable
fun Home(
    state: VaultState,
    detail: String,
    snapshot: VaultSnapshot?,
    onOpen: () -> Unit,
    onRetry: () -> Unit,
) {
    LazyColumn(
        modifier = Modifier
            .fillMaxSize()
            .statusBarsPadding()
            .padding(horizontal = 24.dp, vertical = 20.dp),
    ) {
        item {
            Text("AppTrack-KOT", style = TextStyle(fontSize = 28.sp))
            Text("smoke 2 · state machine", style = MaterialTheme.typography.bodyMedium)
            Divider(modifier = Modifier.padding(vertical = 16.dp))
            Text("state: ${state.label}", style = MaterialTheme.typography.titleMedium)
            Text("step: ${state.step}", style = MaterialTheme.typography.bodySmall)
            Text(detail, style = MaterialTheme.typography.bodySmall)
            if (state == VaultState.Loading) {
                LinearProgressIndicator(modifier = Modifier.padding(top = 12.dp))
            }
        }
        snapshot?.let {
            item {
                Divider(modifier = Modifier.padding(vertical = 16.dp))
                Text("tracked: ${it.tracked}", style = MaterialTheme.typography.titleMedium)
                Text("android inbox: ${it.android}", style = MaterialTheme.typography.bodySmall)
                Text("nix inbox: ${it.nixInbox}", style = MaterialTheme.typography.bodySmall)
                Text("${it.bytes} bytes", style = MaterialTheme.typography.bodySmall)
            }
        }
        if (state == VaultState.Error || state == VaultState.RequestingPermission) {
            item {
                Button(onClick = onRetry, modifier = Modifier.padding(top = 16.dp)) {
                    Text("Grant / retry")
                }
            }
        }
    }
}

private val VaultState.label: String get() = when (this) {
    VaultState.Unchecked -> "unchecked"
    VaultState.RequestingPermission -> "asking"
    VaultState.Loading -> "loading"
    VaultState.Loaded -> "loaded"
    VaultState.Error -> "error"
}

private val VaultState.step: String get() = when (this) {
    VaultState.Unchecked -> "0/5 · not started"
    VaultState.RequestingPermission -> "1/5 · folder permission"
    VaultState.Loading -> "2/5 · locate + read"
    VaultState.Loaded -> "3/5 · parsed"
    VaultState.Error -> "4/5 · failed"
}