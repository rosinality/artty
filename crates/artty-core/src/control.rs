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

/// Socket path for the app process `pid`.
pub fn socket_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("artty-{pid}.sock"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// All tabs.
    List,
    /// Text from a tab.
    Read { tab: Option<SessionId>, source: ReadSource },
    /// Type text into a tab.
    Send { tab: Option<SessionId>, text: String, enter: bool },
    /// Open a tab running the shell, or `command`.
    New { cwd: Option<PathBuf>, command: Vec<String>, title: Option<String>, focus: bool },
    Focus { tab: SessionId },
    Close { tab: SessionId },
    /// Run a command in a tab's shell and wait for it to finish. Needs shell
    /// integration in that shell.
    Run { tab: Option<SessionId>, command: String, timeout_secs: Option<u64> },
    /// Show a notification for a tab.
    Notify { tab: Option<SessionId>, title: Option<String>, body: String },
    /// Report what an agent in a tab is doing (used by agent hooks).
    Status { tab: Option<SessionId>, state: AgentState, message: Option<String> },
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
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error { message: message.into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: SessionId,
    pub title: String,
    pub active: bool,
    pub cwd: Option<PathBuf>,
    /// Foreground process, e.g. `fish` or `claude`.
    pub process: Option<String>,
    /// One of `idle`, `running`, `working`, `waiting`, `done`, `failed`.
    pub status: String,
    /// The command running, or the last agent message.
    pub detail: Option<String>,
    pub needs_attention: bool,
    pub shell_integration: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

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
