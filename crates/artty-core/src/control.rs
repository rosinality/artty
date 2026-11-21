//! The control protocol between the artty app and the `artty` command line
//! tool: one JSON object per line over a Unix socket.
//!
//! Programs inside artty find the socket in `$ARTTY_SOCKET` and their own tab
//! in `$ARTTY_SESSION_ID`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SessionId;

pub const SOCKET_ENV: &str = "ARTTY_SOCKET";
pub const SESSION_ENV: &str = "ARTTY_SESSION_ID";

/// Longest socket path macOS accepts (`sun_path` is 104 bytes with the NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Directories that may hold artty sockets: the user's temp directory, or
/// `/tmp` when that path would be too long for a socket.
fn socket_dirs() -> Vec<PathBuf> {
    let temp = std::env::temp_dir();
    let fallback = PathBuf::from("/tmp");
    if temp == fallback { vec![temp] } else { vec![temp, fallback] }
}

/// A directory only this user can use, for the server's socket and what
/// panes run (the `artty` command, shell integration). macOS's temporary
/// directory is the user's own; elsewhere `/tmp` is everyone's, so it's
/// `/tmp/artty-UID`, made private and checked to be (as tmux does: anyone
/// could have made it first), else `~/.cache/artty/run`.
pub fn private_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return std::env::temp_dir();
    }
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let shared = std::env::temp_dir().join(format!("artty-{uid}"));
    if make_private(&shared, uid).is_ok() {
        return shared;
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let own = home.join(".cache/artty/run");
    if let Err(err) = make_private(&own, uid) {
        log::error!("no private directory ({}: {err})", own.display());
    }
    own
}

/// Make `dir` (mode 0700) unless it's there, and check it's this user's
/// alone (see [`make_private`]).
pub fn make_private_dir(dir: &Path) -> io::Result<()> {
    // SAFETY: getuid can't fail.
    make_private(dir, unsafe { libc::getuid() })
}

/// Make `dir` (mode 0700) unless it's there, and check it's `uid`'s alone:
/// a directory, not a link, no one else's, and no one else may write in it.
fn make_private(dir: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {},
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
        Err(err) => return Err(err),
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::other("someone else's"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The artty server's socket. Everything goes through it: the app's windows,
/// the `artty` command, and agent hooks.
pub fn server_socket_path() -> PathBuf {
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    if !cfg!(target_os = "macos") {
        let path = private_dir().join("server.sock");
        // Servers from before kept theirs in /tmp itself: keep reaching
        // one that's still running (it's ours: anyone could put one there).
        let legacy = std::env::temp_dir().join("artty-server.sock");
        let ours = |path: &Path| {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            std::fs::symlink_metadata(path)
                .is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid)
        };
        if !path.exists() && ours(&legacy) {
            return legacy;
        }
        return path;
    }
    let path = std::env::temp_dir().join("artty-server.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        path
    } else {
        PathBuf::from(format!("/tmp/artty-server-{uid}.sock"))
    }
}

/// Socket path for the app process `pid`.
pub fn socket_path(pid: u32) -> PathBuf {
    let name = format!("artty-{pid}.sock");
    let path = std::env::temp_dir().join(&name);
    if path.as_os_str().len() <= MAX_SOCKET_PATH { path } else { PathBuf::from("/tmp").join(name) }
}

/// Sockets of running artty instances, most recently started first.
pub fn live_sockets() -> Vec<PathBuf> {
    let mut sockets: Vec<_> = socket_dirs()
        .into_iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("artty-") && name.ends_with(".sock")
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    sockets.sort();
    sockets
        .into_iter()
        .rev()
        .map(|(_, path)| path)
        .filter(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// All tabs.
    List,
    /// Open the Settings window.
    Settings,
    /// Open a web page in the app's machine's browser.
    OpenUrl {
        url: String,
    },
    /// Show the Activity view in the frontmost window.
    Activity,
    /// Quit the app and start it again (e.g. after a rebuild); sessions keep
    /// running in the server.
    Restart,
    /// Server status: version, uptime, sessions.
    ServerStatus,
    /// End every session and stop the server.
    StopServer,
    /// Save the sessions and stop the server so a new one (e.g. a newer
    /// build) starts in its place and reopens them.
    RestartServer,
    /// Processes and resource use of panes (all, or `tab`).
    Processes {
        tab: Option<SessionId>,
    },
    /// Terminate a pane's foreground program (`signal`, default TERM), or the
    /// whole pane with `pane: true`.
    Kill {
        tab: SessionId,
        signal: Option<i32>,
        pane: bool,
    },
    /// Send a signal (default SIGTERM) to one process; only processes running
    /// in artty's panes.
    Signal {
        pid: i32,
        signal: Option<i32>,
    },
    /// Hand every session to the server build at `executable` without
    /// ending them (it takes over this process).
    HandOver {
        executable: PathBuf,
    },
    /// Every workspace, shown or not.
    Workspaces,
    /// A workspace with one tab, in the frontmost window.
    NewWorkspace {
        name: Option<String>,
        cwd: Option<PathBuf>,
        command: Vec<String>,
    },
    /// Workspaces are named by id or by name. An empty name goes back to
    /// automatic.
    RenameWorkspace {
        workspace: String,
        name: String,
    },
    /// End every tab in it.
    CloseWorkspace {
        workspace: String,
    },
    /// Show it: in the frontmost window, or bring forward the window
    /// already showing it.
    OpenWorkspace {
        workspace: String,
    },
    /// Take it out of its window; it keeps running, hidden.
    HideWorkspace {
        workspace: String,
    },
    /// Change a workspace's defaults: where new tabs start, its color (or
    /// `none`), and environment variables for new panes.
    ConfigureWorkspace {
        workspace: String,
        #[serde(default)]
        root: Option<PathBuf>,
        #[serde(default)]
        clear_root: bool,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        unset_env: Vec<String>,
    },
    /// Text from a tab.
    Read {
        tab: Option<SessionId>,
        source: ReadSource,
    },
    /// Type text into a tab.
    Send {
        tab: Option<SessionId>,
        text: String,
        enter: bool,
    },
    /// Open a tab running the shell, or `command`; with `split`, open a pane
    /// next to `target` (default: the focused pane) instead.
    New {
        cwd: Option<PathBuf>,
        command: Vec<String>,
        title: Option<String>,
        focus: bool,
        #[serde(default)]
        split: Option<SplitDirection>,
        #[serde(default)]
        target: Option<SessionId>,
        /// Open the tab in the workspace with this name, creating it (in the
        /// frontmost window) if there's none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        /// Run it in a git worktree of its own: this branch's, beside the
        /// repository `cwd` is in (made if need be; removed with the tab if
        /// it has no changes).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
        /// Its first message (for an agent the command starts): an ask,
        /// given it once it's up.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        /// As an ask's: the thread the task is written from (a card there
        /// follows it), and whether its answer goes there too.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<SessionId>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        back: bool,
        /// Start the agent (Claude Code) on a copy of this conversation of
        /// its: everything said so far, under a new id; the conversation
        /// itself is left as it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork: Option<String>,
    },
    Focus {
        tab: SessionId,
    },
    /// Name the tab holding pane `tab`; an empty title goes back to automatic.
    Rename {
        tab: Option<SessionId>,
        title: String,
    },
    Close {
        tab: SessionId,
    },
    /// Run a command in a tab's shell and wait for it to finish. Needs shell
    /// integration in that shell.
    Run {
        tab: Option<SessionId>,
        command: String,
        timeout_secs: Option<u64>,
    },
    /// Show a notification for a tab.
    Notify {
        tab: Option<SessionId>,
        title: Option<String>,
        body: String,
    },
    /// Report what an agent in a tab is doing (used by agent hooks).
    Status {
        tab: Option<SessionId>,
        state: AgentState,
        message: Option<String>,
        /// The agent's conversation id, so the tab can resume it later.
        #[serde(default)]
        agent_session: Option<String>,
        /// Which agent is reporting (`claude`, `codex`, ...).
        #[serde(default)]
        agent: Option<String>,
        /// The hook event behind the report (with `--hook`), which tells
        /// turns apart: the state alone doesn't (a waiting agent may be in
        /// a turn, asking to use a tool, or out of one, idle).
        #[serde(default)]
        hook: Option<HookEvent>,
    },
    /// Give an agent a message as if typed, once it's between turns, and
    /// (with `wait`) answer with its reply when its turn ends.
    Ask {
        tab: SessionId,
        text: String,
        /// Allow text the agent would take as a command (`/...`, `!...`).
        #[serde(default)]
        raw: bool,
        wait: bool,
        /// Answer with how far it got after this long (it carries on).
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// The agent whose thread this is part of (written from its view,
        /// to another agent): a card there follows the ask. An agent
        /// asking from its own pane needs none (its pane is known).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<SessionId>,
        /// Once answered, give the answer to `thread`'s agent too.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        back: bool,
    },
    /// Give an answered ask's answer to the agent whose thread it was
    /// written from (as an ask of its own); the new ask.
    AskForward {
        id: u64,
    },
    /// Where an ask is; with `wait`, once it's over.
    AskResult {
        id: u64,
        wait: bool,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Take back an ask not yet given to its agent; with `abandon`, stop
    /// waiting for one that's with it.
    AskCancel {
        id: u64,
        #[serde(default)]
        abandon: bool,
    },
    /// The asks not yet over (all, or a pane's).
    AskList {
        #[serde(default)]
        tab: Option<SessionId>,
    },
    /// Take back every ask not yet given to its agent (all, or a pane's).
    AskCancelQueued {
        #[serde(default)]
        tab: Option<SessionId>,
    },
    /// An agent asks permission to use a tool (its `PermissionRequest`
    /// hook). Answered once someone decides, or with no decision when the
    /// agent's own prompt was answered, or its turn ended. The agent shows
    /// its prompt meanwhile; the request is over when this connection
    /// closes (the agent ending the hook: its prompt was dismissed).
    Approve {
        tab: Option<SessionId>,
        #[serde(default)]
        agent: Option<String>,
        tool_name: String,
        #[serde(default)]
        tool_input: serde_json::Value,
        /// The permission updates the agent offers for "always" (Claude
        /// Code's `permission_suggestions`), passed back as they are.
        #[serde(default)]
        suggestions: Vec<serde_json::Value>,
    },
    /// Permission requests (all, or a pane's), pending first.
    Approvals {
        #[serde(default)]
        tab: Option<SessionId>,
    },
    /// Answer a permission request. Not from programs running in artty's
    /// panes (an agent mustn't approve its own requests).
    Decide {
        id: u64,
        allow: bool,
        /// Why, for the agent (a denial).
        #[serde(default)]
        message: Option<String>,
        /// Allow, and always: the request's option of this index (one of
        /// the agent's own suggestions).
        #[serde(default)]
        always: Option<usize>,
    },
    /// What needs the user: permission requests, agents waiting, unread
    /// replies (what apps are sent as the `queue` extension).
    Queue,
    /// Mark a reply in the queue read.
    QueueDismiss {
        id: u64,
    },
    /// Every pane running an agent, with what it's doing.
    Agents,
    /// What a pane's agent and the user said and did, oldest first: its
    /// entries after `after` (an entry's `seq`), the last `limit` of them.
    Thread {
        tab: SessionId,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Forget a pane's thread (its entries, here and on disk): what's done
    /// in its terminal isn't touched. Not from programs in artty's panes.
    ThreadClear {
        tab: SessionId,
    },
    /// Every pane's thread (or `tab`'s) searched for `text` (case ignored
    /// unless it has capitals): the newest matches first.
    ThreadSearch {
        text: String,
        #[serde(default)]
        tab: Option<SessionId>,
    },
    /// Wait for the first of these asks to be over (`all`: every one), at
    /// most `timeout_secs`; how they all are then.
    AskWait {
        ids: Vec<u64>,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Agents' conversations on this machine, newest first: the latest
    /// (`limit` at most), or those mentioning `text`. Those going on in
    /// artty's panes aren't listed.
    Sessions {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// What was said in a conversation (Claude Code's), as it stands: the
    /// people's messages and the agent's, the last `limit` of them.
    Conversation {
        session: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Every pane's thread entries after `after` (an entry's `seq`), or
    /// `tab`'s; none yet: wait for one, at most `timeout_secs`. Without
    /// `after`: none, and where they're up to (to go on from).
    Events {
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        tab: Option<SessionId>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
}

/// A message in a conversation: `user` or `assistant`, and its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    pub role: String,
    pub text: String,
}

/// A thread entry that matched a search: where, and the text around it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadHit {
    pub pane: SessionId,
    /// Its tab's title, and its agent (if one runs there now).
    pub title: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub seq: u64,
    pub at_ms: u64,
    /// What the entry is: asked, answered, handed off, permission, ...
    pub kind: String,
    /// The matching text's line, shortened around the match.
    pub snippet: String,
}

/// A thread entry, and whose thread it's in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub pane: SessionId,
    #[serde(flatten)]
    pub entry: ThreadEntry,
}

/// A pane's agent, as `Agents` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane: SessionId,
    /// `claude`, `codex`, ...
    pub agent: String,
    /// Its tab's title, and its workspace's name.
    pub title: String,
    pub workspace: String,
    pub state: AgentState,
    /// What it last said (or asks).
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// When its thread last had something (Unix ms).
    #[serde(default)]
    pub last_ms: Option<u64>,
    /// Its permission requests pending, and replies unread.
    pub pending: usize,
    pub unread: usize,
    /// The ask it's at work on.
    #[serde(default)]
    pub ask: Option<u64>,
    /// Its conversation's id (for `artty new --fork`), as its hooks said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The pane of the agent that started it (or whose conversation it's a
    /// copy of), while that's open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
}

/// The agent an ask came from: run in one of artty's panes. As it was when
/// it asked (its pane's id may since have changed, after a restart).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskFrom {
    pub pane: SessionId,
    /// Its tab's title.
    pub title: String,
    /// `claude`, `codex`, ... if known.
    #[serde(default)]
    pub agent: Option<String>,
}

/// A file an agent changed in a turn, and how (its edits' text, shortened).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: String,
    /// Lines its edits put in and took out (a file written whole: all in).
    pub added: u32,
    pub removed: u32,
    /// Written whole (not edited in place).
    #[serde(default)]
    pub written: bool,
    /// Each edit: what was there, what's there now (shortened).
    #[serde(default)]
    pub hunks: Vec<(String, String)>,
}

/// One thing in an agent's thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadEntry {
    /// Increasing, server-wide: ask for what came after one.
    pub seq: u64,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub item: ThreadItem,
    /// Undone in the agent (Claude Code's rewind): no longer in its
    /// conversation, though it happened.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewound: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadItem {
    /// A turn began: what the agent was given (by `artty ask`, with its id,
    /// or typed in its terminal).
    /// `from`: the agent whose `artty ask` it was, if another agent's.
    Prompt {
        text: String,
        ask: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<AskFrom>,
        /// The agent's id for it (Claude Code's `prompt_id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
    },
    /// A turn ended: the agent's last message.
    Reply { text: String },
    /// It asked permission to use a tool; `state` is how that went.
    Permission {
        id: u64,
        tool_name: String,
        summary: String,
        state: ApprovalState,
        /// The tool's input (an edit's text, before and after: shown as
        /// what it would change).
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
    },
    /// It waits for the user otherwise (a question of its own).
    Waiting { message: String },
    /// Its session: `started`, `resumed`, `cleared`, `compacted`, `ended`.
    Session { event: String },
    /// A message to another agent, from this thread (by the user, from this
    /// agent's view, or by this agent itself): how it went, and its answer.
    Handoff {
        ask: u64,
        to: AskFrom,
        text: String,
        /// This agent sent it (and had the answer as it came).
        #[serde(default)]
        by_agent: bool,
        state: AskState,
        #[serde(default)]
        reply: Option<String>,
        #[serde(default)]
        error: Option<String>,
        /// The answer was brought back here: the ask that did.
        #[serde(default)]
        forwarded: Option<u64>,
        /// It was sent to be brought back once answered.
        #[serde(default)]
        back: bool,
    },
    /// Files it changed in a turn (its edit tools', as its hooks report).
    Edited { files: Vec<FileEdit> },
    /// Another agent changed a file this one did, a moment apart.
    Overlap { file: String, other: AskFrom },
    /// Its turn stopped short (a permission prompt turned down there).
    Interrupted { reason: String },
    /// A turn began with what the agent itself put in, not someone's
    /// message: a background task's notification (`<task-notification>`),
    /// a command's output. Its tag, what it says in short, and all of it.
    Notice { tag: String, summary: String, text: String },
}

/// Someone's answer to a permission request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub allow: bool,
    #[serde(default)]
    pub message: Option<String>,
    /// Permission updates to apply with an allow (one the agent suggested).
    #[serde(default)]
    pub permissions: Vec<serde_json::Value>,
}

/// A permission request, as `artty approvals` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: u64,
    pub pane: SessionId,
    #[serde(default)]
    pub agent: Option<String>,
    pub tool_name: String,
    /// What it would do, in a line: the command, the file, ...
    pub summary: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    pub state: ApprovalState,
    /// How long ago it was asked.
    pub age_secs: u64,
    /// Its "always" choices (`Decide`'s `always` picks one by index).
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
    /// Allowed at the agent's own prompt (the tool ran).
    AllowedThere,
    /// The agent's prompt was dismissed: denied there, or the turn stopped.
    Dismissed,
    /// The turn or session ended first.
    Ended,
    /// Nobody answered in artty for a while: left to the agent's prompt
    /// (or, an agent that can't show one, denied).
    Expired,
}

/// What an agent's hook said, beyond the state: the event's name and, for
/// the events that start and end a turn, the prompt and the reply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    /// `UserPromptSubmit`, `Stop`, `SessionStart`, ... (Gemini CLI's
    /// `BeforeAgent`, `AfterAgent`).
    pub event: String,
    #[serde(default)]
    pub prompt: Option<String>,
    /// The turn's id (Claude Code), the same in its prompt and its end.
    #[serde(default)]
    pub prompt_id: Option<String>,
    /// The agent's last message of the turn, when it ends.
    #[serde(default)]
    pub reply: Option<String>,
    /// Why a session started: `startup`, `resume`, `clear`, or `compact`
    /// (Claude Code compacting its context, which may be mid-turn).
    #[serde(default)]
    pub source: Option<String>,
    /// The tool used (`PostToolUse`), to tell which permission request it
    /// answers.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// What a notification is about (Claude Code): `permission_prompt`,
    /// `idle_prompt`, ...
    #[serde(default)]
    pub notification_type: Option<String>,
}

impl HookEvent {
    /// Whether this event starts a turn (`Some(true)`), ends one or the
    /// session (`Some(false)`), or neither.
    pub fn turn(&self) -> Option<bool> {
        match self.event.as_str() {
            "UserPromptSubmit" | "BeforeAgent" => Some(true),
            _ if self.compacting() => None,
            "Stop" | "AfterAgent" | "SessionStart" | "SessionEnd" => Some(false),
            _ => None,
        }
    }

    /// The agent's session began or ended: what was under way is gone.
    pub fn session_boundary(&self) -> bool {
        matches!(self.event.as_str(), "SessionStart" | "SessionEnd") && !self.compacting()
    }

    /// A session "starting" as the agent compacts its context: the same
    /// session going on, maybe mid-turn.
    fn compacting(&self) -> bool {
        self.event == "SessionStart" && self.source.as_deref() == Some("compact")
    }
}

/// An ask, as `artty ask` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskInfo {
    pub id: u64,
    pub pane: SessionId,
    pub state: AskState,
    /// Asked by another agent (from its pane), not by someone at artty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<AskFrom>,
    /// The agent's reply, once answered.
    #[serde(default)]
    pub reply: Option<String>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Something not quite right, though it went on.
    #[serde(default)]
    pub warning: Option<String>,
    /// The message's start, for listing.
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AskState {
    /// Waiting for the agent to be between turns.
    Queued,
    /// Typed in; waiting for the agent to take it.
    Delivering,
    /// The agent took it and is at work on it.
    Running,
    Answered,
    Failed,
    Cancelled,
}

impl AskState {
    pub fn finished(self) -> bool {
        matches!(self, AskState::Answered | AskState::Failed | AskState::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReadSource {
    /// The visible screen.
    Screen,
    /// The last `lines` lines, including history.
    Scrollback { lines: usize },
    /// Output of the last finished command (needs shell integration).
    LastCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentState {
    /// Thinking or using tools.
    Working,
    /// Needs the user: a question or a permission prompt.
    Waiting,
    /// Finished its turn.
    Done,
    /// Not running.
    Idle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error {
        message: String,
    },
    Tabs {
        tabs: Vec<TabInfo>,
    },
    Text {
        text: String,
    },
    Tab {
        id: SessionId,
    },
    Command {
        command: Option<String>,
        output: String,
        exit: Option<i32>,
        truncated: bool,
    },
    Server {
        info: ServerInfo,
    },
    Processes {
        panes: Vec<PaneProcesses>,
        /// The server's own use.
        #[serde(default)]
        server: Option<ServerUsage>,
    },
    Workspaces {
        workspaces: Vec<WorkspaceInfo>,
    },
    Workspace {
        id: u64,
    },
    Ask {
        ask: AskInfo,
    },
    /// A tab opened with its first message: the new pane, and the ask.
    Started {
        id: SessionId,
        ask: u64,
    },
    Asks {
        asks: Vec<AskInfo>,
    },
    /// The answer to a permission request: none means the agent's own
    /// prompt decides.
    Decision {
        decision: Option<Decision>,
    },
    Approvals {
        approvals: Vec<ApprovalInfo>,
    },
    Queue {
        items: Vec<crate::protocol::QueueItem>,
    },
    Thread {
        pane: SessionId,
        agent: Option<String>,
        entries: Vec<ThreadEntry>,
    },
    Agents {
        agents: Vec<AgentInfo>,
    },
    ThreadHits {
        hits: Vec<ThreadHit>,
    },
    Sessions {
        sessions: Vec<crate::protocol::SessionHit>,
    },
    Conversation {
        messages: Vec<Said>,
    },
    /// Thread entries, oldest first; `last`: where they're up to (the next
    /// `Events` goes on from there).
    Events {
        events: Vec<ThreadEvent>,
        last: u64,
    },
}

/// A workspace, for `artty workspace list` and the app's picker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub name: Option<String>,
    /// Its name, or else its git repository or directory.
    pub title: String,
    #[serde(default)]
    pub root: Option<PathBuf>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// The server's window holding it and that window's number, unless hidden.
    pub window: Option<u64>,
    #[serde(default)]
    pub window_number: usize,
    /// Its window is on screen in the app.
    pub attached: bool,
    pub tabs: usize,
    pub panes: usize,
    /// The most pressing pane status: `waiting`, `working`, `running`, ...
    pub status: String,
    pub needs_attention: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub windows: usize,
    /// Windows shown by the app; the rest are detached.
    pub attached_windows: usize,
    #[serde(default)]
    pub workspaces: usize,
    pub panes: usize,
    /// Connected apps.
    pub clients: usize,
}

/// What the server process itself uses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerUsage {
    pub pid: u32,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

/// A pane and the processes running in it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneProcesses {
    pub id: SessionId,
    pub window: usize,
    #[serde(default)]
    pub workspace: usize,
    // No `skip_serializing_if`: this also travels as postcard, which can't skip fields.
    #[serde(default)]
    pub workspace_name: Option<String>,
    pub tab: usize,
    pub title: String,
    pub status: String,
    pub attached: bool,
    pub cwd: Option<PathBuf>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub processes: Vec<ProcessRow>,
    /// CPU percent and memory once a second, oldest first, up to a minute.
    #[serde(default)]
    pub history: Vec<(f32, u64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessRow {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    /// In the pane's foreground process group (what the user is running).
    pub foreground: bool,
    pub elapsed_secs: u64,
    /// TCP ports it listens on (on Linux).
    #[serde(default)]
    pub ports: Vec<u16>,
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error { message: message.into() }
    }
}

/// A pane (`id`) and the tab it's in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: SessionId,
    /// 1-based window number.
    #[serde(default)]
    pub window: usize,
    /// 1-based workspace number (across windows) and the workspace's name.
    #[serde(default)]
    pub workspace: usize,
    // No `skip_serializing_if`: this also travels as postcard, which can't skip fields.
    #[serde(default)]
    pub workspace_name: Option<String>,
    /// 1-based tab number within the workspace.
    #[serde(default)]
    pub tab: usize,
    pub title: String,
    /// The focused pane of the active tab.
    pub active: bool,
    /// The focused pane of its tab.
    #[serde(default)]
    pub focused: bool,
    pub cwd: Option<PathBuf>,
    /// Foreground process, e.g. `fish` or `claude`.
    pub process: Option<String>,
    /// One of `idle`, `running`, `working`, `waiting`, `done`, `failed`.
    pub status: String,
    /// The command running, or the last agent message.
    pub detail: Option<String>,
    pub needs_attention: bool,
    pub shell_integration: bool,
    /// The program turned on the kitty keyboard protocol.
    #[serde(default)]
    pub kitty_keyboard: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_fit() {
        assert!(socket_path(u32::MAX).as_os_str().len() <= MAX_SOCKET_PATH);
    }

    #[test]
    fn wire_format() {
        let request: Request = serde_json::from_str(
            r#"{"cmd":"read","tab":3,"source":{"kind":"scrollback","lines":50}}"#,
        )
        .unwrap();
        assert!(matches!(
            request,
            Request::Read { tab: Some(3), source: ReadSource::Scrollback { lines: 50 } }
        ));
        let status: Request = serde_json::from_str(
            r#"{"cmd":"status","tab":null,"state":"waiting","message":"Allow Bash?"}"#,
        )
        .unwrap();
        assert!(matches!(status, Request::Status { state: AgentState::Waiting, .. }));
        let response = serde_json::to_string(&Response::Tab { id: 7 }).unwrap();
        assert_eq!(response, r#"{"result":"tab","id":7}"#);
    }

    #[test]
    fn private_dirs_are_made_private_and_links_refused() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: getuid can't fail.
        let uid = unsafe { libc::getuid() };
        let base = std::env::temp_dir().join(format!("artty-private-{}", std::process::id()));
        let dir = base.join("a");
        make_private(&dir, uid).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        // Made open by someone (or an old version): closed again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        make_private(&dir, uid).unwrap();
        assert_eq!(mode(&dir), 0o700);
        // A link put in its place, or someone else's: refused.
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(make_private(&link, uid).is_err());
        assert!(make_private(&dir, uid + 1).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
