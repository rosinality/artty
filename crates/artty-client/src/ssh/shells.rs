//! Shells over one ssh connection, for [`crate::plain`]: each its own
//! session channel, with a terminal (unless `RequestTTY no`), the config's
//! `SetEnv`, and its `RemoteCommand` in place of the login shell. Kept (if
//! asked), each runs in a tmux session there, which outlives the connection
//! and is taken up again by the next.

use std::sync::Arc;
use std::time::Duration;

use artty_core::TerminalSize;
use artty_core::vt::tty::RemoteEnds;
use russh::ChannelMsg;

use super::login::Connection;
use crate::ssh_config::{RequestTty, Resolved};

pub(super) struct Shells {
    connection: Connection,
    files: Arc<super::files::Sftp>,
    target: Resolved,
    /// Each shell in a tmux session there.
    keep: bool,
}

impl Shells {
    pub(super) fn new(connection: Connection, target: Resolved, keep: bool) -> Shells {
        // A RemoteCommand is what runs, kept or not.
        let keep = keep && target.remote_command.is_none();
        let files = Arc::new(super::files::Sftp::new(
            connection.handle.clone(),
            connection.runtime_handle(),
        ));
        Shells { connection, files, target, keep }
    }
}

/// What runs for a kept shell: tmux's session `name` (made, or taken up),
/// its mouse on (swipes scroll its history); without tmux there, the login
/// shell, saying so. In sh, whatever the login shell is (fish, say), and
/// as a login one (for a PATH that finds tmux).
fn in_tmux(name: &str) -> String {
    format!(
        "sh -lc 'if command -v tmux >/dev/null 2>&1; then exec tmux new-session -A -s {name} \\; set-option mouse on; \
         else echo \"(no tmux there: this shell ends with the connection)\"; exec \"$SHELL\" -l; fi'"
    )
}

impl crate::plain::Shells for Shells {
    fn open(
        &self,
        size: TerminalSize,
        ends: RemoteEnds,
        mut sizes: tokio::sync::mpsc::UnboundedReceiver<TerminalSize>,
        session: Option<String>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let handle = self.connection.handle.clone();
        let target = self.target.clone();
        self.connection.spawn(async move {
            let pixels = |s: TerminalSize| {
                let width = u32::from(s.columns) * u32::from(s.cell_width);
                (width, u32::from(s.lines) * u32::from(s.cell_height))
            };
            let opened = async {
                let channel = handle.channel_open_session().await?;
                if target.request_tty != RequestTty::No {
                    let (width, height) = pixels(size);
                    let (columns, lines) = (u32::from(size.columns), u32::from(size.lines));
                    channel
                        .request_pty(true, "xterm-256color", columns, lines, width, height, &[])
                        .await?;
                }
                if target.forward_agent {
                    channel.agent_forward(false).await?;
                }
                // Most servers take only LANG and LC_*: asked, not insisted on.
                channel.set_env(false, "COLORTERM", "truecolor").await?;
                for (name, value) in &target.set_env {
                    channel.set_env(false, name.as_str(), value.as_str()).await?;
                }
                match (&target.remote_command, &session) {
                    (Some(command), _) => channel.exec(true, command.as_str()).await?,
                    (None, Some(name)) => {
                        log::info!("a shell in tmux's session {name}");
                        channel.exec(true, in_tmux(name)).await?
                    },
                    (None, None) => channel.request_shell(true).await?,
                }
                Ok::<_, russh::Error>(channel)
            };
            let mut channel = match opened.await {
                Ok(channel) => channel,
                Err(err) => {
                    log::warn!("a shell couldn't start: {err}");
                    return crate::plain::refuse(ends, &format!("No shell there: {err}"));
                },
            };
            let RemoteEnds { stream, mut ended } = ends;
            let stream = stream
                .set_nonblocking(true)
                .and_then(|()| tokio::net::UnixStream::from_std(stream));
            let Ok(stream) = stream else { return };
            let (mut from_terminal, mut to_terminal) = stream.into_split();
            let mut typed = vec![0; 16 * 1024];
            let mut exited = false;
            loop {
                tokio::select! {
                    message = channel.wait() => match message {
                        Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                            if to_terminal.write_all(&data).await.is_err() {
                                break;
                            }
                        },
                        Some(ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. }) => {
                            exited = true
                        },
                        Some(ChannelMsg::Close) | None => break,
                        Some(_) => {},
                    },
                    read = from_terminal.read(&mut typed) => match read {
                        // The terminal let go (its tab closed): so does the shell.
                        Ok(0) | Err(_) => {
                            let _ = channel.close().await;
                            return;
                        },
                        Ok(n) => {
                            if channel.data(&typed[..n]).await.is_err() {
                                break;
                            }
                        },
                    },
                    Some(size) = sizes.recv() => {
                        let (width, height) = pixels(size);
                        let (columns, lines) = (u32::from(size.columns), u32::from(size.lines));
                        let _ = channel.window_change(columns, lines, width, height).await;
                    },
                }
            }
            // Ended without exiting: the connection is probably going, which
            // ends the window (to be reached again), not this tab.
            if !exited {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if handle.is_closed() {
                    return;
                }
            }
            use std::io::Write;
            let _ = ended.write_all(&[1]);
        });
    }

    fn ping(&self, answered: Box<dyn FnOnce() + Send>) {
        let handle = self.connection.handle.clone();
        self.connection.spawn(async move {
            if handle.send_ping().await.is_ok() && !handle.is_closed() {
                answered();
            }
        });
    }

    fn closed(&self) -> bool {
        self.connection.closed()
    }

    fn files(&self) -> Option<Arc<dyn crate::plain::Files>> {
        Some(self.files.clone())
    }

    fn keeps(&self) -> bool {
        self.keep
    }

    fn sessions(&self) -> Vec<String> {
        let listed = self
            .connection
            .run("sh -lc 'tmux list-sessions -F \"#{session_name}\" 2>/dev/null; true'", None);
        let mut names: Vec<String> = listed
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|name| {
                name.strip_prefix(crate::plain::SESSION_PREFIX)
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .map(str::to_owned)
            .collect();
        // In the order they were made.
        names.sort_by_key(|name| {
            name[crate::plain::SESSION_PREFIX.len()..].parse::<u32>().unwrap_or(0)
        });
        if !names.is_empty() {
            log::info!("taking up tmux's sessions {}", names.join(", "));
        }
        names
    }

    fn end(&self, session: &str) {
        let handle = self.connection.handle.clone();
        let command = format!("sh -lc 'tmux kill-session -t {session} 2>/dev/null; true'");
        self.connection.spawn(async move {
            if let Ok(channel) = handle.channel_open_session().await {
                let _ = channel.exec(true, command).await;
            }
        });
    }
}
