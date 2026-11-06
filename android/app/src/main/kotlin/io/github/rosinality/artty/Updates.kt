package io.github.rosinality.artty

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import org.json.JSONObject

/** A build of the app published at the updates address (publish.sh's version.json). */
data class Release(val versionCode: Long, val versionName: String, val apk: URL, val sha256: String, val size: Long)

/** Where an update is. */
sealed interface UpdateState {
    data object Idle : UpdateState
    data object Checking : UpdateState
    data class UpToDate(val checked: Long) : UpdateState
    data class Available(val release: Release) : UpdateState
    /** Downloading: bytes so far. */
    data class Downloading(val release: Release, val done: Long) : UpdateState
    /** Handed to the system installer, which asks the user. */
    data class Installing(val release: Release) : UpdateState
    data class Failed(val reason: String) : UpdateState
}

/**
 * Updating the app from an address of the user's (a folder publish.sh fills: `version.json`
 * and the APK), without a store or USB: checked when the app opens (at most every few hours),
 * or asked; the APK downloaded, checked against its SHA-256, and handed to the system
 * installer. Android installs it only over this app signed with the same key.
 */
object Updates {
    private val main = Handler(Looper.getMainLooper())
    var state by mutableStateOf<UpdateState>(UpdateState.Idle)
        private set

    /** This build. */
    fun installed(context: Context): Pair<Long, String> {
        val info = context.packageManager.getPackageInfo(context.packageName, 0)
        @Suppress("DEPRECATION")
        val code = if (Build.VERSION.SDK_INT >= 28) info.longVersionCode else info.versionCode.toLong()
        return code to (info.versionName ?: "")
    }

    /** `version.json` at `address` (a folder, or the file itself). */
    private fun manifest(address: String): URL {
        val trimmed = address.trim()
        return URL(if (trimmed.endsWith(".json")) trimmed else trimmed.trimEnd('/') + "/version.json")
    }

    /** Check now, unless checked within `unlessWithinMs` (on opening the app). */
    fun check(context: Context, store: Store, unlessWithinMs: Long = 0) {
        val address = store.updatesUrl ?: return
        if (state is UpdateState.Checking || state is UpdateState.Downloading || state is UpdateState.Installing) return
        if (unlessWithinMs > 0 && System.currentTimeMillis() - store.updatesChecked < unlessWithinMs) return
        state = UpdateState.Checking
        val (installed, _) = installed(context)
        Thread {
            val result = runCatching {
                val url = manifest(address)
                val json = JSONObject(fetch(url).bufferedReader().use { it.readText() })
                Release(
                    json.getLong("versionCode"),
                    json.getString("versionName"),
                    URL(url, json.getString("apk")),
                    json.getString("sha256").lowercase(),
                    json.optLong("size", -1),
                )
            }
            main.post {
                store.updatesChecked = System.currentTimeMillis()
                state = result.fold(
                    { if (it.versionCode > installed) UpdateState.Available(it) else UpdateState.UpToDate(System.currentTimeMillis()) },
                    { UpdateState.Failed("Couldn't check: ${it.message ?: it.javaClass.simpleName}") },
                )
            }
        }.start()
    }

    /**
     * Download and install `release`. The first time, Android asks to let artty install apps:
     * that's opened instead, and Update tapped again after.
     */
    fun install(context: Context, release: Release) {
        val context = context.applicationContext
        if (!context.packageManager.canRequestPackageInstalls()) {
            val intent = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${context.packageName}"))
            context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            return
        }
        state = UpdateState.Downloading(release, 0)
        Thread {
            val result = runCatching {
                val apk = File(context.cacheDir, "update.apk")
                val digest = MessageDigest.getInstance("SHA-256")
                fetch(release.apk).use { input ->
                    apk.outputStream().use { out ->
                        val buffer = ByteArray(64 * 1024)
                        var done = 0L
                        var last = 0L
                        while (true) {
                            val n = input.read(buffer)
                            if (n < 0) break
                            out.write(buffer, 0, n)
                            digest.update(buffer, 0, n)
                            done += n
                            if (done - last > 256 * 1024) {
                                last = done
                                main.post { state = UpdateState.Downloading(release, done) }
                            }
                        }
                    }
                }
                val sha = digest.digest().joinToString("") { "%02x".format(it) }
                check(sha == release.sha256) { "the download isn't what version.json says (SHA-256)" }
                hand(context, apk)
                apk.delete()
            }
            main.post {
                state = result.fold({ UpdateState.Installing(release) }, { UpdateState.Failed("Couldn't update: ${it.message}") })
            }
        }.start()
    }

    /** The APK to the system installer; its answer comes to [Installed]. */
    private fun hand(context: Context, apk: File) {
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName)
            if (Build.VERSION.SDK_INT >= 31) setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        }
        val id = installer.createSession(params)
        installer.openSession(id).use { session ->
            session.openWrite("artty.apk", 0, apk.length()).use { out -> apk.inputStream().use { it.copyTo(out) } }
            val intent = Intent(context, Installed::class.java)
            val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
            session.commit(PendingIntent.getBroadcast(context, id, intent, flags).intentSender)
        }
    }

    internal fun said(reason: String?) {
        main.post { state = if (reason == null) UpdateState.Idle else UpdateState.Failed(reason) }
    }

    private fun fetch(url: URL) = (url.openConnection() as HttpURLConnection).run {
        connectTimeout = 15_000
        readTimeout = 30_000
        setRequestProperty("Cache-Control", "no-cache")
        if (responseCode !in 200..299) throw IllegalStateException("$url: HTTP $responseCode")
        inputStream
    }

    /** The system installer's word: ask the user (it shows its own screen), or how it went. */
    class Installed : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            when (val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
                PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                    @Suppress("DEPRECATION")
                    val confirm = if (Build.VERSION.SDK_INT >= 33) {
                        intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
                    } else {
                        intent.getParcelableExtra(Intent.EXTRA_INTENT)
                    }
                    confirm?.let { context.startActivity(it.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
                }
                // Installed: this process is replaced (and gone) soon.
                PackageInstaller.STATUS_SUCCESS -> said(null)
                else -> {
                    val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "status $status"
                    val why = if (status == PackageInstaller.STATUS_FAILURE_CONFLICT || status == PackageInstaller.STATUS_FAILURE_INCOMPATIBLE) {
                        "$message (signed with another key? Updates install only over a build signed with the same one)"
                    } else {
                        message
                    }
                    said(if (status == PackageInstaller.STATUS_FAILURE_ABORTED) null else "Not installed: $why")
                }
            }
        }
    }
}
