package io.github.rosinality.artty

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.widget.Toast
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
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.rememberDrawerState
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
import androidx.compose.runtime.rememberCoroutineScope
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
import kotlinx.coroutines.launch
import uniffi.artty_android.TabItem

internal val Background = Color(0xff161a22)
internal val Panel = Color(0xff1e2430)
internal val Ink = Color(0xffd8dee9)
internal val Faint = Color(0x99d8dee9)
internal val Accent = Color(0xff7aa2f7)

class MainActivity : ComponentActivity() {
    private lateinit var store: Store

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = Store(applicationContext)
        Session.init(applicationContext, store)
        // For the notification saying it's connected (and, later, what needs you).
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
        }
        // Started by a notification (the activity had gone; the connection may not have).
        if (savedInstanceState == null) reveal(intent)
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

    // A notification tapped: the pane it's about.
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        reveal(intent)
    }

    private fun reveal(intent: Intent?) {
        if (intent?.action != Alerts.SHOW || !intent.hasExtra(Alerts.PANE)) return
        Session.reveal(intent.getLongExtra(Alerts.WINDOW, 0).toULong(), intent.getLongExtra(Alerts.PANE, 0).toULong())
    }

    // In front here, its window's panes take this phone's size; away, the other apps'.
    override fun onResume() {
        super.onResume()
        Session.resumed = true
        Session.focus(true)
        // Back after a while (the phone slept, say): still there?
        Session.probe()
    }

    override fun onPause() {
        super.onPause()
        Session.resumed = false
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
        is Phase.AnotherBuild -> AnotherBuildScreen(phase)
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
    /** The dialog open: adding (`null` server) or editing one. */
    var editing by remember { mutableStateOf<Pair<Boolean, Server?>?>(null) }
    var showingKey by remember { mutableStateOf(false) }
    var removing by remember { mutableStateOf<Server?>(null) }
    fun save() = store.saveServers(servers)
    fun move(server: Server, by: Int) {
        val from = servers.indexOf(server)
        val to = from + by
        if (from < 0 || to !in servers.indices) return
        servers.removeAt(from)
        servers.add(to, server)
        save()
    }
    Column(Modifier.fillMaxSize()) {
        Heading("artty") { TextButton(onClick = { showingKey = true }) { Text("This phone's key") } }
        LazyColumn(Modifier.weight(1f)) {
            itemsIndexed(servers) { index, server ->
                var menu by remember { mutableStateOf(false) }
                Row(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 4.dp)
                        .clip(RoundedCornerShape(10.dp))
                        .background(Panel)
                        .combinedClickable(onClick = { Session.connect(server) }, onLongClick = { menu = true })
                        .padding(start = 14.dp, top = 6.dp, bottom = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f).padding(vertical = 8.dp)) {
                        Text(server.name, fontWeight = FontWeight.Medium)
                        Text(server.address, color = Faint, fontSize = 13.sp)
                    }
                    Box {
                        TextButton(onClick = { menu = true }) { Text("⋯", fontSize = 20.sp, color = Faint) }
                        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                            DropdownMenuItem(text = { Text("Edit") }, onClick = {
                                menu = false
                                editing = true to server
                            })
                            if (index > 0) {
                                DropdownMenuItem(text = { Text("Move up") }, onClick = {
                                    menu = false
                                    move(server, -1)
                                })
                            }
                            if (index < servers.lastIndex) {
                                DropdownMenuItem(text = { Text("Move down") }, onClick = {
                                    menu = false
                                    move(server, 1)
                                })
                            }
                            DropdownMenuItem(text = { Text("Remove") }, onClick = {
                                menu = false
                                removing = server
                            })
                        }
                    }
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
        Button(onClick = { editing = true to null }, modifier = Modifier.fillMaxWidth().padding(12.dp)) { Text("Add a server") }
    }
    editing?.let { (_, old) ->
        ServerDialog(old, onDone = { server ->
            editing = null
            if (server != null) {
                val at = old?.let { servers.indexOf(it) } ?: -1
                if (at >= 0) servers[at] = server else servers.add(server)
                save()
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
                    save()
                    removing = null
                }) { Text("Remove") }
            },
            dismissButton = { TextButton(onClick = { removing = null }) { Text("Cancel") } },
        )
    }
}

/** Adding a server (`old` null), or changing one. */
@Composable
private fun ServerDialog(old: Server?, onDone: (Server?) -> Unit) {
    var name by remember { mutableStateOf(old?.name ?: "") }
    var address by remember { mutableStateOf(old?.address ?: "") }
    var artty by remember { mutableStateOf(old?.artty ?: "") }
    val parsed = Server.parse(name, address.trim(), artty.trim())
    val badAddress = address.isNotBlank() && parsed == null
    AlertDialog(
        onDismissRequest = { onDone(null) },
        title = { Text(if (old == null) "Add a server" else "Edit ${old.name}") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(name, { name = it }, label = { Text("Name") }, singleLine = true)
                OutlinedTextField(
                    address, { address = it },
                    label = { Text("user@host or user@host:port") },
                    singleLine = true,
                    isError = badAddress,
                    supportingText = if (badAddress) {
                        { Text("Like me@devbox, or me@devbox:2222") }
                    } else {
                        null
                    },
                )
                OutlinedTextField(artty, { artty = it }, label = { Text("artty there (if not on its PATH)") }, singleLine = true)
            }
        },
        confirmButton = {
            TextButton(onClick = { onDone(parsed) }, enabled = parsed != null) { Text(if (old == null) "Add" else "Save") }
        },
        dismissButton = { TextButton(onClick = { onDone(null) }) { Text("Cancel") } },
    )
}

@Composable
private fun KeyDialog(store: Store, onDone: () -> Unit) {
    val context = LocalContext.current
    val key = remember { store.publicKey() }
    val keptIn = remember { store.phoneKey.keptIn }
    var oldKey by remember { mutableStateOf(store.oldPublicKey()) }
    var forgetting by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDone,
        title = { Text("This phone's key") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Add it to ~/.ssh/authorized_keys on each server, and this phone can log in.", color = Faint)
                SelectionContainer { Text(key, fontFamily = FontFamily.Monospace, fontSize = 12.sp) }
                val kept = if ("software" in keptIn) {
                    "Its private half is kept in $keptIn: the app can't read it, and it isn't backed up. (This phone has no secure hardware for it.)"
                } else {
                    "Its private half is kept in $keptIn: it can't be read out, copied off the phone, or backed up."
                }
                Text(kept, color = Faint, fontSize = 13.sp)
                oldKey?.let { old ->
                    Spacer(Modifier.height(4.dp))
                    Text("The key this app made before is still tried after it:", color = Faint, fontSize = 13.sp)
                    Text(old, fontFamily = FontFamily.Monospace, fontSize = 11.sp, maxLines = 2, overflow = TextOverflow.Ellipsis, color = Faint)
                    TextButton(onClick = { forgetting = true }) { Text("Forget the old key") }
                }
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
    if (forgetting) {
        AlertDialog(
            onDismissRequest = { forgetting = false },
            title = { Text("Forget the old key?") },
            text = { Text("Servers that have only the old key in authorized_keys won't let this phone in until the new one is added there. (Remove the old one there too.)") },
            confirmButton = {
                TextButton(onClick = {
                    store.forgetOldKey()
                    oldKey = null
                    forgetting = false
                }) { Text("Forget") }
            },
            dismissButton = { TextButton(onClick = { forgetting = false }) { Text("Cancel") } },
        )
    }
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
    var selectMode by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf(false) }
    /** Text that would run line by line, waiting for a yes. */
    var pasting by remember { mutableStateOf<String?>(null) }
    val clipboard = context.getSystemService(ClipboardManager::class.java)
    val paste = {
        val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
        if (!text.isNullOrEmpty()) {
            if (view.pasteRunsLines(text)) pasting = text else view.paste(text)
        }
    }
    DisposableEffect(view) {
        Session.onScreen = { pane -> if (pane == view.pane) view.refresh() }
        Session.onReattached = { view.reattached() }
        view.onSelection = { selected = it }
        view.onModifiers = {
            ctrl = false
            alt = false
        }
        onDispose {
            Session.onScreen = null
            Session.onReattached = null
        }
    }
    val tab = Session.activeTab
    val shown = Session.phase as? Phase.Shown
    val reconnecting = shown?.reconnecting
    val drawer = rememberDrawerState(DrawerValue.Closed)
    val scope = rememberCoroutineScope()
    ModalNavigationDrawer(
        drawerState = drawer,
        // Opened by its button: a swipe from the edge belongs to the terminal (and its programs).
        gesturesEnabled = drawer.isOpen,
        drawerContent = { TabsDrawer(shown?.server?.name ?: "") { scope.launch { drawer.close() } } },
    ) {
        Column(Modifier.fillMaxSize()) {
            TabBar(tab, onTabs = { scope.launch { drawer.open() } }, onPaste = paste)
            // Over the terminal (not above it): showing them mustn't resize the pane.
            Box(Modifier.weight(1f).fillMaxWidth()) {
                AndroidView(factory = { view.also { it.post { it.showKeyboard() } } }, modifier = Modifier.fillMaxSize()) {
                    it.pane = Session.pane
                }
                Column(Modifier.align(Alignment.TopCenter).fillMaxWidth()) {
                    // The last screen stays meanwhile; typing waits for the connection.
                    if (reconnecting != null) {
                        Row(
                            Modifier.fillMaxWidth().background(Color(0x33e0af68)).padding(horizontal = 12.dp, vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp, color = Color(0xffe0af68))
                            Spacer(Modifier.width(8.dp))
                            Text("Reconnecting… ($reconnecting)", fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                    // Something selected: copy it, or let it go.
                    if (selected) {
                        Row(
                            Modifier.fillMaxWidth().background(Panel).padding(horizontal = 8.dp, vertical = 4.dp),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text("Drag to select more", color = Faint, fontSize = 13.sp, modifier = Modifier.weight(1f))
                            KeyButton("Copy") {
                                view.copy { text ->
                                    if (text != null) {
                                        clipboard.setPrimaryClip(ClipData.newPlainText("artty", text))
                                        // Android 13 and later say so themselves.
                                        if (Build.VERSION.SDK_INT < 33) Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
                                    }
                                }
                            }
                            KeyButton("Clear") { view.clearSelection() }
                        }
                    }
                }
            }
            Row(
                Modifier.fillMaxWidth().background(Panel).horizontalScroll(rememberScrollState()).padding(4.dp),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                // The keyboard (a tap on a program that takes the mouse clicks instead).
                KeyButton("⌨") { view.showKeyboard() }
                // Touches select, even where the program takes the mouse.
                KeyButton("Select", on = selectMode) {
                    selectMode = !selectMode
                    view.selectMode = selectMode
                }
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
    pasting?.let { text ->
        AlertDialog(
            onDismissRequest = { pasting = null },
            title = { Text("Paste ${text.trimEnd().lines().size} lines?") },
            text = { Text("The program here hasn't asked for pastes whole, so each line may run as a command as it arrives.", color = Faint) },
            confirmButton = {
                TextButton(onClick = {
                    view.paste(text)
                    pasting = null
                }) { Text("Paste") }
            },
            dismissButton = { TextButton(onClick = { pasting = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun TabBar(tab: TabItem?, onTabs: () -> Unit, onPaste: () -> Unit) {
    Row(Modifier.fillMaxWidth().height(44.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        TextButton(onClick = { Session.disconnect() }) { Text("‹", fontSize = 20.sp) }
        // The workspaces and tabs; a dot when another tab wants you.
        Box {
            TextButton(onClick = onTabs) { Text("☰", fontSize = 18.sp) }
            if (othersWaiting()) {
                Box(Modifier.align(Alignment.TopEnd).padding(top = 10.dp, end = 10.dp).size(8.dp).clip(CircleShape).background(Color(0xffe0af68)))
            }
        }
        Text(
            tab?.let { "${it.workspaceName} · ${Session.title(it)}" } ?: "",
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f).combinedClickableCompat(onTabs).padding(8.dp),
        )
        // A split tab: its panes, one at a time.
        if (tab != null && tab.panes.size > 1) {
            for ((i, pane) in tab.panes.withIndex()) {
                KeyButton("${i + 1}", on = pane == Session.pane) { Session.chosenPane = pane }
            }
        }
        // Here, not in the keys row: just after copying, Android's clipboard preview covers that.
        TextButton(onClick = onPaste) { Text("Paste") }
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
            Button(onClick = { Session.connect(phase.server) }) { Text("Try again") }
            TextButton(onClick = { Session.disconnect() }) { Text("Back") }
        }
    }
}

@Composable
private fun AnotherBuildScreen(phase: Phase.AnotherBuild) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("${phase.server.name} runs another artty build", fontWeight = FontWeight.SemiBold)
        Text(
            "Its server is ${describeBuild(phase.serverVersion)}. It and this app have to be the same build " +
                "to understand each other.",
            color = Faint,
        )
        Text(
            "Installing this app's build there puts it in ~/.local/bin/artty and moves the running server " +
                "onto it: its programs keep running. Apps of another build using it (the Mac app, say) will " +
                "then need this build too.",
            color = Faint,
        )
        phase.error?.let {
            Text("Couldn't install: $it", color = Color(0xfff7768e), fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        }
        if (phase.installing) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                Spacer(Modifier.width(12.dp))
                Text("Installing…")
            }
        } else {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { Session.installHere() }) { Text("Install this app's build") }
                TextButton(onClick = { Session.disconnect() }) { Text("Back") }
            }
        }
    }
}

/** A build id (`0.1.0+<nanoseconds since 1970>`, when it was built or installed), said. */
private fun describeBuild(id: String): String {
    val (version, stamp) = id.split('+', limit = 2).let { it[0] to it.getOrNull(1) }
    val nanos = stamp?.toBigIntegerOrNull() ?: return "artty $id"
    val time = java.time.Instant.ofEpochMilli(nanos.divide(java.math.BigInteger.valueOf(1_000_000)).toLong())
    val shown = java.time.format.DateTimeFormatter.ofPattern("MMM d, HH:mm")
        .withZone(java.time.ZoneId.systemDefault())
        .format(time)
    return "artty $version from $shown"
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
