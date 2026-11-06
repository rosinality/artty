package io.github.rosinality.artty

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.artty_android.TabItem

private val Waiting = Color(0xffe0af68)
private val Done = Color(0xff9ece6a)
private val Failed = Color(0xfff7768e)
private val Busy = Color(0xff8fb4ff)

/** The workspace colors' shades, as the Mac's sidebar has them. */
private val WorkspaceColors = mapOf(
    "red" to Color(0xfff7768e),
    "orange" to Color(0xffff9e64),
    "yellow" to Color(0xffe0af68),
    "green" to Color(0xff9ece6a),
    "teal" to Color(0xff73daca),
    "blue" to Color(0xff7aa2f7),
    "purple" to Color(0xffbb9af7),
    "pink" to Color(0xffff7eb6),
)

/** What a dialog the drawer opened asks. */
private sealed interface Asking {
    data class RenameTab(val tab: TabItem) : Asking
    data class CloseTab(val tab: TabItem) : Asking
    data class RenameWorkspace(val workspace: ULong, val name: String, val named: Boolean) : Asking
    data class CloseWorkspace(val workspace: ULong, val name: String, val tabs: Int) : Asking
    data object NewWorkspace : Asking
}

/**
 * The shown window's workspaces and their tabs, as the Mac's sidebar shows them: what each
 * is doing (an agent waiting, a command running, done), and opening, naming and closing them.
 * `onChosen` after a tab is chosen (the drawer closes).
 */
@Composable
fun TabsDrawer(server: String, onSearch: () -> Unit, onChosen: () -> Unit) {
    var asking by remember { mutableStateOf<Asking?>(null) }
    ModalDrawerSheet(drawerContainerColor = Background, drawerContentColor = Ink) {
        Row(Modifier.fillMaxWidth().padding(start = 20.dp, end = 8.dp, top = 12.dp, bottom = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(server, fontWeight = FontWeight.SemiBold, fontSize = 18.sp, modifier = Modifier.weight(1f))
            // The Mac's ⌘⇧F: every pane's output, and agents' conversations.
            TextButton(onClick = onSearch) { Text("Search everything") }
        }
        val workspaces = Session.tabs.groupBy { it.workspace }.values.toList()
        LazyColumn(Modifier.weight(1f)) {
            for (tabs in workspaces) {
                val first = tabs.first()
                item(key = "w${first.workspace}") {
                    WorkspaceHeading(first, tabs.size) { asking = it }
                }
                items(tabs, key = { "t${it.workspace}:${it.index}" }) { tab ->
                    TabRow(tab, onChosen = {
                        Session.selectTab(tab)
                        onChosen()
                    }) { asking = it }
                }
            }
        }
        Row(Modifier.fillMaxWidth().padding(12.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = {
                Session.newTab()
                onChosen()
            }, modifier = Modifier.weight(1f)) { Text("New tab") }
            OutlinedButton(onClick = { asking = Asking.NewWorkspace }, modifier = Modifier.weight(1f)) { Text("New workspace") }
        }
    }
    when (val a = asking) {
        null -> {}
        is Asking.RenameTab -> NameDialog(
            title = "Name the tab",
            initial = a.tab.title,
            hint = Session.title(a.tab),
            reset = if (a.tab.title.isNotBlank()) "Its programs name it" else null,
            onDone = { name ->
                asking = null
                if (name != null) Session.renameTab(a.tab, name)
            },
        )
        is Asking.CloseTab -> Confirm(
            "Close ${Session.title(a.tab)}?",
            "Its programs stop.",
            "Close",
        ) { yes ->
            asking = null
            if (yes) Session.closeTab(a.tab)
        }
        is Asking.RenameWorkspace -> NameDialog(
            title = "Name the workspace",
            initial = if (a.named) a.name else "",
            hint = a.name,
            reset = if (a.named) "Name it after its folder" else null,
            onDone = { name ->
                asking = null
                if (name != null) Session.renameWorkspace(a.workspace, name)
            },
        )
        is Asking.CloseWorkspace -> Confirm(
            "Close ${a.name}?",
            "Its ${if (a.tabs == 1) "tab" else "${a.tabs} tabs"} close, and their programs stop.",
            "Close",
        ) { yes ->
            asking = null
            if (yes) Session.closeWorkspace(a.workspace)
        }
        Asking.NewWorkspace -> NameDialog(
            title = "New workspace",
            initial = "",
            hint = "Name (or none: its folder's)",
            reset = null,
            confirm = "Open",
            onDone = { name ->
                asking = null
                if (name != null) {
                    Session.newWorkspace(name)
                    onChosen()
                }
            },
        )
    }
}

@Composable
private fun WorkspaceHeading(first: TabItem, tabs: Int, ask: (Asking) -> Unit) {
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier.fillMaxWidth().padding(start = 20.dp, end = 4.dp, top = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        WorkspaceColors[first.workspaceColor]?.let {
            Box(Modifier.size(8.dp).clip(CircleShape).background(it))
            Spacer(Modifier.width(8.dp))
        }
        Text(
            first.workspaceName,
            color = Faint,
            fontSize = 13.sp,
            fontWeight = FontWeight.Medium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        Box {
            TextButton(onClick = { menu = true }) { Text("⋯", color = Faint) }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                DropdownMenuItem(text = { Text("New tab here") }, onClick = {
                    menu = false
                    Session.newTab(first.workspace)
                })
                DropdownMenuItem(text = { Text("Rename") }, onClick = {
                    menu = false
                    ask(Asking.RenameWorkspace(first.workspace, first.workspaceName, first.workspaceNamed))
                })
                DropdownMenuItem(text = { Text("Close workspace") }, onClick = {
                    menu = false
                    ask(Asking.CloseWorkspace(first.workspace, first.workspaceName, tabs))
                })
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TabRow(tab: TabItem, onChosen: () -> Unit, ask: (Asking) -> Unit) {
    var menu by remember { mutableStateOf(false) }
    val state = Session.state(tab)
    Box {
        Row(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 8.dp, vertical = 1.dp)
                .clip(RoundedCornerShape(8.dp))
                .background(if (tab.active) Accent.copy(alpha = 0.18f) else Color.Transparent)
                .combinedClickable(onClick = onChosen, onLongClick = { menu = true })
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            StatusIcon(state?.icon ?: "")
            Spacer(Modifier.width(10.dp))
            Column(Modifier.weight(1f)) {
                Text(Session.title(tab), maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = if (tab.active) FontWeight.Medium else null)
                val detail = listOfNotNull(state?.agent, state?.detail?.takeIf { it != state.agent }).joinToString(" · ")
                if (detail.isNotEmpty()) {
                    Text(detail, color = Faint, fontSize = 12.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
            if (tab.panes.size > 1) Text("${tab.panes.size} panes", color = Faint, fontSize = 12.sp)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text("Rename") }, onClick = {
                menu = false
                ask(Asking.RenameTab(tab))
            })
            DropdownMenuItem(text = { Text("Close tab") }, onClick = {
                menu = false
                ask(Asking.CloseTab(tab))
            })
        }
    }
}

/** `busy`, `waiting`, `attention`, `done`, `failed`, as the Mac's sidebar draws them. */
@Composable
private fun StatusIcon(icon: String) {
    Box(Modifier.size(14.dp), contentAlignment = Alignment.Center) {
        when (icon) {
            "busy" -> CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = Busy)
            "waiting", "attention" -> Box(Modifier.size(8.dp).clip(CircleShape).background(Waiting))
            "done" -> Text("✓", color = Done, fontSize = 12.sp)
            "failed" -> Text("✕", color = Failed, fontSize = 12.sp)
        }
    }
}

/** Asks for a name; `onDone(null)` cancelled, `""` back to none (`reset`'s label, if offered). */
@Composable
private fun NameDialog(
    title: String,
    initial: String,
    hint: String,
    reset: String?,
    confirm: String = "Save",
    onDone: (String?) -> Unit,
) {
    // Selected, so typing replaces it.
    var name by remember { mutableStateOf(TextFieldValue(initial, TextRange(0, initial.length))) }
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    AlertDialog(
        onDismissRequest = { onDone(null) },
        title = { Text(title) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                OutlinedTextField(
                    name, { name = it },
                    placeholder = { Text(hint, color = Faint) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    keyboardActions = KeyboardActions(onDone = { onDone(name.text) }),
                    modifier = Modifier.focusRequester(focus),
                )
                if (reset != null) TextButton(onClick = { onDone("") }) { Text(reset) }
            }
        },
        confirmButton = { TextButton(onClick = { onDone(name.text) }) { Text(confirm) } },
        dismissButton = { TextButton(onClick = { onDone(null) }) { Text("Cancel") } },
    )
}

@Composable
private fun Confirm(title: String, text: String, yes: String, onDone: (Boolean) -> Unit) {
    AlertDialog(
        onDismissRequest = { onDone(false) },
        title = { Text(title) },
        text = { Text(text, color = Faint) },
        confirmButton = { TextButton(onClick = { onDone(true) }) { Text(yes, color = Failed) } },
        dismissButton = { TextButton(onClick = { onDone(false) }) { Text("Cancel") } },
    )
}

/** Another tab wants the user (an agent waiting): a dot on the drawer's button. */
fun othersWaiting(): Boolean =
    Session.tabs.any { !it.active && Session.state(it)?.icon.let { i -> i == "waiting" || i == "attention" } }
