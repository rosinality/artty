//! The Android app's Rust side: [`artty_client`] for Kotlin, through
//! UniFFI. A [`Connection`] to a server (ssh built in), what happens there
//! told to the app's [`Events`], what logging in needs asked of its
//! [`LoginAsker`]; screens as plain records the app draws.

use std::path::PathBuf;
use std::sync::Arc;

use artty_client::ssh::{self, Credentials, HostKey, KnownHosts, Login, Target};
use artty_client::ssh_config::{self, Config, Local};
use artty_client::{Client, Event, Key};
use artty_core::TerminalSize;
use artty_core::protocol::WindowLayout;

uniffi::setup_scaffolding!();

/// Where to connect, as the app keeps it.
#[derive(uniffi::Record, Clone)]
pub struct ServerAddress {
    /// As `ssh` takes it: an alias of the ssh config, `host`, `user@host`,
    /// `user@host:port`.
    pub destination: String,
    /// `artty` there, if not on its PATH.
    pub artty: Option<String>,
    /// No artty there: its shells, reached with plain ssh (tabs of shells
    /// that end with the connection; no files or agents).
    pub plain: bool,
}

/// What went wrong.
#[derive(Debug, uniffi::Error)]
pub enum ConnectError {
    Failed {
        reason: String,
    },
    /// It runs another artty build (`server_version`), whose messages this
    /// app can't read: [`install`] puts this app's there.
    AnotherBuild {
        server_version: String,
    },
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
    /// The passphrase of the key `name`, or none to skip it.
    fn passphrase(&self, name: String) -> Option<String>;
    /// May `host` (`user@alias`, which `ForwardAgent` lends the keys to)
    /// sign with the key `name`?
    fn agent_sign(&self, name: String, host: String) -> bool;
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
    /// Its name, or (`workspace_named` false) its directory's or repository's.
    pub workspace_name: String,
    pub workspace_named: bool,
    /// One of the workspace colors (`red`, `orange`, …), if it has one.
    pub workspace_color: Option<String>,
    /// Its place in its workspace.
    pub index: u32,
    /// The name given it; empty for none (its focused pane's title is shown).
    pub title: String,
    /// Its panes, and the one focused.
    pub panes: Vec<u64>,
    pub focused: u64,
    /// The one the window shows.
    pub active: bool,
}

/// What a pane is doing, for the tabs drawer.
#[derive(uniffi::Record)]
pub struct PaneItem {
    pub pane: u64,
    /// What its programs call it (a tab without a name shows its focused pane's).
    pub title: String,
    /// `busy`, `waiting`, `done`, `failed`, `attention`, or empty.
    pub icon: String,
    /// How much it wants the user (waiting most): a tab shows its most urgent pane's icon.
    pub urgency: u8,
    /// The agent running in it (`claude`, `codex`), if any.
    pub agent: Option<String>,
    /// The command or agent, an exit code, progress (not what the title says).
    pub detail: Option<String>,
}

/// Finding in a pane: whether a match is shown, which of how many.
#[derive(uniffi::Record)]
pub struct FindResult {
    pub found: bool,
    /// 1-based, counted from the top of the history; 0 with no match.
    pub index: u32,
    pub total: u32,
    /// There are more than `total` (counting stopped).
    pub more: bool,
    /// The pane has history to search (a full-screen program's isn't the terminal's).
    pub history: bool,
}

/// A line of a pane's output with what was searched for.
#[derive(uniffi::Record, Clone)]
pub struct FoundLine {
    pub pane: u64,
    pub line: u64,
    /// Where the match is in `text` (characters).
    pub column: u32,
    pub length: u32,
    pub text: String,
}

/// A coding agent's conversation mentioning what was searched for.
#[derive(uniffi::Record, Clone)]
pub struct FoundSession {
    /// `claude` or `codex`.
    pub agent: String,
    pub session: String,
    pub cwd: Option<String>,
    /// Its first request, shortened.
    pub title: String,
    /// The text around the match.
    pub snippet: String,
    /// Last written, seconds since the epoch.
    pub modified: u64,
}

/// A folder on the server, listed.
#[derive(uniffi::Record)]
pub struct Folder {
    /// Where it is (resolved).
    pub path: String,
    pub entries: Vec<FolderEntry>,
    /// Changes to it are told (`ClientEvent::DirChanged`); not on some filesystems.
    pub watched: bool,
}

#[derive(uniffi::Record)]
pub struct FolderEntry {
    pub name: String,
    /// A folder (or a link to one).
    pub dir: bool,
    pub link: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: u64,
}

/// Part of a file on the server.
#[derive(uniffi::Record)]
pub struct FilePart {
    pub data: Vec<u8>,
    /// The whole file's.
    pub length: u64,
}

/// What to do when a file of the name is there already.
#[derive(uniffi::Enum)]
pub enum IfExists {
    /// Fail with `UploadError::Exists` (before anything is written).
    Ask,
    Replace,
    /// Beside it, as `name (1)`.
    KeepBoth,
}

/// Why sending a file failed.
#[derive(Debug, uniffi::Error)]
pub enum UploadError {
    /// A file of the name is there.
    Exists,
    Failed {
        reason: String,
    },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Exists => f.write_str("a file of that name is there"),
            UploadError::Failed { reason } => f.write_str(reason),
        }
    }
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
    /// The folder watched for `pane` (`Connection::list_dir`) changed, or the
    /// pane moved to another: list it again.
    DirChanged {
        pane: u64,
    },
    /// What the shown window's panes are doing (all of them, each time).
    Panes {
        panes: Vec<PaneItem>,
    },
    /// An image a screen shows arrived: `Connection::image` has it.
    Image {
        id: u64,
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
    /// A forward started, stopped, or its connections changed:
    /// `Connection::forwards` has them.
    Forwards,
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

/// How cells are underlined.
#[derive(uniffi::Enum, Clone, Copy)]
pub enum UnderlineStyle {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cells from `column`, `width` wide, underlined in a color.
#[derive(uniffi::Record)]
pub struct Underline {
    pub column: u32,
    pub width: u32,
    pub color: u32,
    pub style: UnderlineStyle,
}

/// Cells from `column`, `cells` wide, showing a row of an image: from its
/// cell (`tile_column`, `tile_row`) on.
#[derive(uniffi::Record)]
pub struct ImageSpan {
    pub column: u32,
    pub cells: u32,
    pub image: u64,
    pub tile_column: u32,
    pub tile_row: u32,
}

/// An image's pixels, and its size in cells.
#[derive(uniffi::Record)]
pub struct Image {
    /// An image file (PNG, JPEG, GIF…), else RGBA rows.
    pub encoded: bool,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub columns: f32,
    pub rows: f32,
}

/// A link on a line: `start..end` columns.
#[derive(uniffi::Record)]
pub struct LinkSpan {
    pub url: String,
    pub start: u32,
    pub end: u32,
}

#[derive(uniffi::Record)]
pub struct Line {
    pub runs: Vec<Run>,
    pub backgrounds: Vec<Rect>,
    /// Search matches, drawn over the backgrounds (the selection marks the
    /// current one).
    pub matches: Vec<Rect>,
    /// Selected cells, drawn over the matches.
    pub selection: Vec<Rect>,
    /// Parts of images, over the backgrounds and under the text.
    pub images: Vec<ImageSpan>,
    pub underlines: Vec<Underline>,
}

#[derive(uniffi::Record)]
pub struct Cursor {
    pub line: u32,
    pub column: u32,
    pub wide: bool,
    pub color: u32,
    /// A block (else a bar or underline: drawn thin).
    pub block: bool,
    pub blinking: bool,
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
                    matches: rects(&line.matches),
                    selection: rects(&line.selection),
                    images: line
                        .images
                        .iter()
                        .map(|i| ImageSpan {
                            column: i.column as u32,
                            cells: i.cells as u32,
                            image: i.image,
                            tile_column: u32::from(i.tile_column),
                            tile_row: u32::from(i.tile_row),
                        })
                        .collect(),
                    underlines: line
                        .underlines
                        .iter()
                        .map(|u| Underline {
                            column: u.column as u32,
                            width: u.width as u32,
                            color: rgb(u.color),
                            style: match u.style {
                                artty_core::UnderlineStyle::Single => UnderlineStyle::Single,
                                artty_core::UnderlineStyle::Double => UnderlineStyle::Double,
                                artty_core::UnderlineStyle::Curly => UnderlineStyle::Curly,
                                artty_core::UnderlineStyle::Dotted => UnderlineStyle::Dotted,
                                artty_core::UnderlineStyle::Dashed => UnderlineStyle::Dashed,
                            },
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
                blinking: c.blinking,
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
                workspace_named: workspace.name.is_some(),
                workspace_color: workspace.color.clone(),
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

/// Logging in, asked of the app; what the server said kept, for why it failed.
struct Asking(Arc<dyn LoginAsker>, std::sync::Mutex<Vec<String>>);

impl Asking {
    fn new(asker: Arc<dyn LoginAsker>) -> Arc<Asking> {
        Arc::new(Asking(asker, Default::default()))
    }
}

impl Login for Asking {
    fn host_key(&self, target: &Target, fingerprint: &str, known: HostKey) -> bool {
        self.0.host_key(target.host.clone(), fingerprint.to_owned(), known == HostKey::Changed)
    }

    fn said(&self, text: &str) {
        self.1.lock().unwrap().push(text.to_owned());
        self.0.said(text.to_owned());
    }

    fn answer(&self, instructions: &str, prompts: &[(String, bool)]) -> Option<Vec<String>> {
        let prompts =
            prompts.iter().map(|(text, echo)| Prompt { text: text.clone(), echo: *echo }).collect();
        self.0.answer(instructions.to_owned(), prompts)
    }

    fn passphrase(&self, name: &str) -> Option<String> {
        self.0.passphrase(name.to_owned())
    }

    fn agent_sign(&self, name: &str, host: &str) -> bool {
        self.0.agent_sign(name.to_owned(), host.to_owned())
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

/// A private key added to the app: what `IdentityFile` names it by (its
/// file name, `id_ed25519`), its text (OpenSSH's format, or PEM), and its
/// passphrase if one is kept.
#[derive(uniffi::Record, Clone)]
pub struct SshKey {
    pub name: String,
    pub text: String,
    pub passphrase: Option<String>,
}

/// What logging in goes by.
#[derive(uniffi::Record, Clone)]
pub struct SshSetup {
    /// The Keystore key's public half (X.509 `SubjectPublicKeyInfo`, as
    /// Keystore gives it); `IdentityFile` names it [`PHONE_KEY`].
    pub phone_key: Option<Vec<u8>>,
    pub keys: Vec<SshKey>,
    /// The app's home for ssh: its `.ssh/config` is the ssh config (and
    /// `Include`s are read from `.ssh`).
    pub home: String,
    /// Machines' keys, as OpenSSH records them.
    pub known_hosts: String,
    /// Lines of history a plain ssh tab keeps.
    pub scrollback: u32,
}

/// What `IdentityFile` names this phone's key by (`IdentityFile ~/.ssh/phone`).
pub const PHONE_KEY: &str = "phone";

/// The keys to log in with: the Keystore one, and those added.
fn credentials(
    keystore: Option<Arc<dyn KeystoreKey>>,
    setup: &SshSetup,
) -> Result<Credentials, ConnectError> {
    let held = match (keystore, &setup.phone_key) {
        (Some(key), Some(public)) => vec![ssh::Held {
            name: PHONE_KEY.to_owned(),
            public: ssh::held_public_key(public, "")?,
            key: Arc::new(Keystore(key)),
        }],
        _ => Vec::new(),
    };
    let keys = setup
        .keys
        .iter()
        .map(|k| ssh::Key {
            name: k.name.clone(),
            text: k.text.clone(),
            passphrase: k.passphrase.clone(),
        })
        .collect();
    Ok(Credentials { agent: false, held, keys })
}

/// The app's ssh config (none: an empty one), and who's connecting.
fn read_config(home: &str) -> (Config, Local) {
    let path = std::path::Path::new(home).join(".ssh/config");
    let config = if path.exists() {
        Config::read(&path, home).unwrap_or_default()
    } else {
        Config::default()
    };
    (config, Local { home: home.to_owned(), user: String::new(), host: String::new() })
}

/// How to reach `destination`, as the app's ssh config says.
fn plan(destination: &str, home: &str) -> Result<ssh::Plan, ConnectError> {
    let (config, local) = read_config(home);
    Ok(ssh::plan(&config, &local, destination)?)
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
pub fn install(
    server: ServerAddress,
    setup: SshSetup,
    keystore: Option<Arc<dyn KeystoreKey>>,
    login: Arc<dyn LoginAsker>,
    builds: Arc<dyn LinuxBuilds>,
) -> Result<Installed, ConnectError> {
    let installed = ssh::install(
        &plan(&server.destination, &setup.home)?,
        &credentials(keystore, &setup)?,
        KnownHosts(PathBuf::from(&setup.known_hosts)),
        Asking::new(login),
        |cpu| builds.build(cpu.to_owned()),
    )?;
    let change = installed.artty_setting(server.artty.as_deref());
    Ok(Installed { change_artty: change.is_some(), artty: change.flatten() })
}

/// A connection to a server.
#[derive(uniffi::Object)]
pub struct Connection {
    client: Client,
    forwards: ssh::Forwards,
}

/// Log in to `server` as the app's ssh config says (`setup`: through its
/// jump hosts, with the keys it names: the Keystore one and those added;
/// the server's own questions are asked too), and greet the artty server
/// there (or, `plain`, serve its shells from here). Blocks: call it off the
/// main thread.
#[uniffi::export]
pub fn connect(
    server: ServerAddress,
    setup: SshSetup,
    keystore: Option<Arc<dyn KeystoreKey>>,
    login: Arc<dyn LoginAsker>,
    events: Arc<dyn Events>,
) -> Result<Arc<Connection>, ConnectError> {
    let plan = plan(&server.destination, &setup.home)?;
    let credentials = credentials(keystore, &setup)?;
    let known = KnownHosts(PathBuf::from(&setup.known_hosts));
    let asking = Asking::new(login);
    let (link, forwards) = if server.plain {
        ssh::shells(&plan, &credentials, known, asking.clone(), setup.scrollback as usize)?
    } else {
        let command = ssh::server_command(server.artty.as_deref());
        ssh::connect(&plan, &command, &credentials, known, asking.clone())?
    };
    {
        let events = events.clone();
        forwards.on_change(move || events.event(ClientEvent::Forwards));
    }
    let (client, received) = match Client::start(link, env!("CARGO_PKG_VERSION")) {
        Ok(started) => started,
        // Its shell saying there's no artty: say so (the app offers plain ssh).
        Err(artty_client::StartError::Failed(reason)) => {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let said = asking.1.lock().unwrap().clone();
            return Err(artty_core::remote::not_installed(&said).unwrap_or(reason).into());
        },
        Err(err) => return Err(err.into()),
    };
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
                    Event::Image { id } => ClientEvent::Image { id },
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
                    Event::DirChanged { pane } => ClientEvent::DirChanged { pane },
                    Event::Clipboard { text } => ClientEvent::Clipboard { text },
                    Event::Notify { title, body, window, pane } => {
                        ClientEvent::Notify { title, body, window, pane }
                    },
                    Event::Closed(reason) => ClientEvent::Closed { reason },
                    Event::Panes(panes) => ClientEvent::Panes {
                        panes: panes
                            .iter()
                            .map(|state| PaneItem {
                                pane: state.info.id,
                                title: state.info.title.clone(),
                                icon: state.icon.clone(),
                                urgency: state.urgency(),
                                agent: state.agent.clone(),
                                detail: state.short_detail(&state.info.title),
                            })
                            .collect(),
                    },
                    Event::Stopping { .. } => continue,
                };
                events.event(event);
            }
        })
        .map_err(|err| err.to_string())?;
    Ok(Arc::new(Connection { client, forwards }))
}

/// A port forward over the connection, as it's doing.
#[derive(uniffi::Record)]
pub struct ForwardItem {
    pub id: u64,
    pub kind: ForwardKind,
    /// As `ssh` writes it: `L 8080 → localhost:80`.
    pub description: String,
    /// From the ssh config (else added while connected).
    pub configured: bool,
    pub listening: Option<u16>,
    /// Why it isn't listening.
    pub error: Option<String>,
    /// Connections through it now, and in all.
    pub open: u32,
    pub total: u32,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum ForwardKind {
    /// Listening on this phone, to there.
    Local,
    /// Listening there, to here.
    Remote,
    /// Listening on this phone: SOCKS says where to there.
    Dynamic,
}

#[uniffi::export]
impl Connection {
    /// Its port forwards.
    pub fn forwards(&self) -> Vec<ForwardItem> {
        self.forwards
            .list()
            .into_iter()
            .map(|f| ForwardItem {
                id: f.id,
                kind: match f.kind {
                    ssh::ForwardKind::Local => ForwardKind::Local,
                    ssh::ForwardKind::Remote => ForwardKind::Remote,
                    ssh::ForwardKind::Dynamic => ForwardKind::Dynamic,
                },
                description: f.describe(),
                configured: f.configured,
                listening: f.listening,
                error: f.error,
                open: f.open as u32,
                total: f.total as u32,
            })
            .collect()
    }

    /// Forward `port` (on `bind`, if given; else the loopback) to `host`:`host_port`
    /// (for a dynamic one, none: SOCKS says where), until stopped or disconnected.
    pub fn add_forward(
        &self,
        kind: ForwardKind,
        bind: Option<String>,
        port: u16,
        host: Option<String>,
        host_port: u16,
    ) -> u64 {
        let kind = match kind {
            ForwardKind::Local => ssh::ForwardKind::Local,
            ForwardKind::Remote => ssh::ForwardKind::Remote,
            ForwardKind::Dynamic => ssh::ForwardKind::Dynamic,
        };
        let to = host.filter(|_| kind != ssh::ForwardKind::Dynamic).map(|h| (h, host_port));
        let bind = bind.filter(|b| !b.trim().is_empty());
        self.forwards.add(kind, ssh_config::Forward { bind, port, to }, false)
    }

    pub fn stop_forward(&self, id: u64) {
        self.forwards.stop(id);
    }

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

    /// A new tab at the end of `workspace`, or of the window's active one.
    pub fn new_tab(&self, window: u64, workspace: Option<u64>) -> Result<(), ConnectError> {
        Ok(self.client.new_tab(window, workspace)?)
    }

    pub fn close_tab(&self, workspace: u64, index: u32) -> Result<(), ConnectError> {
        Ok(self.client.close_tab(workspace, index as usize)?)
    }

    /// `None`: back to what its programs call it.
    pub fn rename_tab(
        &self,
        workspace: u64,
        index: u32,
        title: Option<String>,
    ) -> Result<(), ConnectError> {
        Ok(self.client.rename_tab(workspace, index as usize, title)?)
    }

    pub fn new_workspace(&self, window: u64, name: Option<String>) -> Result<(), ConnectError> {
        Ok(self.client.new_workspace(window, name)?)
    }

    /// `None`: back to its directory's or repository's name.
    pub fn rename_workspace(
        &self,
        workspace: u64,
        name: Option<String>,
    ) -> Result<(), ConnectError> {
        Ok(self.client.rename_workspace(workspace, name)?)
    }

    pub fn close_workspace(&self, workspace: u64) -> Result<(), ConnectError> {
        Ok(self.client.close_workspace(workspace)?)
    }

    /// Find `text` in a pane's output, history too: the next match back
    /// (`older`) or forward, or (`restart`) the newest; selected and
    /// scrolled to. Blocks: call it off the main thread.
    pub fn find(
        &self,
        pane: u64,
        text: String,
        older: bool,
        restart: bool,
    ) -> Result<FindResult, ConnectError> {
        let found = self.client.find(pane, &text, older, restart)?;
        Ok(FindResult {
            found: found.found,
            index: found.index as u32,
            total: found.total as u32,
            more: found.more,
            history: found.history,
        })
    }

    /// Done finding: the pane scrolls back down.
    pub fn end_find(&self, pane: u64) -> Result<(), ConnectError> {
        Ok(self.client.end_find(pane)?)
    }

    /// Lines with `text` in every pane's output, history too, newest first.
    /// Blocks: call it off the main thread.
    pub fn search_output(&self, text: String) -> Result<Vec<FoundLine>, ConnectError> {
        let hits = self.client.search_output(&text)?;
        Ok(hits
            .into_iter()
            .map(|hit| FoundLine {
                pane: hit.pane,
                line: hit.line,
                column: hit.column as u32,
                length: hit.length as u32,
                text: hit.text,
            })
            .collect())
    }

    /// Coding agents' conversations mentioning `text`, newest first. Blocks:
    /// call it off the main thread.
    pub fn search_sessions(&self, text: String) -> Result<Vec<FoundSession>, ConnectError> {
        let hits = self.client.search_sessions(&text)?;
        Ok(hits
            .into_iter()
            .map(|hit| FoundSession {
                agent: hit.agent,
                session: hit.session,
                cwd: hit.cwd.map(|cwd| cwd.display().to_string()),
                title: hit.title,
                snippet: hit.snippet,
                modified: hit.modified,
            })
            .collect())
    }

    /// What's in a folder on the pane's machine: `path`, else the pane's
    /// current directory. With `watch`, `ClientEvent::DirChanged` tells when
    /// it changes (or, without a path, when the pane moves), until `unwatch`.
    /// Blocks: call it off the main thread.
    pub fn list_dir(
        &self,
        pane: u64,
        path: Option<String>,
        watch: bool,
    ) -> Result<Folder, ConnectError> {
        let listing = self.client.list_dir(pane, path.map(PathBuf::from), watch)?;
        Ok(Folder {
            path: listing.path.display().to_string(),
            entries: listing
                .entries
                .unwrap_or_default()
                .into_iter()
                .map(|e| FolderEntry {
                    name: e.name,
                    dir: e.dir,
                    link: e.link,
                    size: e.size,
                    modified: e.modified,
                })
                .collect(),
            watched: listing.watched,
        })
    }

    pub fn unwatch(&self, pane: u64) -> Result<(), ConnectError> {
        Ok(self.client.unwatch(pane)?)
    }

    /// Part of a file on the pane's machine, from `offset` (at most 256 KiB).
    /// Blocks: call it off the main thread.
    pub fn read_file(
        &self,
        pane: u64,
        path: String,
        offset: u64,
    ) -> Result<FilePart, ConnectError> {
        let chunk = self.client.read_file(pane, PathBuf::from(path), offset)?;
        Ok(FilePart { data: chunk.data, length: chunk.length })
    }

    /// Send part of a file into folder `dir` on the pane's machine, as
    /// `name`: every part of one file with the same `upload` (an id of the
    /// app's), the last one saying so; its answer is where the file went.
    /// Blocks: call it off the main thread.
    #[allow(clippy::too_many_arguments)]
    pub fn upload_part(
        &self,
        pane: u64,
        upload: u64,
        name: String,
        data: Vec<u8>,
        last: bool,
        dir: String,
        existing: IfExists,
    ) -> Result<Option<String>, UploadError> {
        use artty_core::protocol::{Existing, UploadError as Refused, UploadTo};
        let existing = match existing {
            IfExists::Ask => Existing::Refuse,
            IfExists::Replace => Existing::Replace,
            IfExists::KeepBoth => Existing::Rename,
        };
        let to = UploadTo::Folder { dir: PathBuf::from(dir), existing };
        match self.client.upload_part(pane, upload, &name, data, last, to) {
            Ok(uploaded) => Ok(uploaded.map(|u| u.path.display().to_string())),
            Err(Refused::Exists) => Err(UploadError::Exists),
            Err(err) => Err(UploadError::Failed { reason: err.to_string() }),
        }
    }

    /// Show a line found: its tab brought forward, the match selected.
    pub fn reveal(&self, found: FoundLine) -> Result<(), ConnectError> {
        Ok(self.client.reveal(&artty_core::protocol::OutputHit {
            pane: found.pane,
            line: found.line,
            column: found.column as usize,
            length: found.length as usize,
            text: found.text,
        })?)
    }

    /// Carry on a conversation found, in a new tab of `window`, in its directory.
    pub fn resume_session(&self, window: u64, found: FoundSession) -> Result<(), ConnectError> {
        Ok(self.client.resume_session(
            window,
            &artty_core::protocol::SessionHit {
                agent: found.agent,
                session: found.session,
                cwd: found.cwd.map(PathBuf::from),
                title: found.title,
                snippet: found.snippet,
                modified: found.modified,
            },
        )?)
    }

    pub fn screen(&self, pane: u64) -> Option<Screen> {
        self.client.screen(pane).map(Screen::from)
    }

    /// An image an `ImageSpan` shows, if it has arrived.
    pub fn image(&self, id: u64) -> Option<Image> {
        let image = self.client.image(id)?;
        Some(Image {
            encoded: matches!(image.format, artty_core::vt::term::cell::ImageFormat::Encoded),
            data: image.data.clone(),
            width: image.width,
            height: image.height,
            columns: image.columns,
            rows: image.rows,
        })
    }

    /// The link under viewport cell (`line`, `column`): an OSC 8 hyperlink,
    /// or a URL in the text. Asks the server: call it off the main thread.
    pub fn link_at(
        &self,
        pane: u64,
        line: u32,
        column: u32,
    ) -> Result<Option<LinkSpan>, ConnectError> {
        let link = self.client.link_at(pane, line as usize, column as usize)?;
        Ok(link.map(|(url, start, end)| LinkSpan { url, start: start as u32, end: end as u32 }))
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

/// How a destination resolves with the app's ssh config: shown while
/// adding a server, and before connecting.
#[derive(uniffi::Record)]
pub struct Resolution {
    /// `user@host:port` (the host's real name), if it resolves.
    pub address: Option<String>,
    /// The jump hosts on the way, by name.
    pub through: Vec<String>,
    /// Its forwards and other settings, as `ssh` would write them.
    pub details: Vec<String>,
    /// The config's options not done here.
    pub ignored: Vec<String>,
    /// Why it can't be reached as it is.
    pub error: Option<String>,
}

/// How `destination` resolves with the ssh config in `home` (see [`SshSetup::home`]).
#[uniffi::export]
pub fn resolve(destination: String, home: String) -> Resolution {
    match plan(&destination, &home) {
        Ok(plan) => {
            let t = &plan.target;
            let mut details = Vec::new();
            let forward = |f: &ssh_config::Forward| {
                let bind = f.bind.as_ref().map(|b| format!("{b}:")).unwrap_or_default();
                let to = f.to.as_ref().map(|(h, p)| format!(" → {h}:{p}")).unwrap_or_default();
                format!("{bind}{}{to}", f.port)
            };
            details.extend(t.local_forwards.iter().map(|f| format!("LocalForward {}", forward(f))));
            details
                .extend(t.remote_forwards.iter().map(|f| format!("RemoteForward {}", forward(f))));
            details.extend(
                t.dynamic_forwards.iter().map(|f| format!("DynamicForward {}", forward(f))),
            );
            if !t.identity_files.is_empty() {
                details.push(format!("IdentityFile {}", t.identity_files.join(", ")));
            }
            if let Ok(Some(proxy)) = ssh::proxy(t) {
                details.push(format!("ProxyCommand, done here: {}", proxy.describe()));
            }
            if t.forward_agent {
                details.push("ForwardAgent yes".into());
            }
            if let Some(command) = &t.remote_command {
                details.push(format!("RemoteCommand {command}"));
            }
            Resolution {
                address: Some(format!(
                    "{}@{}:{}",
                    t.user.clone().unwrap_or_default(),
                    t.host_name,
                    t.port
                )),
                through: plan.hops.iter().map(|h| h.alias.clone()).collect(),
                details,
                ignored: plan.ignored.clone(),
                error: None,
            }
        },
        Err(err) => Resolution {
            address: None,
            through: Vec::new(),
            details: Vec::new(),
            ignored: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

/// The app's ssh config, read: its hosts (to offer as servers), and the
/// lines it couldn't read.
#[derive(uniffi::Record)]
pub struct ConfigCheck {
    pub hosts: Vec<String>,
    pub errors: Vec<String>,
}

/// Read the ssh config `text` (its `Include`s from `home`'s `.ssh`).
#[uniffi::export]
pub fn check_config(text: String, home: String) -> ConfigCheck {
    let config = Config::parse_in(&text, &home);
    ConfigCheck {
        hosts: config.hosts(),
        errors: config.errors.iter().map(|(line, why)| format!("line {line}: {why}")).collect(),
    }
}

/// A private key, read: its public half (as `authorized_keys` takes it),
/// and whether it needs a passphrase.
#[derive(uniffi::Record)]
pub struct KeyInfo {
    pub public: String,
    pub encrypted: bool,
}

#[uniffi::export]
pub fn key_info(text: String) -> Result<KeyInfo, ConnectError> {
    let (public, encrypted) = ssh::key_info(&text)?;
    Ok(KeyInfo { public, encrypted })
}

/// The Keystore key's public half (X.509 `SubjectPublicKeyInfo`) as
/// `authorized_keys` takes it.
#[uniffi::export]
pub fn keystore_public_key(public_key: Vec<u8>, comment: String) -> Result<String, ConnectError> {
    let key = ssh::held_public_key(&public_key, &comment)?;
    Ok(key.to_openssh().map_err(|err| err.to_string())?)
}
