//! Talking to a server over a [`Link`]: the greeting, then windows to show,
//! their layouts and screens kept up to date (each pane's [`Frame`], with
//! the terminal's mode for keys), and input, keys and sizes sent back.
//! What happens arrives as [`Event`]s; a pane's screen is read with
//! [`Client::screen`].

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use artty_core::Frame;
use artty_core::TerminalSize;
use artty_core::keys::Key;
use artty_core::layout::PaneId;
use artty_core::mouse::{self, Action, Button, Wheel};
use artty_core::protocol::{
    ClientMessage, FileChunk, Hello, ImageData, Listing, OutputHit, PaneSpec, PaneState, Query,
    Reply, SearchFound, SelectionOp, ServerMessage, SessionHit, UploadError, UploadTo, Uploaded,
    WindowId, WindowLayout, WindowSummary, WorkspaceId, protocol_version, read_message,
    write_message,
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
    /// An image a screen shows arrived: [`Client::image`] has it.
    Image {
        id: u64,
    },
    /// What every pane shown is doing (titles, agents, progress).
    Panes(Vec<PaneState>),
    Bell {
        pane: PaneId,
    },
    /// The folder watched for `pane` ([`Client::list_dir`]) changed, or the
    /// pane moved to another: list it again.
    DirChanged {
        pane: PaneId,
    },
    Notify {
        title: String,
        body: String,
        /// The pane it's about, and its window.
        window: Option<WindowId>,
        pane: Option<PaneId>,
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

/// What a selection takes: characters, or whole words or lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Characters,
    Words,
    Lines,
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
    /// The images screens show, by id, as sent once each.
    images: Arc<Mutex<HashMap<u64, Arc<ImageData>>>>,
    /// The server's build, from its greeting.
    pub server_version: String,
    /// When it last sent anything.
    heard: Arc<Mutex<Instant>>,
    /// Queries waiting for their answers, by id.
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Reply>>>>,
    next_id: AtomicU64,
    _keep: Mutex<Box<dyn Send>>,
}

/// Why greeting a server failed.
#[derive(Debug)]
pub enum StartError {
    /// It runs another artty build, whose messages this one can't read: the
    /// fix is the same build on both ends.
    AnotherBuild {
        server_version: String,
    },
    Failed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::AnotherBuild { server_version } => {
                write!(f, "it runs another artty build ({server_version})")
            },
            StartError::Failed(reason) => f.write_str(reason),
        }
    }
}

impl From<String> for StartError {
    fn from(reason: String) -> Self {
        StartError::Failed(reason)
    }
}

impl From<StartError> for String {
    fn from(error: StartError) -> Self {
        error.to_string()
    }
}

impl Client {
    /// Greet the server over `link`; then what happens arrives on the
    /// returned channel. Fails if it isn't an artty server, or runs
    /// another build (`app_version`, this app's, is told it).
    pub fn start(
        link: Link,
        app_version: &str,
    ) -> Result<(Client, mpsc::Receiver<Event>), StartError> {
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
                    return Err(StartError::AnotherBuild { server_version });
                }
                server_version
            },
            Ok(_) => return Err("it answered, but not as an artty server".to_owned().into()),
            Err(err) => return Err(format!("no answer: {err}").into()),
        };

        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
        let screens = Arc::new(Mutex::new(HashMap::<PaneId, Screen>::new()));
        let images = Arc::new(Mutex::new(HashMap::<u64, Arc<ImageData>>::new()));
        let heard = Arc::new(Mutex::new(Instant::now()));
        let pending = Arc::new(Mutex::new(HashMap::<u64, mpsc::Sender<Reply>>::new()));
        let (events, received) = mpsc::channel();
        {
            let (writer, screens, heard) = (writer.clone(), screens.clone(), heard.clone());
            let (pending, images) = (pending.clone(), images.clone());
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
                            ServerMessage::Image(image) => {
                                let id = image.id;
                                images.lock().unwrap().insert(id, Arc::new(image));
                                Event::Image { id }
                            },
                            ServerMessage::Panes { panes } => Event::Panes(panes),
                            ServerMessage::Bell { pane } => Event::Bell { pane },
                            ServerMessage::DirChanged { pane } => Event::DirChanged { pane },
                            ServerMessage::Notify { title, body, window, pane } => {
                                Event::Notify { title, body, window, pane }
                            },
                            ServerMessage::Clipboard { text } => Event::Clipboard { text },
                            ServerMessage::Reply { id, reply } => {
                                // Waited for (Client::query), or the windows asked for.
                                if let Some(waiting) = pending.lock().unwrap().remove(&id) {
                                    let _ = waiting.send(reply);
                                    continue;
                                }
                                match reply {
                                    Reply::Windows(windows) => Event::Windows(windows),
                                    _ => continue,
                                }
                            },
                            ServerMessage::Stopping { restart } => Event::Stopping { restart },
                            // Screens are in: more may come.
                            ServerMessage::Sync { seq } => {
                                let gone = forget_unshown(&screens, &images);
                                let mut writer = writer.lock().unwrap();
                                let forget = gone.is_empty() || {
                                    let forget = ClientMessage::ForgetImages { ids: gone };
                                    write_message(&mut *writer, &forget).is_ok()
                                };
                                if !forget
                                    || write_message(&mut *writer, &ClientMessage::Ack { seq })
                                        .is_err()
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
            images,
            server_version,
            heard,
            pending,
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

    /// Ask the server whether it's there now (after the network changed,
    /// say): [`Client::silent_for`] says soon whether it answered.
    pub fn ping(&self) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(&ClientMessage::Ping { id })
    }

    /// Ask the server something, and wait (a while) for its answer.
    fn query(&self, query: Query) -> Result<Reply, String> {
        self.ask(|id| ClientMessage::Query { id, query }, Duration::from_secs(10))
    }

    /// Send what `message` makes of a new id, and wait for the reply to it.
    fn ask(
        &self,
        message: impl FnOnce(u64) -> ClientMessage,
        wait: Duration,
    ) -> Result<Reply, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self.send(&message(id));
        let answer = sent.and_then(|()| rx.recv_timeout(wait).map_err(|_| "no answer".to_owned()));
        self.pending.lock().unwrap().remove(&id);
        answer
    }

    /// Find `text` in a pane's output, history too: the match after the one
    /// shown (`older`: before it; `restart`: from the newest), selected and
    /// scrolled to. Blocks for the answer.
    pub fn find(
        &self,
        pane: PaneId,
        text: &str,
        older: bool,
        restart: bool,
    ) -> Result<SearchFound, String> {
        let query = text.to_owned();
        let message = |id| ClientMessage::Search { id, pane, query, older, restart };
        match self.ask(message, Duration::from_secs(10))? {
            Reply::Search(found) => found,
            _ => Err("an unexpected answer".into()),
        }
    }

    /// Done finding: the match is let go, the pane scrolls back down.
    pub fn end_find(&self, pane: PaneId) -> Result<(), String> {
        self.send(&ClientMessage::EndSearch { pane })
    }

    /// Lines with `text` in every pane's output on the server, history too
    /// (case ignored unless it has capitals), newest first. Blocks.
    pub fn search_output(&self, text: &str) -> Result<Vec<OutputHit>, String> {
        let query = Query::SearchOutput { text: text.to_owned() };
        match self.ask(|id| ClientMessage::Query { id, query }, Duration::from_secs(30))? {
            Reply::Found(hits) => Ok(hits),
            _ => Err("an unexpected answer".into()),
        }
    }

    /// Coding agents' conversations on the server mentioning `text`, newest
    /// first. Blocks.
    pub fn search_sessions(&self, text: &str) -> Result<Vec<SessionHit>, String> {
        let query = Query::SearchSessions { text: text.to_owned() };
        match self.ask(|id| ClientMessage::Query { id, query }, Duration::from_secs(30))? {
            Reply::Sessions(hits) => Ok(hits),
            _ => Err("an unexpected answer".into()),
        }
    }

    /// Show a line [`Client::search_output`] found: its tab brought forward,
    /// the match selected and scrolled to.
    pub fn reveal(&self, hit: &OutputHit) -> Result<(), String> {
        self.send(&ClientMessage::Reveal {
            pane: hit.pane,
            line: hit.line,
            column: hit.column,
            length: hit.length,
        })
    }

    /// Carry on a conversation [`Client::search_sessions`] found, in a new
    /// tab of `window`, in its directory.
    pub fn resume_session(&self, window: WindowId, hit: &SessionHit) -> Result<(), String> {
        self.send(&ClientMessage::ResumeSession {
            window,
            agent: hit.agent.clone(),
            session: hit.session.clone(),
            cwd: hit.cwd.clone(),
        })
    }

    /// Start selecting at viewport cell (`line`, `column`) (`right`: in its
    /// right half): characters, words or lines.
    pub fn select_start(
        &self,
        pane: PaneId,
        kind: SelectionKind,
        line: usize,
        column: usize,
        right: bool,
    ) -> Result<(), String> {
        let kind = match kind {
            SelectionKind::Characters => 0,
            SelectionKind::Words => 1,
            SelectionKind::Lines => 2,
        };
        self.send(&ClientMessage::Selection {
            pane,
            op: SelectionOp::Start { kind, line, column, right },
        })
    }

    /// Take the selection to cell (`line`, `column`).
    pub fn select_update(
        &self,
        pane: PaneId,
        line: usize,
        column: usize,
        right: bool,
    ) -> Result<(), String> {
        self.send(&ClientMessage::Selection {
            pane,
            op: SelectionOp::Update { line, column, right },
        })
    }

    pub fn select_clear(&self, pane: PaneId) -> Result<(), String> {
        self.send(&ClientMessage::Selection { pane, op: SelectionOp::Clear })
    }

    /// An image a screen shows ([`artty_core::frame::FrameImage::image`]).
    pub fn image(&self, id: u64) -> Option<Arc<ImageData>> {
        self.images.lock().unwrap().get(&id).cloned()
    }

    /// The link under a viewport cell (an OSC 8 hyperlink, or a URL in the
    /// text), and the columns it covers on that line. Asks the server: blocks.
    pub fn link_at(
        &self,
        pane: PaneId,
        line: usize,
        column: usize,
    ) -> Result<Option<(String, usize, usize)>, String> {
        match self.query(Query::LinkSpan { pane, line, column })? {
            Reply::Link(link) => Ok(link),
            _ => Err("an unexpected answer".to_owned()),
        }
    }

    /// The text selected in the pane, if any (asks the server: blocks).
    pub fn selection_text(&self, pane: PaneId) -> Result<Option<String>, String> {
        match self.query(Query::SelectionText { pane })? {
            Reply::Text(text) => Ok(text),
            _ => Err("an unexpected answer".to_owned()),
        }
    }

    /// Whether the pane's program takes pastes whole (bracketed paste);
    /// else each line runs as it arrives (worth asking before pasting several).
    pub fn pastes_whole(&self, pane: PaneId) -> bool {
        self.mode(pane).contains(TermMode::BRACKETED_PASTE)
    }

    /// Paste `text` into the pane, as its program takes pastes.
    pub fn paste(&self, pane: PaneId, text: &str) -> Result<(), String> {
        self.input(pane, artty_core::keys::paste(text, self.pastes_whole(pane)))
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

    /// A new tab (a shell) at the end of `workspace`, or of the window's
    /// active one.
    pub fn new_tab(&self, window: WindowId, workspace: Option<WorkspaceId>) -> Result<(), String> {
        self.send(&ClientMessage::NewTab { window, workspace, spec: PaneSpec::default() })
    }

    /// Close a tab: its programs stop.
    pub fn close_tab(&self, workspace: WorkspaceId, tab: usize) -> Result<(), String> {
        self.send(&ClientMessage::CloseTab { workspace, tab })
    }

    /// Name a tab (`None`: back to what its programs say).
    pub fn rename_tab(
        &self,
        workspace: WorkspaceId,
        tab: usize,
        title: Option<String>,
    ) -> Result<(), String> {
        self.send(&ClientMessage::RenameTab { workspace, tab, title })
    }

    /// A workspace with a shell, after the window's others.
    pub fn new_workspace(&self, window: WindowId, name: Option<String>) -> Result<(), String> {
        self.send(&ClientMessage::NewWorkspace { window, name, spec: PaneSpec::default() })
    }

    /// Name a workspace (`None`: back to its directory's or repository's).
    pub fn rename_workspace(
        &self,
        workspace: WorkspaceId,
        name: Option<String>,
    ) -> Result<(), String> {
        self.send(&ClientMessage::RenameWorkspace { workspace, name })
    }

    /// Close a workspace, every tab in it.
    pub fn close_workspace(&self, workspace: WorkspaceId) -> Result<(), String> {
        self.send(&ClientMessage::CloseWorkspace { workspace })
    }

    /// What's in a folder on the pane's machine: `path`, else the pane's
    /// current directory. With `watch`, [`Event::DirChanged`] tells when it
    /// changes (or, without a path, when the pane moves to another folder),
    /// until [`Client::unwatch`]. Blocks.
    pub fn list_dir(
        &self,
        pane: PaneId,
        path: Option<PathBuf>,
        watch: bool,
    ) -> Result<Listing, String> {
        match self.query(Query::ListDir { pane, path, known: None, watch })? {
            Reply::Dir(listing) => listing,
            _ => Err("an unexpected answer".into()),
        }
    }

    /// Stop telling about the folder watched for `pane`.
    pub fn unwatch(&self, pane: PaneId) -> Result<(), String> {
        self.send(&ClientMessage::Unwatch { pane })
    }

    /// Part of a file on the pane's machine, from `offset` (at most 256 KiB),
    /// with its whole length. Blocks.
    pub fn read_file(&self, pane: PaneId, path: PathBuf, offset: u64) -> Result<FileChunk, String> {
        match self.ask(
            |id| ClientMessage::Query { id, query: Query::ReadFile { pane, path, offset } },
            Duration::from_secs(30),
        )? {
            Reply::File(chunk) => chunk,
            _ => Err("an unexpected answer".into()),
        }
    }

    /// Send part of a file to the pane's machine, `to` there: upload `upload`
    /// (an id of the app's, the same for every part) named `name`. The last
    /// part's answer says where it went; a refusal ([`UploadError::Exists`])
    /// comes with the first. Blocks.
    pub fn upload_part(
        &self,
        pane: PaneId,
        upload: u64,
        name: &str,
        data: Vec<u8>,
        last: bool,
        to: UploadTo,
    ) -> Result<Option<Uploaded>, UploadError> {
        let name = name.to_owned();
        let query = Query::Upload { pane, upload, name, data, last, to };
        match self.ask(|id| ClientMessage::Query { id, query }, Duration::from_secs(60)) {
            Ok(Reply::Upload(result)) => result,
            Ok(_) => Err(UploadError::Failed("an unexpected answer".into())),
            Err(err) => Err(UploadError::Failed(err)),
        }
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

/// Let go of images no screen shows anymore; returns their ids, for telling
/// the server (which sends them again if they come back).
fn forget_unshown(
    screens: &Mutex<HashMap<PaneId, Screen>>,
    images: &Mutex<HashMap<u64, Arc<ImageData>>>,
) -> Vec<u64> {
    let mut images = images.lock().unwrap();
    if images.is_empty() {
        return Vec::new();
    }
    let shown: HashSet<u64> = screens
        .lock()
        .unwrap()
        .values()
        .flat_map(|s| s.frame.lines.iter().flat_map(|l| l.images.iter().map(|i| i.image)))
        .collect();
    let gone: Vec<u64> = images.keys().copied().filter(|id| !shown.contains(id)).collect();
    for id in &gone {
        images.remove(id);
    }
    gone
}
