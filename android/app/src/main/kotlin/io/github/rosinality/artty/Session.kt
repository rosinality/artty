package io.github.rosinality.artty

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
import uniffi.artty_android.LoginAsker
import uniffi.artty_android.Prompt
import uniffi.artty_android.ServerAddress
import uniffi.artty_android.TabItem
import uniffi.artty_android.WindowItem
import uniffi.artty_android.connect

/** Where the app is. */
sealed interface Phase {
    data object Servers : Phase
    /** Logging in: what the server says meanwhile (Tailscale's sign-in address, say). */
    data class Connecting(val server: Server, val said: List<String> = emptyList()) : Phase
    /** Connected: which window to show (`null` while asking). */
    data class Choosing(val server: Server, val windows: List<WindowItem>? = null) : Phase
    data class Shown(val server: Server, val window: ULong) : Phase
    data class Failed(val server: Server, val reason: String) : Phase
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
}

/** The connection to a server, and what its windows show. */
object Session : Events, LoginAsker {
    private val main = Handler(Looper.getMainLooper())

    var phase by mutableStateOf<Phase>(Phase.Servers)
        private set
    var question by mutableStateOf<Question?>(null)
        private set
    /** The shown window's tabs. */
    var tabs by mutableStateOf<List<TabItem>>(emptyList())
        private set
    /** A pane of the tab chosen in place of its focused one (in a split). */
    var chosenPane by mutableStateOf<ULong?>(null)
    /** What a program copied, for the clipboard. */
    var copied: ((String) -> Unit)? = null

    var connection: Connection? = null
        private set
    /** Told when a pane's screen changes (on the main thread). */
    var onScreen: ((ULong) -> Unit)? = null

    /** The tab shown, and the pane shown of it. */
    val activeTab get() = tabs.firstOrNull { it.active }
    val pane: ULong?
        get() {
            val tab = activeTab ?: return null
            return chosenPane?.takeIf { it in tab.panes } ?: tab.focused
        }

    fun connect(server: Server, store: Store) {
        disconnect()
        phase = Phase.Connecting(server)
        val address = ServerAddress(server.user, server.host, server.port.toUShort(), server.artty)
        val key = store.privateKey()
        val knownHosts = store.knownHosts
        Thread {
            try {
                val connection = connect(address, key, knownHosts, this, this)
                main.post {
                    if ((phase as? Phase.Connecting)?.server != server) {
                        connection.destroy()
                        return@post
                    }
                    this.connection = connection
                    phase = Phase.Choosing(server)
                    runCatching { connection.listWindows() }
                }
            } catch (e: ConnectException) {
                main.post { if ((phase as? Phase.Connecting)?.server == server) phase = Phase.Failed(server, e.message ?: "couldn't connect") }
            }
        }.start()
    }

    fun disconnect() {
        connection?.let { c ->
            (phase as? Phase.Shown)?.let { runCatching { c.detach(it.window) } }
            Thread { c.destroy() }.start()
        }
        connection = null
        tabs = emptyList()
        chosenPane = null
        question = null
        phase = Phase.Servers
    }

    /** Show `window` (`null`: a new one), shared with the apps showing it. */
    fun show(window: ULong?) {
        runCatching { connection?.attach(window) }
    }

    fun selectTab(tab: TabItem) {
        chosenPane = null
        runCatching { connection?.selectTab(tab.workspace, tab.index) }
    }

    /** The window is in use here (or not): its panes take this phone's size while it is. */
    fun focus(focused: Boolean) {
        val shown = phase as? Phase.Shown ?: return
        runCatching { connection?.focus(shown.window, focused) }
    }

    // From the Rust side, off the main thread.

    override fun event(event: ClientEvent) {
        main.post { handle(event) }
    }

    private fun handle(event: ClientEvent) {
        when (event) {
            is ClientEvent.Windows -> {
                val choosing = phase as? Phase.Choosing ?: return
                phase = choosing.copy(windows = event.windows)
            }
            is ClientEvent.Attached -> {
                val server = (phase as? Phase.Choosing)?.server ?: return
                phase = Phase.Shown(server, event.window)
                focus(true)
            }
            is ClientEvent.Tabs -> {
                if ((phase as? Phase.Shown)?.window == event.window) tabs = event.tabs
            }
            is ClientEvent.WindowClosed -> {
                val shown = phase as? Phase.Shown ?: return
                if (shown.window == event.window) {
                    tabs = emptyList()
                    phase = Phase.Choosing(shown.server)
                    runCatching { connection?.listWindows() }
                }
            }
            is ClientEvent.Screen -> onScreen?.invoke(event.pane)
            is ClientEvent.Clipboard -> copied?.invoke(event.text)
            is ClientEvent.Closed -> {
                val server = when (val p = phase) {
                    is Phase.Shown -> p.server
                    is Phase.Choosing -> p.server
                    else -> return
                }
                connection = null
                tabs = emptyList()
                phase = Phase.Failed(server, "Connection closed: ${event.reason}")
            }
            is ClientEvent.Bell, is ClientEvent.Notify -> {}
        }
    }

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

    override fun answer(instructions: String, prompts: List<Prompt>): List<String>? {
        val reply = CompletableFuture<List<String>?>()
        main.post { question = Question.Answers(instructions, prompts, reply) }
        return reply.get().also { main.post { question = null } }
    }
}
