//! The Android app's Rust side: [`artty_client`] for Kotlin, through
//! UniFFI. A [`Connection`] to a server (ssh built in), what happens there
//! told to the app's [`Events`], what logging in needs asked of its
//! [`LoginAsker`]; screens as plain records the app draws.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use artty_client::ssh::{self, Credentials, HostKey, KnownHosts, Login, Target};
use artty_client::{Client, Event, Key};
use artty_core::TerminalSize;
use artty_core::protocol::WindowLayout;

uniffi::setup_scaffolding!();

/// Where to connect, as the app keeps it.
#[derive(uniffi::Record, Clone)]
pub struct ServerAddress {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// `artty` there, if not on its PATH.
    pub artty: Option<String>,
}

/// A key made on the phone: the private half (OpenSSH format, kept by the
/// app) and the public one (for the server's `authorized_keys`).
#[derive(uniffi::Record)]
pub struct KeyPair {
    pub private_openssh: String,
    pub public_openssh: String,
}

/// What went wrong.
#[derive(Debug, uniffi::Error)]
pub enum ConnectError {
    Failed { reason: String },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Failed { reason } => f.write_str(reason),
        }
    }
}

impl From<String> for ConnectError {
    fn from(reason: String) -> Self {
        ConnectError::Failed { reason }
    }
}

/// A question logging in asks.
#[derive(uniffi::Record)]
pub struct Prompt {
    pub text: String,
    /// What's typed may show (else: a password, a code).
    pub echo: bool,
}

/// What logging in asks of the user; the app answers (it may wait for them:
/// it's asked off the main thread).
#[uniffi::export(foreign)]
pub trait LoginAsker: Send + Sync {
    /// The machine's key: log in? `changed`: not the one recorded.
    fn host_key(&self, host: String, fingerprint: String, changed: bool) -> bool;
    /// What the server says while logging in (Tailscale's sign-in address).
    fn said(&self, text: String);
    /// Answers to its questions, or none to give up.
    fn answer(&self, instructions: String, prompts: Vec<Prompt>) -> Option<Vec<String>>;
}

/// A tab, as the app lists them.
#[derive(uniffi::Record, Clone)]
pub struct TabItem {
    pub workspace: u64,
    pub workspace_name: String,
    /// Its place in its workspace.
    pub index: u32,
    pub title: String,
    /// Its panes, and the one focused.
    pub panes: Vec<u64>,
    pub focused: u64,
    /// The one the window shows.
    pub active: bool,
}

/// A window of the server's, as listed to choose from.
#[derive(uniffi::Record)]
pub struct WindowItem {
    pub window: u64,
    pub tabs: u32,
    /// How many apps show it now.
    pub apps: u32,
    pub titles: Vec<String>,
}

/// What happened.
#[derive(uniffi::Enum)]
pub enum ClientEvent {
    Attached {
        window: u64,
    },
    /// A window's tabs (in sidebar order: workspace by workspace).
    Tabs {
        window: u64,
        tabs: Vec<TabItem>,
    },
    WindowClosed {
        window: u64,
    },
    /// A pane's screen changed: `Connection::screen` has it.
    Screen {
        pane: u64,
    },
    Windows {
        windows: Vec<WindowItem>,
    },
    Bell {
        pane: u64,
    },
    Clipboard {
        text: String,
    },
    Notify {
        title: String,
        body: String,
    },
    Closed {
        reason: String,
    },
}

/// Where what happens goes (called off the main thread).
#[uniffi::export(foreign)]
pub trait Events: Send + Sync {
    fn event(&self, event: ClientEvent);
}

/// A run of text in one style, from `column`.
#[derive(uniffi::Record)]
pub struct Run {
    pub column: u32,
    pub cells: u32,
    pub text: String,
    /// 0xRRGGBB.
    pub fg: u32,
    pub bold: bool,
    pub italic: bool,
    pub strikeout: bool,
}

/// Cells from `column`, `width` wide, in a color.
#[derive(uniffi::Record)]
pub struct Rect {
    pub column: u32,
    pub width: u32,
    pub color: u32,
}

#[derive(uniffi::Record)]
pub struct Line {
    pub runs: Vec<Run>,
    pub backgrounds: Vec<Rect>,
    /// Selected cells, drawn over the backgrounds.
    pub selection: Vec<Rect>,
    /// Underlined cells.
    pub underlines: Vec<Rect>,
}

#[derive(uniffi::Record)]
pub struct Cursor {
    pub line: u32,
    pub column: u32,
    pub wide: bool,
    pub color: u32,
    /// A block (else a bar or underline: drawn thin).
    pub block: bool,
}

/// A pane's screen.
#[derive(uniffi::Record)]
pub struct Screen {
    pub columns: u32,
    pub lines: Vec<Line>,
    pub cursor: Option<Cursor>,
    pub background: u32,
    pub foreground: u32,
    /// Scrolled up into history this many lines.
    pub scrolled: u32,
}

fn rgb(c: artty_core::vt::ansi::Rgb) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

fn rects(rects: &[artty_core::FrameRect]) -> Vec<Rect> {
    rects
        .iter()
        .map(|r| Rect { column: r.column as u32, width: r.width as u32, color: rgb(r.color) })
        .collect()
}

impl From<artty_client::Screen> for Screen {
    fn from(screen: artty_client::Screen) -> Self {
        let frame = screen.frame;
        Screen {
            columns: frame.columns as u32,
            lines: frame
                .lines
                .iter()
                .map(|line| Line {
                    runs: line
                        .runs
                        .iter()
                        .map(|r| Run {
                            column: r.column as u32,
                            cells: r.cells as u32,
                            text: r.text.clone(),
                            fg: rgb(r.fg),
                            bold: r.bold,
                            italic: r.italic,
                            strikeout: r.strikeout,
                        })
                        .collect(),
                    backgrounds: rects(&line.backgrounds),
                    selection: rects(&line.selection),
                    underlines: line
                        .underlines
                        .iter()
                        .map(|u| Rect {
                            column: u.column as u32,
                            width: u.width as u32,
                            color: rgb(u.color),
                        })
                        .collect(),
                })
                .collect(),
            cursor: frame.cursor.map(|c| Cursor {
                line: c.line as u32,
                column: c.column as u32,
                wide: c.wide,
                color: rgb(c.color),
                block: matches!(c.shape, artty_core::vt::ansi::CursorShape::Block),
            }),
            background: rgb(frame.background),
            foreground: rgb(frame.foreground),
            scrolled: frame.display_offset as u32,
        }
    }
}

/// The window's tabs, workspace by workspace.
fn tabs(layout: &WindowLayout) -> Vec<TabItem> {
    let mut tabs = Vec::new();
    for (w, workspace) in layout.workspaces.iter().enumerate() {
        let name = workspace.name.clone().unwrap_or_else(|| workspace.title.clone());
        for (index, tab) in workspace.tabs.iter().enumerate() {
            tabs.push(TabItem {
                workspace: workspace.id,
                workspace_name: name.clone(),
                index: index as u32,
                title: tab.title.clone().unwrap_or_default(),
                panes: tab.root.panes(),
                focused: tab.focused,
                active: w == layout.active && index == workspace.active,
            });
        }
    }
    tabs
}

/// Logging in, asked of the app.
struct Asking(Arc<dyn LoginAsker>);

impl Login for Asking {
    fn host_key(&self, target: &Target, fingerprint: &str, known: HostKey) -> bool {
        self.0.host_key(target.host.clone(), fingerprint.to_owned(), known == HostKey::Changed)
    }

    fn said(&self, text: &str) {
        self.0.said(text.to_owned());
    }

    fn answer(&self, instructions: &str, prompts: &[(String, bool)]) -> Option<Vec<String>> {
        let prompts =
            prompts.iter().map(|(text, echo)| Prompt { text: text.clone(), echo: *echo }).collect();
        self.0.answer(instructions.to_owned(), prompts)
    }
}

/// A connection to a server.
#[derive(uniffi::Object)]
pub struct Connection {
    client: Client,
}

/// Log in to `server` with `private_key` (OpenSSH format; the server's own
/// questions are asked too), and greet the artty server there. Machines'
/// keys are recorded in `known_hosts` (a file of the app's). Blocks: call
/// it off the main thread.
#[uniffi::export]
pub fn connect(
    server: ServerAddress,
    private_key: Option<String>,
    known_hosts: String,
    login: Arc<dyn LoginAsker>,
    events: Arc<dyn Events>,
) -> Result<Arc<Connection>, ConnectError> {
    let target = Target { user: server.user, host: server.host, port: server.port };
    let credentials =
        Credentials { agent: false, keys: private_key.into_iter().map(|k| (k, None)).collect() };
    let link = ssh::connect(
        &target,
        &ssh::server_command(server.artty.as_deref()),
        &credentials,
        KnownHosts(PathBuf::from(known_hosts)),
        Arc::new(Asking(login)),
    )?;
    let (client, received) = Client::start(link, env!("CARGO_PKG_VERSION"))?;
    std::thread::Builder::new()
        .name("artty-events".into())
        .spawn(move || {
            for event in received {
                let event = match event {
                    Event::Attached { window } => ClientEvent::Attached { window },
                    Event::Layout { window, layout } => {
                        ClientEvent::Tabs { window, tabs: tabs(&layout) }
                    },
                    Event::WindowClosed { window } => ClientEvent::WindowClosed { window },
                    Event::Screen { pane } => ClientEvent::Screen { pane },
                    Event::Windows(windows) => ClientEvent::Windows {
                        windows: windows
                            .into_iter()
                            .map(|w| WindowItem {
                                window: w.window,
                                tabs: w.tabs as u32,
                                apps: w.apps as u32,
                                titles: w.titles,
                            })
                            .collect(),
                    },
                    Event::Bell { pane } => ClientEvent::Bell { pane },
                    Event::Clipboard { text } => ClientEvent::Clipboard { text },
                    Event::Notify { title, body } => ClientEvent::Notify { title, body },
                    Event::Closed(reason) => ClientEvent::Closed { reason },
                    Event::Panes(_) | Event::Stopping { .. } => continue,
                };
                events.event(event);
            }
        })
        .map_err(|err| err.to_string())?;
    Ok(Arc::new(Connection { client }))
}

#[uniffi::export]
impl Connection {
    /// The server's build.
    pub fn server_version(&self) -> String {
        self.client.server_version.clone()
    }

    /// Seconds since the server last sent anything.
    pub fn silent_seconds(&self) -> f64 {
        self.client.silent_for().as_secs_f64()
    }

    /// Ask for the server's windows: `ClientEvent::Windows`.
    pub fn list_windows(&self) -> Result<(), ConnectError> {
        Ok(self.client.list_windows()?)
    }

    /// Show a window here (`None`: a new one), shared with the apps that
    /// show it.
    pub fn attach(&self, window: Option<u64>) -> Result<(), ConnectError> {
        Ok(self.client.attach(window, true)?)
    }

    pub fn detach(&self, window: u64) -> Result<(), ConnectError> {
        Ok(self.client.detach(window)?)
    }

    /// In use here (or not): its panes take this phone's size.
    pub fn focus(&self, window: u64, focused: bool) -> Result<(), ConnectError> {
        Ok(self.client.focus(window, focused)?)
    }

    pub fn select_tab(&self, workspace: u64, index: u32) -> Result<(), ConnectError> {
        Ok(self.client.select_tab(workspace, index as usize)?)
    }

    pub fn screen(&self, pane: u64) -> Option<Screen> {
        self.client.screen(pane).map(Screen::from)
    }

    /// Text typed or pasted.
    pub fn text(&self, pane: u64, text: String) -> Result<(), ConnectError> {
        Ok(self.client.input(pane, text.into_bytes())?)
    }

    /// Bytes as they are (a control character).
    pub fn bytes(&self, pane: u64, bytes: Vec<u8>) -> Result<(), ConnectError> {
        Ok(self.client.input(pane, bytes)?)
    }

    /// A key by name (`enter`, `up`, `escape`, `tab`, `a`…) with modifiers,
    /// in the pane's mode. Returns whether a terminal takes it.
    pub fn key(
        &self,
        pane: u64,
        name: String,
        shift: bool,
        alt: bool,
        control: bool,
    ) -> Result<bool, ConnectError> {
        let key = Key { key: &name, shift, alt, control, platform: false };
        Ok(self.client.key(pane, &key)?)
    }

    /// The pane's size here: cells, and a cell's size in pixels.
    pub fn resize(
        &self,
        pane: u64,
        columns: u16,
        lines: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> Result<(), ConnectError> {
        let size = TerminalSize { columns, lines, cell_width, cell_height };
        Ok(self.client.resize(pane, size)?)
    }

    /// Scroll its history (`lines` up, if positive).
    pub fn scroll(&self, pane: u64, lines: i32) -> Result<(), ConnectError> {
        Ok(self.client.scroll(pane, lines)?)
    }
}

/// A new ed25519 key, for this phone to log in with.
#[uniffi::export]
pub fn generate_key(comment: String) -> Result<KeyPair, ConnectError> {
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut seed))
        .map_err(|err| format!("no randomness: {err}"))?;
    let (private_openssh, public_openssh) = ssh::new_ed25519(seed, &comment)?;
    Ok(KeyPair { private_openssh, public_openssh })
}
