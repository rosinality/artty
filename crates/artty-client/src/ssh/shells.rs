//! Shells over one ssh connection, for [`crate::plain`]: each its own
//! session channel, with a terminal (unless `RequestTTY no`), the config's
//! `SetEnv`, and its `RemoteCommand` in place of the login shell.

use std::time::Duration;

use artty_core::TerminalSize;
use artty_core::vt::tty::RemoteEnds;
use russh::ChannelMsg;

use super::login::Connection;
use crate::ssh_config::{RequestTty, Resolved};

pub(super) struct Shells {
    connection: Connection,
    target: Resolved,
}

impl Shells {
    pub(super) fn new(connection: Connection, target: Resolved) -> Shells {
        Shells { connection, target }
    }
}

impl crate::plain::Shells for Shells {
    fn open(
        &self,
        size: TerminalSize,
        ends: RemoteEnds,
        mut sizes: tokio::sync::mpsc::UnboundedReceiver<TerminalSize>,
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
                match &target.remote_command {
                    Some(command) => channel.exec(true, command.as_str()).await?,
                    None => channel.request_shell(true).await?,
                }
                Ok::<_, russh::Error>(channel)
            };
            let mut channel = match opened.await {
                Ok(channel) => channel,
                Err(err) => return crate::plain::refuse(ends, &format!("No shell there: {err}")),
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
}
