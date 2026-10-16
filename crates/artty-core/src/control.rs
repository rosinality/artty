//! The control protocol between the artty app and the `artty` command line
//! tool: one JSON object per line over a Unix socket.
//!
//! Programs inside artty find the socket in `$ARTTY_SOCKET` and their own tab
//! in `$ARTTY_SESSION_ID`.

use std::path::PathBuf;

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

/// The artty server's socket. Everything goes through it: the app's windows,
/// the `artty` command, and agent hooks.
pub fn server_socket_path() -> PathBuf {
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let path = std::env::temp_dir().join("artty-server.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH { path } else { PathBuf::from(format!("/tmp/artty-server-{uid}.sock")) }
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
    Processes { tab: Option<SessionId> },
    /// Terminate a pane's foreground program (`signal`, default TERM), or the
    /// whole pane with `pane: true`.
    Kill { tab: SessionId, signal: Option<i32>, pane: bool },
    /// Send a signal (default SIGTERM) to one process; only processes running
    /// in artty's panes.
    Signal { pid: i32, signal: Option<i32> },
    /// Every workspace, shown or not.
    Workspaces,
    /// A workspace with one tab, in the frontmost window.
    NewWorkspace { name: Option<String>, cwd: Option<PathBuf>, command: Vec<String> },
    /// Workspaces are named by id or by name. An empty name goes back to
    /// automatic.
    RenameWorkspace { workspace: String, name: String },
    /// End every tab in it.
    CloseWorkspace { workspace: String },
    /// Show it: in the frontmost window, or bring forward the window
    /// already showing it.
    OpenWorkspace { workspace: String },
    /// Take it out of its window; it keeps running, hidden.
    HideWorkspace { workspace: String },
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
    Read { tab: Option<SessionId>, source: ReadSource },
    /// Type text into a tab.
    Send { tab: Option<SessionId>, text: String, enter: bool },
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
    },
    Focus { tab: SessionId },
    /// Name the tab holding pane `tab`; an empty title goes back to automatic.
    Rename { tab: Option<SessionId>, title: String },
    Close { tab: SessionId },
    /// Run a command in a tab's shell and wait for it to finish. Needs shell
    /// integration in that shell.
    Run { tab: Option<SessionId>, command: String, timeout_secs: Option<u64> },
    /// Show a notification for a tab.
    Notify { tab: Option<SessionId>, title: Option<String>, body: String },
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
    },
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
    Error { message: String },
    Tabs { tabs: Vec<TabInfo> },
    Text { text: String },
    Tab { id: SessionId },
    Command { command: Option<String>, output: String, exit: Option<i32>, truncated: bool },
    Server { info: ServerInfo },
    Processes {
        panes: Vec<PaneProcesses>,
        /// The server's own use.
        #[serde(default)]
        server: Option<ServerUsage>,
    },
    Workspaces { workspaces: Vec<WorkspaceInfo> },
    Workspace { id: u64 },
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
        let request: Request =
            serde_json::from_str(r#"{"cmd":"read","tab":3,"source":{"kind":"scrollback","lines":50}}"#).unwrap();
        assert!(matches!(request, Request::Read { tab: Some(3), source: ReadSource::Scrollback { lines: 50 } }));
        let status: Request =
            serde_json::from_str(r#"{"cmd":"status","tab":null,"state":"waiting","message":"Allow Bash?"}"#).unwrap();
        assert!(matches!(status, Request::Status { state: AgentState::Waiting, .. }));
        let response = serde_json::to_string(&Response::Tab { id: 7 }).unwrap();
        assert_eq!(response, r#"{"result":"tab","id":7}"#);
    }
}
