package io.github.rosinality.artty

import android.content.ActivityNotFoundException
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
import androidx.compose.material3.Switch
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
import androidx.compose.foundation.verticalScroll
import uniffi.artty_android.LinkSpan
import uniffi.artty_android.checkConfig
import uniffi.artty_android.resolve
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
        // A new build published? (Every few hours at most.)
        Updates.check(this, store, unlessWithinMs = 6 * 60 * 60 * 1000L)
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
        is Phase.Shown -> TerminalScreen(store)
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
    var settings by remember { mutableStateOf(false) }
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
        Heading("artty") { TextButton(onClick = { settings = true }) { Text("Settings") } }
        // A new build published (Settings, Updates).
        if (Updates.state is UpdateState.Available || Updates.state is UpdateState.Downloading || Updates.state is UpdateState.Installing) {
            Box(Modifier.padding(horizontal = 12.dp, vertical = 4.dp).clip(RoundedCornerShape(10.dp)).background(Panel).padding(14.dp)) {
                UpdateStatus(LocalContext.current)
            }
        }
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
                        val kind = when {
                            server.plain && server.tmux -> " · plain ssh, tmux"
                            server.plain -> " · plain ssh"
                            else -> ""
                        }
                        Text(server.destination + kind, color = Faint, fontSize = 13.sp)
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
                        "Add a server: a machine running artty, reached by ssh, or any other with plain ssh. This phone logs in with its own key (Settings, Keys), or keys added; the ssh config (Settings) can say how to reach each.",
                        color = Faint,
                        modifier = Modifier.padding(16.dp),
                    )
                }
            }
        }
        Button(onClick = { editing = true to null }, modifier = Modifier.fillMaxWidth().padding(12.dp)) { Text("Add a server") }
    }
    editing?.let { (_, old) ->
        ServerDialog(store, old, onDone = { server ->
            editing = null
            if (server != null) {
                val at = old?.let { servers.indexOf(it) } ?: -1
                if (at >= 0) servers[at] = server else servers.add(server)
                save()
            }
        })
    }
    if (settings) {
        Surface(Modifier.fillMaxSize(), color = Background, contentColor = Ink) {
            SettingsScreen(store) { settings = false }
        }
    }
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
private fun ServerDialog(store: Store, old: Server?, onDone: (Server?) -> Unit) {
    var name by remember { mutableStateOf(old?.name ?: "") }
    var destination by remember { mutableStateOf(old?.destination ?: "") }
    var artty by remember { mutableStateOf(old?.artty ?: "") }
    var plain by remember { mutableStateOf(old?.plain ?: false) }
    var tmux by remember { mutableStateOf(old?.tmux ?: false) }
    var hostsShown by remember { mutableStateOf(false) }
    val home = store.sshHome.path
    val hosts = remember { checkConfig(store.sshConfig(), home).hosts }
    val resolution = remember(destination) { destination.trim().takeIf { it.isNotEmpty() }?.let { resolve(it, home) } }
    val server = Server(
        name.trim().ifBlank { destination.trim() },
        destination.trim(),
        artty.trim().ifBlank { null }.takeUnless { plain },
        plain,
        tmux && plain,
    ).takeIf { destination.isNotBlank() && resolution?.error == null }
    AlertDialog(
        onDismissRequest = { onDone(null) },
        title = { Text(if (old == null) "Add a server" else "Edit ${old.name}") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(name, { name = it }, label = { Text("Name") }, singleLine = true)
                Box {
                    OutlinedTextField(
                        destination, { destination = it },
                        label = { Text("Host from the ssh config, or user@host[:port]") },
                        singleLine = true,
                        isError = resolution?.error != null,
                        trailingIcon = if (hosts.isNotEmpty()) {
                            { TextButton(onClick = { hostsShown = true }) { Text("▾") } }
                        } else {
                            null
                        },
                    )
                    DropdownMenu(expanded = hostsShown, onDismissRequest = { hostsShown = false }) {
                        for (host in hosts) {
                            DropdownMenuItem(text = { Text(host) }, onClick = {
                                destination = host
                                if (name.isBlank()) name = host
                                hostsShown = false
                            })
                        }
                    }
                }
                resolution?.let { r ->
                    val lines = buildList {
                        r.error?.let { add(it) }
                        r.address?.let { add(if (r.through.isEmpty()) it else "$it, through ${r.through.joinToString(", ")}") }
                        addAll(r.details)
                        if (r.ignored.isNotEmpty()) add("Not done here: ${r.ignored.joinToString(", ")}")
                    }
                    for (line in lines) {
                        Text(line, color = if (line == r.error) Color(0xfff7768e) else Faint, fontFamily = FontFamily.Monospace, fontSize = 11.sp)
                    }
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text("artty on it")
                        Text(
                            if (plain) "Plain ssh: its shells, in tabs, ending when the connection does" else "Tabs that keep running, its files, agents",
                            color = Faint,
                            fontSize = 12.sp,
                        )
                    }
                    Switch(checked = !plain, onCheckedChange = { plain = !it })
                }
                if (!plain) {
                    OutlinedTextField(artty, { artty = it }, label = { Text("artty there (if not on its PATH)") }, singleLine = true)
                } else {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text("Keep shells in tmux")
                            Text(
                                "Each tab's shell runs in a tmux session there: it keeps running when the connection drops, and connecting again takes it up. Closing the tab ends it.",
                                color = Faint,
                                fontSize = 12.sp,
                            )
                        }
                        Switch(checked = tmux, onCheckedChange = { tmux = it })
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onDone(server) }, enabled = server != null) { Text(if (old == null) "Add" else "Save") }
        },
        dismissButton = { TextButton(onClick = { onDone(null) }) { Text("Cancel") } },
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
        var log by remember { mutableStateOf(false) }
        Row {
            TextButton(onClick = { Session.disconnect() }) { Text("Cancel") }
            TextButton(onClick = { log = !log }) { Text(if (log) "Hide the log" else "Show the log") }
        }
        if (log) ConnectionLog(Modifier.fillMaxWidth().weight(1f))
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
private fun TerminalScreen(store: Store) {
    val context = LocalContext.current
    val view = remember { TerminalView(context) }
    val rowKeys = remember { store.rowKeys.split(' ').filter { it.isNotEmpty() } }
    var ctrl by remember { mutableStateOf(false) }
    var alt by remember { mutableStateOf(false) }
    /** Ctrl or Alt stays on (a long press) until tapped again. */
    var locked by remember { mutableStateOf(false) }
    var selectMode by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf(false) }
    /** A link tapped: open it, or copy it. */
    var link by remember { mutableStateOf<LinkSpan?>(null) }
    /** A file to open in Files (a `file://` link tapped). */
    var openFile by remember { mutableStateOf<String?>(null) }
    /** Text that would run line by line, waiting for a yes. */
    var pasting by remember { mutableStateOf<String?>(null) }
    val clipboard = context.getSystemService(ClipboardManager::class.java)
    val paste = {
        val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
        if (!text.isNullOrEmpty()) {
            if (view.pasteRunsLines(text)) pasting = text else view.paste(text)
        }
    }
    val copy = {
        view.copy { text ->
            if (text != null) {
                clipboard.setPrimaryClip(ClipData.newPlainText("artty", text))
                // Android 13 and later say so themselves.
                if (Build.VERSION.SDK_INT < 33) Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
            }
        }
    }
    val tab = Session.activeTab
    val shown = Session.phase as? Phase.Shown
    val reconnecting = shown?.reconnecting
    val drawer = rememberDrawerState(DrawerValue.Closed)
    val scope = rememberCoroutineScope()
    var finding by remember { mutableStateOf(false) }
    /** Search everything, open (with what to search for). */
    var searchingAll by remember { mutableStateOf<String?>(null) }
    var browsing by remember { mutableStateOf(false) }
    /** Another tab of the window, `by` along (wrapping). */
    fun otherTab(by: Int) {
        val tabs = Session.tabs
        val at = tabs.indexOfFirst { it.active }
        if (tabs.size > 1 && at >= 0) Session.selectTab(tabs[(at + by).mod(tabs.size)])
    }
    DisposableEffect(view) {
        Session.onScreen = { pane -> if (pane == view.pane) view.refresh() }
        Session.onImage = { view.imageArrived(it) }
        Session.onReattached = { view.reattached() }
        view.onSelection = { selected = it }
        view.onLink = { link = it }
        view.onModifiers = {
            ctrl = false
            alt = false
        }
        // As the Mac's ⌘ keys (with Meta, or Ctrl+Shift as Linux terminals have them).
        view.onShortcut = { shortcut ->
            when (shortcut) {
                Shortcut.NewTab -> Session.newTab(Session.activeTab?.workspace)
                Shortcut.CloseTab -> Session.activeTab?.let { Session.closeTab(it) }
                Shortcut.NextTab -> otherTab(1)
                Shortcut.PreviousTab -> otherTab(-1)
                is Shortcut.Tab -> Session.tabs.getOrNull(shortcut.index)?.let { Session.selectTab(it) }
                // Nothing selected: nothing to copy (and not Ctrl+C either).
                Shortcut.Copy -> if (view.hasSelection) copy()
                Shortcut.Paste -> paste()
                Shortcut.Find -> finding = true
                Shortcut.Bigger -> view.biggerText(1f)
                Shortcut.Smaller -> view.biggerText(-1f)
                Shortcut.ResetSize -> view.setFontSize(null)
            }
            true
        }
        onDispose {
            Session.onScreen = null
            Session.onImage = null
            Session.onReattached = null
        }
    }
    ModalNavigationDrawer(
        drawerState = drawer,
        // Opened by its button: a swipe from the edge belongs to the terminal (and its programs).
        gesturesEnabled = drawer.isOpen,
        drawerContent = {
            TabsDrawer(shown?.server?.name ?: "", onSearch = {
                scope.launch { drawer.close() }
                searchingAll = ""
            }, onFiles = {
                scope.launch { drawer.close() }
                browsing = true
            }) { scope.launch { drawer.close() } }
        },
    ) {
        Column(Modifier.fillMaxSize()) {
            TabBar(tab, onTabs = { scope.launch { drawer.open() } }, onFind = { finding = true }, onPaste = paste)
            // Over the terminal (not above it): showing them mustn't resize the pane.
            Box(Modifier.weight(1f).fillMaxWidth()) {
                AndroidView(factory = { view.also { it.post { it.showKeyboard() } } }, modifier = Modifier.fillMaxSize()) {
                    it.pane = Session.pane
                }
                Column(Modifier.align(Alignment.TopCenter).fillMaxWidth()) {
                    val pane = Session.pane
                    if (finding && pane != null) {
                        FindBar(pane, onConversations = {
                            finding = false
                            Session.endFind(pane)
                            searchingAll = it
                        }) { finding = false }
                    }
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
                            KeyButton("Copy") { copy() }
                            KeyButton("Clear") { view.clearSelection() }
                        }
                    }
                    // A link tapped (underlined meanwhile).
                    link?.let { span ->
                        val done = {
                            link = null
                            view.link = null
                        }
                        Row(
                            Modifier.fillMaxWidth().background(Panel).padding(horizontal = 8.dp, vertical = 4.dp),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(span.url, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.MiddleEllipsis, modifier = Modifier.weight(1f))
                            KeyButton("Open") {
                                done()
                                val uri = Uri.parse(span.url)
                                // A file on the pane's machine: shown from there.
                                if (uri.scheme == "file") {
                                    openFile = uri.path
                                } else {
                                    try {
                                        context.startActivity(Intent(Intent.ACTION_VIEW, uri))
                                    } catch (_: ActivityNotFoundException) {
                                        Toast.makeText(context, "Nothing here opens ${uri.scheme ?: "it"} links", Toast.LENGTH_SHORT).show()
                                    }
                                }
                            }
                            KeyButton("Copy") {
                                done()
                                clipboard.setPrimaryClip(ClipData.newPlainText("artty", span.url))
                                if (Build.VERSION.SDK_INT < 33) Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
                            }
                            KeyButton("✕") { done() }
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
                // A tap: for the next key. A long press: until tapped again.
                fun modifier(on: Boolean, lock: Boolean) {
                    locked = on && lock
                    view.locked = locked
                }
                KeyButton("Ctrl", on = ctrl, locked = ctrl && locked, onLongClick = {
                    ctrl = true
                    view.ctrl = true
                    modifier(true, lock = true)
                }) {
                    ctrl = !ctrl
                    view.ctrl = ctrl
                    modifier(ctrl || alt, lock = locked && alt)
                }
                KeyButton("Alt", on = alt, locked = alt && locked, onLongClick = {
                    alt = true
                    view.alt = true
                    modifier(true, lock = true)
                }) {
                    alt = !alt
                    view.alt = alt
                    modifier(ctrl || alt, lock = locked && ctrl)
                }
                KeyButton("←") { view.key("left") }
                KeyButton("↓") { view.key("down") }
                KeyButton("↑") { view.key("up") }
                KeyButton("→") { view.key("right") }
                KeyButton("Home") { view.key("home") }
                KeyButton("End") { view.key("end") }
                KeyButton("PgUp") { view.key("pageup") }
                KeyButton("PgDn") { view.key("pagedown") }
                // The row's end: the characters Settings gives (each a key).
                for (c in rowKeys) KeyButton(c) { view.type(c) }
            }
        }
    }
    searchingAll?.let { SearchEverything(it) { searchingAll = null } }
    val pane = Session.pane
    if (browsing && pane != null) {
        FilesScreen(pane, onTypePath = { view.type(shellQuoted(it) + " ") }) { browsing = false }
    }
    val file = openFile
    if (file != null && pane != null) {
        FilesScreen(pane, onTypePath = { view.type(shellQuoted(it) + " ") }, open = file) { openFile = null }
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
private fun TabBar(tab: TabItem?, onTabs: () -> Unit, onFind: () -> Unit, onPaste: () -> Unit) {
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
        // Finding in the pane (the Mac's ⌘F).
        TextButton(onClick = onFind) { Text("⌕", fontSize = 20.sp) }
        // Here, not in the keys row: just after copying, Android's clipboard preview covers that.
        TextButton(onClick = onPaste) { Text("Paste") }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun KeyButton(label: String, on: Boolean = false, locked: Boolean = false, onLongClick: (() -> Unit)? = null, onClick: () -> Unit) {
    Box(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(if (locked) Accent.copy(alpha = 0.8f) else if (on) Accent.copy(alpha = 0.45f) else Color(0x22ffffff))
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .padding(horizontal = 12.dp, vertical = 8.dp),
    ) {
        // Locked: underlined, so it reads as staying on.
        Text(
            label,
            fontFamily = FontFamily.Monospace,
            fontSize = 14.sp,
            textDecoration = if (locked) androidx.compose.ui.text.style.TextDecoration.Underline else null,
        )
    }
}

@Composable
private fun FailedScreen(phase: Phase.Failed, store: Store) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Couldn't connect to ${phase.server.name}", fontWeight = FontWeight.SemiBold)
        Text(phase.reason, color = Faint, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
        // No artty there: its shells, with plain ssh, from now on.
        val noArtty = !phase.server.plain && "artty isn't installed" in phase.reason
        if (noArtty) {
            Text(
                "This app can still open its shells with plain ssh: tabs of shells that end when the " +
                    "connection does, without its files or agents. Or install artty there, for tabs that keep running.",
                color = Faint,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (noArtty) {
                Button(onClick = {
                    val plain = phase.server.copy(artty = null, plain = true)
                    store.saveServers(store.servers().map { if (it == phase.server) plain else it })
                    Session.connect(plain)
                }) { Text("Use plain ssh") }
                TextButton(onClick = { Session.connect(phase.server) }) { Text("Try again") }
            } else {
                Button(onClick = { Session.connect(phase.server) }) { Text("Try again") }
            }
            TextButton(onClick = { Session.disconnect() }) { Text("Back") }
        }
        // What connecting did, step by step: why it failed.
        ConnectionLog(Modifier.fillMaxWidth().weight(1f))
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
        is Question.Passphrase -> {
            var passphrase by remember(question) { mutableStateOf("") }
            var remember by remember(question) { mutableStateOf(false) }
            AlertDialog(
                onDismissRequest = { question.reply.complete(null) },
                title = { Text("The key ${question.key}") },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedTextField(passphrase, { passphrase = it }, label = { Text("Its passphrase") }, singleLine = true, visualTransformation = PasswordVisualTransformation())
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            androidx.compose.material3.Checkbox(remember, { remember = it })
                            Text("Keep it with the key", fontSize = 14.sp)
                        }
                    }
                },
                confirmButton = { TextButton(onClick = { question.reply.complete(passphrase to remember) }) { Text("Unlock") } },
                dismissButton = { TextButton(onClick = { question.reply.complete(null) }) { Text("Skip the key") } },
            )
        }
        is Question.AgentSign -> AlertDialog(
            onDismissRequest = { question.reply.complete(false to false) },
            title = { Text("Sign with ${question.key}?") },
            text = {
                Text(
                    "A program on ${question.host} asks to use this key (agent forwarding): to log in onward, say, or for git. Allow it only if you started something there that needs it.",
                    color = Faint,
                )
            },
            confirmButton = {
                Column(horizontalAlignment = Alignment.End) {
                    TextButton(onClick = { question.reply.complete(true to false) }) { Text("Allow once") }
                    TextButton(onClick = { question.reply.complete(true to true) }) { Text("Allow while connected") }
                }
            },
            dismissButton = { TextButton(onClick = { question.reply.complete(false to false) }) { Text("Refuse") } },
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

/** `path` as a shell takes it as one word: quoted, if it has anything a shell would read. */
private fun shellQuoted(path: String): String =
    if (path.all { it.isLetterOrDigit() || it in "/._-+,:@%~" }) path else "'" + path.replace("'", "'\\''") + "'"
