package io.github.rosinality.artty

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import uniffi.artty_android.generateKey

/** A server to connect to: `user@host:port`, and artty's path there if not on its PATH. */
data class Server(
    val name: String,
    val user: String,
    val host: String,
    val port: Int = 22,
    val artty: String? = null,
) {
    val address get() = if (port == 22) "$user@$host" else "$user@$host:$port"

    companion object {
        /** `user@host` or `user@host:port`. */
        fun parse(name: String, address: String, artty: String?): Server? {
            val at = address.indexOf('@')
            if (at <= 0) return null
            val user = address.substring(0, at)
            val rest = address.substring(at + 1)
            val colon = rest.lastIndexOf(':')
            val (host, port) =
                if (colon > 0) rest.substring(0, colon) to (rest.substring(colon + 1).toIntOrNull() ?: return null)
                else rest to 22
            if (host.isBlank()) return null
            return Server(name.ifBlank { host }, user, host, port, artty?.ifBlank { null })
        }
    }
}

/** What the app keeps: servers, this phone's key, the machines' keys. */
class Store(private val context: Context) {
    private val prefs = context.getSharedPreferences("artty", Context.MODE_PRIVATE)

    fun servers(): List<Server> {
        val list = JSONArray(prefs.getString("servers", "[]"))
        return (0 until list.length()).map { i ->
            val o = list.getJSONObject(i)
            Server(
                o.getString("name"),
                o.getString("user"),
                o.getString("host"),
                o.optInt("port", 22),
                o.optString("artty").ifBlank { null },
            )
        }
    }

    fun saveServers(servers: List<Server>) {
        val list = JSONArray()
        for (s in servers) {
            list.put(
                JSONObject()
                    .put("name", s.name)
                    .put("user", s.user)
                    .put("host", s.host)
                    .put("port", s.port)
                    .put("artty", s.artty ?: ""),
            )
        }
        prefs.edit().putString("servers", list.toString()).apply()
    }

    /** Machines' keys, recorded as OpenSSH does. */
    val knownHosts: String get() = File(context.filesDir, "known_hosts").path

    private val privateKeyFile get() = File(context.filesDir, "id_ed25519")
    private val publicKeyFile get() = File(context.filesDir, "id_ed25519.pub")

    /** This phone's key (made the first time): its private half, kept in the app's own files. */
    fun privateKey(): String {
        ensureKey()
        return privateKeyFile.readText()
    }

    /** Its public half, for the servers' `~/.ssh/authorized_keys`. */
    fun publicKey(): String {
        ensureKey()
        return publicKeyFile.readText().trim()
    }

    private fun ensureKey() {
        if (privateKeyFile.exists() && publicKeyFile.exists()) return
        val pair = generateKey("artty@${android.os.Build.MODEL.replace(' ', '-')}")
        privateKeyFile.writeText(pair.privateOpenssh)
        privateKeyFile.setReadable(false, false)
        privateKeyFile.setReadable(true, true)
        publicKeyFile.writeText(pair.publicOpenssh + "\n")
    }
}
