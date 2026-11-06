package io.github.rosinality.artty

import android.content.Context
import android.util.Base64
import java.io.File
import java.security.MessageDigest
import org.json.JSONArray
import org.json.JSONObject
import uniffi.artty_android.SshKey
import uniffi.artty_android.SshSetup
import uniffi.artty_android.keystorePublicKey

/**
 * A server to connect to: a destination as `ssh` takes it — an alias of the ssh config (which
 * then says its real name, user, port, keys, jump hosts…), or `user@host[:port]` — and artty's
 * path there if not on its PATH; or, `plain`, a machine without artty, whose shells this app
 * runs over plain ssh.
 */
data class Server(
    val name: String,
    val destination: String,
    val artty: String? = null,
    val plain: Boolean = false,
)

/** A machine's key, as `known_hosts` records it. */
data class KnownHost(val line: Int, val hosts: String, val type: String, val fingerprint: String)

/** What the app keeps: servers, keys, the ssh config, the machines' keys, the terminal's settings. */
class Store(private val context: Context) {
    private val prefs = context.getSharedPreferences("artty", Context.MODE_PRIVATE)

    fun servers(): List<Server> {
        val list = JSONArray(prefs.getString("servers", "[]"))
        return (0 until list.length()).map { i -> server(list.getJSONObject(i)) }
    }

    fun saveServers(servers: List<Server>) {
        val list = JSONArray()
        for (s in servers) list.put(json(s))
        prefs.edit().putString("servers", list.toString()).apply()
    }

    private fun json(s: Server) = JSONObject()
        .put("name", s.name)
        .put("destination", s.destination)
        .put("artty", s.artty ?: "")
        .put("plain", s.plain)

    private fun server(o: JSONObject): Server {
        // Saved by earlier builds as user, host and port.
        val destination = o.optString("destination").ifBlank {
            val port = o.optInt("port", 22)
            "${o.optString("user")}@${o.optString("host")}" + if (port == 22) "" else ":$port"
        }
        return Server(o.getString("name"), destination, o.optString("artty").ifBlank { null }, o.optBoolean("plain", false))
    }

    // Keys.

    /** This phone's key, in Keystore. */
    val phoneKey = PhoneKey()

    /** Its public half, for the servers' `~/.ssh/authorized_keys`. */
    fun publicKey(): String =
        keystorePublicKey(phoneKey.publicKey, "artty@${android.os.Build.MODEL.replace(' ', '-')}")

    val keys = AddedKeys(context)

    init {
        // The key earlier builds made (a file of the app's): one of the keys added now.
        val old = File(context.filesDir, "id_ed25519")
        if (old.exists() && keys.add("id_ed25519", old.readText(), null).isSuccess) {
            old.delete()
            File(context.filesDir, "id_ed25519.pub").delete()
        }
    }

    // The ssh config.

    /** The app's home for ssh: its `.ssh/config` is the ssh config (what it includes beside it). */
    val sshHome: File get() = File(context.filesDir, "home").also { File(it, ".ssh").mkdirs() }

    private val configFile get() = File(sshHome, ".ssh/config")

    fun sshConfig(): String = configFile.takeIf { it.exists() }?.readText() ?: ""

    fun saveSshConfig(text: String) {
        configFile.writeText(if (text.isEmpty() || text.endsWith("\n")) text else "$text\n")
    }

    /** What logging in goes by: this phone's key, the keys added, the config, machines' keys. */
    fun sshSetup(): SshSetup {
        val keys = keys.list().map { SshKey(it.name, it.text, it.passphrase) }
        return SshSetup(phoneKey.publicKey, keys, sshHome.path, knownHosts, scrollback.toUInt())
    }

    // Machines' keys.

    /** Machines' keys, recorded as OpenSSH does. */
    val knownHosts: String get() = File(context.filesDir, "known_hosts").path

    fun knownHostList(): List<KnownHost> {
        val file = File(knownHosts)
        if (!file.exists()) return emptyList()
        return file.readLines().mapIndexedNotNull { i, line ->
            val fields = line.trim().split(Regex("\\s+"))
            if (line.isBlank() || line.startsWith("#") || fields.size < 3) return@mapIndexedNotNull null
            // `@cert-authority` and `@revoked` lines have a marker first.
            val (hosts, type, key) = if (fields[0].startsWith("@")) Triple(fields[1], fields[2], fields.getOrNull(3) ?: "") else Triple(fields[0], fields[1], fields[2])
            val blob = runCatching { Base64.decode(key, Base64.DEFAULT) }.getOrNull() ?: return@mapIndexedNotNull null
            val digest = MessageDigest.getInstance("SHA-256").digest(blob)
            val fingerprint = "SHA256:" + Base64.encodeToString(digest, Base64.NO_PADDING or Base64.NO_WRAP)
            KnownHost(i, if (hosts.startsWith("|1|")) "(a hashed name)" else hosts, type, fingerprint)
        }
    }

    /** Forget a machine's key: it's asked about again next time. */
    fun forgetHost(host: KnownHost) {
        val file = File(knownHosts)
        val lines = file.readLines().filterIndexed { i, _ -> i != host.line }
        file.writeText(lines.joinToString("\n", postfix = if (lines.isEmpty()) "" else "\n"))
    }

    // The terminal's settings.

    var fontSize: Float
        get() = prefs.getFloat("font_size", 13f)
        set(value) = prefs.edit().putFloat("font_size", value).apply()

    /** The volume keys, held, as Ctrl and a keys layer (else they only turn the volume). */
    var volumeKeys: Boolean
        get() = prefs.getBoolean("volume_keys", true)
        set(value) = prefs.edit().putBoolean("volume_keys", value).apply()

    /** A bell (a program's `\a`) vibrates. */
    var bellVibrates: Boolean
        get() = prefs.getBoolean("bell_vibrates", false)
        set(value) = prefs.edit().putBoolean("bell_vibrates", value).apply()

    /** The characters at the keys row's end, each a key. */
    var rowKeys: String
        get() = prefs.getString("row_keys", null) ?: DEFAULT_ROW_KEYS
        set(value) = prefs.edit().putString("row_keys", value).apply()

    /** Lines of history a plain ssh tab keeps (an artty server keeps its own). */
    var scrollback: Int
        get() = prefs.getInt("scrollback", 10_000)
        set(value) = prefs.edit().putInt("scrollback", value).apply()

    // Backing up.

    /** Everything but the keys (unless `withKeys`: then them too, readable by whoever has the file). */
    fun backup(withKeys: Boolean): String {
        val o = JSONObject()
            .put("artty", 1)
            .put("servers", JSONArray().apply { servers().forEach { put(json(it)) } })
            .put("ssh_config", sshConfig())
            .put("known_hosts", File(knownHosts).takeIf { it.exists() }?.readText() ?: "")
            .put(
                "terminal",
                JSONObject()
                    .put("font_size", fontSize.toDouble())
                    .put("volume_keys", volumeKeys)
                    .put("bell_vibrates", bellVibrates)
                    .put("row_keys", rowKeys)
                    .put("scrollback", scrollback),
            )
        if (withKeys) {
            o.put(
                "keys",
                JSONArray().apply {
                    keys.list().forEach { put(JSONObject().put("name", it.name).put("text", it.text)) }
                },
            )
        }
        return o.toString(2)
    }

    /**
     * Take in a backup: its servers and keys beside these (the same ones once), machines' keys
     * added to these, its ssh config and terminal settings in place of these. What it took, said.
     */
    fun restore(text: String): Result<String> = runCatching {
        val o = JSONObject(text)
        require(o.has("artty")) { "Not an artty backup" }
        val servers = servers().toMutableList()
        val incoming = o.optJSONArray("servers") ?: JSONArray()
        var added = 0
        for (i in 0 until incoming.length()) {
            val server = server(incoming.getJSONObject(i))
            if (servers.none { it.name == server.name && it.destination == server.destination }) {
                servers += server
                added++
            }
        }
        saveServers(servers)
        o.optString("ssh_config").takeIf { it.isNotEmpty() }?.let { saveSshConfig(it) }
        val known = File(knownHosts)
        val have = known.takeIf { it.exists() }?.readLines()?.toMutableList() ?: mutableListOf()
        for (line in o.optString("known_hosts").lines()) if (line.isNotBlank() && line !in have) have += line
        known.writeText(have.joinToString("\n", postfix = "\n"))
        o.optJSONObject("terminal")?.let { t ->
            fontSize = t.optDouble("font_size", fontSize.toDouble()).toFloat()
            volumeKeys = t.optBoolean("volume_keys", volumeKeys)
            bellVibrates = t.optBoolean("bell_vibrates", bellVibrates)
            rowKeys = t.optString("row_keys", rowKeys)
            scrollback = t.optInt("scrollback", scrollback)
        }
        var keysAdded = 0
        val existing = keys.list().map { it.name }.toSet()
        o.optJSONArray("keys")?.let { list ->
            for (i in 0 until list.length()) {
                val k = list.getJSONObject(i)
                if (k.getString("name") !in existing && keys.add(k.getString("name"), k.getString("text"), null).isSuccess) keysAdded++
            }
        }
        "$added server${if (added == 1) "" else "s"} and $keysAdded key${if (keysAdded == 1) "" else "s"} added; the ssh config and settings taken"
    }

    companion object {
        const val DEFAULT_ROW_KEYS = "| - / ~ _ \\ ` * & $ > <"
    }
}
