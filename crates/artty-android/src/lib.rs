//! The Android app's Rust side: [`artty_client`] for Kotlin, through
//! UniFFI. A [`Connection`] to a server (ssh built in), what happens there
//! told to the app's [`Events`], what logging in needs asked of its
//! [`LoginAsker`]; screens as plain records the app draws.

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

/// What went wrong.
#[derive(Debug, uniffi::Error)]
pub enum ConnectError {
    Failed { reason: String },
    /// It runs another artty build (`server_version`), whose messages this
    /// app can't read: [`install`] puts this app's there.
    AnotherBuild { server_version: String },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Failed { reason } => f.write_str(reason),
            ConnectError::AnotherBuild { server_version } => {
                write!(f, "it runs another artty build ({server_version})")
            },
        }
    }
}

impl From<String> for ConnectError {
    fn from(reason: String) -> Self {
        ConnectError::Failed { reason }
    }
}

impl From<artty_client::StartError> for ConnectError {
    fn from(error: artty_client::StartError) -> Self {
        match error {
            artty_client::StartError::AnotherBuild { server_version } => {
                ConnectError::AnotherBuild { server_version }
            },
            artty_client::StartError::Failed(reason) => ConnectError::Failed { reason },
        }
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

/// What a selection takes.
#[derive(uniffi::Enum)]
pub enum SelectionKind {
    Characters,
    Words,
    Lines,
}

#[derive(uniffi::Enum)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(uniffi::Enum)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
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
    /// Something needs the user: an agent waiting, say. On `pane` of `window`, if known.
    Notify {
        title: String,
        body: String,
        window: Option<u64>,
        pane: Option<u64>,
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

/// This phone's key in Android Keystore: it signs there, and its private half
/// never leaves (called off the main thread).
#[uniffi::export(foreign)]
pub trait KeystoreKey: Send + Sync {
    /// ECDSA P-256 with SHA-256 (`SHA256withECDSA`): the signature in DER.
    /// `None` if it couldn't.
    fn sign(&self, data: Vec<u8>) -> Option<Vec<u8>>;
}

struct Keystore(Arc<dyn KeystoreKey>);

impl ssh::HeldKey for Keystore {
    fn sign(&self, data: &[u8]) -> Option<Vec<u8>> {
        self.0.sign(data.to_vec())
    }
}

/// The keys to log in with: the Keystore one, then `private_key`.
fn credentials(
    keystore: Option<Arc<dyn KeystoreKey>>,
    public_key: Option<Vec<u8>>,
    private_key: Option<String>,
) -> Result<Credentials, ConnectError> {
    let held = match (keystore, public_key) {
        (Some(key), Some(public)) => vec![ssh::Held {
            public: ssh::held_public_key(&public, "")?,
            key: Arc::new(Keystore(key)),
        }],
        _ => Vec::new(),
    };
    Ok(Credentials { agent: false, held, keys: private_key.into_iter().map(|k| (k, None)).collect() })
}

/// The builds of artty for Linux the app carries (its own build).
#[uniffi::export(foreign)]
pub trait LinuxBuilds: Send + Sync {
    /// The executable for `cpu` (`x86_64` or `aarch64`), if carried.
    fn build(&self, cpu: String) -> Option<Vec<u8>>;
}

/// After [`install`]: what the server's "artty there" setting should become
/// so connecting runs what was installed (`None`: found as `artty`), if it
/// should change.
#[derive(uniffi::Record)]
pub struct Installed {
    pub change_artty: bool,
    pub artty: Option<String>,
}

/// Install this app's build of artty on `server`, as the Mac app does: into
/// `~/.local/bin`, moving a running server onto it (its programs keep
/// running). Logs in as [`connect`] does. Blocks: call it off the main thread.
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn install(
    server: ServerAddress,
    keystore: Option<Arc<dyn KeystoreKey>>,
    public_key: Option<Vec<u8>>,
    private_key: Option<String>,
    known_hosts: String,
    login: Arc<dyn LoginAsker>,
    builds: Arc<dyn LinuxBuilds>,
) -> Result<Installed, ConnectError> {
    let target = Target { user: server.user, host: server.host, port: server.port };
    let installed = ssh::install(
        &target,
        &credentials(keystore, public_key, private_key)?,
        KnownHosts(PathBuf::from(known_hosts)),
        Arc::new(Asking(login)),
        |cpu| builds.build(cpu.to_owned()),
    )?;
    let change = installed.artty_setting(server.artty.as_deref());
    Ok(Installed { change_artty: change.is_some(), artty: change.flatten() })
}

/// A connection to a server.
#[derive(uniffi::Object)]
pub struct Connection {
    client: Client,
}

/// Log in to `server` with the Keystore key (`public_key` is its X.509
/// `SubjectPublicKeyInfo`, as Keystore gives it), then `private_key`
/// (OpenSSH format) — the server's own questions are asked too — and greet
/// the artty server there. Machines' keys are recorded in `known_hosts` (a
/// file of the app's). Blocks: call it off the main thread.
#[uniffi::export]
pub fn connect(
    server: ServerAddress,
    keystore: Option<Arc<dyn KeystoreKey>>,
    public_key: Option<Vec<u8>>,
    private_key: Option<String>,
    known_hosts: String,
    login: Arc<dyn LoginAsker>,
    events: Arc<dyn Events>,
) -> Result<Arc<Connection>, ConnectError> {
    let target = Target { user: server.user, host: server.host, port: server.port };
    let credentials = credentials(keystore, public_key, private_key)?;
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
                    Event::Notify { title, body, window, pane } => {
                        ClientEvent::Notify { title, body, window, pane }
                    },
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

    /// Ask the server whether it's there now: `silent_seconds` soon says
    /// whether it answered.
    pub fn ping(&self) -> Result<(), ConnectError> {
        Ok(self.client.ping()?)
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

    /// Whether the pane's program takes the mouse: a touch there is a click.
    pub fn wants_mouse(&self, pane: u64) -> bool {
        self.client.wants_mouse(pane)
    }

    /// A mouse event at viewport cell (`line`, `column`), if the program
    /// asked for such events. Returns whether it was sent.
    pub fn mouse(
        &self,
        pane: u64,
        button: MouseButton,
        action: MouseAction,
        line: u32,
        column: u32,
    ) -> Result<bool, ConnectError> {
        let button = match button {
            MouseButton::Left => artty_client::MouseButton::Left,
            MouseButton::Middle => artty_client::MouseButton::Middle,
            MouseButton::Right => artty_client::MouseButton::Right,
        };
        let action = match action {
            MouseAction::Press => artty_client::MouseAction::Press,
            MouseAction::Release => artty_client::MouseAction::Release,
            MouseAction::Motion => artty_client::MouseAction::Motion,
        };
        Ok(self.client.mouse(pane, button, action, line as usize, column as usize)?)
    }

    /// The wheel turned `lines` (up, if positive) over a cell: to the
    /// program (wheel reports, or arrows in a full-screen one), or the
    /// history.
    pub fn wheel(&self, pane: u64, lines: i32, line: u32, column: u32) -> Result<(), ConnectError> {
        Ok(self.client.wheel(pane, lines, line as usize, column as usize)?)
    }

    /// Start selecting at viewport cell (`line`, `column`); `right`: in its
    /// right half.
    pub fn select_start(
        &self,
        pane: u64,
        kind: SelectionKind,
        line: u32,
        column: u32,
        right: bool,
    ) -> Result<(), ConnectError> {
        let kind = match kind {
            SelectionKind::Characters => artty_client::SelectionKind::Characters,
            SelectionKind::Words => artty_client::SelectionKind::Words,
            SelectionKind::Lines => artty_client::SelectionKind::Lines,
        };
        Ok(self.client.select_start(pane, kind, line as usize, column as usize, right)?)
    }

    /// Take the selection to a cell.
    pub fn select_update(
        &self,
        pane: u64,
        line: u32,
        column: u32,
        right: bool,
    ) -> Result<(), ConnectError> {
        Ok(self.client.select_update(pane, line as usize, column as usize, right)?)
    }

    pub fn select_clear(&self, pane: u64) -> Result<(), ConnectError> {
        Ok(self.client.select_clear(pane)?)
    }

    /// The text selected, if any. Asks the server: call it off the main thread.
    pub fn selection_text(&self, pane: u64) -> Result<Option<String>, ConnectError> {
        Ok(self.client.selection_text(pane)?)
    }

    /// Whether the program takes pastes whole; else each line runs as it
    /// arrives (worth asking before pasting several).
    pub fn pastes_whole(&self, pane: u64) -> bool {
        self.client.pastes_whole(pane)
    }

    /// Paste text, as the program takes pastes.
    pub fn paste(&self, pane: u64, text: String) -> Result<(), ConnectError> {
        Ok(self.client.paste(pane, &text)?)
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

/// The Keystore key's public half (X.509 `SubjectPublicKeyInfo`) as
/// `authorized_keys` takes it.
#[uniffi::export]
pub fn keystore_public_key(public_key: Vec<u8>, comment: String) -> Result<String, ConnectError> {
    let key = ssh::held_public_key(&public_key, &comment)?;
    Ok(key.to_openssh().map_err(|err| err.to_string())?)
}
