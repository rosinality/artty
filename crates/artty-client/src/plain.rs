//! A machine without artty, reached with plain ssh: a stand-in for an artty
//! server, here, that a [`Client`](crate::Client) talks to as to any. Its
//! one window's tabs are shells there (each its own ssh channel), each
//! drawn by a terminal here (`Session::remote`) and sent as the server
//! sends screens; finding, selecting, links and the rest work as they do.
//! Its shells end with the connection: there's nothing to come back to.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use artty_core::control::TabInfo;
use artty_core::layout::{Node, PaneId};
use artty_core::protocol::{
    ClientMessage, ImageData, OutputHit, PaneState, Query, Reply, SearchFound, SelectionOp,
    ServerMessage, TabLayout, UploadError, WindowLayout, WindowSummary, WorkspaceLayout,
    protocol_version, read_message, write_message,
};
use artty_core::vt::event::Event as TermEvent;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::tty::RemoteEnds;
use artty_core::{CursorStyle, Frame, Notice, Session, SessionOptions, TerminalSize, Theme};

use crate::link::Link;

/// Its only window, and workspace.
pub const WINDOW: u64 = 1;
const WORKSPACE: u64 = 1;

/// What the greeting says the server runs: no artty, a shell.
pub const SERVER_VERSION: &str = "plain ssh";

/// Screens go at most this often.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// Screen batches unacknowledged before holding back (as the server does).
const FRAMES_IN_FLIGHT: u64 = 2;
/// A shell ending this soon is kept on screen.
const QUICK_EXIT: Duration = Duration::from_secs(5);
/// Before an app's size arrives.
const DEFAULT_SIZE: TerminalSize =
    TerminalSize { columns: 80, lines: 24, cell_width: 8, cell_height: 16 };

/// Where the shells run.
pub trait Shells: Send + 'static {
    /// Start a shell `size` big: its bytes through `ends` (which it says the
    /// end of, see `RemoteEnds`), new sizes from `sizes`.
    /// With `session`, the shell is kept there by that name (tmux), to be
    /// taken up again after the connection goes.
    fn open(
        &self,
        size: TerminalSize,
        ends: RemoteEnds,
        sizes: tokio::sync::mpsc::UnboundedReceiver<TerminalSize>,
        session: Option<String>,
    );
    /// Call `answered` once the machine answers: it's still there.
    fn ping(&self, answered: Box<dyn FnOnce() + Send>);
    /// The connection is gone.
    fn closed(&self) -> bool;
    /// Shells are kept there (in sessions), outliving the connection.
    fn keeps(&self) -> bool {
        false
    }
    /// The sessions kept there, to take up again (asks: blocks).
    fn sessions(&self) -> Vec<String> {
        Vec::new()
    }
    /// End a session kept there (its tab closed).
    fn end(&self, session: &str) {
        let _ = session;
    }
    /// The machine's files, for the files browser, if they can be reached.
    fn files(&self) -> Option<Arc<dyn Files>> {
        None
    }
}

pub use crate::plain_files::Files;

/// What the kept sessions are called: `artty-1`, `artty-2`…
pub const SESSION_PREFIX: &str = "artty-";

/// Serve the shells of `shells` (on `name`, `user@host`), each keeping
/// `scrollback` lines of history: the link to talk to it as to a server.
pub fn serve(shells: Box<dyn Shells>, name: String, scrollback: usize) -> Result<Link, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|err| err.to_string())?;
    let reader = ours.try_clone().map_err(|err| err.to_string())?;
    let (inbox, inputs) = mpsc::channel();
    {
        let inbox = inbox.clone();
        std::thread::Builder::new()
            .name("artty-plain-reader".into())
            .spawn(move || read_app(reader, inbox))
            .map_err(|err| err.to_string())?;
    }
    let files = shells.files();
    let plain = Plain {
        out: ours,
        shells,
        files,
        uploads: Default::default(),
        name,
        scrollback,
        inbox,
        panes: HashMap::new(),
        tabs: Vec::new(),
        active: 0,
        next_pane: 1,
        attached: false,
        size: DEFAULT_SIZE,
        theme: Arc::new(Theme::default()),
        cursor: CursorStyle::Block,
        sent: HashMap::new(),
        dirty: HashSet::new(),
        images_sent: HashSet::new(),
        batches_sent: 0,
        batches_acked: 0,
        pushed: Instant::now(),
        layout_sent: None,
        panes_sent: Vec::new(),
    };
    std::thread::Builder::new()
        .name("artty-plain".into())
        .spawn(move || plain.run(inputs))
        .map_err(|err| err.to_string())?;
    let writer = theirs.try_clone().map_err(|err| err.to_string())?;
    Ok(Link { reader: Box::new(theirs), writer: Box::new(writer), keep: Box::new(()) })
}

/// What the stand-in reacts to.
enum Input {
    App(ClientMessage),
    /// The app went (or sent what isn't a message).
    AppGone,
    Pane(PaneId, TermEvent),
    /// The machine answered ping `id`.
    Pong(u64),
    /// An answer worked out off this thread (the files browser's).
    Reply(u64, Reply),
}

/// The app's greeting (a line), then its messages.
fn read_app(stream: UnixStream, inbox: Sender<Input>) {
    let mut reader = BufReader::new(stream);
    let mut hello = String::new();
    if reader.read_line(&mut hello).is_err() {
        let _ = inbox.send(Input::AppGone);
        return;
    }
    while let Ok(message) = read_message::<ClientMessage>(&mut reader) {
        if inbox.send(Input::App(message)).is_err() {
            return;
        }
    }
    let _ = inbox.send(Input::AppGone);
}

struct Pane {
    session: Session,
    opened: Instant,
    /// What its program calls it.
    title: String,
    /// A command runs (shell integration says).
    busy: bool,
    progress: Option<u8>,
}

struct Tab {
    pane: PaneId,
    /// The name given it.
    title: Option<String>,
    /// The session its shell is kept in there, if kept.
    session: Option<String>,
}

struct Plain {
    out: UnixStream,
    shells: Box<dyn Shells>,
    /// The machine's files (SFTP), if reachable.
    files: Option<Arc<dyn Files>>,
    /// Files being sent there, part by part.
    uploads: crate::plain_files::Uploads,
    name: String,
    scrollback: usize,
    inbox: Sender<Input>,
    panes: HashMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active: usize,
    next_pane: PaneId,
    /// The app shows the window.
    attached: bool,
    /// The app's size for panes, for new tabs.
    size: TerminalSize,
    theme: Arc<Theme>,
    cursor: CursorStyle,
    /// The screens the app has, by pane, and those changed since.
    sent: HashMap<PaneId, Frame>,
    dirty: HashSet<PaneId>,
    images_sent: HashSet<u64>,
    batches_sent: u64,
    batches_acked: u64,
    /// When screens last went.
    pushed: Instant,
    layout_sent: Option<WindowLayout>,
    panes_sent: Vec<PaneState>,
}

impl Plain {
    fn run(mut self, inputs: Receiver<Input>) {
        self.serve(inputs);
        // Its reader holds the socket too: closed for both, the app hears.
        let _ = self.out.shutdown(std::net::Shutdown::Both);
    }

    fn serve(&mut self, inputs: Receiver<Input>) {
        self.send(ServerMessage::Welcome {
            version: protocol_version(),
            server_version: SERVER_VERSION.to_owned(),
        });
        loop {
            // Screens waiting: no longer than until they're due.
            let wait = match self.due() {
                true => FRAME_INTERVAL.saturating_sub(self.pushed.elapsed()),
                false => Duration::from_secs(1),
            };
            let mut next = match inputs.recv_timeout(wait) {
                Ok(input) => Some(input),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            while let Some(input) = next {
                if !self.handle(input) {
                    return;
                }
                next = inputs.try_recv().ok();
            }
            if self.shells.closed() {
                log::info!("{}: the connection is gone", self.name);
                return;
            }
            self.push();
        }
    }

    fn send(&mut self, message: ServerMessage) {
        // A write failing: the app is gone, and its reader says so.
        let _ = write_message(&mut self.out, &message);
    }

    /// Screens to send, and the app ready for them.
    fn due(&self) -> bool {
        self.attached
            && self.batches_sent - self.batches_acked < FRAMES_IN_FLIGHT
            && self
                .shown()
                .is_some_and(|pane| self.dirty.contains(&pane) || !self.sent.contains_key(&pane))
    }

    /// The pane the window shows.
    fn shown(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|tab| tab.pane)
    }

    /// Whether to carry on.
    fn handle(&mut self, input: Input) -> bool {
        match input {
            Input::App(message) => self.message(message),
            Input::AppGone => return false,
            Input::Pong(id) => self.send(ServerMessage::Pong { id }),
            Input::Reply(id, reply) => self.send(ServerMessage::Reply { id, reply }),
            Input::Pane(id, event) => {
                let Some(pane) = self.panes.get_mut(&id) else { return true };
                match pane.session.handle(event) {
                    Some(Notice::Redraw) => {
                        self.dirty.insert(id);
                    },
                    Some(Notice::Title(title)) => pane.title = title,
                    Some(Notice::ResetTitle) => pane.title.clear(),
                    Some(Notice::Bell) => self.send(ServerMessage::Bell { pane: id }),
                    Some(Notice::ClipboardStore(_, text)) => {
                        self.send(ServerMessage::Clipboard { text })
                    },
                    // The phone's clipboard isn't the program's to read.
                    Some(Notice::ClipboardLoad(..)) => {},
                    Some(Notice::Notification { title, body }) => {
                        let title = title.unwrap_or_else(|| self.name.clone());
                        self.send(ServerMessage::Notify {
                            title,
                            body,
                            window: Some(WINDOW),
                            pane: Some(id),
                        })
                    },
                    Some(Notice::Progress(progress)) => {
                        use artty_core::vt::ansi::Progress;
                        pane.progress = match progress {
                            Progress::Normal(p)
                            | Progress::Paused(Some(p))
                            | Progress::Error(Some(p)) => Some(p),
                            _ => None,
                        };
                    },
                    Some(Notice::CommandStarted(_)) => pane.busy = true,
                    Some(Notice::CommandFinished(_)) => pane.busy = false,
                    Some(Notice::Exited) => {
                        // The connection going ends every shell: that's not
                        // theirs to close, but the app's to come back to.
                        if self.shells.closed() {
                            return false;
                        }
                        // Gone at once (refused, or a message and out, as
                        // GitHub's): kept, for what it said; the user closes it.
                        if pane.opened.elapsed() < QUICK_EXIT {
                            return true;
                        }
                        if let Some(index) = self.tabs.iter().position(|t| t.pane == id) {
                            self.close_tab(index);
                        }
                    },
                    None => {},
                }
            },
        }
        true
    }

    fn message(&mut self, message: ClientMessage) {
        match message {
            ClientMessage::AttachWindow { .. } => {
                // The sessions kept there (from before the connection went), else one.
                if self.tabs.is_empty() {
                    let kept =
                        if self.shells.keeps() { self.shells.sessions() } else { Vec::new() };
                    for session in kept {
                        self.open_tab(Some(session));
                    }
                }
                if self.tabs.is_empty() {
                    let session = self.new_session();
                    self.open_tab(session);
                }
                self.attached = true;
                // The app's copies are new.
                self.sent.clear();
                self.images_sent.clear();
                self.layout_sent = None;
                self.panes_sent.clear();
                self.send(ServerMessage::Attached { window: WINDOW, bounds: None });
            },
            ClientMessage::DetachWindow { .. } => self.attached = false,
            ClientMessage::SelectTab { tab, .. } => {
                if tab < self.tabs.len() {
                    self.active = tab;
                }
            },
            ClientMessage::NewTab { .. } | ClientMessage::NewWorkspace { .. } => {
                let session = self.new_session();
                self.open_tab(session);
            },
            ClientMessage::CloseTab { tab, .. } => {
                if tab < self.tabs.len() {
                    self.close_tab(tab);
                }
            },
            ClientMessage::ClosePane { pane } => {
                if let Some(index) = self.tabs.iter().position(|t| t.pane == pane) {
                    self.close_tab(index);
                }
            },
            ClientMessage::CloseWorkspace { .. } => {
                while !self.tabs.is_empty() {
                    self.close_tab(self.tabs.len() - 1);
                }
            },
            ClientMessage::RenameTab { tab, title, .. } => {
                if let Some(tab) = self.tabs.get_mut(tab) {
                    tab.title = title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
                }
            },
            ClientMessage::Input { pane, bytes } => {
                if let Some(pane) = self.panes.get(&pane) {
                    pane.session.write(bytes);
                }
            },
            ClientMessage::Resize { pane, size } => {
                self.size = size;
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.resize(size);
                }
                self.dirty.insert(pane);
            },
            ClientMessage::Scroll { pane, lines } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.scroll(lines);
                }
                self.dirty.insert(pane);
            },
            ClientMessage::Selection { pane, op } => {
                if let Some(state) = self.panes.get(&pane) {
                    let side = |right: bool| if right { Side::Right } else { Side::Left };
                    match op {
                        SelectionOp::Start { kind, line, column, right } => {
                            let kind = match kind {
                                0 => SelectionType::Simple,
                                1 => SelectionType::Semantic,
                                _ => SelectionType::Lines,
                            };
                            state.session.start_selection(kind, line, column, side(right));
                        },
                        SelectionOp::Update { line, column, right } => {
                            state.session.update_selection(line, column, side(right))
                        },
                        SelectionOp::Clear => state.session.clear_selection(),
                    }
                }
                self.dirty.insert(pane);
            },
            ClientMessage::Search { id, pane, query, older, restart } => {
                let result = match self.panes.get_mut(&pane) {
                    Some(state) => {
                        if restart {
                            state.session.clear_selection();
                        }
                        if query.is_empty() {
                            Ok(SearchFound {
                                found: false,
                                index: 0,
                                total: 0,
                                more: false,
                                history: true,
                            })
                        } else {
                            state.session.search_counted(&query, older)
                        }
                    },
                    None => Err("no such pane".into()),
                };
                self.send(ServerMessage::Reply { id, reply: Reply::Search(result) });
                self.dirty.insert(pane);
            },
            ClientMessage::EndSearch { pane } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.end_search();
                }
                self.dirty.insert(pane);
            },
            ClientMessage::Reveal { pane, line, column, length } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.reveal(line, column, length);
                }
                if let Some(index) = self.tabs.iter().position(|t| t.pane == pane) {
                    self.active = index;
                }
                self.dirty.insert(pane);
            },
            ClientMessage::ClearBuffer { pane } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.clear_buffer();
                }
                self.dirty.insert(pane);
            },
            ClientMessage::Appearance { theme, cursor } => {
                self.theme = Arc::new(theme);
                self.cursor = cursor;
                for pane in self.panes.values_mut() {
                    pane.session.set_theme(self.theme.clone());
                    pane.session.set_cursor_shape(cursor.shape());
                }
                self.dirty.extend(self.panes.keys().copied());
            },
            ClientMessage::Ack { seq } => self.batches_acked = seq.min(self.batches_sent),
            ClientMessage::ForgetImages { ids } => {
                for id in ids {
                    self.images_sent.remove(&id);
                }
            },
            ClientMessage::Ping { id } => {
                let inbox = self.inbox.clone();
                self.shells.ping(Box::new(move || drop(inbox.send(Input::Pong(id)))));
            },
            // The files browser's: through the machine's files, off this thread.
            ClientMessage::Query {
                id,
                query:
                    query @ (Query::ListDir { .. } | Query::ReadFile { .. } | Query::Upload { .. }),
            } if self.files.is_some() => {
                let files = self.files.clone().expect("there");
                let cwd = match &query {
                    Query::ListDir { pane, .. } => {
                        self.panes.get(pane).and_then(|p| p.session.cwd())
                    },
                    _ => None,
                };
                let cwd = cwd.map(|cwd| cwd.to_string_lossy().into_owned());
                let (inbox, uploads) = (self.inbox.clone(), self.uploads.clone());
                std::thread::spawn(move || {
                    let reply = crate::plain_files::answer(&*files, query, cwd, &uploads);
                    let _ = inbox.send(Input::Reply(id, reply));
                });
            },
            ClientMessage::Query { id, query } => {
                let reply = self.answer(query);
                self.send(ServerMessage::Reply { id, reply });
            },
            // Splits, workspaces, agents and the rest: an artty server's.
            _ => {},
        }
    }

    fn answer(&self, query: Query) -> Reply {
        const NO_FILES: &str = "plain ssh: its files can't be reached (no SFTP there)";
        match query {
            Query::AllWindows | Query::DetachedWindows => Reply::Windows(vec![WindowSummary {
                window: WINDOW,
                tabs: self.tabs.len(),
                bounds: None,
                apps: usize::from(self.attached),
                titles: self.tabs.iter().map(|t| self.tab_title(t)).collect(),
            }]),
            Query::LinkSpan { pane, line, column } => Reply::Link(
                self.panes
                    .get(&pane)
                    .and_then(|p| p.session.link_span(line, column))
                    .map(|(url, span)| (url, span.start, span.end)),
            ),
            Query::SelectionText { pane } => {
                Reply::Text(self.panes.get(&pane).and_then(|p| p.session.selection_text()))
            },
            Query::Cwd { pane } => Reply::Path(self.panes.get(&pane).and_then(|p| p.session.cwd())),
            Query::CommandOutput { pane } => {
                Reply::Output(self.panes.get(&pane).and_then(|p| p.session.command_output()))
            },
            Query::SearchOutput { text } => Reply::Found(self.search_output(&text)),
            Query::SearchSessions { .. } => Reply::Sessions(Vec::new()),
            Query::ListDir { .. } => Reply::Dir(Err(NO_FILES.into())),
            Query::ReadFile { .. } => Reply::File(Err(NO_FILES.into())),
            Query::Upload { .. } => Reply::Upload(Err(UploadError::Failed(NO_FILES.into()))),
        }
    }

    /// Lines with `text` in every tab's output, as the server finds them.
    fn search_output(&self, text: &str) -> Vec<OutputHit> {
        const PER_PANE: usize = 20;
        const ALL: usize = 300;
        if text.trim().is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for tab in &self.tabs {
            let Some(state) = self.panes.get(&tab.pane) else { continue };
            let left = ALL.saturating_sub(hits.len()).min(PER_PANE);
            for (line, column, length, text) in state.session.find_lines(text, left) {
                hits.push(OutputHit { pane: tab.pane, line, column, length, text });
            }
        }
        hits
    }

    /// A shell in a new tab, after the others, shown.
    /// A name for a new tab's session there (if shells are kept): the first
    /// `artty-N` not taken.
    fn new_session(&self) -> Option<String> {
        if !self.shells.keeps() {
            return None;
        }
        let taken: HashSet<&str> = self.tabs.iter().filter_map(|t| t.session.as_deref()).collect();
        (1..).map(|n| format!("{SESSION_PREFIX}{n}")).find(|name| !taken.contains(name.as_str()))
    }

    fn open_tab(&mut self, kept: Option<String>) {
        let id = self.next_pane;
        self.next_pane += 1;
        let inbox = self.inbox.clone();
        let sink = Arc::new(move |id, event| drop(inbox.send(Input::Pane(id, event))));
        let (sizes, resized) = tokio::sync::mpsc::unbounded_channel();
        let resize = Box::new(move |size| {
            let _ = sizes.send(size);
        });
        let options = SessionOptions {
            cursor_shape: self.cursor.shape(),
            scrollback: Some(self.scrollback),
            ..Default::default()
        };
        match Session::remote(id, options, self.size, self.theme.clone(), sink, resize) {
            Ok((session, ends)) => {
                self.shells.open(self.size, ends, resized, kept.clone());
                let pane = Pane {
                    session,
                    opened: Instant::now(),
                    title: String::new(),
                    busy: false,
                    progress: None,
                };
                self.panes.insert(id, pane);
                self.tabs.push(Tab { pane: id, title: None, session: kept });
                self.active = self.tabs.len() - 1;
            },
            Err(err) => log::error!("no terminal for a shell: {err}"),
        }
    }

    /// Close a tab: its shell ends. The last one closes the window.
    fn close_tab(&mut self, index: usize) {
        let tab = self.tabs.remove(index);
        // Its session there ends too (gone already, if its shell exited).
        if let Some(session) = &tab.session {
            self.shells.end(session);
        }
        // Dropped, its terminal lets go of the channel, which ends the shell.
        self.panes.remove(&tab.pane);
        self.sent.remove(&tab.pane);
        self.dirty.remove(&tab.pane);
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.tabs.is_empty() && self.attached {
            self.attached = false;
            self.send(ServerMessage::WindowClosed { window: WINDOW });
        }
    }

    fn tab_title(&self, tab: &Tab) -> String {
        tab.title
            .clone()
            .or_else(|| Some(self.panes.get(&tab.pane)?.title.clone()))
            .unwrap_or_default()
    }

    fn layout(&self) -> WindowLayout {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| TabLayout {
                title: tab.title.clone(),
                root: Node::Pane(tab.pane),
                focused: tab.pane,
                zoomed: false,
                broadcast: false,
            })
            .collect();
        WindowLayout {
            workspaces: vec![WorkspaceLayout {
                id: WORKSPACE,
                name: None,
                title: self.name.clone(),
                color: None,
                root: None,
                tabs,
                active: self.active,
                collapsed: false,
                cpu_percent: 0.,
                memory_bytes: 0,
            }],
            active: 0,
            hidden: 0,
        }
    }

    fn pane_states(&self) -> Vec<PaneState> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let pane = self.panes.get(&tab.pane)?;
                let info = TabInfo {
                    id: tab.pane,
                    window: 1,
                    workspace: WORKSPACE as usize,
                    workspace_name: None,
                    tab: index + 1,
                    title: pane.title.clone(),
                    active: index == self.active,
                    focused: true,
                    cwd: pane.session.cwd(),
                    process: None,
                    status: if pane.busy { "running" } else { "idle" }.to_owned(),
                    detail: None,
                    needs_attention: false,
                    shell_integration: pane.session.shell_integration(),
                    kitty_keyboard: false,
                };
                let icon = if pane.busy { "busy" } else { "" };
                Some(PaneState {
                    info,
                    icon: icon.to_owned(),
                    progress: pane.progress,
                    agent: None,
                    ports: Vec::new(),
                })
            })
            .collect()
    }

    /// Send the app what changed: the layout, the panes' states, the shown
    /// pane's screen (and its images, each once).
    fn push(&mut self) {
        if !self.attached {
            return;
        }
        let layout = self.layout();
        if self.layout_sent.as_ref() != Some(&layout) {
            self.layout_sent = Some(layout.clone());
            self.send(ServerMessage::Layout { window: WINDOW, layout });
        }
        let states = self.pane_states();
        if states != self.panes_sent {
            self.panes_sent = states.clone();
            self.send(ServerMessage::Panes { panes: states });
        }
        if !self.due() || self.pushed.elapsed() < FRAME_INTERVAL {
            return;
        }
        let Some(pane) = self.shown() else { return };
        let Some(state) = self.panes.get_mut(&pane) else { return };
        let frame = state.session.frame();
        let mode = state.session.mode().bits();
        let mut messages = Vec::new();
        for image in &frame.images {
            if self.images_sent.insert(image.id) {
                messages.push(ServerMessage::Image(ImageData {
                    id: image.id,
                    format: image.format,
                    data: image.data.to_vec(),
                    width: image.width,
                    height: image.height,
                    columns: image.columns,
                    rows: image.rows,
                }));
            }
        }
        let update = artty_core::protocol::FrameUpdate::diff(self.sent.get(&pane), &frame, mode);
        messages.push(ServerMessage::Frame { pane, update });
        self.sent.insert(pane, frame);
        self.dirty.remove(&pane);
        self.batches_sent += 1;
        messages.push(ServerMessage::Sync { seq: self.batches_sent });
        self.pushed = Instant::now();
        for message in messages {
            self.send(message);
        }
    }
}

/// Write `text` to a shell's terminal and end it: why it couldn't start.
pub fn refuse(ends: RemoteEnds, text: &str) {
    let RemoteEnds { mut stream, mut ended } = ends;
    let _ = stream.write_all(format!("\r\n{text}\r\n").as_bytes());
    let _ = ended.write_all(&[1]);
}
