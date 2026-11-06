package io.github.rosinality.artty

import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.util.concurrent.CompletableFuture
import uniffi.artty_android.ClientEvent
import uniffi.artty_android.ConnectException
import uniffi.artty_android.Connection
import uniffi.artty_android.Events
import uniffi.artty_android.FindResult
import uniffi.artty_android.ForwardItem
import uniffi.artty_android.ForwardKind
import uniffi.artty_android.FoundLine
import uniffi.artty_android.FoundSession
import uniffi.artty_android.LoginAsker
import uniffi.artty_android.PaneItem
import uniffi.artty_android.Prompt
import uniffi.artty_android.ServerAddress
import uniffi.artty_android.TabItem
import uniffi.artty_android.LinuxBuilds
import uniffi.artty_android.WindowItem
import uniffi.artty_android.connect
import uniffi.artty_android.install

/** Where the app is. */
sealed interface Phase {
    data object Servers : Phase
    /** Logging in: what the server says meanwhile (Tailscale's sign-in address, say). */
    data class Connecting(val server: Server, val said: List<String> = emptyList()) : Phase
    /** Connected: which window to show (`null` while asking). */
    data class Choosing(val server: Server, val windows: List<WindowItem>? = null) : Phase
    /** A window shown; `reconnecting` says why it's being reached again, meanwhile. */
    data class Shown(val server: Server, val window: ULong, val reconnecting: String? = null) : Phase
    data class Failed(val server: Server, val reason: String) : Phase
    /**
     * It runs another artty build (`serverVersion`), which this app can't talk to: offering
     * to install this app's there. `installing` while it does; `error` if that failed.
     */
    data class AnotherBuild(
        val server: Server,
        val serverVersion: String,
        val installing: Boolean = false,
        val error: String? = null,
    ) : Phase
}

/** What logging in asks, waiting for the user's answer. */
sealed interface Question {
    data class HostKey(
        val host: String,
        val fingerprint: String,
        val changed: Boolean,
        val reply: CompletableFuture<Boolean>,
    ) : Question

    data class Answers(
        val instructions: String,
        val prompts: List<Prompt>,
        val reply: CompletableFuture<List<String>?>,
    ) : Question

    /** A key's passphrase (`remember`: kept with the key). */
    data class Passphrase(val key: String, val reply: CompletableFuture<Pair<String, Boolean>?>) : Question

    /** May `host` sign with `key` (agent forwarding)? Yes (`always`: for the rest of the connection) or no. */
    data class AgentSign(val key: String, val host: String, val reply: CompletableFuture<Pair<Boolean, Boolean>>) : Question
}

/**
 * The connection to a server, and what its windows show.
 *
 * A connection lost — closed, silent too long, or not answering after the network changed —
 * is made again, and the window shown is shown again (the server kept it running): a phone
 * sleeping, or going from Wi-Fi to mobile data, only pauses it. A foreground service keeps
 * the app running meanwhile, in the background too.
 */
object Session : LoginAsker {
    private val main = Handler(Looper.getMainLooper())

    var phase by mutableStateOf<Phase>(Phase.Servers)
        private set
    var question by mutableStateOf<Question?>(null)
        private set
    /** The shown window's tabs. */
    var tabs by mutableStateOf<List<TabItem>>(emptyList())
        private set
    /** The connection's port forwards. */
    var forwards by mutableStateOf<List<ForwardItem>>(emptyList())
        private set
    /** What the shown window's panes are doing, by pane. */
    var panes by mutableStateOf<Map<ULong, PaneItem>>(emptyMap())
        private set
    /** A pane of the tab chosen in place of its focused one (in a split). */
    var chosenPane by mutableStateOf<ULong?>(null)
    /** What a program copied, for the clipboard. */
    var copied: ((String) -> Unit)? = null

    var connection: Connection? = null
        private set
    /** Told when a pane's screen changes (on the main thread). */
    var onScreen: ((ULong) -> Unit)? = null
    /** An image a screen shows arrived. */
    var onImage: ((ULong) -> Unit)? = null
    /** Told when the folder watched for a pane changed (on the main thread). */
    var onDirChanged: ((ULong) -> Unit)? = null
    /** Told when a window is shown again after reconnecting: sizes are sent again. */
    var onReattached: (() -> Unit)? = null

    /** Connected with plain ssh: no artty there, so no files, workspaces or agents. */
    val plain get() = server?.plain == true

    /** The tab shown, and the pane shown of it. */
    val activeTab get() = tabs.firstOrNull { it.active }
    val pane: ULong?
        get() {
            val tab = activeTab ?: return null
            return chosenPane?.takeIf { it in tab.panes } ?: tab.focused
        }

    private lateinit var context: Context
    private lateinit var store: Store
    /** The server connected to (or being reached again), until the user disconnects. */
    private var server: Server? = null
    /** Which connection is current: what older ones say is ignored. */
    private var generation = 0
    /** Failed attempts in a row, for waiting longer between them. */
    private var failures = 0
    /** Reaching the server again, after a wait. */
    private val retry = Runnable { open() }
    /** A pane to show once its window is (a notification tapped). */
    private var revealing: ULong? = null
    /** The app is in front: the pane shown is seen. */
    var resumed = false
        set(value) {
            field = value
            seeShown()
        }

    /** Silent this long (seconds), the connection is taken for lost (pings go every 5). */
    private const val SILENCE = 20.0
    /** After a probe (a ping), an answer is expected within this long. */
    private const val PROBE_MS = 5000L

    fun init(context: Context, store: Store) {
        if (this::context.isInitialized) return
        this.context = context.applicationContext
        this.store = store
        // A new network: the connection is probably on the old one.
        context.getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    main.post { networkChanged() }
                }
            },
        )
        main.post(watchdog)
    }

    fun connect(server: Server) {
        disconnect()
        this.server = server
        failures = 0
        phase = Phase.Connecting(server)
        updateService()
        open()
    }

    fun disconnect() {
        main.removeCallbacks(retry)
        server = null
        generation++
        drop()
        tabs = emptyList()
        panes = emptyMap()
        forwards = emptyList()
        added.clear()
        agentAllowed.clear()
        chosenPane = null
        revealing = null
        question = null
        phase = Phase.Servers
        updateService()
    }

    /** Let the connection go (it's lost, or done with). */
    private fun drop() {
        val connection = connection ?: return
        this.connection = null
        forwards = emptyList()
        (phase as? Phase.Shown)?.let { runCatching { connection.detach(it.window) } }
        Thread { connection.destroy() }.start()
    }

    /** Log in to the server (again); a window shown before is shown again. */
    private fun open() {
        val server = server ?: return
        val generation = ++generation
        val address = ServerAddress(server.destination, server.artty, server.plain, server.tmux)
        val setup = store.sshSetup()
        Thread {
            try {
                val phoneKey = store.phoneKey
                val connection = connect(address, setup, phoneKey, this, ConnectionEvents(generation))
                main.post {
                    if (generation != this.generation) {
                        Thread { connection.destroy() }.start()
                        return@post
                    }
                    this.connection = connection
                    failures = 0
                    // Forwards added while connected before: again (the config's start by themselves).
                    for (f in added) f.id = runCatching { connection.addForward(f.kind, f.bind, f.port.toUShort(), f.host, f.hostPort.toUShort()) }.getOrNull()
                    forwards = runCatching { connection.forwards() }.getOrDefault(emptyList())
                    // Shown before: found again in the server's windows (WindowsListed).
                    if (phase !is Phase.Shown) phase = Phase.Choosing(server)
                    updateService()
                    runCatching { connection.listWindows() }
                }
            } catch (e: ConnectException.AnotherBuild) {
                main.post { if (generation == this.generation) anotherBuild(e.serverVersion) }
            } catch (e: ConnectException) {
                // Its own words (the message adds the field's name).
                val reason = (e as? ConnectException.Failed)?.reason ?: "couldn't connect"
                main.post { if (generation == this.generation) lost(reason) }
            }
        }.start()
    }

    /** It runs another build: trying again won't help (installing this one there will). */
    private fun anotherBuild(serverVersion: String) {
        val server = server ?: return
        generation++
        drop()
        tabs = emptyList()
        phase = Phase.AnotherBuild(server, serverVersion)
        this.server = null
        updateService()
    }

    /**
     * Install this app's build of artty on the server shown as running another (into
     * `~/.local/bin`, its running server moved onto it), then connect again.
     */
    fun installHere() {
        val shown = phase as? Phase.AnotherBuild ?: return
        if (shown.installing) return
        phase = shown.copy(installing = true, error = null)
        val server = shown.server
        val address = ServerAddress(server.destination, server.artty, false, false)
        val setup = store.sshSetup()
        val builds = AppBuilds(context)
        Thread {
            try {
                val phoneKey = store.phoneKey
                val installed = install(address, setup, phoneKey, this, builds)
                main.post {
                    // Connecting runs what was installed.
                    val updated = if (installed.changeArtty) server.copy(artty = installed.artty) else server
                    if (updated != server) store.saveServers(store.servers().map { if (it == server) updated else it })
                    if (phase is Phase.AnotherBuild) connect(updated)
                }
            } catch (e: ConnectException) {
                val reason = (e as? ConnectException.Failed)?.reason ?: e.toString()
                main.post {
                    val now = phase as? Phase.AnotherBuild ?: return@post
                    phase = now.copy(installing = false, error = reason)
                }
            }
        }.start()
    }

    /** The builds of artty for Linux in the app's assets (build-rust.sh). */
    private class AppBuilds(private val context: Context) : LinuxBuilds {
        override fun build(cpu: String): ByteArray? =
            runCatching { context.assets.open("linux/artty-$cpu").use { it.readBytes() } }.getOrNull()
    }

    /** The connection is gone (or never came): reach the server again, unless that can't help. */
    private fun lost(reason: String) {
        val server = server ?: return
        generation++
        drop()
        // Not a matter of trying again: the user is asked, or must change something.
        val final = listOf("not let in", "login cancelled", "isn't trusted", "not as an artty server", "artty isn't")
            .any { it in reason }
        val connectedBefore = phase is Phase.Shown || phase is Phase.Choosing
        if (final || !connectedBefore && failures == 0 && phase is Phase.Connecting) {
            // A first attempt failing: say so (the user may have a typo to fix).
            phase = Phase.Failed(server, reason)
            this.server = null
            updateService()
            return
        }
        failures++
        val wait = (1L shl (failures - 1).coerceAtMost(5)).coerceAtMost(30) * 1000
        phase = when (val p = phase) {
            is Phase.Shown -> p.copy(reconnecting = reason)
            else -> Phase.Connecting(server, listOf("Reconnecting: $reason"))
        }
        updateService()
        main.removeCallbacks(retry)
        main.postDelayed(retry, wait)
    }

    /** Is the connection still there? Asked after the network changed, or the app came back. */
    fun probe() {
        val connection = connection ?: return
        if (runCatching { connection.ping() }.isFailure) return lost("couldn't reach it")
        val generation = generation
        main.postDelayed({
            if (generation == this.generation && this.connection === connection &&
                runCatching { connection.silentSeconds() }.getOrDefault(Double.MAX_VALUE) * 1000 > PROBE_MS - 500
            ) {
                lost("no answer")
            }
        }, PROBE_MS)
    }

    private fun networkChanged() {
        if (connection != null) return probe()
        // Waiting to reach it again: now, on the new network.
        if (server != null && (phase as? Phase.Shown)?.reconnecting != null) {
            main.removeCallbacks(retry)
            open()
        }
    }

    /** Silence long enough means the connection is gone. */
    private val watchdog = object : Runnable {
        override fun run() {
            connection?.let { c ->
                if (runCatching { c.silentSeconds() }.getOrDefault(Double.MAX_VALUE) > SILENCE) lost("no answer for ${SILENCE.toInt()}s")
            }
            main.postDelayed(this, 3000)
        }
    }

    /** Show `window` (`null`: a new one), shared with the apps showing it. */
    fun show(window: ULong?) {
        runCatching { connection?.attach(window) }
    }

    fun selectTab(tab: TabItem) {
        chosenPane = null
        runCatching { connection?.selectTab(tab.workspace, tab.index) }
    }

    /** A tab's name: the one given it, else its focused pane's title. */
    fun title(tab: TabItem): String =
        tab.title.ifBlank { panes[tab.focused]?.title?.ifBlank { null } ?: "shell" }

    /** The tab's most urgent pane's state (waiting before busy, before done…). */
    fun state(tab: TabItem): PaneItem? =
        tab.panes.mapNotNull { panes[it] }.filter { it.urgency > 0u }.maxByOrNull { it.urgency }
            ?: panes[tab.focused]

    private val window get() = (phase as? Phase.Shown)?.window

    /** A shell in a new tab, at the end of `workspace` (or of the active one). */
    fun newTab(workspace: ULong? = null) {
        val window = window ?: return
        runCatching { connection?.newTab(window, workspace) }
    }

    fun closeTab(tab: TabItem) {
        runCatching { connection?.closeTab(tab.workspace, tab.index) }
    }

    /** `null` or blank: back to what its programs call it. */
    fun renameTab(tab: TabItem, title: String?) {
        runCatching { connection?.renameTab(tab.workspace, tab.index, title?.trim()?.ifBlank { null }) }
    }

    fun newWorkspace(name: String?) {
        val window = window ?: return
        runCatching { connection?.newWorkspace(window, name?.trim()?.ifBlank { null }) }
    }

    /** `null` or blank: back to its directory's or repository's name. */
    fun renameWorkspace(workspace: ULong, name: String?) {
        runCatching { connection?.renameWorkspace(workspace, name?.trim()?.ifBlank { null }) }
    }

    fun closeWorkspace(workspace: ULong) {
        runCatching { connection?.closeWorkspace(workspace) }
    }

    // Searching (on its own thread: the server's answer is waited for).

    /** Find `text` in `pane`'s output: `done` with the result, or why not, on the main thread. */
    fun find(pane: ULong, text: String, older: Boolean, restart: Boolean, done: (Result<FindResult>) -> Unit) {
        val connection = connection ?: return
        Thread {
            val result = runCatching { connection.find(pane, text, older, restart) }
            main.post { done(result) }
        }.start()
    }

    fun endFind(pane: ULong) {
        runCatching { connection?.endFind(pane) }
    }

    /**
     * `work` with the connection, on a thread of its own (it waits for the server); `done` with
     * what came of it on the main thread. Nothing without a connection.
     */
    fun <T> io(work: (Connection) -> T, done: (Result<T>) -> Unit) {
        val connection = connection ?: return done(Result.failure(IllegalStateException("not connected")))
        Thread {
            val result = runCatching { work(connection) }
            main.post { done(result) }
        }.start()
    }

    /** What every pane printed, and agents' conversations, mentioning `text`. */
    class Found(val lines: List<FoundLine>, val sessions: List<FoundSession>)

    fun searchEverything(text: String, done: (Result<Found>) -> Unit) {
        val connection = connection ?: return
        Thread {
            val result = runCatching {
                // Both at once: conversations take longer.
                var sessions: List<FoundSession> = emptyList()
                val other = Thread { sessions = runCatching { connection.searchSessions(text) }.getOrDefault(emptyList()) }
                other.start()
                val lines = connection.searchOutput(text)
                other.join()
                Found(lines, sessions)
            }
            main.post { done(result) }
        }.start()
    }

    /** Show a line found: the server brings its tab forward, its pane focused. */
    fun reveal(line: FoundLine) {
        chosenPane = null
        runCatching { connection?.reveal(line) }
    }

    /** Carry on a conversation found, in a new tab. */
    fun resume(session: FoundSession) {
        val window = window ?: return
        runCatching { connection?.resumeSession(window, session) }
    }

    /** Show `pane` of `window` (a notification about it tapped): its tab, its window first. */
    fun reveal(window: ULong, pane: ULong) {
        if (connection == null) return
        val shown = phase as? Phase.Shown
        if (shown?.window == window && tabs.isNotEmpty()) {
            choose(pane)
        } else {
            revealing = pane
            show(window)
        }
    }

    private fun choose(pane: ULong) {
        val tab = tabs.firstOrNull { pane in it.panes } ?: return
        if (!tab.active) selectTab(tab)
        chosenPane = pane.takeIf { it != tab.focused }
        seeShown()
    }

    /** The pane shown, in front: its notification is done with. */
    private fun seeShown() {
        if (resumed && phase is Phase.Shown) pane?.let { Alerts.seen(context, it) }
    }

    /** The window is in use here (or not): its panes take this phone's size while it is. */
    fun focus(focused: Boolean) {
        val shown = phase as? Phase.Shown ?: return
        runCatching { connection?.focus(shown.window, focused) }
    }

    /** The service keeping the app running while connected, and its notification. */
    private fun updateService() {
        val server = server
        val intent = Intent(context, ConnectionService::class.java)
        if (server == null) {
            context.stopService(intent)
            return
        }
        val reconnecting = when (val p = phase) {
            is Phase.Shown -> p.reconnecting != null
            is Phase.Connecting -> p.said.any { it.startsWith("Reconnecting") }
            else -> false
        }
        intent.putExtra(ConnectionService.TEXT, if (reconnecting) "Reconnecting to ${server.name}…" else "Connected to ${server.name}")
        context.startForegroundService(intent)
    }

    /** What one connection says: dropped once it's not the current one. */
    private class ConnectionEvents(private val generation: Int) : Events {
        override fun event(event: ClientEvent) {
            main.post { if (generation == Session.generation) handle(event) }
        }
    }

    private fun handle(event: ClientEvent) {
        val server = server ?: return
        when (event) {
            is ClientEvent.Windows -> when (val p = phase) {
                // Reconnected: the window shown, if it's still there.
                is Phase.Shown -> {
                    if (event.windows.any { it.window == p.window }) show(p.window)
                    else {
                        tabs = emptyList()
                        phase = Phase.Choosing(server, event.windows)
                    }
                }
                // Plain ssh: one window, its shells, shown at once.
                is Phase.Choosing -> if (server.plain) show(event.windows.firstOrNull()?.window) else phase = p.copy(windows = event.windows)
                else -> {}
            }
            is ClientEvent.Attached -> {
                val again = (phase as? Phase.Shown)?.window == event.window
                phase = Phase.Shown(server, event.window)
                applyAppearance()
                updateService()
                focus(true)
                if (again) onReattached?.invoke()
            }
            is ClientEvent.Tabs -> {
                if ((phase as? Phase.Shown)?.window == event.window) {
                    tabs = event.tabs
                    revealing?.let {
                        revealing = null
                        choose(it)
                    }
                    seeShown()
                }
            }
            is ClientEvent.WindowClosed -> {
                val shown = phase as? Phase.Shown ?: return
                if (shown.window == event.window) {
                    // Plain ssh: the last shell ended, and with it the visit.
                    if (server.plain) return disconnect()
                    tabs = emptyList()
                    phase = Phase.Choosing(server)
                    runCatching { connection?.listWindows() }
                }
            }
            is ClientEvent.Panes -> panes = event.panes.associateBy { it.pane }
            is ClientEvent.Screen -> onScreen?.invoke(event.pane)
            is ClientEvent.Image -> onImage?.invoke(event.id)
            is ClientEvent.DirChanged -> onDirChanged?.invoke(event.pane)
            is ClientEvent.Clipboard -> copied?.invoke(event.text)
            is ClientEvent.Closed -> lost(event.reason)
            is ClientEvent.Notify ->
                Alerts.post(context, server.name, event.title, event.body, event.window, event.pane)
            is ClientEvent.Bell -> if (store.bellVibrates) vibrate()
            is ClientEvent.Forwards -> forwards = connection?.let { runCatching { it.forwards() }.getOrNull() } ?: emptyList()
        }
    }

    /** The chosen colors and cursor, to a plain ssh machine's panes (an artty server keeps the Mac app's). */
    fun applyAppearance() {
        if (server?.plain != true) return
        runCatching { connection?.setAppearance(store.theme, store.cursorShape) }
    }

    /** A forward added while connected: started again when the connection is made again. */
    private data class Added(val kind: ForwardKind, val bind: String?, val port: Int, val host: String?, val hostPort: Int, var id: ULong?)

    private val added = mutableListOf<Added>()

    /** Forward `port` (to `host`:`hostPort`; for a dynamic one, SOCKS says where). */
    fun addForward(kind: ForwardKind, bind: String?, port: Int, host: String?, hostPort: Int) {
        val connection = connection ?: return
        val id = runCatching { connection.addForward(kind, bind, port.toUShort(), host, hostPort.toUShort()) }.getOrNull() ?: return
        added += Added(kind, bind, port, host, hostPort, id)
    }

    fun stopForward(forward: ForwardItem) {
        runCatching { connection?.stopForward(forward.id) }
        // Not again, either.
        added.removeAll { it.id == forward.id }
    }

    private fun vibrate() {
        val vibrator = context.getSystemService(android.os.Vibrator::class.java) ?: return
        vibrator.vibrate(android.os.VibrationEffect.createOneShot(40, android.os.VibrationEffect.DEFAULT_AMPLITUDE))
    }

    /** Keys the connection may sign with for the rest of it (agent forwarding). */
    private val agentAllowed = java.util.Collections.synchronizedSet(mutableSetOf<String>())

    // Logging in asks the user, from the Rust side (off the main thread).

    override fun hostKey(host: String, fingerprint: String, changed: Boolean): Boolean {
        val reply = CompletableFuture<Boolean>()
        main.post { question = Question.HostKey(host, fingerprint, changed, reply) }
        return reply.get().also { main.post { question = null } }
    }

    override fun said(text: String) {
        main.post {
            val connecting = phase as? Phase.Connecting ?: return@post
            phase = connecting.copy(said = connecting.said + text.trim().lines().filter { it.isNotBlank() })
        }
    }

    override fun passphrase(name: String): String? {
        val reply = CompletableFuture<Pair<String, Boolean>?>()
        main.post { question = Question.Passphrase(name, reply) }
        val (passphrase, remember) = reply.get().also { main.post { question = null } } ?: return null
        if (remember) store.keys.setPassphrase(name, passphrase)
        return passphrase
    }

    override fun agentSign(name: String, host: String): Boolean {
        if (name in agentAllowed) return true
        val reply = CompletableFuture<Pair<Boolean, Boolean>>()
        main.post { question = Question.AgentSign(name, host, reply) }
        val (yes, always) = reply.get().also { main.post { question = null } }
        if (yes && always) agentAllowed += name
        return yes
    }

    override fun answer(instructions: String, prompts: List<Prompt>): List<String>? {
        val reply = CompletableFuture<List<String>?>()
        main.post { question = Question.Answers(instructions, prompts, reply) }
        return reply.get().also { main.post { question = null } }
    }
}
