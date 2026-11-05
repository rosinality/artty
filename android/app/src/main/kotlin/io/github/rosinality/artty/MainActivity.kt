package io.github.rosinality.artty

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import uniffi.artty_android.TabItem

private val Background = Color(0xff161a22)
private val Panel = Color(0xff1e2430)
private val Ink = Color(0xffd8dee9)
private val Faint = Color(0x99d8dee9)
private val Accent = Color(0xff7aa2f7)

class MainActivity : ComponentActivity() {
    private lateinit var store: Store

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = Store(applicationContext)
        Session.copied = { text ->
            getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("artty", text))
        }
        setContent {
            MaterialTheme(
                colorScheme = darkColorScheme(
                    primary = Accent,
                    background = Background,
                    surface = Panel,
                    onSurface = Ink,
                    onBackground = Ink,
                ),
            ) {
                Surface(Modifier.fillMaxSize(), color = Background, contentColor = Ink) {
                    Box(Modifier.fillMaxSize().safeDrawingPadding()) { App(store) }
                }
            }
        }
    }

    // In front here, its window's panes take this phone's size; away, the other apps'.
    override fun onResume() {
        super.onResume()
        Session.focus(true)
    }

    override fun onPause() {
        super.onPause()
        Session.focus(false)
    }
}

@Composable
private fun App(store: Store) {
    when (val phase = Session.phase) {
        Phase.Servers -> ServersScreen(store)
        is Phase.Connecting -> ConnectingScreen(phase)
        is Phase.Choosing -> WindowsScreen(phase)
        is Phase.Shown -> TerminalScreen()
        is Phase.Failed -> FailedScreen(phase, store)
    }
    Session.question?.let { QuestionDialog(it) }
}

@Composable
private fun Heading(title: String, action: @Composable () -> Unit = {}) {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(title, fontSize = 20.sp, fontWeight = FontWeight.SemiBold, modifier = Modifier.weight(1f))
        action()
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ServersScreen(store: Store) {
    val servers = remember { mutableStateListOf<Server>().apply { addAll(store.servers()) } }
    var adding by remember { mutableStateOf(false) }
    var showingKey by remember { mutableStateOf(false) }
    var removing by remember { mutableStateOf<Server?>(null) }
    Column(Modifier.fillMaxSize()) {
        Heading("artty") { TextButton(onClick = { showingKey = true }) { Text("This phone's key") } }
        LazyColumn(Modifier.weight(1f)) {
            items(servers) { server ->
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 4.dp)
                        .clip(RoundedCornerShape(10.dp))
                        .background(Panel)
                        .combinedClickable(onClick = { Session.connect(server, store) }, onLongClick = { removing = server })
                        .padding(14.dp),
                ) {
                    Text(server.name, fontWeight = FontWeight.Medium)
                    Text(server.address, color = Faint, fontSize = 13.sp)
                }
            }
            if (servers.isEmpty()) {
                item {
                    Text(
                        "Add a server: a machine running artty, reached by ssh. This phone logs in with its own key (see “This phone's key”).",
                        color = Faint,
                        modifier = Modifier.padding(16.dp),
                    )
                }
            }
        }
        Button(onClick = { adding = true }, modifier = Modifier.fillMaxWidth().padding(12.dp)) { Text("Add a server") }
    }
    if (adding) {
        AddServerDialog(onDone = { server ->
            adding = false
            if (server != null) {
                servers.add(server)
                store.saveServers(servers)
            }
        })
    }
    if (showingKey) KeyDialog(store) { showingKey = false }
    removing?.let { server ->
        AlertDialog(
            onDismissRequest = { removing = null },
            title = { Text("Remove ${server.name}?") },
            confirmButton = {
                TextButton(onClick = {
                    servers.remove(server)
                    store.saveServers(servers)
                    removing = null
                }) { Text("Remove") }
            },
            dismissButton = { TextButton(onClick = { removing = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun AddServerDialog(onDone: (Server?) -> Unit) {
    var name by remember { mutableStateOf("") }
    var address by remember { mutableStateOf("") }
    var artty by remember { mutableStateOf("") }
    val parsed = Server.parse(name, address.trim(), artty.trim())
    AlertDialog(
        onDismissRequest = { onDone(null) },
        title = { Text("Add a server") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(name, { name = it }, label = { Text("Name") }, singleLine = true)
                OutlinedTextField(address, { address = it }, label = { Text("user@host or user@host:port") }, singleLine = true)
                OutlinedTextField(artty, { artty = it }, label = { Text("artty there (if not on its PATH)") }, singleLine = true)
            }
        },
        confirmButton = { TextButton(onClick = { onDone(parsed) }, enabled = parsed != null) { Text("Add") } },
        dismissButton = { TextButton(onClick = { onDone(null) }) { Text("Cancel") } },
    )
}

@Composable
private fun KeyDialog(store: Store, onDone: () -> Unit) {
    val context = LocalContext.current
    val key = remember { store.publicKey() }
    AlertDialog(
        onDismissRequest = onDone,
        title = { Text("This phone's key") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Add it to ~/.ssh/authorized_keys on each server, and this phone can log in.", color = Faint)
                SelectionContainer { Text(key, fontFamily = FontFamily.Monospace, fontSize = 12.sp) }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("key", key))
                onDone()
            }) { Text("Copy") }
        },
        dismissButton = { TextButton(onClick = onDone) { Text("Close") } },
    )
}

@Composable
private fun ConnectingScreen(phase: Phase.Connecting) {
    val context = LocalContext.current
    val url = phase.said.firstNotNullOfOrNull { Regex("https://\\S+").find(it)?.value }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            Spacer(Modifier.width(12.dp))
            Text("Logging in to ${phase.server.name}…")
        }
        for (line in phase.said) Text(line, color = Faint, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        if (url != null) {
            Text("It waits for you to sign in; then it goes on.", color = Faint)
            Button(onClick = { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url))) }) { Text("Open in the browser") }
        }
        TextButton(onClick = { Session.disconnect() }) { Text("Cancel") }
    }
}

@Composable
private fun WindowsScreen(phase: Phase.Choosing) {
    Column(Modifier.fillMaxSize()) {
        Heading(phase.server.name) { TextButton(onClick = { Session.disconnect() }) { Text("Disconnect") } }
        val windows = phase.windows
        if (windows == null) {
            CircularProgressIndicator(Modifier.padding(24.dp))
            return@Column
        }
        LazyColumn(Modifier.weight(1f)) {
            items(windows) { window ->
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 4.dp)
                        .clip(RoundedCornerShape(10.dp))
                        .background(Panel)
                        .combinedClickableCompat { Session.show(window.window) }
                        .padding(14.dp),
                ) {
                    val shown = when (window.apps) {
                        0u -> "not shown"
                        1u -> "shown in another app"
                        else -> "shown in ${window.apps} apps"
                    }
                    Text("${window.tabs} tab${if (window.tabs == 1u) "" else "s"} · $shown", fontWeight = FontWeight.Medium)
                    Text(window.titles.joinToString(" · "), color = Faint, fontSize = 13.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
                }
            }
        }
        Button(onClick = { Session.show(null) }, modifier = Modifier.fillMaxWidth().padding(12.dp)) { Text("New window") }
    }
}

@OptIn(ExperimentalFoundationApi::class)
private fun Modifier.combinedClickableCompat(onClick: () -> Unit) = combinedClickable(onClick = onClick)

@Composable
private fun TerminalScreen() {
    val context = LocalContext.current
    val view = remember { TerminalView(context) }
    var ctrl by remember { mutableStateOf(false) }
    var alt by remember { mutableStateOf(false) }
    DisposableEffect(view) {
        Session.onScreen = { pane -> if (pane == view.pane) view.refresh() }
        view.onModifiers = {
            ctrl = false
            alt = false
        }
        onDispose { Session.onScreen = null }
    }
    val tab = Session.activeTab
    Column(Modifier.fillMaxSize()) {
        TabBar(tab)
        AndroidView(factory = { view.also { it.post { it.showKeyboard() } } }, modifier = Modifier.weight(1f).fillMaxWidth()) {
            it.pane = Session.pane
        }
        Row(
            Modifier.fillMaxWidth().background(Panel).horizontalScroll(rememberScrollState()).padding(4.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            // The keyboard (a tap on a program that takes the mouse clicks instead).
            KeyButton("⌨") { view.showKeyboard() }
            KeyButton("Esc") { view.key("escape") }
            KeyButton("Tab") { view.key("tab") }
            KeyButton("Ctrl", on = ctrl) {
                ctrl = !ctrl
                view.ctrl = ctrl
            }
            KeyButton("Alt", on = alt) {
                alt = !alt
                view.alt = alt
            }
            KeyButton("←") { view.key("left") }
            KeyButton("↓") { view.key("down") }
            KeyButton("↑") { view.key("up") }
            KeyButton("→") { view.key("right") }
            for (c in listOf("|", "-", "/", "~", "_")) KeyButton(c) { view.type(c) }
        }
    }
}

@Composable
private fun TabBar(tab: TabItem?) {
    var open by remember { mutableStateOf(false) }
    Row(Modifier.fillMaxWidth().height(44.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        TextButton(onClick = { Session.disconnect() }) { Text("‹", fontSize = 20.sp) }
        Box(Modifier.weight(1f)) {
            Text(
                tab?.let { "${it.workspaceName} · ${it.title.ifBlank { "shell" }}" } ?: "",
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.combinedClickableCompat { open = true }.padding(8.dp),
            )
            DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                for (item in Session.tabs) {
                    DropdownMenuItem(
                        text = {
                            Text(
                                "${item.workspaceName} · ${item.title.ifBlank { "shell" }}",
                                color = if (item.active) Accent else Ink,
                            )
                        },
                        onClick = {
                            open = false
                            Session.selectTab(item)
                        },
                    )
                }
            }
        }
        // A split tab: its panes, one at a time.
        if (tab != null && tab.panes.size > 1) {
            for ((i, pane) in tab.panes.withIndex()) {
                KeyButton("${i + 1}", on = pane == Session.pane) { Session.chosenPane = pane }
            }
        }
    }
}

@Composable
private fun KeyButton(label: String, on: Boolean = false, onClick: () -> Unit) {
    Box(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(if (on) Accent.copy(alpha = 0.45f) else Color(0x22ffffff))
            .combinedClickableCompat(onClick)
            .padding(horizontal = 12.dp, vertical = 8.dp),
    ) { Text(label, fontFamily = FontFamily.Monospace, fontSize = 14.sp) }
}

@Composable
private fun FailedScreen(phase: Phase.Failed, store: Store) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Couldn't connect to ${phase.server.name}", fontWeight = FontWeight.SemiBold)
        Text(phase.reason, color = Faint, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { Session.connect(phase.server, store) }) { Text("Try again") }
            TextButton(onClick = { Session.disconnect() }) { Text("Back") }
        }
    }
}

@Composable
private fun QuestionDialog(question: Question) {
    when (question) {
        is Question.HostKey -> AlertDialog(
            onDismissRequest = { question.reply.complete(false) },
            title = { Text(if (question.changed) "${question.host}'s key changed" else "Trust ${question.host}?") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (question.changed) {
                        Text("It isn't the key recorded for it: someone may be in between. Not logging in.", color = Color(0xfff7768e))
                    } else {
                        Text("Its key, seen for the first time:", color = Faint)
                    }
                    Text(question.fingerprint, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
                }
            },
            confirmButton = {
                if (!question.changed) TextButton(onClick = { question.reply.complete(true) }) { Text("Trust") }
            },
            dismissButton = { TextButton(onClick = { question.reply.complete(false) }) { Text("Cancel") } },
        )
        is Question.Answers -> {
            val answers = remember(question) { mutableStateListOf(*Array(question.prompts.size) { "" }) }
            AlertDialog(
                onDismissRequest = { question.reply.complete(null) },
                title = { Text("Logging in") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (question.instructions.isNotBlank()) Text(question.instructions, color = Faint)
                        for ((i, prompt) in question.prompts.withIndex()) {
                            OutlinedTextField(
                                answers[i],
                                { answers[i] = it },
                                label = { Text(prompt.text.trim()) },
                                singleLine = true,
                                visualTransformation = if (prompt.echo) VisualTransformation.None else PasswordVisualTransformation(),
                            )
                        }
                    }
                },
                confirmButton = { TextButton(onClick = { question.reply.complete(answers.toList()) }) { Text("Log In") } },
                dismissButton = { TextButton(onClick = { question.reply.complete(null) }) { Text("Cancel") } },
            )
        }
    }
}
