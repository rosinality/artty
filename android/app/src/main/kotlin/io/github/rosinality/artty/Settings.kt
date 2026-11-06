package io.github.rosinality.artty

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay
import uniffi.artty_android.checkConfig
import uniffi.artty_android.keyInfo

private val Warn = Color(0xfff7768e)

private enum class Page(val title: String, val about: String) {
    Keys("Keys", "This phone's key, and private keys added for the ssh config's IdentityFiles"),
    Config("SSH config", "As ~/.ssh/config: hosts, users, keys, jump hosts, forwards"),
    KnownHosts("Known hosts", "The machines' keys this phone trusts"),
    Terminal("Terminal", "Text size, the volume keys, the keys row, the bell"),
    Updates("Updates", "Installing new builds from where they're published"),
    Backup("Back up and restore", "Servers, the ssh config, machines' keys and settings, to a file"),
}

/** The app's settings, page by page; `onClose` when done. */
@Composable
fun SettingsScreen(store: Store, onClose: () -> Unit) {
    var page by remember { mutableStateOf<Page?>(null) }
    BackHandler { if (page != null) page = null else onClose() }
    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { if (page != null) page = null else onClose() }) { Text("‹", fontSize = 22.sp) }
            Text(page?.title ?: "Settings", fontSize = 20.sp, fontWeight = FontWeight.SemiBold)
        }
        when (page) {
            null -> LazyColumn {
                items(Page.entries.toList()) { p ->
                    Column(
                        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp)
                            .clip(RoundedCornerShape(10.dp)).background(Panel)
                            .clickable { page = p }.padding(14.dp),
                    ) {
                        Text(p.title, fontWeight = FontWeight.Medium)
                        Text(p.about, color = Faint, fontSize = 13.sp)
                    }
                }
            }
            Page.Keys -> KeysPage(store)
            Page.Config -> ConfigPage(store)
            Page.KnownHosts -> KnownHostsPage(store)
            Page.Terminal -> TerminalPage(store)
            Page.Updates -> UpdatesPage(store)
            Page.Backup -> BackupPage(store)
        }
    }
}

private fun copy(context: Context, text: String) {
    context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("artty", text))
}

/** A file's name, as the picker gives it. */
private fun displayName(context: Context, uri: Uri): String? =
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
        if (it.moveToFirst()) it.getString(0) else null
    }

private fun readText(context: Context, uri: Uri): String? =
    runCatching { context.contentResolver.openInputStream(uri)?.use { it.readBytes().decodeToString() } }.getOrNull()

@Composable
private fun Section(title: String) {
    Text(title, color = Faint, fontSize = 13.sp, fontWeight = FontWeight.Medium, modifier = Modifier.padding(start = 20.dp, top = 16.dp, bottom = 4.dp))
}

@Composable
private fun Card(onClick: (() -> Unit)? = null, content: @Composable () -> Unit) {
    val base = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp).clip(RoundedCornerShape(10.dp)).background(Panel)
    Column((if (onClick != null) base.clickable(onClick = onClick) else base).padding(14.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) { content() }
}

// Keys.

@Composable
private fun KeysPage(store: Store) {
    val context = LocalContext.current
    var keys by remember { mutableStateOf(store.keys.list()) }
    var adding by remember { mutableStateOf<Pair<String, String>?>(null) }
    var chosen by remember { mutableStateOf<AddedKey?>(null) }
    val phoneKey = remember { store.publicKey() }
    val keptIn = remember { store.phoneKey.keptIn }
    val pick = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) adding = (displayName(context, uri) ?: "key").removeSuffix(".pem") to (readText(context, uri) ?: "")
    }
    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Section("This phone's key")
            Card {
                Text("Add it to ~/.ssh/authorized_keys on each server, and this phone can log in. Its private half is kept in $keptIn: it can't be read out or copied, the app included.", color = Faint, fontSize = 13.sp)
                SelectionContainer { Text(phoneKey, fontFamily = FontFamily.Monospace, fontSize = 11.sp) }
                Text("In the ssh config, IdentityFile ~/.ssh/phone names it.", color = Faint, fontSize = 13.sp)
                TextButton(onClick = { copy(context, phoneKey) }) { Text("Copy") }
            }
            Section("Keys added")
        }
        items(keys, key = { it.name }) { key ->
            Card(onClick = { chosen = key }) {
                Text(key.name, fontWeight = FontWeight.Medium)
                Text(key.public, fontFamily = FontFamily.Monospace, fontSize = 11.sp, color = Faint, maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (key.encrypted) Text(if (key.passphrase != null) "Passphrase kept" else "Asks for its passphrase", color = Faint, fontSize = 12.sp)
            }
        }
        item {
            if (keys.isEmpty()) Text("None. A host's IdentityFile names a key by its file name: IdentityFile ~/.ssh/work is the key named work.", color = Faint, fontSize = 13.sp, modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp))
            Row(Modifier.padding(12.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { pick.launch(arrayOf("*/*")) }) { Text("Add from a file") }
                OutlinedButton(onClick = { adding = "" to "" }) { Text("Paste one") }
            }
            Text(
                "Added keys are kept encrypted with a key of this phone's Keystore. They are copies: whoever has the original has them too.",
                color = Faint, fontSize = 12.sp, modifier = Modifier.padding(horizontal = 20.dp),
            )
        }
    }
    adding?.let { (name, text) ->
        AddKeyDialog(name, text, onDone = { added ->
            adding = null
            if (added != null) {
                store.keys.add(added.first, added.second, added.third).onSuccess { keys = store.keys.list() }
                    .onFailure { android.widget.Toast.makeText(context, it.message ?: "Not a key", android.widget.Toast.LENGTH_LONG).show() }
            }
        })
    }
    chosen?.let { key ->
        KeyDialog(store, key) {
            chosen = null
            keys = store.keys.list()
        }
    }
}

/** A key to add: its name, text and passphrase (kept, if given). */
@Composable
private fun AddKeyDialog(name: String, text: String, onDone: (Triple<String, String, String?>?) -> Unit) {
    var name by remember { mutableStateOf(name) }
    // Read from a file: not shown (it's a secret), only what it is.
    val fromFile = remember { text.isNotEmpty() }
    var text by remember { mutableStateOf(text) }
    var passphrase by remember { mutableStateOf("") }
    val info = remember(text) { runCatching { keyInfo(text) } }
    AlertDialog(
        onDismissRequest = { onDone(null) },
        title = { Text("Add a key") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(name, { name = it.trim() }, label = { Text("Name (as IdentityFile names it)") }, singleLine = true)
                if (!fromFile) {
                    OutlinedTextField(
                        text, { text = it },
                        label = { Text("The private key") },
                        textStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 10.sp),
                        minLines = 4, maxLines = 8,
                        isError = text.isNotBlank() && info.isFailure,
                    )
                }
                info.getOrNull()?.let {
                    Text(it.public, fontFamily = FontFamily.Monospace, fontSize = 10.sp, color = Faint, maxLines = 2, overflow = TextOverflow.Ellipsis)
                    if (it.encrypted) {
                        OutlinedTextField(passphrase, { passphrase = it }, label = { Text("Passphrase (blank: asked each time)") }, singleLine = true, visualTransformation = PasswordVisualTransformation())
                    }
                }
                if (text.isNotBlank() && info.isFailure) Text(info.exceptionOrNull()?.message ?: "Not a private key", color = Warn, fontSize = 12.sp)
            }
        },
        confirmButton = {
            TextButton(enabled = name.isNotBlank() && info.isSuccess, onClick = { onDone(Triple(name, text, passphrase.ifEmpty { null })) }) { Text("Add") }
        },
        dismissButton = { TextButton(onClick = { onDone(null) }) { Text("Cancel") } },
    )
}

/** A key added: its public half; renaming, forgetting its passphrase, removing it. */
@Composable
private fun KeyDialog(store: Store, key: AddedKey, onDone: () -> Unit) {
    val context = LocalContext.current
    var name by remember { mutableStateOf(key.name) }
    var removing by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDone,
        title = { Text(key.name) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                SelectionContainer { Text(key.public, fontFamily = FontFamily.Monospace, fontSize = 11.sp) }
                OutlinedTextField(name, { name = it.trim() }, label = { Text("Name") }, singleLine = true)
                Row {
                    TextButton(onClick = { copy(context, key.public) }) { Text("Copy public key") }
                    if (key.passphrase != null) TextButton(onClick = { store.keys.setPassphrase(key.name, null); onDone() }) { Text("Forget passphrase") }
                }
                TextButton(onClick = { removing = true }) { Text("Remove", color = Warn) }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                if (name != key.name) store.keys.rename(key, name).onFailure {
                    android.widget.Toast.makeText(context, it.message, android.widget.Toast.LENGTH_LONG).show()
                }
                onDone()
            }) { Text("Done") }
        },
    )
    if (removing) {
        AlertDialog(
            onDismissRequest = { removing = false },
            title = { Text("Remove ${key.name}?") },
            text = { Text("This phone won't log in with it any more. (It's still wherever else it is.)") },
            confirmButton = { TextButton(onClick = { store.keys.remove(key.name); removing = false; onDone() }) { Text("Remove") } },
            dismissButton = { TextButton(onClick = { removing = false }) { Text("Cancel") } },
        )
    }
}

// The ssh config.

@Composable
private fun ConfigPage(store: Store) {
    val context = LocalContext.current
    val saved = remember { store.sshConfig() }
    var text by remember { mutableStateOf(saved) }
    var kept by remember { mutableStateOf(saved) }
    var check by remember { mutableStateOf(checkConfig(saved, store.sshHome.path)) }
    LaunchedEffect(text) {
        delay(300)
        check = checkConfig(text, store.sshHome.path)
    }
    val open = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) readText(context, uri)?.let { text = it }
    }
    val export = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("text/plain")) { uri ->
        if (uri != null) runCatching { context.contentResolver.openOutputStream(uri)?.use { it.write(text.toByteArray()) } }
    }
    Column(Modifier.fillMaxSize().padding(horizontal = 12.dp)) {
        Text(
            "Read as ssh reads ~/.ssh/config: Host and Match blocks, HostName, User, Port, IdentityFile (a key added, by name), ProxyJump, LocalForward, RemoteForward, DynamicForward, ForwardAgent, ServerAliveInterval, Ciphers and the rest. A server's destination can be one of its hosts.",
            color = Faint, fontSize = 12.sp,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            text, { text = it },
            modifier = Modifier.fillMaxWidth().weight(1f),
            textStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp),
            placeholder = { Text("Host devbox\n  HostName dev.example.com\n  User me\n  ProxyJump bastion", fontFamily = FontFamily.Monospace, fontSize = 12.sp) },
        )
        for (error in check.errors.take(3)) Text(error, color = Warn, fontSize = 12.sp)
        if (check.hosts.isNotEmpty()) Text("Hosts: " + check.hosts.joinToString(", "), color = Faint, fontSize = 12.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
        Row(Modifier.padding(vertical = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(enabled = text != kept, onClick = {
                store.saveSshConfig(text)
                kept = text
            }) { Text("Save") }
            OutlinedButton(onClick = { open.launch(arrayOf("*/*")) }) { Text("Import") }
            OutlinedButton(onClick = { export.launch("ssh_config") }) { Text("Export") }
        }
    }
}

// Machines' keys.

@Composable
private fun KnownHostsPage(store: Store) {
    var hosts by remember { mutableStateOf(store.knownHostList()) }
    var forgetting by remember { mutableStateOf<KnownHost?>(null) }
    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Text(
                "A machine's key is recorded the first time this phone logs in, and checked every time after. Forget one that changed for a good reason (the machine was set up again): it's asked about next time.",
                color = Faint, fontSize = 12.sp, modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
            )
        }
        items(hosts, key = { it.line }) { host ->
            Card(onClick = { forgetting = host }) {
                Text(host.hosts, fontWeight = FontWeight.Medium)
                Text("${host.type} ${host.fingerprint}", fontFamily = FontFamily.Monospace, fontSize = 11.sp, color = Faint)
            }
        }
        if (hosts.isEmpty()) item { Text("None yet.", color = Faint, modifier = Modifier.padding(20.dp)) }
    }
    forgetting?.let { host ->
        AlertDialog(
            onDismissRequest = { forgetting = null },
            title = { Text("Forget ${host.hosts}'s key?") },
            text = { Text("Logging in to it next time asks whether to trust the key it shows.") },
            confirmButton = {
                TextButton(onClick = {
                    store.forgetHost(host)
                    hosts = store.knownHostList()
                    forgetting = null
                }) { Text("Forget") }
            },
            dismissButton = { TextButton(onClick = { forgetting = null }) { Text("Cancel") } },
        )
    }
}

// The terminal.

@Composable
private fun TerminalPage(store: Store) {
    var size by remember { mutableFloatStateOf(store.fontSize) }
    var volume by remember { mutableStateOf(store.volumeKeys) }
    var bell by remember { mutableStateOf(store.bellVibrates) }
    var row by remember { mutableStateOf(store.rowKeys) }
    var scrollback by remember { mutableStateOf(store.scrollback.toString()) }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        Section("Text size: ${"%.0f".format(size)}")
        Slider(size, { size = it; store.fontSize = it }, valueRange = 8f..28f, steps = 19, modifier = Modifier.padding(horizontal = 20.dp))
        Text("Pinching the terminal changes it too.", color = Faint, fontSize = 12.sp, modifier = Modifier.padding(horizontal = 20.dp))
        Toggle("The volume keys as keys", "Held: volume down is Ctrl; volume up with W A S D the arrows, E Esc, T Tab, 1–0 F1–F10. Off: they only turn the volume.", volume) {
            volume = it
            store.volumeKeys = it
        }
        Toggle("A bell vibrates", "When a program rings the bell (\\a).", bell) {
            bell = it
            store.bellVibrates = it
        }
        Section("The keys row's characters")
        OutlinedTextField(
            row, { row = it; store.rowKeys = it },
            modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp),
            textStyle = TextStyle(fontFamily = FontFamily.Monospace),
            supportingText = { Text("Separated by spaces; each a key at the row's end.") },
        )
        TextButton(onClick = { row = Store.DEFAULT_ROW_KEYS; store.rowKeys = row }, modifier = Modifier.padding(horizontal = 8.dp)) { Text("As at first") }
        Section("History of a plain ssh tab")
        OutlinedTextField(
            scrollback,
            { value ->
                scrollback = value.filter { it.isDigit() }.take(6)
                scrollback.toIntOrNull()?.let { store.scrollback = it.coerceIn(100, 200_000) }
            },
            modifier = Modifier.padding(horizontal = 12.dp),
            singleLine = true,
            supportingText = { Text("Lines; an artty server keeps its own.") },
        )
    }
}

@Composable
private fun Toggle(title: String, about: String, on: Boolean, change: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().clickable { change(!on) }.padding(horizontal = 20.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title)
            Text(about, color = Faint, fontSize = 12.sp)
        }
        Switch(on, change)
    }
}

// Updates.

@Composable
private fun UpdatesPage(store: Store) {
    val context = LocalContext.current
    var url by remember { mutableStateOf(store.updatesUrl ?: "") }
    var saved by remember { mutableStateOf(url) }
    val (code, name) = remember { Updates.installed(context) }
    Column(Modifier.fillMaxSize().padding(16.dp).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("This is artty $name (build $code).", fontWeight = FontWeight.Medium)
        Text(
            "Where publish.sh puts new builds: a folder served over HTTPS, holding version.json and the APK. The app looks there when it opens (every few hours at most), and offers what's newer. Installing asks you each time; Android takes it only if it's signed with the same key as this one.",
            color = Faint, fontSize = 13.sp,
        )
        OutlinedTextField(
            url, { url = it.trim() },
            label = { Text("Address (https://…/android/)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(enabled = url.isNotBlank(), onClick = {
                store.updatesUrl = url
                saved = url
                Updates.check(context, store)
            }) { Text("Check now") }
            if (url != saved) {
                OutlinedButton(onClick = {
                    store.updatesUrl = url
                    saved = url
                }) { Text("Save") }
            }
        }
        UpdateStatus(context)
    }
}

/** What updating is doing, and Update when there's one. */
@Composable
fun UpdateStatus(context: Context) {
    when (val s = Updates.state) {
        UpdateState.Idle -> {}
        UpdateState.Checking -> Text("Checking…", color = Faint)
        is UpdateState.UpToDate -> Text("Up to date.", color = Faint)
        is UpdateState.Available -> Row(verticalAlignment = Alignment.CenterVertically) {
            Text("artty ${s.release.versionName} is out.", modifier = Modifier.weight(1f))
            Button(onClick = { Updates.install(context, s.release) }) { Text("Update") }
        }
        is UpdateState.Downloading -> Text(
            "Downloading… " + if (s.release.size > 0) "${s.done * 100 / s.release.size}%" else "${s.done / 1024} KiB",
            color = Faint,
        )
        is UpdateState.Installing -> Text("Installing ${s.release.versionName}: the system asks you to confirm.", color = Faint)
        is UpdateState.Failed -> Text(s.reason, color = Warn, fontSize = 13.sp)
    }
}

// Backing up.

@Composable
private fun BackupPage(store: Store) {
    val context = LocalContext.current
    var withKeys by remember { mutableStateOf(false) }
    var said by remember { mutableStateOf<String?>(null) }
    val export = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/json")) { uri ->
        if (uri != null) {
            said = runCatching {
                context.contentResolver.openOutputStream(uri)?.use { it.write(store.backup(withKeys).toByteArray()) }
                "Saved."
            }.getOrElse { "Couldn't save: ${it.message}" }
        }
    }
    val import = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) {
            said = readText(context, uri)?.let { store.restore(it).getOrElse { e -> "Couldn't restore: ${e.message}" } } ?: "Couldn't read it"
        }
    }
    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("A backup has the servers, the ssh config, the machines' keys and the terminal's settings: for another phone, or this one again.", color = Faint)
        Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.clickable { withKeys = !withKeys }) {
            Checkbox(withKeys, { withKeys = it })
            Column {
                Text("With the keys added")
                Text("Their private halves, readable by whoever has the file. (This phone's own key can't leave it.)", color = if (withKeys) Warn else Faint, fontSize = 12.sp)
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { export.launch("artty-backup.json") }) { Text("Back up") }
            OutlinedButton(onClick = { import.launch(arrayOf("application/json", "*/*")) }) { Text("Restore") }
        }
        Text("Restoring adds its servers and keys to these, its machines' keys too, and takes its ssh config and settings in place of these.", color = Faint, fontSize = 12.sp)
        said?.let { Text(it) }
    }
}
