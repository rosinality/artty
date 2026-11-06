package io.github.rosinality.artty

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.delay
import uniffi.artty_android.FindResult
import uniffi.artty_android.FoundLine
import uniffi.artty_android.FoundSession

/** How long typing pauses before a search. */
private const val PAUSE_MS = 300L

private val Missing = Color(0xfff7768e)

/**
 * Finding in the pane shown (the Mac's ⌘F): each match selected and scrolled to, newest first,
 * "3 of 17". In a full-screen agent's pane (its history isn't the terminal's) it offers to
 * search the agent's conversations instead: `onConversations` with the text.
 */
@Composable
fun FindBar(pane: ULong, onConversations: (String) -> Unit, onClose: () -> Unit) {
    var text by remember { mutableStateOf("") }
    var result by remember { mutableStateOf<Result<FindResult>?>(null) }
    val focus = remember { FocusRequester() }
    fun run(older: Boolean, restart: Boolean) {
        if (text.isEmpty()) return
        val asked = text
        Session.find(pane, asked, older, restart) { if (asked == text) result = it }
    }
    LaunchedEffect(Unit) { focus.requestFocus() }
    // Typing: from the newest match back.
    LaunchedEffect(text, pane) {
        if (text.isEmpty()) {
            result = null
            Session.endFind(pane)
            return@LaunchedEffect
        }
        delay(PAUSE_MS)
        run(older = true, restart = true)
    }
    val found = result?.getOrNull()
    val agent = Session.panes[pane]?.agent
    Row(
        Modifier.fillMaxWidth().background(Panel).padding(start = 12.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.weight(1f)) {
            if (text.isEmpty()) Text("Find in this pane", color = Faint, fontSize = 14.sp)
            BasicTextField(
                text, { text = it },
                singleLine = true,
                textStyle = TextStyle(color = Ink, fontSize = 14.sp, fontFamily = FontFamily.Monospace),
                cursorBrush = SolidColor(Accent),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                // Enter: the one before, as on the Mac.
                keyboardActions = KeyboardActions(onSearch = { run(older = true, restart = false) }),
                modifier = Modifier.fillMaxWidth().focusRequester(focus),
            )
        }
        val status = when {
            result?.isFailure == true -> "invalid pattern" to Missing
            found == null -> null
            !found.history && agent != null -> null
            !found.found -> "no match" to Missing
            else -> "${found.index} of ${found.total}${if (found.more) "+" else ""}" to Faint
        }
        status?.let { (label, color) -> Text(label, color = color, fontSize = 12.sp, modifier = Modifier.padding(horizontal = 6.dp)) }
        // A full-screen agent keeps its history itself: its conversations can be searched.
        if (found != null && !found.history && agent != null) {
            TextButton(onClick = { onConversations(text) }) { Text("Search $agent's conversations", fontSize = 12.sp) }
        }
        TextButton(onClick = { run(older = true, restart = false) }, enabled = found?.found == true) { Text("↑") }
        TextButton(onClick = { run(older = false, restart = false) }, enabled = found?.found == true) { Text("↓") }
        TextButton(onClick = {
            Session.endFind(pane)
            onClose()
        }) { Text("✕") }
    }
}

/** What a search everything found, as its list shows it. */
private sealed interface Hit {
    data class Line(val line: FoundLine) : Hit
    data class Conversation(val session: FoundSession) : Hit
    data class Heading(val text: String) : Hit
}

/**
 * Search everything (the Mac's ⌘⇧F): what every pane of the shown window printed, history too,
 * and coding agents' conversations on the server. A line found is shown in its pane, selected;
 * a conversation carries on in a new tab, in its directory.
 */
@Composable
fun SearchEverything(initial: String, onClose: () -> Unit) {
    var text by remember { mutableStateOf(initial) }
    var found by remember { mutableStateOf<Session.Found?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var searching by remember { mutableIntStateOf(0) }
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    LaunchedEffect(text) {
        if (text.isBlank()) {
            found = null
            return@LaunchedEffect
        }
        delay(PAUSE_MS)
        val asked = text
        searching++
        Session.searchEverything(asked) { result ->
            searching--
            if (asked != text) return@searchEverything
            found = result.getOrNull()
            error = result.exceptionOrNull()?.let { "Couldn't search: ${it.message ?: it}" }
        }
    }
    // The phone shows one window: lines in others are counted, not listed.
    val shown = Session.tabs.flatMap { it.panes }.toSet()
    val lines = found?.lines.orEmpty()
    val here = lines.filter { it.pane in shown }
    val elsewhere = lines.size - here.size
    val sessions = found?.sessions.orEmpty()
    val rows = buildList {
        if (here.isNotEmpty() || elsewhere > 0) {
            add(Hit.Heading(if (elsewhere > 0) "Output · $elsewhere more in other windows" else "Output"))
            here.forEach { add(Hit.Line(it)) }
        }
        if (sessions.isNotEmpty()) {
            add(Hit.Heading("Conversations"))
            sessions.forEach { add(Hit.Conversation(it)) }
        }
    }
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Column(Modifier.fillMaxSize().background(Background).safeDrawingPadding()) {
            Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp, top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.weight(1f)) {
                    if (text.isEmpty()) Text("Search output and conversations", color = Faint, fontSize = 16.sp)
                    BasicTextField(
                        text, { text = it },
                        singleLine = true,
                        textStyle = TextStyle(color = Ink, fontSize = 16.sp),
                        cursorBrush = SolidColor(Accent),
                        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                        modifier = Modifier.fillMaxWidth().focusRequester(focus),
                    )
                }
                if (searching > 0) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                TextButton(onClick = onClose) { Text("Done") }
            }
            error?.let { Text(it, color = Missing, fontSize = 13.sp, modifier = Modifier.padding(horizontal = 16.dp)) }
            if (found != null && rows.isEmpty() && searching == 0) {
                Text("Nothing mentions it.", color = Faint, modifier = Modifier.padding(16.dp))
            }
            LazyColumn(Modifier.weight(1f)) {
                items(rows) { row ->
                    when (row) {
                        is Hit.Heading -> Text(
                            row.text, color = Faint, fontSize = 12.sp, fontWeight = FontWeight.Medium,
                            modifier = Modifier.padding(start = 16.dp, top = 14.dp, bottom = 4.dp),
                        )
                        is Hit.Line -> LineRow(row.line, text) {
                            Session.reveal(row.line)
                            onClose()
                        }
                        is Hit.Conversation -> ConversationRow(row.session, text) {
                            Session.resume(row.session)
                            onClose()
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun LineRow(line: FoundLine, query: String, onClick: () -> Unit) {
    val tab = Session.tabs.firstOrNull { line.pane in it.panes }
    Column(Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 16.dp, vertical = 8.dp)) {
        Text(
            marked(line.text.trim(), query),
            fontFamily = FontFamily.Monospace,
            fontSize = 13.sp,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
        tab?.let { Text("${it.workspaceName} · ${Session.title(it)}", color = Faint, fontSize = 12.sp, maxLines = 1) }
    }
}

@Composable
private fun ConversationRow(session: FoundSession, query: String, onClick: () -> Unit) {
    val (mark, color) = when (session.agent) {
        "claude" -> "✳" to Color(0xffd97757)
        "codex" -> "◇" to Color(0xff10a37f)
        "gemini" -> "✦" to Color(0xff4796e3)
        else -> "●" to Color(0xff9aa0a6)
    }
    Row(Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 16.dp, vertical = 8.dp)) {
        Text(mark, color = color, fontSize = 14.sp)
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(session.title.ifBlank { session.agent }, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.Medium)
            Text(marked(session.snippet, query), color = Faint, fontSize = 13.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
            val where = listOfNotNull(session.cwd?.let(::shortPath), ago(session.modified)).joinToString(" · ")
            Text(where, color = Faint, fontSize = 12.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
    }
}

/** `text` with `query` in it marked, matched as the server does: case ignored unless it has capitals. */
private fun marked(text: String, query: String): AnnotatedString = buildAnnotatedString {
    val q = query.trim()
    val ignoreCase = q == q.lowercase()
    var at = 0
    while (q.isNotEmpty()) {
        val found = text.indexOf(q, at, ignoreCase)
        if (found < 0) break
        append(text.substring(at, found))
        withStyle(SpanStyle(color = Accent, fontWeight = FontWeight.SemiBold)) { append(text.substring(found, found + q.length)) }
        at = found + q.length
    }
    append(text.substring(at))
}

/** A path with its home shortened to `~`. */
private fun shortPath(path: String): String =
    Regex("^/(home|Users)/[^/]+").replace(path, "~")

/** How long ago `seconds` (since the epoch) was, roughly. */
private fun ago(seconds: ULong): String? {
    if (seconds == 0uL) return null
    val minutes = (System.currentTimeMillis() / 1000 - seconds.toLong()) / 60
    return when {
        minutes < 1 -> "just now"
        minutes < 60 -> "${minutes}m ago"
        minutes < 60 * 24 -> "${minutes / 60}h ago"
        else -> "${minutes / 60 / 24}d ago"
    }
}
