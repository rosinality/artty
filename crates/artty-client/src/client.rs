//! Talking to a server over a [`Link`]: the greeting, then windows to show,
//! their layouts and screens kept up to date (each pane's [`Frame`], with
//! the terminal's mode for keys), and input, keys and sizes sent back.
//! What happens arrives as [`Event`]s; a pane's screen is read with
//! [`Client::screen`].

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use artty_core::Frame;
use artty_core::TerminalSize;
use artty_core::keys::Key;
use artty_core::layout::PaneId;
use artty_core::mouse::{self, Action, Button, Wheel};
use artty_core::protocol::{
    ClientMessage, Hello, PaneState, Query, Reply, ServerMessage, WindowId, WindowLayout,
    WindowSummary, WorkspaceId, protocol_version, read_message, write_message,
};
use artty_core::vt::term::TermMode;

use crate::link::Link;

/// How often the server is asked whether it's there.
const PING_EVERY: Duration = Duration::from_secs(5);

/// What happened.
#[derive(Clone, Debug)]
pub enum Event {
    /// A window is shown here (the answer to [`Client::attach`]).
    Attached {
        window: WindowId,
    },
    Layout {
        window: WindowId,
        layout: WindowLayout,
    },
    WindowClosed {
        window: WindowId,
    },
    /// A pane's screen changed: [`Client::screen`] has it.
    Screen {
        pane: PaneId,
    },
    /// What every pane shown is doing (titles, agents, progress).
    Panes(Vec<PaneState>),
    Bell {
        pane: PaneId,
    },
    Notify {
        title: String,
        body: String,
    },
    /// A program copied this.
    Clipboard {
        text: String,
    },
    /// The server's windows (the answer to [`Client::list_windows`]).
    Windows(Vec<WindowSummary>),
    /// The server is going (restarting, if `restart`: it comes back).
    Stopping {
        restart: bool,
    },
    /// The connection ended.
    Closed(String),
}

/// A pane's screen, and the terminal's mode (for keys and the mouse).
#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub frame: Frame,
    pub mode: TermMode,
}

/// A connection to a server.
pub struct Client {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screens: Arc<Mutex<HashMap<PaneId, Screen>>>,
    /// The server's build, from its greeting.
    pub server_version: String,
    /// When it last sent anything.
    heard: Arc<Mutex<Instant>>,
    next_id: AtomicU64,
    _keep: Mutex<Box<dyn Send>>,
}

impl Client {
    /// Greet the server over `link`; then what happens arrives on the
    /// returned channel. Fails if it isn't an artty server, or runs
    /// another build (`app_version`, this app's, is told it).
    pub fn start(link: Link, app_version: &str) -> Result<(Client, mpsc::Receiver<Event>), String> {
        let Link { mut reader, mut writer, keep } = link;
        let hello =
            Hello::Attach { version: protocol_version(), app_version: app_version.to_owned() };
        let hello = serde_json::to_string(&hello).map_err(|err| err.to_string())?;
        writer
            .write_all(format!("{hello}\n").as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|err| format!("couldn't greet it: {err}"))?;
        let server_version = match read_message::<ServerMessage>(&mut reader) {
            Ok(ServerMessage::Welcome { version, server_version }) => {
                if version != protocol_version() {
                    return Err(format!("it runs another artty build ({server_version})"));
                }
                server_version
            },
            Ok(_) => return Err("it answered, but not as an artty server".to_owned()),
            Err(err) => return Err(format!("no answer: {err}")),
        };

        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
        let screens = Arc::new(Mutex::new(HashMap::<PaneId, Screen>::new()));
        let heard = Arc::new(Mutex::new(Instant::now()));
        let (events, received) = mpsc::channel();
        {
            let (writer, screens, heard) = (writer.clone(), screens.clone(), heard.clone());
            std::thread::Builder::new()
                .name("artty-client-reader".into())
                .spawn(move || {
                    let why = loop {
                        let message = match read_message::<ServerMessage>(&mut reader) {
                            Ok(message) => message,
                            Err(err) => break err.to_string(),
                        };
                        *heard.lock().unwrap() = Instant::now();
                        let event = match message {
                            ServerMessage::Attached { window, .. } => Event::Attached { window },
                            ServerMessage::Layout { window, layout } => {
                                Event::Layout { window, layout }
                            },
                            ServerMessage::WindowClosed { window } => {
                                Event::WindowClosed { window }
                            },
                            ServerMessage::Frame { pane, update } => {
                                let mut screens = screens.lock().unwrap();
                                let screen = screens.entry(pane).or_default();
                                screen.mode = TermMode::from_bits_truncate(update.mode);
                                update.apply(&mut screen.frame);
                                Event::Screen { pane }
                            },
                            ServerMessage::Panes { panes } => Event::Panes(panes),
                            ServerMessage::Bell { pane } => Event::Bell { pane },
                            ServerMessage::Notify { title, body } => Event::Notify { title, body },
                            ServerMessage::Clipboard { text } => Event::Clipboard { text },
                            ServerMessage::Reply { reply: Reply::Windows(windows), .. } => {
                                Event::Windows(windows)
                            },
                            ServerMessage::Stopping { restart } => Event::Stopping { restart },
                            // Screens are in: more may come.
                            ServerMessage::Sync { seq } => {
                                let mut writer = writer.lock().unwrap();
                                if write_message(&mut *writer, &ClientMessage::Ack { seq }).is_err()
                                {
                                    break "couldn't answer it".to_owned();
                                }
                                continue;
                            },
                            _ => continue,
                        };
                        if events.send(event).is_err() {
                            return;
                        }
                    };
                    let _ = events.send(Event::Closed(why));
                })
                .map_err(|err| err.to_string())?;
        }
        // Asked now and then whether it's there (and so it knows this app is).
        {
            let writer = Arc::downgrade(&writer);
            std::thread::Builder::new()
                .name("artty-client-ping".into())
                .spawn(move || {
                    let mut id = 1u64 << 63;
                    loop {
                        std::thread::sleep(PING_EVERY);
                        let Some(writer) = writer.upgrade() else { return };
                        id += 1;
                        let mut writer = writer.lock().unwrap();
                        if write_message(&mut *writer, &ClientMessage::Ping { id }).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|err| err.to_string())?;
        }
        let client = Client {
            writer,
            screens,
            server_version,
            heard,
            next_id: AtomicU64::new(1),
            _keep: Mutex::new(keep),
        };
        Ok((client, received))
    }

    fn send(&self, message: &ClientMessage) -> Result<(), String> {
        let mut writer = self.writer.lock().unwrap();
        write_message(&mut *writer, message).map_err(|err| err.to_string())
    }

    /// How long since the server last sent anything (it answers pings every
    /// few seconds: much longer means the connection is gone).
    pub fn silent_for(&self) -> Duration {
        self.heard.lock().unwrap().elapsed()
    }

    /// Ask for the server's windows: [`Event::Windows`].
    pub fn list_windows(&self) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(&ClientMessage::Query { id, query: Query::AllWindows })
    }

    /// Show a window here: `None` opens a new one. With `share`, one another
    /// app shows is shown here too (its panes in the size of the app used
    /// last); else another is opened.
    pub fn attach(&self, window: Option<WindowId>, share: bool) -> Result<(), String> {
        self.send(&ClientMessage::AttachWindow {
            window,
            spec: Default::default(),
            bounds: None,
            share,
        })
    }

    /// Stop showing a window here (it keeps running).
    pub fn detach(&self, window: WindowId) -> Result<(), String> {
        self.send(&ClientMessage::DetachWindow { window })
    }

    /// The window is in front here (or not): in use here, its panes take
    /// this app's sizes.
    pub fn focus(&self, window: WindowId, focused: bool) -> Result<(), String> {
        self.send(&ClientMessage::WindowFocused { window, focused })
    }

    /// Show a workspace's tab.
    pub fn select_tab(&self, workspace: WorkspaceId, tab: usize) -> Result<(), String> {
        self.send(&ClientMessage::SelectTab { workspace, tab })
    }

    /// A pane's screen as last sent.
    pub fn screen(&self, pane: PaneId) -> Option<Screen> {
        self.screens.lock().unwrap().get(&pane).cloned()
    }

    /// Text typed (or pasted) into a pane.
    pub fn input(&self, pane: PaneId, bytes: Vec<u8>) -> Result<(), String> {
        self.send(&ClientMessage::Input { pane, bytes })
    }

    /// A key that isn't text (Enter, an arrow, Ctrl+C), in the pane's mode.
    /// Returns whether it means anything to a terminal.
    pub fn key(&self, pane: PaneId, key: &Key) -> Result<bool, String> {
        let mode = self.mode(pane);
        match artty_core::keys::to_esc(key, mode, true) {
            Some(bytes) => self.input(pane, bytes).map(|()| true),
            None => Ok(false),
        }
    }

    fn mode(&self, pane: PaneId) -> TermMode {
        self.screens.lock().unwrap().get(&pane).map(|s| s.mode).unwrap_or_default()
    }

    /// Whether the pane's program takes the mouse (a click there is its).
    pub fn wants_mouse(&self, pane: PaneId) -> bool {
        self.mode(pane).intersects(TermMode::MOUSE_MODE)
    }

    /// A mouse event at viewport cell (`line`, `column`), reported to the
    /// program if it asked for such events. Returns whether it was.
    pub fn mouse(
        &self,
        pane: PaneId,
        button: Button,
        action: Action,
        line: usize,
        column: usize,
    ) -> Result<bool, String> {
        let report =
            mouse::report(button, action, line, column, &Default::default(), self.mode(pane));
        match report {
            Some(bytes) => self.input(pane, bytes).map(|()| true),
            None => Ok(false),
        }
    }

    /// The wheel turned `lines` (up, if positive) over cell (`line`,
    /// `column`): to the program, or the history, as the terminal would.
    pub fn wheel(
        &self,
        pane: PaneId,
        lines: i32,
        line: usize,
        column: usize,
    ) -> Result<(), String> {
        match mouse::wheel(lines, line, column, &Default::default(), self.mode(pane)) {
            Wheel::Send(bytes) => self.input(pane, bytes),
            Wheel::History(lines) => self.scroll(pane, lines),
        }
    }

    /// The pane's size here, in cells (and a cell's size in pixels).
    pub fn resize(&self, pane: PaneId, size: TerminalSize) -> Result<(), String> {
        self.send(&ClientMessage::Resize { pane, size })
    }

    /// Scroll the pane's history (`lines` up, if positive).
    pub fn scroll(&self, pane: PaneId, lines: i32) -> Result<(), String> {
        self.send(&ClientMessage::Scroll { pane, lines })
    }
}
