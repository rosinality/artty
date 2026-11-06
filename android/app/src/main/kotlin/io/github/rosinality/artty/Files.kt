package io.github.rosinality.artty

import android.content.Context
import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.content.FileProvider
import java.io.File
import java.io.OutputStream
import uniffi.artty_android.Connection
import uniffi.artty_android.FolderEntry
import uniffi.artty_android.Folder
import uniffi.artty_android.IfExists
import uniffi.artty_android.UploadException

/** How much of a file goes in one message up: keys typed meanwhile wait behind at most this. */
private const val UP_CHUNK = 128 * 1024
/** Images larger than this aren't shown (they're fetched whole for it). */
private const val MAX_IMAGE = 20L shl 20
private val IMAGES = setOf("png", "jpg", "jpeg", "gif", "webp", "bmp")

/** A file on the server, for the menus and the viewer. */
private data class Picked(val path: String, val entry: FolderEntry)

/** A file from the phone to send, and what to do if one of its name is there. */
private data class Sending(val uri: Uri, val name: String)

/**
 * The files on the shown pane's machine, from its directory (as the Mac's files drawer): browse
 * folders, look into a file (text, or an image), save one to the phone or hand it to another app,
 * send files from the phone into the folder shown, and type a path into the pane.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun FilesScreen(pane: ULong, onTypePath: (String) -> Unit, onClose: () -> Unit) {
    val context = LocalContext.current
    /** The folder asked for; `null`: the pane's own (and follow it as it moves). */
    var asked by remember { mutableStateOf<String?>(null) }
    var folder by remember { mutableStateOf<Folder?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var loading by remember { mutableStateOf(false) }
    var hidden by remember { mutableStateOf(false) }
    var viewing by remember { mutableStateOf<Picked?>(null) }
    var menu by remember { mutableStateOf<Picked?>(null) }
    /** A transfer under way, said ("Saving notes.md · 40%"), or its outcome. */
    var transfer by remember { mutableStateOf<String?>(null) }
    var saving by remember { mutableStateOf<Picked?>(null) }
    /** Files to send waiting, and the one whose name is taken there (asking what to do). */
    var queue by remember { mutableStateOf<List<Sending>>(emptyList()) }
    var taken by remember { mutableStateOf<Sending?>(null) }

    fun load(path: String?) {
        loading = true
        Session.io({ it.listDir(pane, path, true) }) { result ->
            loading = false
            result.onSuccess {
                folder = it
                error = null
            }.onFailure { error = it.message ?: it.toString() }
        }
    }
    fun go(path: String?) {
        asked = path
        load(path)
    }
    LaunchedEffect(Unit) { load(null) }
    DisposableEffect(pane) {
        Session.onDirChanged = { if (it == pane) load(asked) }
        onDispose {
            Session.onDirChanged = null
            Session.io({ it.unwatch(pane) }) {}
        }
    }

    /** Send the files waiting, one by one; `existing` for the first (its name was taken). */
    fun sendNext(existing: IfExists = IfExists.ASK) {
        val next = queue.firstOrNull() ?: return
        val dir = folder?.path ?: return
        transfer = "Sending ${next.name}…"
        Session.io({ send(context, it, pane, next, dir, existing) { done -> transfer = "Sending ${next.name} · $done%" } }) { result ->
            result.onSuccess {
                queue = queue.drop(1)
                transfer = "Sent ${next.name}"
                if (queue.isNotEmpty()) sendNext() else load(asked)
            }.onFailure {
                if (it is UploadException.Exists) {
                    taken = next
                    transfer = null
                } else {
                    transfer = "Couldn't send ${next.name}: ${(it as? UploadException.Failed)?.reason ?: it.message}"
                    queue = emptyList()
                }
            }
        }
    }
    val pickToSend = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        queue = uris.map { Sending(it, displayName(context, it)) }
        sendNext()
    }
    val pickWhereToSave = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("*/*")) { uri ->
        val file = saving ?: return@rememberLauncherForActivityResult
        saving = null
        if (uri == null) return@rememberLauncherForActivityResult
        transfer = "Saving ${file.entry.name}…"
        Session.io({ connection ->
            context.contentResolver.openOutputStream(uri)!!.use { out ->
                fetch(connection, pane, file.path, out) { done -> transfer = "Saving ${file.entry.name} · $done%" }
            }
        }) { result -> transfer = result.fold({ "Saved ${file.entry.name}" }, { "Couldn't save ${file.entry.name}: ${it.message}" }) }
    }
    fun share(file: Picked) {
        transfer = "Fetching ${file.entry.name}…"
        val dir = File(context.cacheDir, "shared").apply { mkdirs() }
        // One at a time: the last shared is all that's kept.
        dir.listFiles()?.forEach { it.delete() }
        val local = File(dir, file.entry.name)
        Session.io({ connection ->
            local.outputStream().use { fetch(connection, pane, file.path, it) { done -> transfer = "Fetching ${file.entry.name} · $done%" } }
        }) { result ->
            result.onSuccess {
                transfer = null
                val uri = FileProvider.getUriForFile(context, "${context.packageName}.files", local)
                val send = Intent(Intent.ACTION_SEND)
                    .setType(mimeType(file.entry.name))
                    .putExtra(Intent.EXTRA_STREAM, uri)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                context.startActivity(Intent.createChooser(send, file.entry.name))
            }.onFailure { transfer = "Couldn't fetch ${file.entry.name}: ${it.message}" }
        }
    }

    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Column(Modifier.fillMaxSize().background(Background).safeDrawingPadding()) {
            val path = folder?.path
            Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                val parent = path?.let { File(it).parent }
                TextButton(onClick = { go(parent) }, enabled = parent != null) { Text("↑", fontSize = 18.sp) }
                Text(
                    path?.let(::shortPath) ?: "",
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.StartEllipsis,
                    modifier = Modifier.weight(1f),
                )
                if (loading) CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp)
                TextButton(onClick = { hidden = !hidden }) { Text(if (hidden) "Hide .files" else "Show .files", fontSize = 12.sp) }
                TextButton(onClick = onClose) { Text("Done") }
            }
            Row(Modifier.fillMaxWidth().padding(horizontal = 8.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                TextButton(onClick = { pickToSend.launch(arrayOf("*/*")) }, enabled = path != null && queue.isEmpty()) { Text("Send files here") }
                TextButton(onClick = { go(null) }, enabled = asked != null) { Text("The pane's folder") }
            }
            transfer?.let { Text(it, color = Faint, fontSize = 13.sp, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) }
            error?.let { Text(it, color = Color(0xfff7768e), fontSize = 13.sp, modifier = Modifier.padding(16.dp)) }
            val entries = folder?.entries.orEmpty()
                .filter { hidden || !it.name.startsWith(".") }
                .sortedWith(compareBy<FolderEntry> { !it.dir }.thenBy { it.name.lowercase() })
            if (folder != null && entries.isEmpty()) Text("Nothing here.", color = Faint, modifier = Modifier.padding(16.dp))
            LazyColumn(Modifier.weight(1f)) {
                items(entries, key = { it.name }) { entry ->
                    val full = File(path ?: "", entry.name).path
                    Box {
                        Row(
                            Modifier
                                .fillMaxWidth()
                                .combinedClickable(
                                    onClick = { if (entry.dir) go(full) else viewing = Picked(full, entry) },
                                    onLongClick = { menu = Picked(full, entry) },
                                )
                                .padding(horizontal = 16.dp, vertical = 10.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(if (entry.dir) "▸" else " ", color = Accent, modifier = Modifier.width(18.dp))
                            Text(
                                entry.name + if (entry.link) " ↗" else "",
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                fontWeight = if (entry.dir) FontWeight.Medium else null,
                                modifier = Modifier.weight(1f),
                            )
                            if (!entry.dir) Text(size(entry.size), color = Faint, fontSize = 12.sp)
                        }
                        DropdownMenu(expanded = menu?.path == full, onDismissRequest = { menu = null }) {
                            DropdownMenuItem(text = { Text("Type its path") }, onClick = {
                                menu = null
                                onTypePath(full)
                                onClose()
                            })
                            if (!entry.dir) {
                                DropdownMenuItem(text = { Text("Save to the phone") }, onClick = {
                                    menu = null
                                    saving = Picked(full, entry)
                                    pickWhereToSave.launch(entry.name)
                                })
                                DropdownMenuItem(text = { Text("Share") }, onClick = {
                                    menu = null
                                    share(Picked(full, entry))
                                })
                            }
                        }
                    }
                }
            }
        }
    }
    viewing?.let { file ->
        FileViewer(pane, file, onClose = { viewing = null }, onSave = {
            saving = file
            pickWhereToSave.launch(file.entry.name)
        }, onShare = { share(file) }, onTypePath = {
            viewing = null
            onTypePath(file.path)
            onClose()
        })
    }
    taken?.let { sending ->
        AlertDialog(
            onDismissRequest = {},
            title = { Text("${sending.name} is there") },
            text = { Text("A file of that name is in this folder already.", color = Faint) },
            confirmButton = {
                Row {
                    TextButton(onClick = {
                        taken = null
                        sendNext(IfExists.KEEP_BOTH)
                    }) { Text("Keep both") }
                    TextButton(onClick = {
                        taken = null
                        sendNext(IfExists.REPLACE)
                    }) { Text("Replace") }
                }
            },
            dismissButton = {
                TextButton(onClick = {
                    taken = null
                    queue = queue.drop(1)
                    if (queue.isNotEmpty()) sendNext() else load(asked)
                }) { Text("Skip") }
            },
        )
    }
}

/** A look into a file: an image, or (the first part of) its text. */
@Composable
private fun FileViewer(pane: ULong, file: Picked, onClose: () -> Unit, onSave: () -> Unit, onShare: () -> Unit, onTypePath: () -> Unit) {
    var text by remember { mutableStateOf<String?>(null) }
    var image by remember { mutableStateOf<ImageBitmap?>(null) }
    var note by remember { mutableStateOf<String?>(null) }
    val extension = file.entry.name.substringAfterLast('.', "").lowercase()
    LaunchedEffect(file.path) {
        if (extension in IMAGES && file.entry.size <= MAX_IMAGE.toULong()) {
            Session.io({ connection ->
                val bytes = java.io.ByteArrayOutputStream().also { fetch(connection, pane, file.path, it) {} }.toByteArray()
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
            }) { result ->
                image = result.getOrNull()?.asImageBitmap()
                if (image == null) note = "Couldn't show it: ${result.exceptionOrNull()?.message ?: "not an image this phone reads"}"
            }
        } else {
            Session.io({ it.readFile(pane, file.path, 0uL) }) { result ->
                result.onSuccess { part ->
                    val data = part.data
                    // A NUL early on: not text.
                    if (data.take(8192).any { it == 0.toByte() }) {
                        note = "It isn't text (${size(part.length)}): save it, or share it to an app that opens it."
                    } else {
                        text = String(data, Charsets.UTF_8)
                        if (part.length > data.size.toULong()) note = "The first ${size(data.size.toULong())} of ${size(part.length)}."
                    }
                }.onFailure { note = "Couldn't read it: ${it.message}" }
            }
        }
    }
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Column(Modifier.fillMaxSize().background(Background).safeDrawingPadding()) {
            Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                TextButton(onClick = onClose) { Text("‹", fontSize = 20.sp) }
                Text(file.entry.name, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.Medium, modifier = Modifier.weight(1f))
                TextButton(onClick = onTypePath) { Text("Type path") }
                TextButton(onClick = onSave) { Text("Save") }
                TextButton(onClick = onShare) { Text("Share") }
            }
            note?.let { Text(it, color = Faint, fontSize = 13.sp, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) }
            when {
                image != null -> Image(image!!, file.entry.name, Modifier.fillMaxSize().padding(8.dp))
                text != null -> Box(Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()).horizontalScroll(rememberScrollState())) {
                    Text(text!!, fontFamily = FontFamily.Monospace, fontSize = 12.sp, softWrap = false, modifier = Modifier.padding(12.dp))
                }
                note == null -> CircularProgressIndicator(Modifier.padding(24.dp))
            }
        }
    }
}

/** Fetch a whole file from the pane's machine into `out`; `progress` in percent (on the main thread). */
private fun fetch(connection: Connection, pane: ULong, path: String, out: OutputStream, progress: (Int) -> Unit) {
    var offset = 0uL
    while (true) {
        val part = connection.readFile(pane, path, offset)
        out.write(part.data)
        offset += part.data.size.toULong()
        if (part.length > 0uL) postProgress(progress, (offset * 100uL / part.length).toInt())
        if (part.data.isEmpty() || offset >= part.length) return
    }
}

/** Send a file of the phone's into `dir` on the pane's machine, part by part. */
private fun send(
    context: Context,
    connection: Connection,
    pane: ULong,
    file: Sending,
    dir: String,
    existing: IfExists,
    progress: (Int) -> Unit,
): String? {
    val length = context.contentResolver.openAssetFileDescriptor(file.uri, "r")?.use { it.length }?.takeIf { it > 0 }
    val upload = kotlin.random.Random.nextLong().toULong()
    context.contentResolver.openInputStream(file.uri)!!.use { input ->
        var sent = 0L
        val buffer = ByteArray(UP_CHUNK)
        while (true) {
            var filled = 0
            while (filled < buffer.size) {
                val read = input.read(buffer, filled, buffer.size - filled)
                if (read < 0) break
                filled += read
            }
            val last = filled < buffer.size
            val placed = connection.uploadPart(pane, upload, file.name, buffer.copyOf(filled), last, dir, existing)
            sent += filled
            if (length != null) postProgress(progress, (sent * 100 / length).toInt())
            if (last) return placed
        }
    }
}

private val mainThread = android.os.Handler(android.os.Looper.getMainLooper())

private fun postProgress(progress: (Int) -> Unit, percent: Int) = mainThread.post { progress(percent.coerceIn(0, 100)) }

/** What a document picked on the phone is called. */
private fun displayName(context: Context, uri: Uri): String =
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
        if (it.moveToFirst()) it.getString(0) else null
    } ?: uri.lastPathSegment?.substringAfterLast('/') ?: "file"

private fun mimeType(name: String): String =
    MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase()) ?: "application/octet-stream"

/** A size, said: 12 B, 3.4 KB, 120 MB. */
private fun size(bytes: ULong): String {
    val b = bytes.toDouble()
    return when {
        b < 1024 -> "${bytes} B"
        b < 1024 * 1024 -> "%.1f KB".format(b / 1024)
        b < 1024.0 * 1024 * 1024 -> "%.1f MB".format(b / 1024 / 1024)
        else -> "%.1f GB".format(b / 1024 / 1024 / 1024)
    }
}

/** A path with its home shortened to `~`. */
private fun shortPath(path: String): String = Regex("^/(home|Users)/[^/]+").replace(path, "~")
