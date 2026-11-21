//! The server's state: windows of workspaces of tabs of panes, the apps
//! attached to them, and everything that happens to the sessions.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{
    AgentInfo, AgentState, AskState, Decision, PaneProcesses, ProcessRow, ReadSource, Request,
    Response, SESSION_ENV, SOCKET_ENV, ServerInfo, ServerUsage, SplitDirection, TabInfo,
    ThreadItem, WorkspaceInfo,
};
use artty_core::layout::{Axis, Node, PaneId};
use artty_core::pane::{Pane, Status};
use artty_core::protocol::{
    AppCommand, ClientMessage, DirEntry, Existing, FileChunk, Listing, OutputHit, PaneSpec,
    PaneState, Query, QueueItem, QueueKind, QueueUpdate, Reply, SelectionOp, ServerExtension,
    ServerMessage, StatusLine, StatusUpdate, TabLayout, UploadError, UploadTo, Uploaded,
    WORKSPACE_COLORS, WindowId, WindowLayout, WindowSummary, WorkspaceId, WorkspaceLayout,
    extension,
};
use artty_core::vt::ansi::Progress;
use artty_core::vt::event::Event;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::term::TermMode;
use artty_core::{
    Config, Frame, Notice, SavedLayout, SavedPane, SavedState, SavedTab, SavedWindow,
    SavedWindowState, SavedWorkspace, Session, SessionOptions, ShellPhase, TerminalSize, Theme,
    process_tree, processes,
};

use crate::handover::{self, HandedPane};
use crate::inbox::{Inbox, PaneEvents};
use crate::{ClientId, Input, Peer};

/// Each visible pane gets at most one screen update per frame at 120 Hz.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// Batches of screen updates an app may have unacknowledged; more wait, and
/// it gets the newest screens once it catches up.
const FRAMES_IN_FLIGHT: u64 = 2;
/// Used within this long, an app is the one its user is at: notifications go to it alone.
const IN_USE: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// An app silent this long (it pings every 5 s) is taken as gone.
const CLIENT_SILENCE: Duration = Duration::from_secs(60);
/// How often the socket is marked as used, for /tmp cleaners (which leave
/// what's newer than days).
const TOUCH_INTERVAL: Duration = Duration::from_secs(3600);
/// A server with no sessions and no app exits after this long.
const IDLE_EXIT: Duration = Duration::from_secs(20);
/// How much CPU and memory history each pane keeps, one sample a second.
const HISTORY: usize = 60;
/// Background commands running at least this long notify when they finish.
const LONG_COMMAND: Duration = Duration::from_secs(10);
/// Shown under a restored pane's old output.
const RESTORED_BANNER: &str = "\r\n\x1b[2m── restored ──\x1b[0m\r\n";
/// Closed tabs kept for reopening.
const CLOSED_TABS: usize = 10;
const DEFAULT_SIZE: TerminalSize =
    TerminalSize { columns: 100, lines: 30, cell_width: 8, cell_height: 18 };

struct Tab {
    title: Option<String>,
    root: Node,
    focused: PaneId,
    zoomed: bool,
    /// Typing goes to every pane.
    broadcast: bool,
}

impl Tab {
    fn new(pane: PaneId) -> Self {
        Self { title: None, root: Node::Pane(pane), focused: pane, zoomed: false, broadcast: false }
    }

    fn visible(&self) -> Vec<PaneId> {
        if self.zoomed { vec![self.focused] } else { self.root.panes() }
    }
}

/// What a workspace gives the panes started in it.
#[derive(Clone, Debug, Default)]
struct Defaults {
    /// New tabs start here, unless the pane they're opened from is inside it.
    root: Option<PathBuf>,
    env: BTreeMap<String, String>,
}

/// A named group of tabs; never empty (it closes with its last tab).
struct Workspace {
    name: Option<String>,
    tabs: Vec<Tab>,
    active: usize,
    collapsed: bool,
    defaults: Defaults,
    color: Option<String>,
    /// Its name when it has none: its git repository or directory.
    title: String,
    /// CPU percent and memory of its panes, as of the last sample.
    usage: (f32, u64),
}

impl Workspace {
    fn new(name: Option<String>, tabs: Vec<Tab>, active: usize) -> Self {
        let active = active.min(tabs.len().saturating_sub(1));
        Self {
            name: name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
            tabs,
            active,
            collapsed: false,
            defaults: Defaults::default(),
            color: None,
            title: String::new(),
            usage: (0., 0),
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn layout(&self, id: WorkspaceId) -> WorkspaceLayout {
        WorkspaceLayout {
            id,
            name: self.name.clone(),
            title: self.title.clone(),
            color: self.color.clone(),
            root: self.defaults.root.clone(),
            tabs: self
                .tabs
                .iter()
                .map(|tab| TabLayout {
                    title: tab.title.clone(),
                    root: tab.root.clone(),
                    focused: tab.focused,
                    zoomed: tab.zoomed,
                    broadcast: tab.broadcast,
                })
                .collect(),
            active: self.active,
            collapsed: self.collapsed,
            cpu_percent: self.usage.0,
            memory_bytes: self.usage.1,
        }
    }
}

struct Window {
    /// Its workspaces, in sidebar order; never empty.
    workspaces: Vec<WorkspaceId>,
    /// The workspace whose active tab the window shows.
    active: usize,
    bounds: Option<SavedWindow>,
    /// The apps showing this window (a Mac's, a phone's), the one that last
    /// used it last: its panes take that one's sizes.
    clients: Vec<ClientId>,
    /// The apps it has focus in.
    focused: Vec<ClientId>,
    /// When it last had focus, to find the frontmost window.
    last_focus: Instant,
}

impl Window {
    fn active_workspace(&self) -> Option<WorkspaceId> {
        self.workspaces.get(self.active).copied()
    }

    fn shown(&self) -> bool {
        !self.clients.is_empty()
    }

    /// The app that last used it, whose sizes its panes take (and which
    /// gets what's for the user: a bell, the clipboard, being brought
    /// forward).
    fn driver(&self) -> Option<ClientId> {
        self.clients.last().copied()
    }

    fn focused(&self) -> bool {
        !self.focused.is_empty()
    }

    /// `client` lets it go.
    fn remove_client(&mut self, client: ClientId) {
        self.clients.retain(|&c| c != client);
        self.focused.retain(|&c| c != client);
    }
}

struct Client {
    tx: Sender<ServerMessage>,
    /// Its process at the other end of the socket (for `artty attach`, or
    /// `artty server --stdio` relaying a remote app), if the system says.
    pid: Option<i32>,
    /// The queue as last sent (apps taking the `queue` extension).
    queue_sent: Option<QueueUpdate>,
    /// Last screen sent per visible pane, for sending only changes.
    sent: HashMap<PaneId, (Frame, Instant)>,
    dirty: HashSet<PaneId>,
    panes_sent: Vec<PaneState>,
    /// Its windows' status lines as last sent (none: empty).
    status_sent: HashMap<WindowId, StatusLine>,
    /// Images it has (by id), so each is sent once.
    images_sent: HashSet<u64>,
    /// Batches of screen updates sent, and acknowledged.
    batches_sent: u64,
    batches_acked: u64,
    /// When it last said anything; and whether it pings (apps do, every
    /// few seconds), so its silence means it's gone.
    last_heard: Instant,
    pings: bool,
    /// When its user last typed, scrolled or brought a window of it forward.
    last_used: Option<Instant>,
    /// The extension kinds it reads (its `Hello`'s).
    extensions: Vec<String>,
    /// A pane whose screens it wants though no window of it shows it
    /// (`peek`).
    peek: Option<PaneId>,
}

impl Client {
    /// It reads extension `kind`: only then is it sent (or worked out).
    fn takes(&self, kind: &str) -> bool {
        self.extensions.iter().any(|k| k == kind)
    }

    /// It's behind on screen updates: send it no more for now.
    fn behind(&self) -> bool {
        self.batches_sent - self.batches_acked >= FRAMES_IN_FLIGHT
    }
}

/// Who's waiting for `artty run` to finish.
enum Waiter {
    Command(Sender<Response>),
    App(ClientId, u64),
}

struct PendingRun {
    pane: PaneId,
    waiter: Waiter,
    deadline: Option<Instant>,
}

/// Who's waiting for an ask to be over; at `deadline`, they're told how
/// far it got (it carries on).
/// Someone waiting for asks to be over: `asks[0]` (`artty ask --result`),
/// or the first of `asks` (`all`: every one) for `artty wait`.
struct AskWaiter {
    asks: Vec<u64>,
    all: bool,
    /// Answer with every ask's state (`artty wait`), not one's.
    many: bool,
    waiter: Waiter,
    deadline: Option<Instant>,
}

impl AskWaiter {
    fn one(ask: u64, waiter: Waiter, deadline: Option<Instant>) -> Self {
        AskWaiter { asks: vec![ask], all: false, many: false, waiter, deadline }
    }

    fn over(&self, asks: &crate::asks::Asks) -> bool {
        match self.all {
            true => self.asks.iter().all(|&id| asks.finished(id)),
            false => self.asks.iter().any(|&id| asks.finished(id)),
        }
    }
}

/// The prompts a Claude Code conversation still has, once its transcript's
/// read (see `Server::check_rewinds`).
type KeptPrompts = std::sync::mpsc::Receiver<Option<HashSet<String>>>;

/// Someone waiting for thread entries after `after` (`artty events`).
struct EventWaiter {
    after: u64,
    tab: Option<PaneId>,
    waiter: Waiter,
    deadline: Option<Instant>,
}

pub(crate) struct Server {
    socket: PathBuf,
    sink: Arc<dyn Fn(PaneId, Event) + Send + Sync>,
    started: Instant,
    config: Config,
    /// The cursor panes get unless a program picks one: the config's, or the
    /// app's (see `appearance`).
    cursor: artty_core::CursorStyle,
    /// An app sent its colors and cursor: they win over the config file's.
    appearance: bool,
    config_modified: Option<SystemTime>,
    theme: Arc<Theme>,
    agents: Arc<[String]>,
    cli_dir: Option<PathBuf>,
    integration_dir: Option<PathBuf>,
    panes: HashMap<PaneId, Pane>,
    windows: BTreeMap<WindowId, Window>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    clients: HashMap<ClientId, Client>,
    runs: Vec<PendingRun>,
    /// `artty ask`s, and who waits for them.
    asks: crate::asks::Asks,
    ask_waiters: Vec<AskWaiter>,
    event_waiters: Vec<EventWaiter>,
    /// Agents' permission requests, and their hooks waiting for answers.
    approvals: crate::approvals::Approvals,
    approval_waiters: HashMap<crate::approvals::Conn, Waiter>,
    /// Agents' replies not yet read, for the queue.
    replies: crate::queue::Replies,
    /// Since when each pane's agent has waited (Unix ms), for the queue.
    waiting_since: HashMap<PaneId, u64>,
    /// Each agent's thread.
    threads: crate::threads::Threads,
    /// Files agents change: each turn's, and who changed which lately.
    edits: crate::edits::Edits,
    /// Claude Code conversations being read for what was rewound (see
    /// `check_rewinds`): pane, the turn's prompt, and the prompts kept.
    rewinds: Vec<(PaneId, String, KeptPrompts)>,
    /// The pane each agent was started from by another (`artty new
    /// --task` from inside a pane).
    parents: HashMap<PaneId, PaneId>,
    /// The pane whose agent's conversation each copy started from outside
    /// a pane copies (listed under it; not started by it, so not counted
    /// in how deep agents start agents).
    copied_from: HashMap<PaneId, PaneId>,
    /// The worktrees tabs were opened in (`artty new --worktree`), removed
    /// with them if they have no changes.
    worktrees: HashMap<PaneId, crate::worktrees::Worktree>,
    next_pane: PaneId,
    next_window: WindowId,
    next_workspace: WorkspaceId,
    had_panes: bool,
    idle_since: Instant,
    stopping: bool,
    /// Stopping, but save the sessions for the next server to reopen.
    restarting: bool,
    last_poll: Instant,
    last_save: Instant,
    /// When the socket was last marked as used ([`touch`]).
    last_touch: Option<Instant>,
    last_saved: Option<String>,
    /// The asks, and the unread replies, as last kept on disk (see `keep`).
    last_kept: Option<String>,
    last_kept_replies: Option<String>,
    last_kept_parents: Option<String>,
    /// Previous CPU samples (pid → time used, when), for CPU percentages.
    cpu_samples: HashMap<i32, (u64, Instant)>,
    /// Every pane's processes as of the last sample, with history.
    usage: Vec<PaneProcesses>,
    history: HashMap<PaneId, VecDeque<(f32, u64)>>,
    /// The TCP ports each pane's programs listen on, as of the last sample.
    ports: HashMap<PaneId, Vec<u16>>,
    /// Folders apps' files drawers show.
    watches: crate::watch::Watches,
    /// Files being sent by apps, by app and upload.
    uploads: HashMap<(ClientId, u64), Upload>,
    server_usage: Option<ServerUsage>,
    /// The listening socket's descriptor, kept open across a hand-over.
    listener_fd: i32,
    /// Asked to hand over to this build, after answering.
    handover_to: Option<PathBuf>,
    /// Panes taken over from a previous build, until `restored`.
    adopted: Vec<PaneId>,
    /// Panes a previous build handed over, until their layout places them.
    handed: HashMap<PaneId, (HandedPane, Option<artty_core::vt::term::TermSnapshot>)>,
    /// Tabs closed lately, newest last, for reopening.
    closed: Vec<ClosedTab>,
    /// Each app's size for each pane it shows (see [`Window::driver`]).
    sizes: HashMap<(ClientId, PaneId), TerminalSize>,
    /// Windows' status lines, and what goes into them.
    status: crate::status::Status,
}

/// A closed tab as saved, and where it was.
struct ClosedTab {
    workspace: WorkspaceId,
    index: usize,
    tab: SavedTab,
    defaults: Defaults,
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

/// Put an `artty` command on the PATH of shells: a directory holding only a
/// link to this executable.
fn cli_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let link = |dir: &Path| {
        let link = dir.join("artty");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&exe, &link)
    };
    // Panes started by servers from before have /tmp/artty-bin on their
    // PATH (on Linux): keep it leading to this build, if it's ours.
    let legacy = std::env::temp_dir().join("artty-bin");
    if !cfg!(target_os = "macos") && owned(&legacy) {
        let _ = link(&legacy);
    }
    let dir = artty_core::control::private_dir().join("artty-bin");
    std::fs::create_dir_all(&dir).ok()?;
    link(&dir).ok()?;
    Some(dir)
}

/// Whether `path` is this user's (and not a link to something).
fn owned(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == uid)
}

/// Mark `path` as used now, so cleaners of old files in /tmp leave it.
fn touch(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
    // SAFETY: a valid C string; null times mean now.
    unsafe { libc::utimes(path.as_ptr(), std::ptr::null()) };
}

/// A workspace's automatic name for a directory: its git repository's, else
/// its own (`~` for home).
fn directory_title(dir: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let repository = dir
        .ancestors()
        .take_while(|d| Some(*d) != home.as_deref())
        .find(|d| d.join(".git").exists());
    match repository.or(Some(dir)) {
        Some(d) if Some(d) == home.as_deref() => "~".to_owned(),
        Some(d) => d
            .file_name()
            .map_or_else(|| d.display().to_string(), |n| n.to_string_lossy().into_owned()),
        None => String::new(),
    }
}

/// A color name artty knows, or `none`/empty for none.
fn workspace_color(color: &str) -> Result<Option<String>, String> {
    let color = color.trim().to_lowercase();
    match color.as_str() {
        "" | "none" => Ok(None),
        c if WORKSPACE_COLORS.contains(&c) => Ok(Some(color)),
        c => Err(format!("unknown color {c} (one of {}, or none)", WORKSPACE_COLORS.join(", "))),
    }
}

fn osascript_notify(title: &str, body: &str) {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script =
        format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// A file an app is sending: arriving at `part`, for `dir` as `name`.
struct Upload {
    part: PathBuf,
    dir: PathBuf,
    name: std::ffi::OsString,
    file: std::fs::File,
    place: Place,
}

/// Where a file sent takes its place once whole.
enum Place {
    /// Its name, else `name (1)`... beside a file of it.
    Beside,
    /// Its name, replacing a file of it (if, given, that's unchanged since
    /// this modification time).
    Instead(Option<u64>),
}

/// A file's modification time, in nanoseconds since the epoch (to tell
/// changes apart within a second).
fn modified_time(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_nanos() as u64)
}

/// `name` in `dir`, or, if that's taken, `name (1)`, `name (2)`... (before
/// its extension).
fn free_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let name = Path::new(name);
    let stem = name.file_stem().unwrap_or(name.as_os_str()).to_string_lossy();
    let extension = name.extension().map(|e| format!(".{}", e.to_string_lossy()));
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){}", extension.as_deref().unwrap_or(""))))
        .find(|path| !path.exists())
        .expect("some number is free")
}

/// The most entries a folder's listing has.
const MAX_LISTED: usize = 5000;

/// The largest part of a file read at once ([`Query::ReadFile`]).
const FILE_CHUNK: usize = 256 * 1024;

/// Uploads are kept for a week.
const UPLOADS_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Where files apps send go (in a folder of their own each).
fn uploads_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None if cfg!(target_os = "macos") => home.join("Library/Caches"),
        None => home.join(".cache"),
    };
    Some(base.join("artty").join("uploads"))
}

/// Forget uploads older than [`UPLOADS_KEPT`].
fn prune_uploads(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > UPLOADS_KEPT));
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Part of a file, from `offset`, with its length and modification time.
fn read_file(path: &Path, offset: u64) -> Result<FileChunk, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let metadata = file.metadata().map_err(|err| err.to_string())?;
    if !metadata.is_file() {
        return Err("not a file".into());
    }
    file.seek(SeekFrom::Start(offset)).map_err(|err| err.to_string())?;
    let mut data = Vec::with_capacity(FILE_CHUNK);
    file.take(FILE_CHUNK as u64).read_to_end(&mut data).map_err(|err| err.to_string())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    Ok(FileChunk { data, length: metadata.len(), modified })
}

fn pbcopy(text: &str) {
    use std::io::Write;
    if let Ok(mut child) =
        std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn pbpaste() -> String {
    std::process::Command::new("pbpaste")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

impl Server {
    pub(crate) fn new(socket: PathBuf, inbox: Arc<Inbox>, listener_fd: i32) -> Self {
        let (config, error) = Config::load();
        if let Some(error) = error {
            log::error!("config: {error}");
        }
        let watches = crate::watch::Watches::new(inbox.clone());
        let status = {
            let inbox = inbox.clone();
            crate::status::Status::new(
                &config.status,
                Arc::new(move |job, result| {
                    let _ = inbox.send(Input::Status(job, result));
                }),
            )
        };
        let sink: Arc<dyn Fn(PaneId, Event) + Send + Sync> =
            Arc::new(move |id, event| inbox.pane_event(id, event));
        let now = Instant::now();
        Self {
            socket,
            sink,
            started: now,
            theme: Arc::new(config.theme()),
            cursor: config.cursor_style,
            appearance: false,
            agents: config.agents.clone().into(),
            config,
            config_modified: config_modified(),
            cli_dir: cli_dir(),
            integration_dir: artty_core::integration::install().ok(),
            panes: HashMap::new(),
            windows: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            clients: HashMap::new(),
            runs: Vec::new(),
            asks: crate::asks::Asks::new(),
            ask_waiters: Vec::new(),
            event_waiters: Vec::new(),
            approvals: crate::approvals::Approvals::new(),
            approval_waiters: HashMap::new(),
            replies: crate::queue::Replies::new(),
            waiting_since: HashMap::new(),
            threads: crate::threads::Threads::new(crate::store::Store::open()),
            edits: crate::edits::Edits::default(),
            parents: HashMap::new(),
            copied_from: HashMap::new(),
            rewinds: Vec::new(),
            worktrees: HashMap::new(),
            next_pane: 1,
            next_window: 1,
            sizes: HashMap::new(),
            next_workspace: 1,
            had_panes: false,
            idle_since: now,
            stopping: false,
            restarting: false,
            last_poll: now,
            last_save: now,
            last_touch: None,
            last_saved: None,
            last_kept: None,
            last_kept_replies: None,
            last_kept_parents: None,
            cpu_samples: HashMap::new(),
            usage: Vec::new(),
            history: HashMap::new(),
            uploads: HashMap::new(),
            ports: HashMap::new(),
            watches,
            server_usage: None,
            listener_fd,
            handover_to: None,
            handed: HashMap::new(),
            adopted: Vec::new(),
            closed: Vec::new(),
            status,
        }
    }

    /// Reopen saved windows (detached until the app attaches).
    pub(crate) fn start(&mut self) {
        if !self.config.restore_sessions {
            return;
        }
        let Some(mut state) = SavedState::load() else { return };
        let hidden = std::mem::take(&mut state.hidden);
        for saved in state.into_windows() {
            let workspaces: Vec<Workspace> =
                saved.workspaces.into_iter().filter_map(|w| self.restore_workspace(w)).collect();
            if !workspaces.is_empty() {
                self.add_window(workspaces, saved.active, saved.window);
            }
        }
        for saved in hidden {
            if let Some(workspace) = self.restore_workspace(saved) {
                self.add_workspace(workspace);
            }
        }
    }

    fn restore_workspace(&mut self, saved: SavedWorkspace) -> Option<Workspace> {
        let defaults = Defaults { root: saved.root, env: saved.env };
        let mut tabs = Vec::new();
        for tab in saved.tabs {
            let Some(root) = self.restore_layout(tab.layout, &defaults) else { continue };
            let panes = root.panes();
            let focused = panes[tab.focused.min(panes.len() - 1)];
            tabs.push(Tab {
                title: tab.title,
                root,
                focused,
                zoomed: tab.zoomed && panes.len() > 1,
                broadcast: false,
            });
        }
        if tabs.is_empty() {
            return None;
        }
        let mut workspace = Workspace::new(saved.name, tabs, saved.active);
        workspace.collapsed = saved.collapsed;
        workspace.color = saved.color.and_then(|c| workspace_color(&c).ok().flatten());
        workspace.defaults = defaults;
        Some(workspace)
    }

    fn add_workspace(&mut self, workspace: Workspace) -> WorkspaceId {
        let id = self.next_workspace;
        self.next_workspace += 1;
        self.workspaces.insert(id, workspace);
        id
    }

    fn add_window(
        &mut self,
        workspaces: Vec<Workspace>,
        active: usize,
        bounds: Option<SavedWindow>,
    ) -> WindowId {
        let workspaces: Vec<WorkspaceId> =
            workspaces.into_iter().map(|w| self.add_workspace(w)).collect();
        self.insert_window(workspaces, active, bounds)
    }

    /// A window of existing workspaces, detached until an app shows it.
    fn insert_window(
        &mut self,
        workspaces: Vec<WorkspaceId>,
        active: usize,
        bounds: Option<SavedWindow>,
    ) -> WindowId {
        let active = active.min(workspaces.len().saturating_sub(1));
        let id = self.next_window;
        self.next_window += 1;
        self.windows.insert(
            id,
            Window {
                workspaces,
                active,
                bounds,
                clients: Vec::new(),
                focused: Vec::new(),
                last_focus: Instant::now(),
            },
        );
        id
    }

    /// A window with one workspace of one tab.
    fn add_window_with(&mut self, pane: PaneId, bounds: Option<SavedWindow>) -> WindowId {
        let workspace = Workspace::new(None, vec![Tab::new(pane)], 0);
        self.add_window(vec![workspace], 0, bounds)
    }

    fn restore_layout(&mut self, layout: SavedLayout, defaults: &Defaults) -> Option<Node> {
        match layout {
            SavedLayout::Pane(saved) => {
                let thread = saved.thread;
                let take_thread = |server: &mut Self, id: PaneId| {
                    if let Some(key) = thread {
                        server.threads.adopt(id, key);
                    }
                    id
                };
                if let Some((handed, snapshot)) = saved.pane.and_then(|id| self.handed.remove(&id))
                {
                    let id = self.adopt_pane(handed, snapshot, saved)?;
                    self.adopted.push(id);
                    return Some(Node::Pane(take_thread(self, id)));
                }
                let initial_output = (!saved.scrollback.is_empty())
                    .then(|| format!("{}{RESTORED_BANNER}", saved.scrollback).into_bytes());
                let spec = PaneSpec { command: saved.command, cwd: saved.cwd, title: saved.title };
                let id = self
                    .spawn_pane(spec, None, initial_output, defaults)
                    .inspect_err(|err| log::error!("restoring: {err}"))
                    .ok()?;
                let resume = saved.agent.filter(|_| self.config.resume_agents).and_then(|agent| {
                    self.config.resume.get(&agent.name)?.command(agent.session.as_deref())
                });
                if let (Some(resume), Some(pane)) = (resume, self.panes.get_mut(&id)) {
                    pane.type_when_ready(resume);
                }
                Some(Node::Pane(take_thread(self, id)))
            },
            SavedLayout::Split { vertical, ratio, first, second } => {
                match (
                    self.restore_layout(*first, defaults),
                    self.restore_layout(*second, defaults),
                ) {
                    (Some(first), Some(second)) => Some(Node::Split {
                        axis: if vertical { Axis::Vertical } else { Axis::Horizontal },
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            },
        }
    }

    // Panes, tabs, windows.

    /// Start a session for a new pane (not yet placed in a tab), with its
    /// workspace's defaults. It starts in `spec.cwd`, else in `near`'s
    /// directory if that's inside the workspace's root, else in the root.
    fn spawn_pane(
        &mut self,
        spec: PaneSpec,
        near: Option<PaneId>,
        initial_output: Option<Vec<u8>>,
        defaults: &Defaults,
    ) -> Result<PaneId, String> {
        let id = self.next_pane;
        self.next_pane += 1;
        let explicit = spec.command.clone().filter(|c| !c.is_empty());
        let mut command =
            explicit.clone().or_else(|| Some(self.config.shell.clone())).filter(|c| !c.is_empty());
        let near_cwd = near.and_then(|near| self.panes.get(&near)?.session.cwd());
        let cwd = spec.cwd.or(match (&defaults.root, near_cwd) {
            (Some(root), Some(dir)) if dir.starts_with(root) => Some(dir),
            (Some(root), _) => Some(root.clone()),
            (None, dir) => dir,
        });

        let mut env: HashMap<String, String> =
            defaults.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if self.config.shell_integration
            && let Some(dir) = &self.integration_dir
            && let Some(injection) = artty_core::integration::inject(dir, command.as_deref())
        {
            env.extend(injection.env);
            command = injection.command.or(command);
        }
        env.insert(SESSION_ENV.to_owned(), id.to_string());
        env.insert(SOCKET_ENV.to_owned(), self.socket.display().to_string());
        if let Some(dir) = &self.cli_dir {
            let path = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".to_owned(), format!("{}:{path}", dir.display()));
            // Off a Mac, web pages programs open (logins, docs) open on the
            // app's machine, unless a browser is set.
            // (Programs split it into words.)
            let artty = dir.join("artty").display().to_string();
            if !cfg!(target_os = "macos")
                && std::env::var_os("BROWSER").is_none()
                && !artty.contains(char::is_whitespace)
            {
                env.insert("BROWSER".to_owned(), format!("{artty} open"));
            }
        }
        let size =
            near.and_then(|p| self.panes.get(&p)).map_or(DEFAULT_SIZE, |pane| pane.session.size());
        let options = SessionOptions {
            command: command.map(|mut c| (c.remove(0), c)),
            working_directory: cwd.filter(|dir| dir.is_dir()),
            env,
            scrollback: Some(self.config.scrollback),
            initial_output,
            cursor_shape: self.cursor.shape(),
        };
        let session = Session::spawn(id, options, size, self.theme.clone(), self.sink.clone())
            .map_err(|err| format!("failed to start: {err}"))?;
        self.panes.insert(id, Pane::new(session, spec.title, explicit, self.agents.clone()));
        self.had_panes = true;
        Ok(id)
    }

    /// The workspace and tab holding a pane.
    fn place_of(&self, pane: PaneId) -> Option<(WorkspaceId, usize)> {
        self.workspaces.iter().find_map(|(&id, workspace)| {
            workspace.tabs.iter().position(|tab| tab.root.contains(pane)).map(|tab| (id, tab))
        })
    }

    /// The window showing a workspace, and the workspace's place in it.
    fn window_of_workspace(&self, workspace: WorkspaceId) -> Option<(WindowId, usize)> {
        self.windows.iter().find_map(|(&id, window)| {
            window.workspaces.iter().position(|&w| w == workspace).map(|i| (id, i))
        })
    }

    fn window_of(&self, pane: PaneId) -> Option<WindowId> {
        self.window_of_workspace(self.place_of(pane)?.0).map(|(window, _)| window)
    }

    fn active_tab(&self, window: WindowId) -> Option<&Tab> {
        self.workspaces.get(&self.windows.get(&window)?.active_workspace()?)?.active_tab()
    }

    fn active_tab_mut(&mut self, window: WindowId) -> Option<&mut Tab> {
        let workspace = self.workspaces.get_mut(&self.windows.get(&window)?.active_workspace()?)?;
        workspace.tabs.get_mut(workspace.active)
    }

    fn window_visible(&self, window: WindowId) -> Vec<PaneId> {
        self.active_tab(window).map(Tab::visible).unwrap_or_default()
    }

    fn window_focused_pane(&self, window: WindowId) -> Option<PaneId> {
        self.active_tab(window).map(|tab| tab.focused)
    }

    fn window_layout(&self, window: &Window) -> WindowLayout {
        WindowLayout {
            workspaces: window
                .workspaces
                .iter()
                .filter_map(|&id| Some(self.workspaces.get(&id)?.layout(id)))
                .collect(),
            active: window.active,
            hidden: self.workspaces.keys().filter(|&&id| !self.on_screen(id)).count(),
        }
    }

    /// Whether a workspace is in a window the app shows.
    fn on_screen(&self, workspace: WorkspaceId) -> bool {
        self.window_of_workspace(workspace).is_some_and(|(window, _)| self.windows[&window].shown())
    }

    /// The window the user last used.
    fn frontmost(&self) -> Option<WindowId> {
        let attached = self.windows.iter().filter(|(_, w)| w.shown());
        attached
            .max_by_key(|(_, w)| w.last_focus)
            .or_else(|| self.windows.iter().max_by_key(|(_, w)| w.last_focus))
            .map(|(&id, _)| id)
    }

    fn focused_pane(&self) -> Option<PaneId> {
        self.window_focused_pane(self.frontmost()?)
    }

    /// Windows the user is looking at.
    fn seen_windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.windows.iter().filter(|(_, w)| w.focused()).map(|(&id, _)| id)
    }

    /// Whether the user can see a pane right now.
    fn visible(&self, pane: PaneId) -> bool {
        self.seen_windows().any(|w| self.window_visible(w).contains(&pane))
    }

    /// Show a workspace's tab in its window.
    fn select_tab(&mut self, workspace: WorkspaceId, tab: usize) {
        let Some(state) = self.workspaces.get_mut(&workspace).filter(|w| tab < w.tabs.len()) else {
            return;
        };
        state.active = tab;
        if let Some((window, index)) = self.window_of_workspace(workspace) {
            self.windows.get_mut(&window).unwrap().active = index;
        }
    }

    /// Open a tab at the end of `workspace`, or of the window's active one.
    fn open_tab(
        &mut self,
        window: WindowId,
        workspace: Option<WorkspaceId>,
        spec: PaneSpec,
        focus: bool,
    ) -> Result<PaneId, String> {
        let workspace = match workspace {
            Some(workspace) => workspace,
            None => self
                .windows
                .get(&window)
                .and_then(Window::active_workspace)
                .ok_or("no such window")?,
        };
        if !self.workspaces.contains_key(&workspace) {
            return Err("no such workspace".into());
        }
        let near = self.workspaces[&workspace]
            .active_tab()
            .map(|t| t.focused)
            .or_else(|| self.window_focused_pane(window));
        let defaults = self.workspaces[&workspace].defaults.clone();
        let id = self.spawn_pane(spec, near, None, &defaults)?;
        let state = self.workspaces.get_mut(&workspace).unwrap();
        state.tabs.push(Tab::new(id));
        let tab = state.tabs.len() - 1;
        if focus {
            self.select_tab(workspace, tab);
        }
        Ok(id)
    }

    /// A workspace with one tab, after the window's others.
    fn new_workspace(
        &mut self,
        window: WindowId,
        name: Option<String>,
        spec: PaneSpec,
        focus: bool,
    ) -> Result<(WorkspaceId, PaneId), String> {
        if !self.windows.contains_key(&window) {
            return Err("no such window".into());
        }
        let (workspace, pane) = self.create_workspace(name, spec)?;
        let state = self.windows.get_mut(&window).unwrap();
        state.workspaces.push(workspace);
        if focus {
            state.active = state.workspaces.len() - 1;
        }
        Ok((workspace, pane))
    }

    /// A workspace (in no window yet) with one tab. A directory given for
    /// it becomes its root; otherwise it starts in your home directory.
    fn create_workspace(
        &mut self,
        name: Option<String>,
        spec: PaneSpec,
    ) -> Result<(WorkspaceId, PaneId), String> {
        let defaults =
            Defaults { root: spec.cwd.clone().filter(|dir| dir.is_dir()), env: BTreeMap::new() };
        let spec = PaneSpec {
            cwd: spec.cwd.or_else(|| std::env::var_os("HOME").map(PathBuf::from)),
            ..spec
        };
        let pane = self.spawn_pane(spec, None, None, &defaults)?;
        let mut workspace = Workspace::new(name, vec![Tab::new(pane)], 0);
        workspace.defaults = defaults;
        let id = self.add_workspace(workspace);
        self.refresh_title(id);
        Ok((id, pane))
    }

    /// Work out a workspace's automatic name from its root, or from the
    /// directory its active tab is in.
    fn refresh_title(&mut self, id: WorkspaceId) {
        let Some(workspace) = self.workspaces.get(&id) else { return };
        let dir = workspace.defaults.root.clone().or_else(|| {
            let pane = workspace.active_tab()?.focused;
            self.panes.get(&pane)?.session.cwd()
        });
        let title = dir.map(|dir| directory_title(&dir)).unwrap_or_default();
        self.workspaces.get_mut(&id).unwrap().title = title;
    }

    fn split(
        &mut self,
        target: PaneId,
        axis: Axis,
        spec: PaneSpec,
        focus: bool,
    ) -> Result<PaneId, String> {
        let (workspace, tab) = self.place_of(target).ok_or_else(|| format!("no pane {target}"))?;
        let defaults = self.workspaces[&workspace].defaults.clone();
        let id = self.spawn_pane(spec, Some(target), None, &defaults)?;
        let tab_state = &mut self.workspaces.get_mut(&workspace).unwrap().tabs[tab];
        tab_state.root.split(target, id, axis);
        tab_state.zoomed = false;
        if focus {
            tab_state.focused = id;
            self.select_tab(workspace, tab);
        }
        Ok(id)
    }

    fn focus_pane(&mut self, pane: PaneId) {
        let Some((workspace, tab)) = self.place_of(pane) else { return };
        let before = self.focused_pane();
        self.select_tab(workspace, tab);
        let tab = &mut self.workspaces.get_mut(&workspace).unwrap().tabs[tab];
        if tab.zoomed && tab.focused != pane {
            tab.zoomed = false;
        }
        tab.focused = pane;
        self.report_focus_change(before);
    }

    /// Tell programs that track focus (vim, etc.) that it moved.
    fn report_focus_change(&self, before: Option<PaneId>) {
        let after = self.focused_pane();
        let window_focused =
            self.frontmost().and_then(|w| self.windows.get(&w)).is_some_and(Window::focused);
        if before != after && window_focused {
            self.report_focus(before, false);
            self.report_focus(after, true);
        }
    }

    fn report_focus(&self, pane: Option<PaneId>, focused: bool) {
        if let Some(pane) = pane.and_then(|id| self.panes.get(&id))
            && pane.session.mode().contains(TermMode::FOCUS_IN_OUT)
        {
            pane.session.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }

    /// Close a pane; its tab closes with its last pane, its workspace with
    /// its last tab, and its window with its last workspace.
    fn close_pane(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
        self.fail_runs(pane, "the pane closed");
        let Some((workspace, tab)) = self.place_of(pane) else { return };
        let tab_state = &mut self.workspaces.get_mut(&workspace).unwrap().tabs[tab];
        let order = tab_state.root.panes();
        let root = std::mem::replace(&mut tab_state.root, Node::Pane(pane));
        match root.without(pane) {
            Some(root) => {
                let remaining = root.panes();
                if tab_state.focused == pane {
                    let position = order.iter().position(|&p| p == pane).unwrap_or(0);
                    tab_state.focused = remaining[position.min(remaining.len() - 1)];
                }
                if remaining.len() == 1 {
                    tab_state.zoomed = false;
                }
                tab_state.root = root;
            },
            None => {
                self.take_tab(workspace, tab);
            },
        }
    }

    fn end_panes(&mut self, panes: Vec<PaneId>) {
        for pane in panes {
            self.panes.remove(&pane);
            self.fail_runs(pane, "the pane closed");
        }
    }

    fn close_tab(&mut self, workspace: WorkspaceId, tab: usize) {
        if let Some(tab) = self.take_tab(workspace, tab) {
            self.end_panes(tab.root.panes());
        }
    }

    /// Keep a tab about to close, to reopen: its layout, directories,
    /// recent output and agents, as a restart would bring them back.
    fn remember_closed(&mut self, workspace: WorkspaceId, index: usize) {
        let Some(state) = self.workspaces.get(&workspace) else { return };
        let Some(tab) = state.tabs.get(index) else { return };
        let saved = SavedTab {
            title: tab.title.clone(),
            layout: self.saved_layout(&tab.root, false),
            focused: tab.root.panes().iter().position(|&id| id == tab.focused).unwrap_or(0),
            zoomed: tab.zoomed,
        };
        let defaults = state.defaults.clone();
        if self.closed.len() == CLOSED_TABS {
            self.closed.remove(0);
        }
        self.closed.push(ClosedTab { workspace, index, tab: saved, defaults });
    }

    /// Bring back the tab closed last, where it was (or in the window's
    /// active workspace if that one is gone), and show it.
    fn reopen_closed_tab(&mut self, window: WindowId) {
        let Some(closed) = self.closed.pop() else { return };
        let workspace = match self.workspaces.contains_key(&closed.workspace) {
            true => closed.workspace,
            false => match self.windows.get(&window).and_then(Window::active_workspace) {
                Some(workspace) => workspace,
                None => return,
            },
        };
        let Some(root) = self.restore_layout(closed.tab.layout, &closed.defaults) else { return };
        let panes = root.panes();
        let tab = Tab {
            title: closed.tab.title,
            focused: panes[closed.tab.focused.min(panes.len() - 1)],
            zoomed: closed.tab.zoomed && panes.len() > 1,
            root,
            broadcast: false,
        };
        let state = self.workspaces.get_mut(&workspace).unwrap();
        let index = closed.index.min(state.tabs.len());
        state.tabs.insert(index, tab);
        let before = self.focused_pane();
        self.select_tab(workspace, index);
        self.report_focus_change(before);
    }

    /// Take a pane out of its tab (not the tab's only one). Its tab stays.
    fn detach_pane(&mut self, pane: PaneId) -> Option<(WorkspaceId, usize)> {
        let (workspace, index) = self.place_of(pane)?;
        let tab = &mut self.workspaces.get_mut(&workspace).unwrap().tabs[index];
        let order = tab.root.panes();
        let root = tab.root.clone().without(pane)?;
        let remaining = root.panes();
        if tab.focused == pane {
            let position = order.iter().position(|&p| p == pane).unwrap_or(0);
            tab.focused = remaining[position.min(remaining.len() - 1)];
        }
        if remaining.len() == 1 {
            tab.zoomed = false;
            tab.broadcast = false;
        }
        tab.root = root;
        Some((workspace, index))
    }

    /// A pane into a tab of its own, right after the one it leaves.
    fn pane_to_new_tab(&mut self, pane: PaneId) {
        let Some((workspace, index)) = self.detach_pane(pane) else { return };
        let state = self.workspaces.get_mut(&workspace).unwrap();
        state.tabs.insert(index + 1, Tab::new(pane));
        let before = self.focused_pane();
        self.select_tab(workspace, index + 1);
        self.report_focus_change(before);
    }

    /// A pane into another tab, to the right of that tab's focused pane.
    /// Its own tab closes if it was the only pane there.
    fn move_pane(&mut self, pane: PaneId, workspace: WorkspaceId, tab: usize) {
        let Some(from) = self.place_of(pane) else { return };
        if from == (workspace, tab)
            || self.workspaces.get(&workspace).is_none_or(|w| tab >= w.tabs.len())
        {
            return;
        }
        let mut target = tab;
        if self.detach_pane(pane).is_none() {
            // Its tab's only pane: the tab goes, and later tabs move up.
            self.take_tab(from.0, from.1);
            if from.0 == workspace && from.1 < tab {
                target -= 1;
            }
            if !self.workspaces.contains_key(&workspace) {
                return;
            }
        }
        let state = self.workspaces.get_mut(&workspace).unwrap();
        let Some(tab_state) = state.tabs.get_mut(target) else { return };
        tab_state.root.split(tab_state.focused, pane, Axis::Horizontal);
        tab_state.focused = pane;
        tab_state.zoomed = false;
        let before = self.focused_pane();
        self.select_tab(workspace, target);
        self.report_focus_change(before);
    }

    fn close_workspace(&mut self, workspace: WorkspaceId) {
        let Some(state) = self.workspaces.get(&workspace) else { return };
        let panes = state.tabs.iter().flat_map(|t| t.root.panes()).collect();
        self.end_panes(panes);
        self.remove_workspace(workspace);
    }

    /// Remove a tab from its workspace (its panes keep running), closing the
    /// workspace if it was the last.
    fn take_tab(&mut self, workspace: WorkspaceId, tab: usize) -> Option<Tab> {
        let state = self.workspaces.get_mut(&workspace).filter(|w| tab < w.tabs.len())?;
        let removed = state.tabs.remove(tab);
        if state.tabs.is_empty() {
            self.remove_workspace(workspace);
        } else if tab < state.active || state.active >= state.tabs.len() {
            state.active = state.active.saturating_sub(1);
        }
        Some(removed)
    }

    /// Forget a workspace, closing its window if it was the last there.
    fn remove_workspace(&mut self, workspace: WorkspaceId) {
        self.workspaces.remove(&workspace);
        self.unlink_workspace(workspace);
    }

    /// Take a workspace out of its window (closing the window if it was the
    /// last there); the workspace itself stays. Returns where it was.
    fn unlink_workspace(&mut self, workspace: WorkspaceId) -> Option<(WindowId, usize)> {
        let (window_id, index) = self.window_of_workspace(workspace)?;
        let window = self.windows.get_mut(&window_id).unwrap();
        window.workspaces.remove(index);
        if window.workspaces.is_empty() {
            let clients = std::mem::take(&mut window.clients);
            self.windows.remove(&window_id);
            for client in clients {
                self.send(client, ServerMessage::WindowClosed { window: window_id });
            }
        } else if index < window.active || window.active >= window.workspaces.len() {
            window.active = window.active.saturating_sub(1);
        }
        Some((window_id, index))
    }

    /// Move a tab within its workspace or into another one, and show it.
    fn move_tab(
        &mut self,
        workspace: WorkspaceId,
        from: usize,
        to_workspace: WorkspaceId,
        to: usize,
    ) {
        if !self.workspaces.contains_key(&to_workspace) {
            return;
        }
        if workspace == to_workspace {
            let Some(state) = self
                .workspaces
                .get_mut(&workspace)
                .filter(|w| from < w.tabs.len() && to < w.tabs.len())
            else {
                return;
            };
            let active = state.tabs[state.active].focused;
            let tab = state.tabs.remove(from);
            state.tabs.insert(to, tab);
            state.active = state.tabs.iter().position(|t| t.focused == active).unwrap_or(0);
            return;
        }
        let before = self.focused_pane();
        let Some(tab) = self.take_tab(workspace, from) else { return };
        // The source may have closed (and with it, a window), but not the target.
        let target = self.workspaces.get_mut(&to_workspace).unwrap();
        let to = to.min(target.tabs.len());
        target.tabs.insert(to, tab);
        self.select_tab(to_workspace, to);
        self.report_focus_change(before);
    }

    /// Move a workspace to index `to` among `window`'s, closing the window it
    /// leaves if that was its last.
    fn move_workspace(&mut self, workspace: WorkspaceId, window: WindowId, to: usize) {
        if !self.workspaces.contains_key(&workspace) || !self.windows.contains_key(&window) {
            return;
        }
        let before = self.focused_pane();
        if let Some((from_window, index)) = self.window_of_workspace(workspace)
            && from_window == window
        {
            let state = self.windows.get_mut(&window).unwrap();
            let active = state.active_workspace();
            state.workspaces.remove(index);
            state.workspaces.insert(to.min(state.workspaces.len()), workspace);
            state.active = state.workspaces.iter().position(|&w| Some(w) == active).unwrap_or(0);
            return;
        }
        // Moving its window's last workspace closes that window.
        self.unlink_workspace(workspace);
        let Some(state) = self.windows.get_mut(&window) else { return };
        let to = to.min(state.workspaces.len());
        state.workspaces.insert(to, workspace);
        state.active = to;
        self.report_focus_change(before);
    }

    /// Move a tab into a new, unnamed workspace right after its own. A
    /// workspace's only tab stays put: that would change nothing.
    fn tab_to_new_workspace(&mut self, workspace: WorkspaceId, tab: usize) {
        let Some((window, index)) = self.window_of_workspace(workspace) else { return };
        if self.workspaces.get(&workspace).is_none_or(|w| w.tabs.len() < 2 || tab >= w.tabs.len()) {
            return;
        }
        let before = self.focused_pane();
        let Some(moved) = self.take_tab(workspace, tab) else { return };
        let new = self.add_workspace(Workspace::new(None, vec![moved], 0));
        let state = self.windows.get_mut(&window).unwrap();
        state.workspaces.insert(index + 1, new);
        state.active = index + 1;
        self.report_focus_change(before);
    }

    /// Show a workspace in `window`; if another window on screen shows it,
    /// bring that one forward instead.
    fn show_workspace(&mut self, workspace: WorkspaceId, window: WindowId) {
        let before = self.focused_pane();
        match self.window_of_workspace(workspace) {
            Some((current, index)) if current == window || self.windows[&current].shown() => {
                self.windows.get_mut(&current).unwrap().active = index;
                if current != window
                    && let Some(client) = self.windows[&current].driver()
                {
                    self.send(client, ServerMessage::App(AppCommand::Activate { window: current }));
                }
            },
            _ => {
                let to = self.windows.get(&window).map_or(0, |w| w.workspaces.len());
                self.move_workspace(workspace, window, to);
            },
        }
        self.report_focus_change(before);
    }

    /// Move a workspace into a window of its own, and have `client` show it.
    fn workspace_to_new_window(&mut self, workspace: WorkspaceId, client: ClientId) {
        // Already alone in its window: nothing would change.
        if self
            .window_of_workspace(workspace)
            .is_some_and(|(w, _)| self.windows[&w].workspaces.len() < 2)
        {
            return;
        }
        if !self.workspaces.contains_key(&workspace) {
            return;
        }
        self.unlink_workspace(workspace);
        let window = self.insert_window(vec![workspace], 0, None);
        self.send(client, ServerMessage::App(AppCommand::OpenWindow { window }));
    }

    /// A workspace named by its id or its name.
    fn resolve_workspace(&self, name: &str) -> Result<WorkspaceId, String> {
        if let Ok(id) = name.parse::<WorkspaceId>()
            && self.workspaces.contains_key(&id)
        {
            return Ok(id);
        }
        self.workspace_named(name).ok_or_else(|| format!("no workspace {name}"))
    }

    /// Where to show something new: the frontmost window on screen, or any.
    fn target_window(&self) -> Option<WindowId> {
        self.frontmost()
    }

    /// Workspaces as shown, window by window, then hidden ones.
    fn workspace_order(&self) -> Vec<(WorkspaceId, Option<(WindowId, usize)>)> {
        let mut order: Vec<_> = self
            .windows
            .iter()
            .enumerate()
            .flat_map(|(number, (&window, w))| {
                w.workspaces.iter().map(move |&id| (id, Some((window, number + 1))))
            })
            .collect();
        order.extend(
            self.workspaces
                .keys()
                .filter(|id| self.window_of_workspace(**id).is_none())
                .map(|&id| (id, None)),
        );
        order
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        const ORDER: [&str; 6] = ["waiting", "working", "running", "failed", "done", "idle"];
        self.workspace_order()
            .into_iter()
            .filter_map(|(id, place)| {
                let workspace = self.workspaces.get(&id)?;
                let panes: Vec<&Pane> = workspace
                    .tabs
                    .iter()
                    .flat_map(|t| t.root.panes())
                    .filter_map(|p| self.panes.get(&p))
                    .collect();
                let status = panes
                    .iter()
                    .map(|p| p.status.label())
                    .min_by_key(|label| {
                        ORDER.iter().position(|o| o == label).unwrap_or(ORDER.len())
                    })
                    .unwrap_or("idle");
                let title = workspace
                    .name
                    .clone()
                    .or_else(|| Some(workspace.title.clone()).filter(|t| !t.is_empty()))
                    .unwrap_or_else(|| {
                        workspace
                            .active_tab()
                            .and_then(|t| {
                                t.title
                                    .clone()
                                    .or_else(|| Some(self.panes.get(&t.focused)?.title()))
                            })
                            .unwrap_or_default()
                    });
                Some(WorkspaceInfo {
                    id,
                    name: workspace.name.clone(),
                    title,
                    root: workspace.defaults.root.clone(),
                    color: workspace.color.clone(),
                    env: workspace.defaults.env.clone(),
                    window: place.map(|(window, _)| window),
                    window_number: place.map_or(0, |(_, number)| number),
                    attached: self.on_screen(id),
                    tabs: workspace.tabs.len(),
                    panes: panes.len(),
                    status: status.to_owned(),
                    needs_attention: panes.iter().any(|p| p.attention),
                })
            })
            .collect()
    }

    /// The workspace named `name`, preferring the frontmost window's; failing
    /// that, an unnamed one going by that automatic name.
    fn workspace_named(&self, name: &str) -> Option<WorkspaceId> {
        let front = self.frontmost().and_then(|w| self.windows.get(&w));
        let find = |matches: &dyn Fn(&Workspace) -> bool| {
            let found = |id: &WorkspaceId| self.workspaces.get(id).is_some_and(matches);
            front
                .and_then(|w| w.workspaces.iter().copied().find(found))
                .or_else(|| self.workspaces.keys().copied().find(found))
        };
        find(&|w| w.name.as_deref() == Some(name))
            .or_else(|| find(&|w| w.name.is_none() && w.title == name))
    }

    fn fail_runs(&mut self, pane: PaneId, message: &str) {
        for run in self.runs.extract_if(.., |run| run.pane == pane) {
            Self::answer(&self.clients, run.waiter, Response::error(message));
        }
        self.asks.fail_pane(pane, message, Instant::now());
        self.replies.pane_closed(pane);
        self.threads.pane_closed(pane);
        self.edits.pane_closed(pane);
        self.parents.remove(&pane);
        self.copied_from.remove(&pane);
        if let Some(tree) = self.worktrees.remove(&pane) {
            crate::worktrees::remove(tree);
        }
        for release in self.approvals.pane_closed(pane, Instant::now()) {
            self.release_approval(release);
        }
    }

    /// A way to answer `waiter` from another thread.
    fn reply_later(&self, waiter: Waiter) -> Box<dyn FnOnce(Response) + Send> {
        match waiter {
            Waiter::Command(reply) => Box::new(move |response| {
                let _ = reply.send(response);
            }),
            Waiter::App(client, id) => {
                let tx = self.clients.get(&client).map(|c| c.tx.clone());
                Box::new(move |response| {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    if let Some(tx) = tx {
                        let _ = tx.send(ServerMessage::Control { id, response: json });
                    }
                })
            },
        }
    }

    fn answer(clients: &HashMap<ClientId, Client>, waiter: Waiter, response: Response) {
        match waiter {
            Waiter::Command(reply) => {
                let _ = reply.send(response);
            },
            Waiter::App(client, id) => {
                if let Some(client) = clients.get(&client) {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    let _ = client.tx.send(ServerMessage::Control { id, response: json });
                }
            },
        }
    }

    fn send(&self, client: ClientId, message: ServerMessage) {
        if let Some(client) = self.clients.get(&client) {
            let _ = client.tx.send(message);
        }
    }

    fn mark_dirty(&mut self, pane: PaneId) {
        for client in self.clients.values_mut() {
            client.dirty.insert(pane);
        }
    }

    /// Tell the user about a pane that needs them, unless they're looking at it.
    fn notify_user(&self, pane: PaneId, title: Option<&str>, body: &str) {
        if !self.config.notifications || self.visible(pane) {
            return;
        }
        let Some(pane_state) = self.panes.get(&pane) else { return };
        let pane_title = pane_state.title();
        let title = title.map_or_else(|| pane_title.clone(), |t| format!("{t} — {pane_title}"));
        let body =
            if body.is_empty() { "Needs your attention".to_owned() } else { body.to_owned() };
        // Attached apps post native notifications; otherwise use osascript.
        if self.clients.is_empty() {
            return osascript_notify(&title, &body);
        }
        let window = self.window_of(pane);
        for client in self.told_of(Instant::now()) {
            self.send(
                client,
                ServerMessage::Notify {
                    title: title.clone(),
                    body: body.clone(),
                    window,
                    pane: Some(pane),
                },
            );
        }
    }

    /// The apps to tell: the one used just now, so a phone in a pocket stays quiet while
    /// its user is at the Mac (and the other way round); every one, when none is in use.
    fn told_of(&self, now: Instant) -> Vec<ClientId> {
        let in_use = self
            .clients
            .iter()
            .filter_map(|(&id, c)| Some((id, c.last_used?)))
            .filter(|&(_, at)| now.duration_since(at) < IN_USE)
            .max_by_key(|&(_, at)| at);
        match in_use {
            Some((id, _)) => vec![id],
            None => self.clients.keys().copied().collect(),
        }
    }

    // Inputs.

    pub(crate) fn handle(&mut self, input: Input) {
        match input {
            Input::Control(peer, request, reply) => {
                self.control(*request, Waiter::Command(reply), Some(peer))
            },
            Input::ControlClosed(conn) => self.control_closed(conn),
            Input::Connected(id, pid, tx, extensions) => {
                let build = std::env::current_exe()
                    .map(|exe| artty_core::protocol::build_id(&exe))
                    .unwrap_or_default();
                let _ = tx.send(ServerMessage::Welcome {
                    version: artty_core::protocol::protocol_version(),
                    server_version: build,
                });
                let kinds = ServerExtension::KINDS
                    .iter()
                    .chain(artty_core::protocol::CLIENT_KINDS)
                    .map(|&k| k.to_owned())
                    .collect();
                let _ = tx.send(ServerMessage::Extensions { kinds });
                self.clients.insert(
                    id,
                    Client {
                        tx,
                        pid,
                        queue_sent: None,
                        sent: HashMap::new(),
                        dirty: HashSet::new(),
                        panes_sent: Vec::new(),
                        status_sent: HashMap::new(),
                        images_sent: HashSet::new(),
                        batches_sent: 0,
                        batches_acked: 0,
                        last_heard: Instant::now(),
                        last_used: None,
                        pings: false,
                        extensions,
                        peek: None,
                    },
                );
            },
            Input::Client(id, message) => self.handle_client(id, message),
            Input::DirsChanged(dirs) => {
                for (client, pane) in self.watches.changed(&dirs) {
                    self.send(client, ServerMessage::DirChanged { pane });
                }
            },
            // (Sent with the next updates, if it changed a line.)
            Input::Status(job, result) => {
                self.status.finished(job, result);
            },
            Input::Disconnected(id) => {
                self.clients.remove(&id);
                self.watches.forget(id);
                // What it was sending is cut short: no half files.
                for (_, receiving) in self.uploads.extract_if(|(client, _), _| *client == id) {
                    let _ = std::fs::remove_file(receiving.part);
                }
                // Its windows keep running, detached (or shown by the
                // others, now in their sizes).
                let driven: Vec<WindowId> = self
                    .windows
                    .iter()
                    .filter(|(_, w)| w.driver() == Some(id))
                    .map(|(&window, _)| window)
                    .collect();
                for window in self.windows.values_mut() {
                    window.remove_client(id);
                }
                self.sizes.retain(|&(client, _), _| client != id);
                for window in driven {
                    self.apply_sizes(window);
                }
                for run in self.runs.extract_if(
                    ..,
                    |run| matches!(run.waiter, Waiter::App(client, _) if client == id),
                ) {
                    drop(run);
                }
                // (Their asks carry on.)
                self.ask_waiters
                    .retain(|w| !matches!(w.waiter, Waiter::App(client, _) if client == id));
                self.event_waiters
                    .retain(|w| !matches!(w.waiter, Waiter::App(client, _) if client == id));
            },
        }
    }

    /// A pane's events, merged while they waited.
    pub(crate) fn handle_pane(&mut self, id: PaneId, events: PaneEvents) {
        if events.dropped > 0 {
            log::warn!("pane {id}: dropped {} events sent faster than handled", events.dropped);
        }
        for event in events.into_events() {
            self.handle_event(id, event);
        }
    }

    fn handle_event(&mut self, id: PaneId, event: Event) {
        let visible = self.visible(id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let Some(notice) = pane.session.handle(event) else { return };
        // Output alone changes no status (and the status reads the terminal,
        // so waiting for its reader on every piece of output): poll() also
        // refreshes it every second.
        let refresh = !matches!(notice, Notice::Redraw);
        match notice {
            Notice::Redraw => {},
            Notice::Title(title) => pane.set_title(Some(title)),
            Notice::ResetTitle => pane.set_title(None),
            Notice::Bell => {
                if visible {
                    let client = self
                        .seen_windows()
                        .find(|&w| self.window_visible(w).contains(&id))
                        .and_then(|w| self.windows[&w].driver());
                    if let Some(client) = client {
                        self.send(client, ServerMessage::Bell { pane: id });
                    }
                } else {
                    pane.alert(None);
                    self.notify_user(id, None, "Bell");
                }
            },
            Notice::Notification { title, body } => {
                pane.alert(Some(body.clone()).filter(|b| !b.is_empty()).or(title.clone()));
                self.notify_user(id, title.as_deref(), &body);
            },
            Notice::Progress(progress) => {
                pane.progress = (progress != Progress::Remove).then_some(progress)
            },
            Notice::CommandStarted(command) => pane.command_started(command),
            Notice::CommandFinished(record) => {
                pane.command_finished(record.exit);
                let long = record.finished.is_some_and(|end| end - record.started >= LONG_COMMAND);
                let agent = pane.agent_name().is_some();
                let response = Response::Command {
                    command: record.command.clone(),
                    output: record.output.clone(),
                    exit: record.exit,
                    truncated: record.truncated,
                };
                for run in self.runs.extract_if(.., |run| run.pane == id) {
                    Self::answer(&self.clients, run.waiter, response.clone());
                }
                if long && !visible && !agent {
                    let what = record.command.clone().unwrap_or_else(|| "Command".to_owned());
                    let result = match record.exit {
                        Some(0) | None => "finished".to_owned(),
                        Some(code) => format!("failed (exit {code})"),
                    };
                    let message = format!("{what} {result}");
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.alert(Some(message.clone()));
                    }
                    self.notify_user(id, None, &message);
                }
            },
            Notice::ClipboardStore(_, text) => {
                // To the clipboard of the app showing it (this machine's
                // may be a server's, with no clipboard).
                let client = self
                    .window_of(id)
                    .and_then(|w| self.windows.get(&w)?.driver())
                    .or_else(|| self.app_client());
                match client {
                    Some(client) => self.send(client, ServerMessage::Clipboard { text }),
                    None if cfg!(target_os = "macos") => pbcopy(&text),
                    None => log::info!("pane {id} copied, with no app to take it"),
                }
            },
            Notice::ClipboardLoad(_, format) => pane.session.reply_clipboard(&format, &pbpaste()),
            Notice::Exited => {
                self.close_pane(id);
                return;
            },
        }
        if refresh && let Some(pane) = self.panes.get_mut(&id) {
            pane.refresh();
        }
        self.mark_dirty(id);
    }

    fn handle_client(&mut self, client: ClientId, message: ClientMessage) {
        if let Some(c) = self.clients.get_mut(&client) {
            c.last_heard = Instant::now();
            c.pings |= matches!(message, ClientMessage::Ping { .. });
        }
        match message {
            ClientMessage::AttachWindow { window, spec, bounds, share } => {
                let window = match window
                    .filter(|w| self.windows.get(w).is_some_and(|w| share || !w.shown()))
                {
                    Some(window) => window,
                    None => {
                        let near = self.focused_pane();
                        match self.spawn_pane(spec, near, None, &Defaults::default()) {
                            Ok(pane) => self.add_window_with(pane, bounds),
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            },
                        }
                    },
                };
                let state = self.windows.get_mut(&window).unwrap();
                // Shown here too: in the sizes of the app that uses it, until
                // this one does.
                if !state.clients.contains(&client) {
                    state.clients.insert(0, client);
                }
                state.last_focus = Instant::now();
                let bounds = state.bounds;
                self.send(client, ServerMessage::Attached { window, bounds });
                if let Some(c) = self.clients.get_mut(&client) {
                    c.panes_sent.clear();
                }
            },
            ClientMessage::DetachWindow { window } => {
                if let Some(state) = self.windows.get_mut(&window) {
                    let driving = state.driver() == Some(client);
                    state.remove_client(client);
                    if driving {
                        self.apply_sizes(window);
                    }
                }
            },
            ClientMessage::WindowBounds { window, bounds } => {
                if let Some(state) = self.windows.get_mut(&window) {
                    state.bounds = Some(bounds);
                }
            },
            ClientMessage::WindowFocused { window, focused } => {
                let before = self.focused_pane();
                if let Some(state) = self.windows.get_mut(&window) {
                    state.focused.retain(|&c| c != client);
                    if focused {
                        state.focused.push(client);
                        state.last_focus = Instant::now();
                    }
                }
                if focused {
                    // Its user is at it.
                    self.drive(client, window);
                    self.report_focus_change(before);
                    self.report_focus(self.focused_pane(), true);
                } else {
                    self.report_focus(self.window_focused_pane(window), false);
                }
            },
            ClientMessage::NewTab { window, workspace, spec } => {
                let before = self.focused_pane();
                if let Err(err) = self.open_tab(window, workspace, spec, true) {
                    log::error!("{err}");
                }
                self.report_focus_change(before);
            },
            ClientMessage::NewWorkspace { window, name, spec } => {
                let before = self.focused_pane();
                if let Err(err) = self.new_workspace(window, name, spec, true) {
                    log::error!("{err}");
                }
                self.report_focus_change(before);
            },
            ClientMessage::RenameWorkspace { workspace, name } => {
                if let Some(state) = self.workspaces.get_mut(&workspace) {
                    state.name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
                }
            },
            ClientMessage::CloseWorkspace { workspace } => self.close_workspace(workspace),
            ClientMessage::SetCollapsed { workspace, collapsed } => {
                if let Some(state) = self.workspaces.get_mut(&workspace) {
                    state.collapsed = collapsed;
                }
            },
            ClientMessage::MoveWorkspace { workspace, window, to } => {
                self.move_workspace(workspace, window, to)
            },
            ClientMessage::TabToNewWorkspace { workspace, tab } => {
                self.tab_to_new_workspace(workspace, tab)
            },
            ClientMessage::ForgetImages { ids } => {
                if let Some(c) = self.clients.get_mut(&client) {
                    for id in ids {
                        c.images_sent.remove(&id);
                    }
                }
            },
            ClientMessage::OpenWorkspace { workspace, window } => {
                if self.workspaces.contains_key(&workspace) && self.windows.contains_key(&window) {
                    self.show_workspace(workspace, window);
                }
            },
            ClientMessage::HideWorkspace { workspace } => {
                let before = self.focused_pane();
                self.unlink_workspace(workspace);
                self.report_focus_change(before);
            },
            ClientMessage::WorkspaceToNewWindow { workspace } => {
                self.workspace_to_new_window(workspace, client)
            },
            ClientMessage::SetWorkspaceColor { workspace, color } => {
                if let (Some(state), Ok(color)) = (
                    self.workspaces.get_mut(&workspace),
                    workspace_color(color.as_deref().unwrap_or_default()),
                ) {
                    state.color = color;
                }
            },
            ClientMessage::SetWorkspaceRoot { workspace, root } => {
                if let Some(state) = self.workspaces.get_mut(&workspace) {
                    state.defaults.root = root.filter(|dir| dir.is_dir());
                }
                self.refresh_title(workspace);
            },
            ClientMessage::Split { pane, axis, spec } => {
                if let Err(err) = self.split(pane, axis, spec, true) {
                    log::error!("{err}");
                }
            },
            ClientMessage::ClosePane { pane } => {
                // Closing a tab's last pane closes the tab: keep it to reopen.
                if let Some((workspace, tab)) = self.place_of(pane)
                    && self.workspaces[&workspace].tabs[tab].root == Node::Pane(pane)
                {
                    self.remember_closed(workspace, tab);
                }
                self.close_pane(pane)
            },
            ClientMessage::CloseTab { workspace, tab } => {
                self.remember_closed(workspace, tab);
                self.close_tab(workspace, tab)
            },
            ClientMessage::SelectTab { workspace, tab } => {
                let before = self.focused_pane();
                self.select_tab(workspace, tab);
                self.report_focus_change(before);
            },
            ClientMessage::FocusPane { pane } => self.focus_pane(pane),
            ClientMessage::MoveTab { workspace, from, to_workspace, to } => {
                self.move_tab(workspace, from, to_workspace, to)
            },
            ClientMessage::RenameTab { workspace, tab, title } => {
                if let Some(tab) =
                    self.workspaces.get_mut(&workspace).and_then(|w| w.tabs.get_mut(tab))
                {
                    tab.title = title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
                }
            },
            ClientMessage::ToggleZoom { window } => {
                if let Some(tab) = self.active_tab_mut(window)
                    && matches!(tab.root, Node::Split { .. })
                {
                    tab.zoomed = !tab.zoomed;
                }
            },
            ClientMessage::SetRatio { window, path, ratio } => {
                if let Some(tab) = self.active_tab_mut(window) {
                    tab.root.set_ratio(&path, ratio);
                }
            },
            ClientMessage::Input { pane, bytes } => {
                // Typed here: this app's user is at it.
                self.drive_pane(client, pane);
                // A broadcasting tab gets it in every pane.
                let targets = self
                    .place_of(pane)
                    .map(|(workspace, tab)| &self.workspaces[&workspace].tabs[tab])
                    .filter(|tab| tab.broadcast)
                    .map_or_else(|| vec![pane], |tab| tab.root.panes());
                let now = Instant::now();
                for target in targets {
                    if let Some(pane) = self.panes.get(&target) {
                        pane.session.write(bytes.clone());
                        self.asks.typed(target, now);
                    }
                }
            },
            ClientMessage::Resize { pane, size } => {
                // Its size here; the pane takes it if this app is the one
                // using its window (or the only one).
                self.sizes.insert((client, pane), size);
                let driver = self.window_of(pane).and_then(|w| self.windows[&w].driver());
                if driver.is_none_or(|driver| driver == client)
                    && let Some(pane) = self.panes.get_mut(&pane)
                {
                    pane.session.resize(size);
                }
                self.mark_dirty(pane);
            },
            ClientMessage::Scroll { pane, lines } => {
                self.drive_pane(client, pane);
                if let Some(pane) = self.panes.get(&pane) {
                    pane.session.scroll(lines);
                }
                self.mark_dirty(pane);
            },
            ClientMessage::Selection { pane, op } => {
                if matches!(op, SelectionOp::Start { .. }) {
                    self.drive_pane(client, pane);
                }
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
                self.mark_dirty(pane);
            },
            ClientMessage::Search { id, pane, query, older, restart } => {
                let result = match self.panes.get_mut(&pane) {
                    Some(state) => {
                        if restart {
                            state.session.clear_selection();
                        }
                        if query.is_empty() {
                            Ok(artty_core::protocol::SearchFound {
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
                self.send(client, ServerMessage::Reply { id, reply: Reply::Search(result) });
                self.mark_dirty(pane);
            },
            ClientMessage::EndSearch { pane } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.end_search();
                }
                self.mark_dirty(pane);
            },
            ClientMessage::JumpToPrompt { pane, older } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.jump_to_prompt(older);
                }
                self.mark_dirty(pane);
            },
            ClientMessage::CopyMode { pane, op } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.copy_mode(op);
                }
                self.mark_dirty(pane);
            },
            ClientMessage::EqualizePanes { window } => {
                if let Some(tab) = self.active_tab_mut(window) {
                    tab.root.equalize();
                }
            },
            ClientMessage::SwapPanes { a, b } => {
                if let (Some(place), Some(other)) = (self.place_of(a), self.place_of(b))
                    && place == other
                {
                    let (workspace, tab) = place;
                    self.workspaces.get_mut(&workspace).unwrap().tabs[tab].root.swap(a, b);
                }
            },
            ClientMessage::PaneToNewTab { pane } => self.pane_to_new_tab(pane),
            ClientMessage::MovePane { pane, workspace, tab } => {
                self.move_pane(pane, workspace, tab)
            },
            ClientMessage::ToggleBroadcast { window } => {
                if let Some(tab) = self.active_tab_mut(window) {
                    tab.broadcast = !tab.broadcast && matches!(tab.root, Node::Split { .. });
                }
            },
            ClientMessage::ReopenClosedTab { window } => self.reopen_closed_tab(window),
            ClientMessage::Ack { seq } => {
                if let Some(client) = self.clients.get_mut(&client) {
                    client.batches_acked = seq.min(client.batches_sent);
                }
            },
            ClientMessage::Ping { id } => self.send(client, ServerMessage::Pong { id }),
            ClientMessage::Extension { kind, data } => match kind.as_str() {
                artty_core::protocol::extension::PEEK => {
                    let peek = artty_core::protocol::Peek::decode(&data);
                    if let (Some(peek), Some(client)) = (peek, self.clients.get_mut(&client)) {
                        client.peek = peek.pane;
                        if let Some(pane) = peek.pane {
                            client.dirty.insert(pane);
                        }
                    }
                },
                _ => log::debug!("skipped an extension of unknown kind {kind:?}"),
            },
            ClientMessage::Unwatch { pane } => self.watches.unwatch(client, pane),
            ClientMessage::Reveal { pane, line, column, length } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.reveal(line, column, length);
                }
                self.focus_pane(pane);
                self.mark_dirty(pane);
            },
            ClientMessage::ResumeSession { window, agent, session, cwd } => {
                let command =
                    self.config.resume.get(&agent).and_then(|r| r.command(Some(&session)));
                let Some(command) = command else {
                    return log::warn!("no way to resume {agent} configured");
                };
                let before = self.focused_pane();
                let spec = PaneSpec { command: None, cwd: cwd.filter(|c| c.is_dir()), title: None };
                match self.open_tab(window, None, spec, true) {
                    Ok(pane) => {
                        if let Some(pane) = self.panes.get_mut(&pane) {
                            pane.type_when_ready(command);
                        }
                    },
                    Err(err) => log::error!("{err}"),
                }
                self.report_focus_change(before);
            },
            ClientMessage::Appearance { theme, cursor } => {
                self.appearance = true;
                if *self.theme != theme || self.cursor != cursor {
                    self.theme = Arc::new(theme);
                    self.cursor = cursor;
                    for pane in self.panes.values_mut() {
                        pane.session.set_theme(self.theme.clone());
                        pane.session.set_cursor_shape(cursor.shape());
                    }
                    let all: Vec<PaneId> = self.panes.keys().copied().collect();
                    for pane in all {
                        self.mark_dirty(pane);
                    }
                }
            },
            ClientMessage::ClearBuffer { pane } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.clear_buffer();
                }
                self.mark_dirty(pane);
            },
            // Reading agents' transcripts takes a while: off the main loop.
            ClientMessage::Query { id, query: Query::SearchSessions { text } } => {
                let Some(tx) = self.clients.get(&client).map(|c| c.tx.clone()) else { return };
                let _ =
                    std::thread::Builder::new().name("artty-sessions".into()).spawn(move || {
                        let reply = Reply::Sessions(crate::sessions::search(&text));
                        let _ = tx.send(ServerMessage::Reply { id, reply });
                    });
            },
            ClientMessage::Query { id, query } => {
                let reply = match query {
                    Query::DetachedWindows => Reply::Windows(
                        self.windows
                            .iter()
                            .filter(|(_, w)| !w.shown())
                            .map(|(&window, w)| self.window_summary(window, w))
                            .collect(),
                    ),
                    Query::AllWindows => Reply::Windows(
                        self.windows
                            .iter()
                            .map(|(&window, w)| self.window_summary(window, w))
                            .collect(),
                    ),
                    Query::LinkSpan { pane, line, column } => Reply::Link(
                        self.panes
                            .get(&pane)
                            .and_then(|p| p.session.link_span(line, column))
                            .map(|(url, span)| (url, span.start, span.end)),
                    ),
                    Query::SelectionText { pane } => {
                        Reply::Text(self.panes.get(&pane).and_then(|p| p.session.selection_text()))
                    },
                    Query::Cwd { pane } => {
                        Reply::Path(self.panes.get(&pane).and_then(|p| p.session.cwd()))
                    },
                    Query::CommandOutput { pane } => Reply::Output(
                        self.panes.get(&pane).and_then(|p| p.session.command_output()),
                    ),
                    Query::Upload { pane: _, upload, name, data, last, to } => {
                        Reply::Upload(self.upload(client, upload, &name, &data, last, to))
                    },
                    Query::SearchOutput { text } => Reply::Found(self.search_output(&text)),
                    Query::SearchSessions { .. } => unreachable!("handled before"),
                    Query::ListDir { pane, path, known, watch } => {
                        let follows = path.is_none();
                        let mut listing = self.list_dir(pane, path, known);
                        if let (true, Ok(listing)) = (watch, listing.as_mut()) {
                            let cwd = self.panes.get(&pane).and_then(|p| p.session.cwd());
                            listing.watched =
                                self.watches.watch(client, pane, &listing.path, follows, cwd);
                        }
                        Reply::Dir(listing)
                    },
                    Query::ReadFile { pane: _, path, offset } => {
                        Reply::File(read_file(&path, offset))
                    },
                };
                self.send(client, ServerMessage::Reply { id, reply });
            },
            ClientMessage::Control { id, request } => {
                let request = match serde_json::from_str::<Request>(&request) {
                    Ok(request) => request,
                    Err(err) => {
                        let response = Response::error(format!("bad request: {err}"));
                        self.send(
                            client,
                            ServerMessage::Control {
                                id,
                                response: serde_json::to_string(&response).unwrap_or_default(),
                            },
                        );
                        return;
                    },
                };
                // Apps are programs on the socket too: one started in a pane
                // (`artty attach`, a relay) is held to what the pane's are.
                let pid = self.clients.get(&client).and_then(|c| c.pid);
                let peer = Peer { conn: client, pid };
                self.control(request, Waiter::App(client, id), Some(peer));
            },
        }
    }

    // Control requests (the `artty` command and agent hooks).

    /// A control request, answered now or (`artty run`, `ask`, `approve`)
    /// once what it waits for happens. `peer`: from the `artty` command
    /// (else from an app).
    fn control(&mut self, request: Request, waiter: Waiter, peer: Option<Peer>) {
        let request = match self.acp_agents(request) {
            Ok(request) => request,
            Err(message) => return Self::answer(&self.clients, waiter, Response::error(message)),
        };
        let deadline = |secs: Option<u64>| secs.map(|s| Instant::now() + Duration::from_secs(s));
        match request {
            Request::Approve { tab, agent, tool_name, tool_input, suggestions } => {
                let Some(peer) = peer.filter(|_| matches!(waiter, Waiter::Command(_))) else {
                    let error = Response::error("permission requests come from agents' hooks");
                    return Self::answer(&self.clients, waiter, error);
                };
                match self.open_approval(tab, peer, agent, tool_name, tool_input, suggestions) {
                    Ok(()) => {
                        self.approval_waiters.insert(peer.conn, waiter);
                    },
                    Err(message) => Self::answer(&self.clients, waiter, Response::error(message)),
                }
            },
            // Reading agents' transcripts takes a while: off the main loop.
            Request::Sessions { text, limit } => {
                let limit = limit.unwrap_or(30).min(200);
                let live: HashSet<String> = self
                    .panes
                    .values()
                    .filter_map(|p| p.agent_session().map(str::to_owned))
                    .collect();
                let reply = self.reply_later(waiter);
                let _ =
                    std::thread::Builder::new().name("artty-sessions".into()).spawn(move || {
                        let sessions = match text.as_deref().map(str::trim) {
                            Some(text) if !text.is_empty() => crate::sessions::search_past(text)
                                .into_iter()
                                .filter(|past| !live.contains(&past.hit.session))
                                .take(limit)
                                .collect(),
                            _ => crate::sessions::recent(limit, &live),
                        };
                        reply(Response::Sessions { sessions });
                    });
            },
            Request::Conversation { session, limit } => {
                let reply = self.reply_later(waiter);
                let _ =
                    std::thread::Builder::new().name("artty-sessions".into()).spawn(move || {
                        reply(match crate::sessions::conversation(&session, limit.unwrap_or(60)) {
                            Some(messages) => Response::Conversation { messages },
                            None => Response::error(format!(
                                "no conversation {session} (only Claude Code's are read)"
                            )),
                        })
                    });
            },
            Request::ThreadClear { tab } => {
                let response = if self.in_a_pane(peer) {
                    Response::error(
                        "a thread is the user's to clear, not from inside artty's panes",
                    )
                } else if !self.panes.contains_key(&tab) {
                    Response::error(format!("no pane {tab}"))
                } else {
                    self.threads.clear(tab);
                    self.replies.read_pane(tab);
                    self.keep(Instant::now());
                    Response::Ok
                };
                Self::answer(&self.clients, waiter, response);
            },
            Request::Decide { id, allow, message, always } => {
                let decision = Decision { allow, message, permissions: Vec::new() };
                let response = self.decide(id, decision, always, peer);
                Self::answer(&self.clients, waiter, response.unwrap_or_else(Response::error));
            },
            // Not keys from a program in a pane (an agent answering its own
            // prompt) while asking permission.
            Request::Send { tab, .. } if self.in_a_pane(peer) && self.asking_permission(tab) => {
                let error = Response::error(
                    "the pane's agent is asking for permission, which can't be answered from \
                     inside artty's panes: answer it in the pane, or in the artty app",
                );
                Self::answer(&self.clients, waiter, error);
            },
            Request::Run { tab, command, timeout_secs } => match self.start_run(tab, &command) {
                Ok(pane) => {
                    self.runs.push(PendingRun { pane, waiter, deadline: deadline(timeout_secs) })
                },
                Err(message) => Self::answer(&self.clients, waiter, Response::error(message)),
            },
            request @ (Request::New { task: Some(_), .. } | Request::New { fork: Some(_), .. }) => {
                let response = self.start_with_task(request, peer).unwrap_or_else(Response::error);
                Self::answer(&self.clients, waiter, response);
                self.pump_asks(Instant::now());
            },
            Request::Ask { tab, text, raw, wait, timeout_secs, thread, back } => {
                let from = self.pane_of_peer(peer);
                let added = self.add_ask(tab, text, raw, from);
                if let Ok(ask) = added {
                    // Written from an agent's thread (or by that agent):
                    // a card there follows it.
                    let here = from.or(thread).filter(|&p| p != tab);
                    if let Some(here) = here.filter(|p| self.panes.contains_key(p)) {
                        self.start_handoff(ask, here, from.is_some(), back && from.is_none());
                    }
                }
                match added {
                    Ok(ask) if wait => {
                        self.ask_waiters.push(AskWaiter::one(ask, waiter, deadline(timeout_secs)))
                    },
                    Ok(ask) => {
                        let info = self.asks.info(ask).expect("just added");
                        Self::answer(&self.clients, waiter, Response::Ask { ask: info });
                    },
                    Err(message) => Self::answer(&self.clients, waiter, Response::error(message)),
                }
                self.pump_asks(Instant::now());
            },
            Request::AskResult { id, wait, timeout_secs } => match self.asks.info(id) {
                None => Self::answer(
                    &self.clients,
                    waiter,
                    Response::error(format!("no ask {id} (asks are kept an hour once over)")),
                ),
                Some(info) if !wait || info.state.finished() => {
                    Self::answer(&self.clients, waiter, Response::Ask { ask: info })
                },
                Some(_) => {
                    self.ask_waiters.push(AskWaiter::one(id, waiter, deadline(timeout_secs)))
                },
            },
            Request::AskWait { ids, all, timeout_secs } => {
                if ids.is_empty() {
                    return Self::answer(&self.clients, waiter, Response::error("which asks?"));
                }
                if let Some(id) = ids.iter().find(|&&id| self.asks.info(id).is_none()) {
                    let message = format!("no ask {id} (asks are kept an hour once over)");
                    return Self::answer(&self.clients, waiter, Response::error(message));
                }
                let waiting = AskWaiter {
                    asks: ids,
                    all,
                    many: true,
                    waiter,
                    deadline: deadline(timeout_secs),
                };
                match waiting.over(&self.asks) || timeout_secs == Some(0) {
                    true => self.answer_asks(waiting),
                    false => self.ask_waiters.push(waiting),
                }
            },
            Request::Events { after, tab, timeout_secs } => {
                let Some(after) = after else {
                    let last = self.threads.newest();
                    let response = Response::Events { events: Vec::new(), last };
                    return Self::answer(&self.clients, waiter, response);
                };
                let waiting = EventWaiter { after, tab, waiter, deadline: deadline(timeout_secs) };
                if timeout_secs == Some(0) || !self.threads.since(after, tab).is_empty() {
                    self.answer_events(waiting);
                } else {
                    self.event_waiters.push(waiting);
                }
            },
            request => {
                let response = self.respond(request).unwrap_or_else(Response::error);
                Self::answer(&self.clients, waiter, response);
            },
        }
    }

    /// A request from a program running in one of artty's panes (or one
    /// that can't be told apart from one). Apps' requests aren't.
    /// What a hook event says of files: an edit tool used (kept for the
    /// turn; another agent having changed the file lately is told in both
    /// threads), or a turn begun or over (the last one's files go in its
    /// thread, before its reply).
    fn follow_edits(&mut self, pane: PaneId, hook: &artty_core::control::HookEvent, now: Instant) {
        if let (Some(tool), Some(input)) = (&hook.tool_name, &hook.tool_input) {
            for (other, file) in self.edits.tool_used(pane, tool, input, now) {
                let (Some(this), Some(that)) = (self.ask_from(pane), self.ask_from(other)) else {
                    continue;
                };
                let (this_title, that_title) = (this.title.clone(), that.title.clone());
                self.threads.add(pane, ThreadItem::Overlap { file: file.clone(), other: that });
                self.threads.add(other, ThreadItem::Overlap { file: file.clone(), other: this });
                let name = std::path::Path::new(&file)
                    .file_name()
                    .map_or(file.clone(), |n| n.to_string_lossy().into_owned());
                self.notify_user(
                    pane,
                    Some("Two agents, one file"),
                    &format!("{this_title} and {that_title} both changed {name}"),
                );
            }
        }
        if hook.turn().is_some() || hook.session_boundary() {
            let files = self.edits.take_turn(pane);
            if !files.is_empty() {
                self.threads.add(pane, ThreadItem::Edited { files });
            }
        }
    }

    /// How many agents up `pane` was started from (by `artty new --task`
    /// from inside a pane): 0 for one the user started.
    fn depth(&self, pane: PaneId) -> usize {
        let mut depth = 0;
        let mut at = pane;
        while let Some(&parent) = self.parents.get(&at) {
            depth += 1;
            at = parent;
            // (A loop can't be made, but just in case.)
            if depth > self.panes.len() {
                break;
            }
        }
        depth
    }

    /// An agent named `AGENT-acp` (`artty new --agent claude-acp`, `/new
    /// claude-acp`) is run by `artty acp AGENT`, if it has an adapter; so is
    /// one started with a task (`headless-tasks`).
    fn acp_agents(&self, mut request: Request) -> Result<Request, String> {
        let exe = || {
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "artty".to_owned())
        };
        if let Request::New { command, fork: Some(source), cwd, .. } = &mut request {
            let agent = match command.as_slice() {
                [name] => name.strip_suffix("-acp").unwrap_or(name),
                _ => {
                    return Err("--fork: a copy of an agent's conversation (--agent claude)".into());
                },
            };
            if agent != "claude" || !self.config.acp.contains_key(agent) {
                return Err(format!(
                    "--fork: only Claude Code's conversations are copied (through its Agent \
                     Client Protocol adapter), not {agent}'s"
                ));
            }
            if source.is_empty() || !source.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(format!("--fork: {source} isn't a conversation's id"));
            }
            if cwd.is_none() {
                *cwd = crate::sessions::claude_cwd(source).filter(|dir| dir.is_dir());
            }
            *command = vec![exe(), "acp".into(), agent.to_owned(), "--fork".into(), source.clone()];
            return Ok(request);
        }
        if let Request::New { command, task, .. } = &mut request
            && let [name] = command.as_slice()
            && let Some(agent) = name
                .strip_suffix("-acp")
                .or(Some(name.as_str()).filter(|_| task.is_some() && self.config.headless_tasks))
            && self.config.acp.contains_key(agent)
        {
            *command = vec![exe(), "acp".to_owned(), agent.to_owned()];
        }
        Ok(request)
    }

    /// Open a tab with its first message: the tab as asked, then the task
    /// as an ask of its agent, from the agent asking (if one is: its
    /// thread gets a card), so many agents deep at most.
    fn start_with_task(
        &mut self,
        request: Request,
        peer: Option<Peer>,
    ) -> Result<Response, String> {
        let Request::New {
            cwd,
            command,
            title,
            focus,
            split,
            target,
            workspace,
            worktree,
            task,
            thread,
            back,
            fork,
        } = request
        else {
            unreachable!("only a tab")
        };
        if let Some(task) = &task
            && (task.trim().is_empty() || crate::asks::commandlike(task))
        {
            return Err("--task: a message for the agent (not a /, ! or # command)".into());
        }
        let asker = self.pane_of_peer(peer);
        let most = self.config.max_agent_depth;
        if let Some(asker) = asker
            && most > 0
            && self.depth(asker) >= most
        {
            return Err(format!(
                "agents started by agents go {most} deep at most (the user's max-agent-depth \
                 setting)"
            ));
        }
        let open = Request::New {
            cwd,
            command,
            title,
            focus,
            split,
            target,
            workspace,
            worktree,
            task: None,
            thread: None,
            back: false,
            fork: None,
        };
        let Response::Tab { id } = self.respond(open)? else {
            return Err("the tab didn't open".into());
        };
        // Under the agent that started it, else (a copy) under the one whose
        // conversation it's a copy of, if that's running here.
        let source = fork.as_deref().and_then(|session| {
            self.panes
                .iter()
                .find(|&(&p, pane)| p != id && pane.agent_session() == Some(session))
                .map(|(&p, _)| p)
        });
        match (asker, source) {
            (Some(asker), _) => _ = self.parents.insert(id, asker),
            (None, Some(source)) => _ = self.copied_from.insert(id, source),
            (None, None) => {},
        }
        let Some(task) = task else { return Ok(Response::Tab { id }) };
        let ask = self.add_ask(id, task, false, asker)?;
        let here = asker.or(thread).filter(|&p| p != id && self.panes.contains_key(&p));
        if let Some(here) = here {
            self.start_handoff(ask, here, asker.is_some(), back && asker.is_none());
        }
        Ok(Response::Started { id, ask })
    }

    /// The agent in `pane`, as an ask names it.
    fn ask_from(&self, pane: PaneId) -> Option<artty_core::control::AskFrom> {
        let p = self.panes.get(&pane)?;
        let agent = match &p.status {
            Status::Agent { name, .. } => Some(name.clone()),
            _ => p.agent_name(),
        };
        Some(artty_core::control::AskFrom { pane, title: p.title(), agent })
    }

    /// Ask `ask` was written in `here`'s thread (by its agent, `by_agent`):
    /// a card there follows it; `back`: its answer goes there too.
    fn start_handoff(&mut self, ask: u64, here: PaneId, by_agent: bool, back: bool) {
        let Some(info) = self.asks.info(ask) else { return };
        let Some(to) = self.ask_from(info.pane) else { return };
        let text = self.asks.text_of(ask).unwrap_or_default().to_owned();
        self.asks.set_handoff(ask, here, back);
        let card = ThreadItem::Handoff {
            ask,
            to,
            text,
            by_agent,
            state: info.state,
            reply: None,
            error: None,
            forwarded: None,
            back,
        };
        self.threads.add(here, card);
        self.keep(Instant::now());
    }

    /// Bring handoffs' cards up to date with their asks; give the answers
    /// asked for back.
    fn follow_handoffs(&mut self, now: Instant) {
        for id in self.asks.handoffs_due() {
            let (Some(info), Some(handoff)) = (self.asks.info(id), self.asks.handoff(id)) else {
                continue;
            };
            self.threads.handoff(id, info.state, info.reply.clone(), info.error.clone(), None);
            if !info.state.finished() {
                continue;
            }
            let mut forwarded = None;
            if handoff.back && info.state == AskState::Answered {
                match self.forward(id) {
                    Ok(ask) => forwarded = Some(ask),
                    Err(message) => {
                        let error = format!("not brought back: {message}");
                        self.threads.handoff(id, info.state, info.reply, Some(error), None);
                    },
                }
            }
            self.asks.handoff_synced(id, forwarded);
            self.keep(now);
        }
    }

    /// Give answered ask `id`'s answer to the agent whose thread it was
    /// written in: an ask of its own, from the agent that answered.
    fn forward(&mut self, id: u64) -> Result<u64, String> {
        let handoff = self.asks.handoff(id).ok_or("that ask wasn't written in another thread")?;
        if let Some(already) = handoff.forwarded {
            return Err(format!("already brought back (ask {already})"));
        }
        let info = self.asks.info(id).ok_or(format!("no ask {id}"))?;
        let reply = info.reply.clone().filter(|_| info.state == AskState::Answered);
        let reply = reply.ok_or("it isn't answered (yet)")?;
        let to = self.ask_from(info.pane);
        let name = match to.as_ref() {
            Some(to) => match &to.agent {
                Some(agent) => format!("{} ({agent})", to.title),
                None => to.title.clone(),
            },
            None => format!("pane {}", info.pane),
        };
        let asked: String = self
            .asks
            .text_of(id)
            .unwrap_or_default()
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect();
        let text = format!(
            "Answer from {name}, to a message the user sent it from your thread (in artty):\n\
             {asked}\n{reply}"
        );
        let ask = self.add_ask(handoff.pane, text, false, Some(info.pane))?;
        self.asks.handoff_synced(id, Some(ask));
        self.threads.handoff(id, info.state, Some(reply), None, Some(ask));
        self.keep(Instant::now());
        Ok(ask)
    }

    /// The pane `peer` runs in (a program started in it, or under one).
    fn pane_of_peer(&self, peer: Option<Peer>) -> Option<PaneId> {
        let pid = peer?.pid?;
        self.panes.iter().find_map(|(&id, pane)| {
            let shell = pane.session.shell_pid()?;
            artty_core::descends_from(pid, &HashSet::from([shell])).then_some(id)
        })
    }

    fn in_a_pane(&self, peer: Option<Peer>) -> bool {
        let Some(peer) = peer else { return false };
        let Some(pid) = peer.pid else { return true };
        let panes: HashSet<i32> =
            self.panes.values().filter_map(|p| p.session.shell_pid()).collect();
        artty_core::descends_from(pid, &panes)
    }

    fn asking_permission(&self, tab: Option<PaneId>) -> bool {
        self.resolve(tab).is_ok_and(|pane| self.approvals.pending_in(pane))
    }

    fn open_approval(
        &mut self,
        tab: Option<PaneId>,
        peer: Peer,
        agent: Option<String>,
        tool_name: String,
        tool_input: serde_json::Value,
        suggestions: Vec<serde_json::Value>,
    ) -> Result<(), String> {
        let pane_id = tab.ok_or("which pane? (permission requests come from agents in artty)")?;
        let pane = self.panes.get_mut(&pane_id).ok_or(format!("no pane {pane_id}"))?;
        let now = Instant::now();
        let id = self.approvals.open(
            pane_id,
            peer.conn,
            agent.clone(),
            tool_name,
            tool_input,
            suggestions,
            now,
        );
        let info = self.approvals.info(id, now).expect("just opened");
        let message = format!("Allow {}?", info.summary);
        self.threads.add(
            pane_id,
            ThreadItem::Permission {
                id,
                tool_name: info.tool_name.clone(),
                summary: info.summary.clone(),
                state: info.state,
                input: info.tool_input.clone(),
            },
        );
        pane.process = pane.session.process();
        pane.report(AgentState::Waiting, Some(message.clone()), None, agent);
        self.notify_user(pane_id, Some("Needs permission"), &message);
        Ok(())
    }

    /// Answer a permission request: from an app, or the `artty` command
    /// run outside artty's panes (not by an agent, nor what it started).
    fn decide(
        &mut self,
        id: u64,
        decision: Decision,
        always: Option<usize>,
        peer: Option<Peer>,
    ) -> Result<Response, String> {
        if self.in_a_pane(peer) {
            return Err("permission requests can't be answered from inside artty's panes (an \
                        agent mustn't approve its own): answer in the artty app, or from a \
                        terminal outside artty"
                .into());
        }
        let release = self.approvals.decide(id, decision, always, Instant::now())?;
        self.release_approval(release);
        Ok(Response::Approvals {
            approvals: self.approvals.info(id, Instant::now()).into_iter().collect(),
        })
    }

    fn release_approval(&mut self, release: crate::approvals::Release) {
        if let Some(waiter) = self.approval_waiters.remove(&release.conn) {
            let response = Response::Decision { decision: release.decision };
            Self::answer(&self.clients, waiter, response);
        }
    }

    /// A control connection closed while waiting: a permission hook ended
    /// by its agent (its prompt dismissed, the turn stopped).
    fn control_closed(&mut self, conn: ClientId) {
        self.approval_waiters.remove(&conn);
        let now = Instant::now();
        if let Some(pane) = self.approvals.hangup(conn, now) {
            let why = "the agent's turn was interrupted at a permission prompt (check the pane)";
            self.asks.interrupted(pane, why, now);
            let reason = "a permission prompt was turned down".to_owned();
            self.threads.add(pane, ThreadItem::Interrupted { reason });
            if let Some(pane) = self.panes.get_mut(&pane) {
                let message = Some("Permission prompt dismissed".to_owned());
                pane.report(AgentState::Done, message, None, None);
            }
        }
    }

    /// `from`: the pane asking, if it's one of ours (an agent asking
    /// another).
    fn add_ask(
        &mut self,
        tab: PaneId,
        text: String,
        raw: bool,
        from: Option<PaneId>,
    ) -> Result<u64, String> {
        if from == Some(tab) {
            return Err(format!(
                "pane {tab} is your own: an agent can't ask itself (the ask would wait for the \
                 turn you're in to end). `artty agents` lists the others"
            ));
        }
        let from = from.and_then(|id| self.panes.get(&id)).zip(from).map(|(pane, id)| {
            let agent = match &pane.status {
                Status::Agent { name, .. } => Some(name.clone()),
                _ => pane.agent_name(),
            };
            artty_core::control::AskFrom { pane: id, title: pane.title(), agent }
        });
        let pane = self.panes.get(&tab).ok_or(format!("no pane {tab}"))?;
        let text = text.trim_end_matches(['\r', '\n']).to_owned();
        if text.trim().is_empty() {
            return Err("nothing to ask".into());
        }
        if !raw && crate::asks::commandlike(&text) {
            return Err(
                "the agent would take this as a command (/, ! or #); pass --raw to send it anyway"
                    .into(),
            );
        }
        // An agent, or a pane just opened (its program not yet seen: maybe
        // one starting). Nothing's typed until it's an agent.
        let starting = pane.process.foreground.is_none();
        if !matches!(pane.status, Status::Agent { .. }) && !starting {
            return Err(format!("no agent is running in pane {tab}"));
        }
        let now = Instant::now();
        let id = self.asks.add_from(tab, text, from, now);
        self.keep(now);
        Ok(id)
    }

    /// Type asks into panes ready for them; tell whoever waits for asks now
    /// over, or waited long enough.
    fn pump_asks(&mut self, now: Instant) {
        let panes = &self.panes;
        // Asked just before typing: the status may be a second old, from
        // before the agent exited.
        let view = |id: PaneId| {
            let pane = panes.get(&id)?;
            let state = match &pane.status {
                Status::Agent { state, .. } => Some(*state),
                _ => None,
            };
            let now = pane.session.process();
            let still = pane.process.foreground_group.is_some()
                && now.foreground_group == pane.process.foreground_group;
            Some(crate::asks::PaneView {
                agent: state.is_some() && still,
                shell: now.foreground.as_deref().is_some_and(artty_core::pane::is_shell),
                hooks: pane.hooks_reported(),
                state,
                bracketed: pane.session.mode().contains(TermMode::BRACKETED_PASTE),
            })
        };
        let writes = self.asks.pump(now, view);
        // On disk before anything's typed: after a crash, an ask being typed
        // in fails rather than going in again.
        if !writes.is_empty() {
            self.keep(now);
        }
        for write in writes {
            if let Some(pane) = self.panes.get(&write.pane) {
                pane.session.write(write.bytes);
            }
        }
        let asks = &self.asks;
        let over = |w: &mut AskWaiter| w.over(asks) || w.deadline.is_some_and(|d| d <= now);
        let done: Vec<AskWaiter> = self.ask_waiters.extract_if(.., over).collect();
        for waiter in done {
            self.answer_asks(waiter);
        }
    }

    /// Tell someone waiting for asks how they are.
    fn answer_asks(&self, waiter: AskWaiter) {
        let response = match waiter.many {
            true => Response::Asks {
                asks: waiter.asks.iter().filter_map(|&id| self.asks.info(id)).collect(),
            },
            false => match self.asks.info(waiter.asks[0]) {
                Some(ask) => Response::Ask { ask },
                None => Response::error(format!("ask {} is gone", waiter.asks[0])),
            },
        };
        Self::answer(&self.clients, waiter.waiter, response);
    }

    /// Tell someone waiting for thread entries what came (perhaps nothing,
    /// at its deadline).
    fn answer_events(&self, waiter: EventWaiter) {
        let found = self.threads.since(waiter.after, waiter.tab);
        let last = found.last().map_or(waiter.after.max(self.threads.newest()), |(_, e)| e.seq);
        let events = found
            .into_iter()
            .map(|(pane, entry)| artty_core::control::ThreadEvent { pane, entry })
            .collect();
        Self::answer(&self.clients, waiter.waiter, Response::Events { events, last });
    }

    /// Answer those waiting for thread entries, once there are some (or
    /// they've waited long enough).
    fn follow_events(&mut self, now: Instant) {
        if self.event_waiters.is_empty() {
            return;
        }
        let newest = self.threads.newest();
        let ready = |w: &mut EventWaiter| {
            w.deadline.is_some_and(|d| d <= now)
                || (newest > w.after && !self.threads.since(w.after, w.tab).is_empty())
        };
        let ready: Vec<EventWaiter> = self.event_waiters.extract_if(.., ready).collect();
        for waiter in ready {
            self.answer_events(waiter);
        }
    }

    fn resolve(&self, pane: Option<PaneId>) -> Result<PaneId, String> {
        match pane {
            Some(id) if self.panes.contains_key(&id) => Ok(id),
            Some(id) => Err(format!("no pane {id}")),
            None => self.focused_pane().ok_or_else(|| "no panes".to_owned()),
        }
    }

    fn start_run(&mut self, pane: Option<PaneId>, command: &str) -> Result<PaneId, String> {
        let id = self.resolve(pane)?;
        let session = &self.panes[&id].session;
        if !session.shell_integration() {
            return Err("this pane's shell doesn't report commands (no shell integration)".into());
        }
        if session.shell_phase() != ShellPhase::Prompt || self.runs.iter().any(|run| run.pane == id)
        {
            return Err("the pane is busy".into());
        }
        session.write(format!("{command}\r").into_bytes());
        Ok(id)
    }

    /// Every pane, window by window, workspace by workspace. Workspaces are
    /// numbered in that order.
    fn pane_infos(&self) -> Vec<TabInfo> {
        let front = self.frontmost();
        let mut infos = Vec::new();
        for (workspace_id, place) in self.workspace_order() {
            let Some(workspace) = self.workspaces.get(&workspace_id) else { continue };
            // Shown: the window's active workspace in the frontmost window.
            let shown = place.is_some_and(|(window, _)| {
                Some(window) == front
                    && self.windows[&window].active_workspace() == Some(workspace_id)
            });
            for (index, tab) in workspace.tabs.iter().enumerate() {
                for id in tab.root.panes() {
                    let focused = tab.focused == id;
                    let active = shown && index == workspace.active && focused;
                    if let Some(pane) = self.panes.get(&id) {
                        let mut info = pane.info(
                            place.map_or(0, |(_, number)| number),
                            index + 1,
                            active,
                            focused,
                        );
                        info.workspace = workspace_id as usize;
                        info.workspace_name = workspace.name.clone();
                        infos.push(info);
                    }
                }
            }
        }
        infos
    }

    /// `client`'s user is at `window`: it's the one whose sizes its panes
    /// take now.
    fn drive(&mut self, client: ClientId, window: WindowId) {
        if let Some(state) = self.clients.get_mut(&client) {
            state.last_used = Some(Instant::now());
        }
        let Some(state) = self.windows.get_mut(&window) else { return };
        if state.driver() == Some(client) || !state.clients.contains(&client) {
            return;
        }
        state.clients.retain(|&c| c != client);
        state.clients.push(client);
        self.apply_sizes(window);
    }

    fn drive_pane(&mut self, client: ClientId, pane: PaneId) {
        if let Some(window) = self.window_of(pane) {
            self.drive(client, window);
        }
    }

    /// `window`'s panes in the sizes of the app using it.
    fn apply_sizes(&mut self, window: WindowId) {
        let Some(state) = self.windows.get(&window) else { return };
        let Some(driver) = state.driver() else { return };
        let panes: Vec<PaneId> = state
            .workspaces
            .iter()
            .filter_map(|id| self.workspaces.get(id))
            .flat_map(|ws| ws.tabs.iter().flat_map(|tab| tab.root.panes()))
            .collect();
        for id in panes {
            if let Some(&size) = self.sizes.get(&(driver, id))
                && let Some(pane) = self.panes.get_mut(&id)
                && pane.session.size() != size
            {
                pane.session.resize(size);
                self.mark_dirty(id);
            }
        }
    }

    fn window_summary(&self, window: WindowId, state: &Window) -> WindowSummary {
        let tabs: Vec<&Tab> = state
            .workspaces
            .iter()
            .filter_map(|id| self.workspaces.get(id))
            .flat_map(|ws| ws.tabs.iter())
            .collect();
        WindowSummary {
            window,
            tabs: tabs.len(),
            bounds: state.bounds,
            apps: state.clients.len(),
            titles: tabs
                .iter()
                .map(|tab| {
                    tab.title.clone().unwrap_or_else(|| {
                        self.panes.get(&tab.focused).map(|p| p.title()).unwrap_or_default()
                    })
                })
                .collect(),
        }
    }

    fn app_client(&self) -> Option<ClientId> {
        self.frontmost()
            .and_then(|w| self.windows.get(&w)?.driver())
            .or_else(|| self.clients.keys().next().copied())
    }

    /// Part of a file an app sends: on the end of the file it started.
    /// Part of a file an app sends: on the end of the file it started. It
    /// arrives as a hidden `.part` file, and takes its place once whole
    /// (see [`UploadTo`]).
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        client: ClientId,
        upload: u64,
        name: &str,
        data: &[u8],
        last: bool,
        to: UploadTo,
    ) -> Result<Option<Uploaded>, UploadError> {
        use std::io::Write;
        let failed = |err: std::io::Error| UploadError::Failed(err.to_string());
        let key = (client, upload);
        if let std::collections::hash_map::Entry::Vacant(vacant) = self.uploads.entry(key) {
            // In place of a file: its name, whatever the file sent was called.
            let name = match &to {
                UploadTo::File { path, .. } => path.as_path(),
                _ => Path::new(name),
            };
            let name = name
                .file_name()
                .ok_or_else(|| UploadError::Failed("no file name".into()))?
                .to_owned();
            let (dir, place) = match to {
                UploadTo::Cache => {
                    let cache = uploads_dir()
                        .ok_or_else(|| UploadError::Failed("no home directory".into()))?;
                    prune_uploads(&cache);
                    let stamp = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let dir = cache.join(format!("{stamp}-{client}-{upload}"));
                    std::fs::create_dir_all(&dir).map_err(failed)?;
                    (dir, Place::Beside)
                },
                UploadTo::Folder { dir, existing } => {
                    if !dir.is_dir() {
                        return Err(UploadError::Failed(format!(
                            "{} isn't a folder",
                            dir.display()
                        )));
                    }
                    let taken = dir.join(&name).exists();
                    let place = match existing {
                        Existing::Refuse if taken => return Err(UploadError::Exists),
                        Existing::Rename => Place::Beside,
                        Existing::Refuse | Existing::Replace => Place::Instead(None),
                    };
                    (dir, place)
                },
                UploadTo::File { path, modified } => {
                    if modified.is_some() && modified != modified_time(&path) {
                        return Err(UploadError::Changed);
                    }
                    let dir = path
                        .parent()
                        .ok_or_else(|| UploadError::Failed("no folder".into()))?
                        .to_owned();
                    (dir, Place::Instead(modified))
                },
            };
            let part = dir.join(format!(".{}.{client}-{upload}.part", name.to_string_lossy()));
            let file = std::fs::File::create(&part).map_err(failed)?;
            vacant.insert(Upload { part, dir, name, file, place });
        }
        let receiving = self.uploads.get_mut(&key).expect("just made");
        if let Err(err) = receiving.file.write_all(data) {
            let receiving = self.uploads.remove(&key).expect("there");
            let _ = std::fs::remove_file(receiving.part);
            return Err(failed(err));
        }
        if !last {
            return Ok(None);
        }
        let receiving = self.uploads.remove(&key).expect("there");
        let path = match receiving.place {
            Place::Beside => free_name(&receiving.dir, &receiving.name),
            Place::Instead(expected) => {
                let path = receiving.dir.join(&receiving.name);
                // Changed meanwhile, after all?
                if expected.is_some() && expected != modified_time(&path) {
                    let _ = std::fs::remove_file(&receiving.part);
                    return Err(UploadError::Changed);
                }
                // As the file it replaces was (a script stays runnable).
                if let Ok(metadata) = std::fs::metadata(&path) {
                    let _ = std::fs::set_permissions(&receiving.part, metadata.permissions());
                }
                path
            },
        };
        drop(receiving.file);
        std::fs::rename(&receiving.part, &path).map_err(|err| {
            let _ = std::fs::remove_file(&receiving.part);
            failed(err)
        })?;
        let modified = modified_time(&path).unwrap_or_default();
        Ok(Some(Uploaded { path, modified }))
    }

    /// Lines with `text` in every pane's output, newest first in each, a few
    /// per pane and so many in all.
    fn search_output(&self, text: &str) -> Vec<OutputHit> {
        const PER_PANE: usize = 20;
        const ALL: usize = 300;
        if text.trim().is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (&pane, state) in &self.panes {
            let left = ALL.saturating_sub(hits.len()).min(PER_PANE);
            for (line, column, length, text) in state.session.find_lines(text, left) {
                hits.push(OutputHit { pane, line, column, length, text });
            }
        }
        hits
    }

    /// What's in `path` (else the pane's current directory): folders
    /// first, then by name.
    fn list_dir(
        &self,
        pane: PaneId,
        path: Option<PathBuf>,
        known: Option<u64>,
    ) -> Result<Listing, String> {
        let path = match path {
            Some(path) => path,
            None => self
                .panes
                .get(&pane)
                .and_then(|p| p.session.cwd())
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .ok_or("no directory")?,
        };
        let path = path.canonicalize().map_err(|err| format!("{}: {err}", path.display()))?;
        let mut entries: Vec<DirEntry> = std::fs::read_dir(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?
            .flatten()
            .take(MAX_LISTED)
            .filter_map(|entry| {
                let link = entry.file_type().ok()?.is_symlink();
                // A link: what it leads to (a folder to go into, say).
                let metadata =
                    std::fs::metadata(entry.path()).or_else(|_| entry.metadata()).ok()?;
                Some(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    dir: metadata.is_dir(),
                    link,
                    size: metadata.len(),
                    modified: metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs()),
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        // Unchanged since the app's last look: no need to send it again.
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut hasher);
            for entry in &entries {
                (&entry.name, entry.dir, entry.link, entry.size, entry.modified).hash(&mut hasher);
            }
            hasher.finish()
        };
        let entries = (known != Some(fingerprint)).then_some(entries);
        Ok(Listing { path, fingerprint, entries, watched: false })
    }

    fn respond(&mut self, request: Request) -> Result<Response, String> {
        Ok(match request {
            Request::List => Response::Tabs { tabs: self.pane_infos() },
            Request::Read { tab, source } => {
                let session = &self.panes[&self.resolve(tab)?].session;
                let text = match source {
                    ReadSource::Screen => session.screen_text(),
                    ReadSource::Scrollback { lines } => session.scrollback_text(lines),
                    ReadSource::LastCommand => {
                        if !session.shell_integration() {
                            return Err(
                                "this pane's shell doesn't report commands (no shell integration)"
                                    .into(),
                            );
                        }
                        let last = session.last_command().ok_or("no command has finished yet")?;
                        return Ok(Response::Command {
                            command: last.command.clone(),
                            output: last.output.clone(),
                            exit: last.exit,
                            truncated: last.truncated,
                        });
                    },
                };
                Response::Text { text }
            },
            Request::Send { tab, text, enter } => {
                let id = self.resolve(tab)?;
                self.asks.typed(id, Instant::now());
                let session = &self.panes[&id].session;
                let mut bytes = text.into_bytes();
                if enter {
                    bytes.push(b'\r');
                }
                session.write(bytes);
                Response::Ok
            },
            Request::New {
                cwd, command, title, focus, split, target, workspace, worktree, ..
            } => {
                // In a worktree: the tab makes it, then runs the command there.
                let (command, cwd, title, worktree) = match worktree {
                    None => (command, cwd, title, None),
                    Some(name) => {
                        crate::worktrees::check_name(&name)?;
                        if command.is_empty() {
                            return Err("--worktree: what to run in it (--agent NAME)".into());
                        }
                        let folder = cwd.ok_or("--worktree: a folder in the repository (--cwd)")?;
                        let repo = crate::worktrees::repo_of(&folder)?;
                        let dir = crate::worktrees::dir_for(&repo, &name);
                        let tree = crate::worktrees::Worktree { repo: repo.clone(), dir };
                        let command = crate::worktrees::command(&tree, &name, command);
                        (command, Some(repo), title.or(Some(name)), Some(tree))
                    },
                };
                let command = (!command.is_empty()).then_some(command);
                let spec = PaneSpec { command, cwd, title };
                let named =
                    workspace.map(|name| name.trim().to_owned()).filter(|name| !name.is_empty());
                let id = match split {
                    Some(direction) => {
                        let target = self.resolve(target)?;
                        let axis = match direction {
                            SplitDirection::Right => Axis::Horizontal,
                            SplitDirection::Down => Axis::Vertical,
                        };
                        self.split(target, axis, spec, focus)?
                    },
                    None => match (self.frontmost(), named) {
                        (Some(window), Some(name)) => match self.workspace_named(&name) {
                            Some(workspace) => {
                                if focus && !self.on_screen(workspace) {
                                    self.show_workspace(workspace, window);
                                }
                                let window =
                                    self.window_of_workspace(workspace).map_or(window, |(w, _)| w);
                                self.open_tab(window, Some(workspace), spec, focus)?
                            },
                            None => self.new_workspace(window, Some(name), spec, focus)?.1,
                        },
                        (Some(window), None) => self.open_tab(window, None, spec, focus)?,
                        // No windows: a detached one, shown when the app attaches.
                        (None, name) => {
                            let (workspace, pane) = self.create_workspace(name, spec)?;
                            self.insert_window(vec![workspace], 0, None);
                            pane
                        },
                    },
                };
                if focus {
                    self.focus_pane(id);
                }
                if let Some(tree) = worktree {
                    self.worktrees.insert(id, tree);
                }
                Response::Tab { id }
            },
            Request::Focus { tab } => {
                let id = self.resolve(Some(tab))?;
                // A pane out of sight comes into the frontmost window.
                if let Some((workspace, _)) = self.place_of(id)
                    && !self.on_screen(workspace)
                    && let Some(window) = self.target_window()
                {
                    self.show_workspace(workspace, window);
                }
                self.focus_pane(id);
                if let Some(window) = self.window_of(id)
                    && let Some(client) = self.windows[&window].driver()
                {
                    self.send(client, ServerMessage::App(AppCommand::Activate { window }));
                }
                Response::Ok
            },
            Request::Close { tab } => {
                let id = self.resolve(Some(tab))?;
                self.close_pane(id);
                Response::Ok
            },
            Request::Rename { tab, title } => {
                let id = self.resolve(tab)?;
                let (workspace, tab) = self.place_of(id).ok_or("the pane closed")?;
                let title = title.trim();
                self.workspaces.get_mut(&workspace).unwrap().tabs[tab].title =
                    (!title.is_empty()).then(|| title.to_owned());
                Response::Ok
            },
            Request::Run { .. }
            | Request::Ask { .. }
            | Request::AskResult { .. }
            | Request::Approve { .. }
            | Request::AskWait { .. }
            | Request::Events { .. }
            | Request::Decide { .. }
            | Request::ThreadClear { .. }
            | Request::Sessions { .. }
            | Request::Conversation { .. } => unreachable!("handled before"),
            Request::AskForward { id } => {
                let ask = self.forward(id)?;
                self.pump_asks(Instant::now());
                Response::Ask { ask: self.asks.info(ask).ok_or("the ask is gone")? }
            },
            Request::Queue => Response::Queue { items: self.queue_items() },
            Request::Agents => {
                let mut agents: Vec<AgentInfo> = self
                    .panes
                    .iter()
                    .filter_map(|(&id, pane)| {
                        let Status::Agent { name, state } = &pane.status else { return None };
                        let workspace = self
                            .place_of(id)
                            .and_then(|(w, _)| self.workspaces.get(&w))
                            .map(|w| w.name.clone().unwrap_or_else(|| w.title.clone()))
                            .unwrap_or_default();
                        Some(AgentInfo {
                            pane: id,
                            agent: name.clone(),
                            title: pane.title(),
                            workspace,
                            state: *state,
                            message: pane.message.clone(),
                            cwd: pane.session.cwd(),
                            last_ms: self.threads.last_ms(id),
                            pending: self.approvals.pending_count(id),
                            unread: self.replies.unread_in(id),
                            ask: self.asks.running_in(id),
                            session: pane.agent_session().map(str::to_owned),
                            parent: self
                                .parents
                                .get(&id)
                                .or(self.copied_from.get(&id))
                                .copied()
                                .filter(|p| self.panes.contains_key(p)),
                        })
                    })
                    .collect();
                agents.sort_by_key(|a| a.pane);
                Response::Agents { agents }
            },
            Request::ThreadSearch { text, tab } => {
                let hits = self
                    .threads
                    .search(&text, tab, 50)
                    .into_iter()
                    .map(|(pane, entry, kind, snippet)| {
                        let shown = self.panes.get(&pane);
                        let agent = shown.and_then(|p| match &p.status {
                            Status::Agent { name, .. } => Some(name.clone()),
                            _ => p.agent_name(),
                        });
                        artty_core::control::ThreadHit {
                            pane,
                            title: shown.map(|p| p.title()).unwrap_or_default(),
                            agent,
                            seq: entry.seq,
                            at_ms: entry.at_ms,
                            kind: kind.to_owned(),
                            snippet,
                        }
                    })
                    .collect();
                Response::ThreadHits { hits }
            },
            Request::Thread { tab, after, limit } => {
                let pane = self.panes.get(&tab).ok_or(format!("no pane {tab}"))?;
                let agent = match &pane.status {
                    Status::Agent { name, .. } => Some(name.clone()),
                    _ => pane.agent_name(),
                };
                // Permission requests as they are now (answered since, say).
                self.sync_permissions(Instant::now());
                Response::Thread {
                    pane: tab,
                    agent,
                    entries: self.threads.entries(tab, after, limit),
                }
            },
            Request::QueueDismiss { id } => {
                if !self.replies.dismiss(id) {
                    return Err(format!("no unread reply {id}"));
                }
                Response::Ok
            },
            Request::Approvals { tab } => {
                Response::Approvals { approvals: self.approvals.list(tab, Instant::now()) }
            },
            Request::AskList { tab } => {
                let ids = self.asks.pending_ids(tab);
                Response::Asks {
                    asks: ids.into_iter().filter_map(|id| self.asks.info(id)).collect(),
                }
            },
            Request::AskCancelQueued { tab } => {
                let now = Instant::now();
                let mut cancelled = Vec::new();
                for id in self.asks.pending_ids(tab) {
                    if self.asks.cancel(id, false, now).is_ok() {
                        cancelled.extend(self.asks.info(id));
                    }
                }
                Response::Asks { asks: cancelled }
            },
            Request::AskCancel { id, abandon } => {
                self.asks.cancel(id, abandon, Instant::now())?;
                Response::Ask { ask: self.asks.info(id).ok_or("no such ask")? }
            },
            Request::Notify { tab, title, body } => {
                let id = self.resolve(tab)?;
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.alert(Some(body.clone()));
                }
                self.notify_user(id, title.as_deref(), &body);
                Response::Ok
            },
            Request::Status { tab, state, message, agent_session, agent, hook } => {
                let reporter = agent.clone();
                let id = self.resolve(tab)?;
                let pane = self.panes.get_mut(&id).ok_or("the pane closed")?;
                let was_waiting =
                    matches!(pane.status, Status::Agent { state: AgentState::Waiting, .. });
                // The agent's in the foreground now, though the last sample
                // (once a second) may be from before it started: without
                // it, its first report would be taken for an agent gone.
                pane.process = pane.session.process();
                pane.report(state, message.clone(), agent_session, agent);
                Self::title_session(pane);
                // A turn cut short (`artty acp`: the agent said it was
                // cancelled): no answer for its ask.
                if let Some(hook) = hook.as_ref().filter(|h| h.event == "Interrupt") {
                    let now = Instant::now();
                    let how = hook.reply.clone().unwrap_or_else(|| "interrupted".to_owned());
                    self.cut_short(id, &how, now);
                    self.keep(now);
                    return Ok(Response::Ok);
                }
                if let Some(hook) = &hook {
                    let now = Instant::now();
                    self.asks.hook(id, hook, now);
                    self.follow_edits(id, hook, now);
                    // (After the asks: a turn that took one says so.)
                    let ask = (hook.turn() == Some(true))
                        .then(|| self.asks.running_in(id))
                        .flatten()
                        .map(|ask| (ask, self.asks.asker_of(ask)));
                    self.threads.hook(id, hook, message.as_deref(), ask);
                    for release in self.approvals.hook(id, hook, now) {
                        self.release_approval(release);
                    }
                    // A Claude Code turn over: anything rewound since?
                    let claude = self.panes.get(&id).is_some_and(
                        |p| matches!(&p.status, Status::Agent { name, .. } if name == "claude"),
                    );
                    if claude
                        && hook.event == "Stop"
                        && let Some(prompt) = hook.prompt_id.clone()
                    {
                        self.check_rewinds(id, prompt);
                    }
                    match (hook.turn(), hook.event.as_str()) {
                        (Some(true), _) => {
                            // As the thread shows it: a notice by its summary.
                            let prompt = hook.prompt.as_deref().map(crate::asks::unwrap_pastes);
                            let shown =
                                prompt.as_deref().map(|p| match crate::threads::notice(p) {
                                    Some((_, summary)) => summary,
                                    None => p.to_owned(),
                                });
                            self.replies.prompted(id, shown.as_deref());
                        },
                        (Some(false), "Stop" | "AfterAgent") => {
                            let agent = reporter
                                .or_else(|| self.panes.get(&id).and_then(|p| p.agent_name()));
                            self.replies.replied(id, agent, hook.reply.as_deref().unwrap_or(""));
                        },
                        _ => {},
                    }
                    self.keep(now);
                }
                match state {
                    AgentState::Waiting if !was_waiting => {
                        self.notify_user(id, Some("Needs input"), message.as_deref().unwrap_or(""));
                    },
                    AgentState::Done => {
                        self.notify_user(id, Some("Done"), message.as_deref().unwrap_or(""))
                    },
                    _ => {},
                }
                Response::Ok
            },
            Request::Activity => {
                let window = self
                    .frontmost()
                    .filter(|w| self.windows[w].shown())
                    .ok_or("the artty app isn't running")?;
                let client = self.windows[&window].driver().unwrap();
                self.send(client, ServerMessage::App(AppCommand::ShowActivity { window }));
                Response::Ok
            },
            Request::OpenUrl { url } => match self.app_client() {
                Some(client) => {
                    self.send(client, ServerMessage::App(AppCommand::OpenUrl { url }));
                    Response::Ok
                },
                None if cfg!(target_os = "macos") => {
                    std::process::Command::new("open")
                        .arg(&url)
                        .spawn()
                        .map_err(|err| err.to_string())?;
                    Response::Ok
                },
                None => return Err("no artty app is attached to open it".into()),
            },
            Request::Settings => {
                let client = self.app_client().ok_or("the artty app isn't running")?;
                self.send(client, ServerMessage::App(AppCommand::OpenSettings));
                Response::Ok
            },
            Request::Restart => {
                let client = self.app_client().ok_or("the artty app isn't running")?;
                self.send(client, ServerMessage::App(AppCommand::Restart));
                Response::Ok
            },
            Request::ServerStatus => Response::Server {
                info: ServerInfo {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    pid: std::process::id(),
                    uptime_secs: self.started.elapsed().as_secs(),
                    windows: self.windows.len(),
                    attached_windows: self.windows.values().filter(|w| w.shown()).count(),
                    workspaces: self.workspaces.len(),
                    panes: self.panes.len(),
                    clients: self.clients.len(),
                },
            },
            Request::StopServer => {
                self.stopping = true;
                Response::Ok
            },
            Request::RestartServer => {
                self.stopping = true;
                self.restarting = true;
                Response::Ok
            },
            Request::Processes { tab } => {
                if self.usage.is_empty() {
                    self.sample_usage();
                }
                let panes: Vec<PaneProcesses> = self
                    .usage
                    .iter()
                    .filter(|p| tab.is_none_or(|id| id == p.id))
                    .cloned()
                    .collect();
                if tab.is_some() && panes.is_empty() {
                    return Err("no such pane".into());
                }
                Response::Processes { panes, server: self.server_usage.clone() }
            },
            Request::Workspaces => Response::Workspaces { workspaces: self.workspace_infos() },
            Request::HandOver { executable } => {
                if !executable.is_file() {
                    return Err(format!("{} isn't a file", executable.display()));
                }
                // After this answer goes out.
                self.handover_to = Some(executable);
                Response::Ok
            },
            Request::Signal { pid, signal } => {
                // Only what runs in a pane: never the server, the app, or
                // anything else of yours.
                let samples = processes();
                let ours = self
                    .panes
                    .values()
                    .filter_map(|pane| pane.session.shell_pid())
                    .any(|shell| process_tree(&samples, shell).iter().any(|p| p.pid == pid));
                if !ours {
                    return Err(format!("process {pid} isn't running in an artty pane"));
                }
                // SAFETY: plain syscall.
                if unsafe { libc::kill(pid, signal.unwrap_or(libc::SIGTERM)) } != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Response::Ok
            },
            Request::NewWorkspace { name, cwd, command } => {
                let command = (!command.is_empty()).then_some(command);
                let spec = PaneSpec { command, cwd, title: None };
                let id = match self.target_window() {
                    Some(window) => self.new_workspace(window, name, spec, true)?.0,
                    None => {
                        let (workspace, _) = self.create_workspace(name, spec)?;
                        self.insert_window(vec![workspace], 0, None);
                        workspace
                    },
                };
                Response::Workspace { id }
            },
            Request::RenameWorkspace { workspace, name } => {
                let id = self.resolve_workspace(&workspace)?;
                let name = name.trim();
                self.workspaces.get_mut(&id).unwrap().name =
                    (!name.is_empty()).then(|| name.to_owned());
                Response::Ok
            },
            Request::CloseWorkspace { workspace } => {
                let id = self.resolve_workspace(&workspace)?;
                self.close_workspace(id);
                Response::Ok
            },
            Request::OpenWorkspace { workspace } => {
                let id = self.resolve_workspace(&workspace)?;
                match self.target_window() {
                    Some(window) => self.show_workspace(id, window),
                    // No windows: one of its own, shown when the app attaches.
                    None => {
                        self.insert_window(vec![id], 0, None);
                    },
                }
                Response::Ok
            },
            Request::ConfigureWorkspace { workspace, root, clear_root, color, env, unset_env } => {
                let id = self.resolve_workspace(&workspace)?;
                let root = match root {
                    Some(root) if !root.is_dir() => {
                        return Err(format!("{} isn't a directory", root.display()));
                    },
                    root => root,
                };
                let color = color.map(|c| workspace_color(&c)).transpose()?;
                let state = self.workspaces.get_mut(&id).unwrap();
                if clear_root {
                    state.defaults.root = None;
                }
                if let Some(root) = root {
                    state.defaults.root = Some(root);
                }
                if let Some(color) = color {
                    state.color = color;
                }
                for key in unset_env {
                    state.defaults.env.remove(&key);
                }
                state.defaults.env.extend(env);
                self.refresh_title(id);
                Response::Ok
            },
            Request::HideWorkspace { workspace } => {
                let id = self.resolve_workspace(&workspace)?;
                let before = self.focused_pane();
                self.unlink_workspace(id);
                self.report_focus_change(before);
                Response::Ok
            },
            Request::Kill { tab, signal, pane } => {
                let id = self.resolve(Some(tab))?;
                if pane {
                    self.close_pane(id);
                } else {
                    let group =
                        self.panes[&id].process.foreground_group.ok_or("no foreground process")?;
                    // A pane opened with a program has no shell to protect.
                    if self.panes[&id].process.at_shell && self.panes[&id].command.is_none() {
                        return Err(
                            "only the shell is running; use --pane to close the pane".into()
                        );
                    }
                    // SAFETY: plain syscall.
                    if unsafe { libc::killpg(group, signal.unwrap_or(libc::SIGTERM)) } != 0 {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                }
                Response::Ok
            },
        })
    }

    /// Sample every pane's processes, with CPU and memory use, and add to
    /// their history (once a second, whether or not anyone looks).
    fn sample_usage(&mut self) {
        let samples = processes();
        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0., |d| d.as_secs_f64());
        let cpu =
            |previous: &HashMap<i32, (u64, Instant)>, sample: &artty_core::ProcessSample| -> f32 {
                match previous.get(&sample.pid) {
                    // Since the last look, if recent; else the lifetime average.
                    Some(&(used, at))
                        if now.duration_since(at) <= Duration::from_secs(10)
                            && sample.cpu_ns >= used =>
                    {
                        let wall = now.duration_since(at).as_nanos().max(1) as f64;
                        ((sample.cpu_ns - used) as f64 / wall * 100.) as f32
                    },
                    _ => {
                        let alive = (unix_now - sample.started).max(0.05) * 1e9;
                        (sample.cpu_ns as f64 / alive * 100.) as f32
                    },
                }
            };
        let mut usage = Vec::new();
        let infos = self.pane_infos();
        let trees: HashMap<PaneId, Vec<&artty_core::ProcessSample>> = infos
            .iter()
            .filter_map(|info| {
                let shell = self.panes.get(&info.id)?.session.shell_pid()?;
                Some((info.id, process_tree(&samples, shell)))
            })
            .collect();
        let pids: Vec<i32> = trees.values().flatten().map(|p| p.pid).collect();
        let mut ports = artty_core::listening_ports(&pids);
        self.ports = trees
            .iter()
            .map(|(&pane, tree)| {
                let mut theirs: Vec<u16> =
                    tree.iter().filter_map(|p| ports.get(&p.pid)).flatten().copied().collect();
                theirs.sort_unstable();
                theirs.dedup();
                (pane, theirs)
            })
            .filter(|(_, theirs)| !theirs.is_empty())
            .collect();
        for info in infos {
            let (Some(pane), Some(tree)) = (self.panes.get(&info.id), trees.get(&info.id)) else {
                continue;
            };
            let foreground = pane.process.foreground_group;
            let rows: Vec<ProcessRow> = tree
                .iter()
                .map(|p| ProcessRow {
                    pid: p.pid,
                    ppid: p.ppid,
                    name: p.name.clone(),
                    cpu_percent: cpu(&self.cpu_samples, p),
                    memory_bytes: p.memory,
                    foreground: Some(p.pgid) == foreground,
                    elapsed_secs: (unix_now - p.started).max(0.) as u64,
                    ports: ports.remove(&p.pid).unwrap_or_default(),
                })
                .collect();
            let cpu_percent = rows.iter().map(|r| r.cpu_percent).fold(0., |a, b| a + b);
            let memory_bytes = rows.iter().map(|r| r.memory_bytes).sum();
            let attached = self.window_of(info.id).is_some_and(|w| self.windows[&w].shown());
            let history = self.history.entry(info.id).or_default();
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back((cpu_percent, memory_bytes));
            usage.push(PaneProcesses {
                id: info.id,
                window: info.window,
                workspace: info.workspace,
                workspace_name: info.workspace_name,
                tab: info.tab,
                title: info.title,
                status: info.status,
                attached,
                cwd: info.cwd,
                cpu_percent,
                memory_bytes,
                processes: rows,
                history: history.iter().copied().collect(),
            });
        }
        self.history.retain(|id, _| self.panes.contains_key(id));
        for workspace in self.workspaces.values_mut() {
            let panes: HashSet<PaneId> =
                workspace.tabs.iter().flat_map(|t| t.root.panes()).collect();
            let mine = usage.iter().filter(|p| panes.contains(&p.id));
            // Rounded, so layouts don't change for noise.
            let (cpu, memory) =
                mine.fold((0f32, 0u64), |(c, m), p| (c + p.cpu_percent, m + p.memory_bytes));
            workspace.usage = ((cpu * 10.).round() / 10., memory >> 20 << 20);
        }
        let pid = std::process::id();
        self.server_usage = samples.iter().find(|s| s.pid == pid as i32).map(|s| ServerUsage {
            pid,
            cpu_percent: cpu(&self.cpu_samples, s),
            memory_bytes: s.memory,
        });
        self.cpu_samples = samples.iter().map(|s| (s.pid, (s.cpu_ns, now))).collect();
        self.usage = usage;
    }

    // Per-batch work.

    /// When the main loop must wake up next.
    pub(crate) fn next_deadline(&self) -> Instant {
        let mut deadline = (self.last_poll + POLL_INTERVAL).min(self.last_save + SAVE_INTERVAL);
        let now = Instant::now();
        let asks = self.asks.next_due(now).into_iter();
        let waiters = self.ask_waiters.iter().filter_map(|w| w.deadline);
        let waiters = waiters.chain(self.event_waiters.iter().filter_map(|w| w.deadline));
        if let Some(due) = asks.chain(waiters).min() {
            deadline = deadline.min(due);
        }
        // (One that's behind wakes the loop when its acknowledgment comes.)
        for client in self.clients.values().filter(|c| !c.behind()) {
            for pane in &client.dirty {
                if let Some((_, sent)) = client.sent.get(pane) {
                    deadline = deadline.min(*sent + FRAME_INTERVAL);
                }
            }
        }
        deadline
    }

    pub(crate) fn after_batch(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            self.poll();
        }
        if self.asks.pending() || !self.ask_waiters.is_empty() {
            self.pump_asks(now);
        }
        self.follow_handoffs(now);
        self.follow_rewinds();
        self.follow_events(now);
        self.clear_seen();
        self.push_updates(now);
        if now.duration_since(self.last_save) >= SAVE_INTERVAL {
            self.last_save = now;
            self.keep(now);
            self.save();
        }
        if !self.panes.is_empty() || !self.clients.is_empty() {
            self.idle_since = now;
        }
    }

    /// Once a second: processes, statuses, `artty run` timeouts, config.
    /// A Claude Code conversation (resumed, or carried over from an earlier
    /// build): its title, till its agent titles the terminal (Claude Code
    /// does once given a message). Looked up once a conversation.
    fn title_session(pane: &mut Pane) {
        if matches!(&pane.status, Status::Agent { name, .. } if name == "claude")
            && let Some(session) = pane.untitled_session().map(str::to_owned)
        {
            pane.set_session_title(&session, crate::sessions::claude_title(&session));
        }
    }

    /// Claude Code turns cut short with no hook to say so (Esc fires no
    /// `Stop`): at work by its hooks, for a while, and its title at rest
    /// (`✳`, no spinner) for a while — not asking permission meanwhile.
    fn turns_cut_short(&mut self, now: Instant) {
        const SETTLED: Duration = Duration::from_secs(3);
        let cut: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, pane)| {
                matches!(
                    &pane.status,
                    Status::Agent { name, state: AgentState::Working } if name == "claude"
                )
            })
            .filter(|(_, pane)| pane.title.as_deref().is_some_and(|t| t.starts_with('✳')))
            .filter(|(_, pane)| pane.title_since().is_none_or(|at| now.duration_since(at) >= SETTLED))
            // (Not known since when: one from before this build took over.)
            .filter(|&(&id, _)| {
                self.asks.turn_open_since(id).is_none_or(|at| now.duration_since(at) >= SETTLED)
            })
            .filter(|&(&id, _)| !self.approvals.pending_in(id))
            .map(|(&id, _)| id)
            .collect();
        for pane in cut {
            self.cut_short(pane, "interrupted in its terminal", now);
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.report(AgentState::Idle, None, None, None);
            }
        }
    }

    /// `pane`'s agent's turn ended without an answer (interrupted: `how`):
    /// its ask fails, the queued ones go on, the thread says so.
    fn cut_short(&mut self, pane: PaneId, how: &str, now: Instant) {
        self.asks.turn_cut_short(pane, &format!("the agent's turn was {how} (no answer)"), now);
        let files = self.edits.take_turn(pane);
        if !files.is_empty() {
            self.threads.add(pane, ThreadItem::Edited { files });
        }
        self.threads.add(pane, ThreadItem::Interrupted { reason: how.to_owned() });
        if let Some(prompt) = self.threads.last_prompt_id(pane) {
            self.check_rewinds(pane, prompt);
        }
        for release in self.approvals.hook(
            pane,
            &artty_core::control::HookEvent { event: "Stop".into(), ..Default::default() },
            now,
        ) {
            self.release_approval(release);
        }
    }

    /// A Claude Code turn ended (`prompt`: its prompt's id): read its
    /// transcript (off this loop) for what was rewound since, to mark it.
    fn check_rewinds(&mut self, pane: PaneId, prompt: String) {
        let Some(session) = self.panes.get(&pane).and_then(|p| p.agent_session()) else { return };
        let session = session.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let wanted = prompt.clone();
        let started = std::thread::Builder::new().name("artty-rewinds".into()).spawn(move || {
            let _ = tx.send(crate::sessions::claude_prompts_kept(&session, &wanted));
        });
        if started.is_ok() {
            self.rewinds.push((pane, prompt, rx));
        }
    }

    /// What reading transcripts for rewinds found: marked in the threads.
    fn follow_rewinds(&mut self) {
        let mut done = Vec::new();
        self.rewinds.retain(|(pane, prompt, rx)| match rx.try_recv() {
            Ok(kept) => {
                done.extend(kept.map(|kept| (*pane, prompt.clone(), kept)));
                false
            },
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for (pane, prompt, kept) in done {
            self.threads.mark_rewound(pane, &kept, &prompt);
        }
    }

    fn poll(&mut self) {
        // Conversations whose agent hasn't reported since (one running when
        // this build took over): their titles too.
        for pane in self.panes.values_mut() {
            Self::title_session(pane);
        }
        self.turns_cut_short(Instant::now());
        // Files drawers following a pane's directory: it moved.
        let panes = &self.panes;
        let moved = self.watches.moved(|pane| panes.get(&pane)?.session.cwd());
        for (client, pane) in moved {
            self.send(client, ServerMessage::DirChanged { pane });
        }
        // An app that went quiet (a Mac asleep, a network gone) can take a
        // long while to be noticed as disconnected: let its windows go, for
        // another app to show. (It reconnects when it's back.)
        let silent: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.pings && c.last_heard.elapsed() >= CLIENT_SILENCE)
            .map(|(&id, _)| id)
            .collect();
        for id in silent {
            log::info!("app {id} silent for {}s: disconnecting it", CLIENT_SILENCE.as_secs());
            self.handle(Input::Disconnected(id));
        }
        if self.last_touch.is_none_or(|t| t.elapsed() >= TOUCH_INTERVAL) {
            self.last_touch = Some(Instant::now());
            touch(&self.socket);
            if let Some(dir) = self.socket.parent().filter(|_| !cfg!(target_os = "macos")) {
                touch(dir);
            }
        }
        for pane in self.panes.values_mut() {
            pane.process = pane.session.process();
            pane.refresh();
            pane.flush_pending_input();
        }
        let workspaces: Vec<WorkspaceId> = self.workspaces.keys().copied().collect();
        for workspace in workspaces {
            self.refresh_title(workspace);
        }
        self.sample_usage();
        for release in self.approvals.expire(Instant::now()) {
            self.release_approval(release);
        }
        let windows = &self.windows;
        self.status.retain(|window| windows.contains_key(&window));
        let now = Instant::now();
        for run in self.runs.extract_if(.., |run| run.deadline.is_some_and(|d| d <= now)) {
            Self::answer(&self.clients, run.waiter, Response::error("timed out"));
        }
        let modified = config_modified();
        if modified != self.config_modified {
            self.config_modified = modified;
            let (config, error) = Config::load();
            if let Some(error) = error {
                log::error!("config: {error}");
            } else {
                if !self.appearance {
                    self.theme = Arc::new(config.theme());
                    self.cursor = config.cursor_style;
                }
                self.agents = config.agents.clone().into();
                self.status.configure(&config.status);
                for pane in self.panes.values_mut() {
                    pane.session.set_theme(self.theme.clone());
                    pane.session.set_cursor_shape(self.cursor.shape());
                    pane.set_agents(self.agents.clone());
                }
                self.config = config;
                let all: Vec<PaneId> = self.panes.keys().copied().collect();
                for pane in all {
                    self.mark_dirty(pane);
                }
            }
        }
    }

    /// Panes the user is looking at have been seen.
    fn clear_seen(&mut self) {
        let seen: Vec<PaneId> = self.seen_windows().flat_map(|w| self.window_visible(w)).collect();
        // Replies are read only by someone there: an app used lately.
        let now = Instant::now();
        let someone =
            self.clients.values().any(|c| c.last_used.is_some_and(|at| now - at < IN_USE));
        for &pane in seen.iter().filter(|_| someone) {
            self.replies.read_pane(pane);
        }
        for pane in seen {
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.seen();
            }
        }
    }

    /// What `window`'s status line shows of its focused pane, and of its panes.
    fn status_facts(&self, window: WindowId) -> Option<crate::status::Facts> {
        let id = self.window_focused_pane(window)?;
        let pane = self.panes.get(&id)?;
        let state = self.windows.get(&window)?;
        let panes: Vec<PaneId> = state
            .workspaces
            .iter()
            .filter_map(|w| self.workspaces.get(w))
            .flat_map(|w| w.tabs.iter().flat_map(|t| t.root.panes()))
            .collect();
        let waiting = panes
            .iter()
            .filter_map(|p| self.panes.get(p))
            .filter(|p| matches!(p.status, Status::Agent { state: AgentState::Waiting, .. }))
            .count();
        let mut ports: Vec<u16> =
            panes.iter().filter_map(|p| self.ports.get(p)).flatten().copied().collect();
        ports.sort_unstable();
        ports.dedup();
        let (command, agent) = match &pane.status {
            Status::Running { command, .. } => (command.clone(), None),
            Status::Agent { name, .. } => (None, Some(format!("{name} {}", pane.status.label()))),
            _ => (None, None),
        };
        let last = pane.session.last_command();
        let progress = match pane.progress {
            Some(Progress::Normal(p) | Progress::Paused(Some(p)) | Progress::Error(Some(p))) => {
                Some(p)
            },
            _ => None,
        };
        Some(crate::status::Facts {
            pane: id,
            cwd: pane.session.cwd(),
            command,
            exit: pane.last_exit(),
            duration: last.and_then(|c| Some(c.finished? - c.started)),
            agent,
            waiting,
            progress,
            ports,
        })
    }

    fn pane_state(&self, pane: &Pane, info: TabInfo) -> PaneState {
        let icon = match &pane.status {
            status if status.busy() => "busy",
            Status::Agent { state: AgentState::Waiting, .. } => "waiting",
            // Marked until the user has looked.
            Status::Agent { state: AgentState::Done, .. } if !pane.done_seen => "done",
            Status::Failed { .. } => "failed",
            _ if pane.attention => "attention",
            _ => "",
        };
        let progress = match pane.progress {
            Some(Progress::Normal(p) | Progress::Paused(Some(p)) | Progress::Error(Some(p))) => {
                Some(p)
            },
            _ => None,
        };
        let ports = self.ports.get(&info.id).cloned().unwrap_or_default();
        PaneState { info, icon: icon.to_owned(), progress, agent: pane.agent_name(), ports }
    }

    /// Send each app its windows' layouts, pane states and screen updates.
    fn push_updates(&mut self, now: Instant) {
        let infos = self.pane_infos();
        // The status line of each window shown to an app taking them,
        // worked out once.
        let mut lines: HashMap<WindowId, StatusLine> = HashMap::new();
        let takes_status =
            |c: &ClientId| self.clients.get(c).is_some_and(|c| c.takes(extension::STATUS));
        let shown: Vec<WindowId> = match self.status.is_empty() {
            true => Vec::new(),
            false => self
                .windows
                .iter()
                .filter(|(_, w)| w.clients.iter().any(takes_status))
                .map(|(&id, _)| id)
                .collect(),
        };
        for window in shown {
            let facts = self.status_facts(window);
            lines.insert(window, self.status.line(window, facts.as_ref(), &self.theme));
        }
        // The queue, worked out once, for apps taking it.
        let queue = self
            .clients
            .values()
            .any(|c| c.takes(extension::QUEUE))
            .then(|| QueueUpdate { items: self.queue_items() });
        let client_ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for client_id in client_ids {
            let windows: Vec<(WindowId, WindowLayout, Vec<PaneId>)> = self
                .windows
                .iter()
                .filter(|(_, w)| w.clients.contains(&client_id))
                .map(|(&id, w)| (id, self.window_layout(w), self.window_visible(id)))
                .collect();
            let window_panes: HashSet<PaneId> = windows
                .iter()
                .flat_map(|(_, layout, _)| layout.tabs().flat_map(|(_, _, tab)| tab.root.panes()))
                .collect();
            let states: Vec<PaneState> = infos
                .iter()
                .filter(|info| window_panes.contains(&info.id))
                .filter_map(|info| Some(self.pane_state(self.panes.get(&info.id)?, info.clone())))
                .collect();
            let mut visible: Vec<PaneId> =
                windows.iter().flat_map(|(_, _, v)| v.iter().copied()).collect();
            // And the one it peeks at.
            let peek = self.clients.get(&client_id).and_then(|c| c.peek);
            if let Some(pane) = peek.filter(|p| self.panes.contains_key(p) && !visible.contains(p))
            {
                visible.push(pane);
            }

            let mut frames = Vec::new();
            {
                let client = self.clients.get_mut(&client_id).unwrap();
                // Forget panes no longer shown, so they get a full update if
                // they come back.
                client.sent.retain(|pane, _| visible.contains(pane));
                let behind = client.behind();
                for &pane in visible.iter().filter(|_| !behind) {
                    let due = match client.sent.get(&pane) {
                        None => true,
                        Some((_, at)) => {
                            client.dirty.contains(&pane)
                                && now.duration_since(*at) >= FRAME_INTERVAL
                        },
                    };
                    if due {
                        frames.push(pane);
                    }
                }
            }
            let mut messages: Vec<ServerMessage> = windows
                .into_iter()
                .map(|(window, layout, _)| ServerMessage::Layout { window, layout })
                .collect();
            for pane in frames {
                let Some(state) = self.panes.get_mut(&pane) else { continue };
                let frame = state.session.frame();
                let mode = state.session.mode().bits();
                let client = self.clients.get_mut(&client_id).unwrap();
                // Images first, each once.
                for image in &frame.images {
                    if client.images_sent.insert(image.id) {
                        messages.push(ServerMessage::Image(artty_core::protocol::ImageData {
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
                let previous = client.sent.get(&pane).map(|(f, _)| f);
                let update = artty_core::protocol::FrameUpdate::diff(previous, &frame, mode);
                client.dirty.remove(&pane);
                client.sent.insert(pane, (frame, now));
                messages.push(ServerMessage::Frame { pane, update });
            }
            let client = self.clients.get_mut(&client_id).unwrap();
            if messages.iter().any(|m| matches!(m, ServerMessage::Frame { .. })) {
                client.batches_sent += 1;
                messages.push(ServerMessage::Sync { seq: client.batches_sent });
            }
            if client.panes_sent != states {
                client.panes_sent = states.clone();
                messages.push(ServerMessage::Panes { panes: states });
            }
            let mine: Vec<WindowId> = messages
                .iter()
                .filter_map(|m| match m {
                    ServerMessage::Layout { window, .. } => Some(*window),
                    _ => None,
                })
                .collect();
            client.status_sent.retain(|window, _| mine.contains(window));
            let mine = if client.takes(extension::STATUS) { mine } else { Vec::new() };
            for window in mine {
                let line = lines.get(&window).cloned().unwrap_or_default();
                let sent = client.status_sent.get(&window);
                if sent.unwrap_or(&StatusLine::default()) != &line {
                    client.status_sent.insert(window, line.clone());
                    let update = StatusUpdate { window, line };
                    messages.push(ServerExtension::Status(update).message());
                }
            }
            if client.takes(extension::QUEUE)
                && let Some(queue) = &queue
                && client.queue_sent.as_ref() != Some(queue)
            {
                client.queue_sent = Some(queue.clone());
                messages.push(ServerExtension::Queue(queue.clone()).message());
            }
            for message in messages {
                let _ = client.tx.send(message);
            }
        }
    }

    /// What needs the user now: permission requests (oldest first), agents
    /// waiting otherwise, then unread replies (newest first).
    fn queue_items(&mut self) -> Vec<QueueItem> {
        use crate::queue::start_of;
        let waiting: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, p)| matches!(p.status, Status::Agent { state: AgentState::Waiting, .. }))
            .map(|(&id, _)| id)
            .filter(|&id| !self.approvals.pending_in(id))
            .collect();
        self.waiting_since.retain(|pane, _| waiting.contains(pane));
        for &pane in &waiting {
            self.waiting_since.entry(pane).or_insert_with(crate::queue::now_ms);
        }
        let now = Instant::now();
        let place = |pane: PaneId| -> (String, String) {
            let title = self.panes.get(&pane).map(|p| p.title()).unwrap_or_default();
            let workspace = self
                .place_of(pane)
                .and_then(|(w, _)| self.workspaces.get(&w))
                .map(|w| w.name.clone().unwrap_or_else(|| w.title.clone()))
                .unwrap_or_default();
            (title, workspace)
        };
        let mut items = Vec::new();
        for (approval, since_ms) in self.approvals.pending(now) {
            let (title, workspace) = place(approval.pane);
            items.push(QueueItem {
                kind: QueueKind::Approval,
                id: approval.id,
                pane: approval.pane,
                agent: approval.agent,
                title,
                workspace,
                text: approval.summary,
                context: start_of(&approval.tool_input.to_string(), 4000),
                options: approval.options,
                since_ms,
            });
        }
        for pane in waiting {
            let since_ms = self.waiting_since[&pane];
            let state = &self.panes[&pane];
            let (title, workspace) = place(pane);
            items.push(QueueItem {
                kind: QueueKind::Waiting,
                id: pane,
                pane,
                agent: state.agent_name(),
                title,
                workspace,
                text: state.message.clone().unwrap_or_else(|| "Waiting for you".to_owned()),
                context: String::new(),
                options: Vec::new(),
                since_ms,
            });
        }
        for reply in self.replies.unread() {
            let (title, workspace) = place(reply.pane);
            items.push(QueueItem {
                kind: QueueKind::Reply,
                id: reply.id,
                pane: reply.pane,
                agent: reply.agent.clone(),
                title,
                workspace,
                text: start_of(&reply.text, 4000),
                context: start_of(&reply.prompt, 1000),
                options: Vec::new(),
                since_ms: reply.at_ms,
            });
        }
        items
    }

    /// A tab's layout as saved; for a hand-over, each pane names its running
    /// self instead of carrying its output.
    fn saved_layout(&self, node: &Node, handover: bool) -> SavedLayout {
        match node {
            Node::Pane(id) => SavedLayout::Pane(
                self.panes
                    .get(id)
                    .map(|p| match handover {
                        true => SavedPane {
                            pane: Some(*id),
                            command: p.command.clone(),
                            thread: self.threads.key_of(*id),
                            ..p.saved(0)
                        },
                        false => SavedPane {
                            thread: self.threads.key_of(*id),
                            ..p.saved(self.config.restore_scrollback)
                        },
                    })
                    .unwrap_or_default(),
            ),
            Node::Split { axis, ratio, first, second } => SavedLayout::Split {
                vertical: *axis == Axis::Vertical,
                ratio: *ratio,
                first: Box::new(self.saved_layout(first, handover)),
                second: Box::new(self.saved_layout(second, handover)),
            },
        }
    }

    /// Keep the asks on disk (threads are kept as they change), for after a
    /// restart or a hand-over.
    fn keep(&mut self, now: Instant) {
        self.sync_permissions(now);
        let saved = self.asks.saved(now, |pane, finished| match finished {
            // (Not given a key: its pane may be gone.)
            true => self.threads.key_of(pane).unwrap_or(0),
            false => self.threads.key(pane),
        });
        let Ok(json) = serde_json::to_string(&saved) else { return };
        if self.last_kept.as_ref() != Some(&json) {
            self.threads.store().set_record("asks", &json);
            self.last_kept = Some(json);
        }
        let replies = self.replies.saved(|pane| self.threads.key(pane));
        let Ok(json) = serde_json::to_string(&replies) else { return };
        if self.last_kept_replies.as_ref() != Some(&json) {
            self.threads.store().set_record("replies", &json);
            self.last_kept_replies = Some(json);
        }
        // Which agent started which, by their threads' keys (panes' ids
        // change in a restart).
        // (`true`: a copy, listed under the agent it copies.)
        let mut parents: Vec<(PaneId, PaneId, bool)> = self
            .parents
            .iter()
            .map(|(&child, &parent)| (child, parent, false))
            .chain(self.copied_from.iter().map(|(&child, &source)| (child, source, true)))
            .filter(|(child, parent, _)| {
                self.panes.contains_key(child) && self.panes.contains_key(parent)
            })
            .collect();
        parents.sort();
        let parents: Vec<(u64, u64, bool)> = parents
            .into_iter()
            .map(|(child, parent, copy)| (self.threads.key(child), self.threads.key(parent), copy))
            .collect();
        let Ok(json) = serde_json::to_string(&parents) else { return };
        if self.last_kept_parents.as_ref() != Some(&json) {
            self.threads.store().set_record("parents", &json);
            self.last_kept_parents = Some(json);
        }
    }

    /// Threads' permission requests brought up to date (answered since).
    fn sync_permissions(&mut self, now: Instant) {
        for id in self.threads.pending_permissions() {
            if let Some(info) = self.approvals.info(id, now) {
                self.threads.permission(id, info.state);
            }
        }
    }

    /// Started (from saved windows, or a hand-over): take back the asks kept
    /// on disk, and forget the threads of panes that didn't come back.
    pub(crate) fn restored(&mut self) {
        let now = Instant::now();
        let saved: Vec<crate::asks::SavedAsk> = self
            .threads
            .store()
            .record("asks")
            .and_then(|json| {
                serde_json::from_str(&json)
                    .inspect_err(|err| log::warn!("kept asks unreadable: {err}"))
                    .ok()
            })
            .unwrap_or_default();
        let back = |key: u64| {
            let pane = self.threads.pane_of(key)?;
            Some(match self.adopted.contains(&pane) {
                true => crate::asks::Back::Running(pane),
                false => crate::asks::Back::Reopened(pane),
            })
        };
        self.asks.restore(saved, now, back);
        let replies: crate::queue::SavedReplies = self
            .threads
            .store()
            .record("replies")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        self.replies.restore(replies, |key| self.threads.pane_of(key));
        let parents: Vec<(u64, u64, bool)> = self
            .threads
            .store()
            .record("parents")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        for (child, parent, copy) in parents {
            if let (Some(child), Some(parent)) =
                (self.threads.pane_of(child), self.threads.pane_of(parent))
            {
                match copy {
                    false => self.parents.insert(child, parent),
                    true => self.copied_from.insert(child, parent),
                };
            }
        }
        self.adopted.clear();
        self.threads.forget_others();
        self.keep(now);
    }

    /// Save every window for the next start (after a reboot or crash).
    fn save(&mut self) {
        if !self.config.restore_sessions {
            return;
        }
        let state = self.saved_state(false);
        let Ok(json) = serde_json::to_string(&state) else { return };
        if self.last_saved.as_ref() == Some(&json) {
            return;
        }
        match state.save() {
            Ok(()) => self.last_saved = Some(json),
            Err(err) => log::error!("saving: {err}"),
        }
    }

    fn saved_state(&self, handover: bool) -> SavedState {
        let saved_workspace = |workspace: &Workspace| SavedWorkspace {
            name: workspace.name.clone(),
            active: workspace.active,
            collapsed: workspace.collapsed,
            root: workspace.defaults.root.clone(),
            color: workspace.color.clone(),
            env: workspace.defaults.env.clone(),
            tabs: workspace
                .tabs
                .iter()
                .map(|tab| SavedTab {
                    title: tab.title.clone(),
                    layout: self.saved_layout(&tab.root, handover),
                    focused: tab.root.panes().iter().position(|&id| id == tab.focused).unwrap_or(0),
                    zoomed: tab.zoomed,
                })
                .collect(),
        };
        let windows = self
            .windows
            .values()
            .map(|window| {
                let workspaces = window
                    .workspaces
                    .iter()
                    .filter_map(|id| self.workspaces.get(id))
                    .map(saved_workspace)
                    .collect();
                SavedWindowState::new(window.bounds, window.active, workspaces)
            })
            .collect();
        let hidden = self
            .workspaces
            .iter()
            .filter(|(id, _)| self.window_of_workspace(**id).is_none())
            .map(|(_, workspace)| saved_workspace(workspace))
            .collect();
        SavedState::new(windows, hidden)
    }

    // Handing over to a new build (see `handover`).

    pub(crate) fn take_handover(&mut self) -> Option<PathBuf> {
        self.handover_to.take()
    }

    /// Hand every session to the build at `executable` by becoming it.
    /// Returns only if that failed, with everything running as before.
    pub(crate) fn hand_over(&mut self, executable: &Path) -> std::io::Error {
        let copy = match crate::server_copy(executable) {
            Ok(copy) => copy,
            Err(err) => return err,
        };
        // Once `exec`ed there's no way back, so make sure first that the
        // build can take over, and reads what this one writes.
        let check = std::process::Command::new(&copy).args(["server", "--handover-check"]).output();
        let expected = crate::handover_check();
        match check {
            Ok(out) if String::from_utf8_lossy(&out.stdout).trim() == expected => {},
            Ok(out) => {
                let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                let err =
                    std::io::Error::other(format!("that build can't take over (it said {said:?})"));
                log::error!("hand-over called off: {err}");
                return err;
            },
            Err(err) => {
                log::error!("hand-over called off: {err}");
                return err;
            },
        }
        // Apps reconnect, to the new build.
        for client in self.clients.values() {
            let _ = client.tx.send(ServerMessage::Stopping { restart: true });
        }
        std::thread::sleep(Duration::from_millis(150));
        // Every reader stops with what it read parsed; output waits in the PTY.
        let resumes: Vec<_> = self.panes.values().map(|pane| pane.session.freeze()).collect();
        let resume = |err: std::io::Error| {
            for resume in &resumes {
                let _ = resume.send(());
            }
            log::error!("hand-over failed, carrying on: {err}");
            err
        };
        let mut panes = Vec::new();
        let mut snapshots = Vec::new();
        for (&id, pane) in &self.panes {
            if let Some((session, snapshot)) = pane.session.handover() {
                panes.push(HandedPane { session, meta: pane.meta() });
                snapshots.push((id, snapshot));
            }
        }
        self.keep(Instant::now());
        let header = handover::Header {
            version: 0,
            listener_fd: self.listener_fd,
            next_pane: self.next_pane,
            state: self.saved_state(true),
            panes,
        };
        let path = match handover::write(header, &snapshots) {
            Ok(path) => path,
            Err(err) => return resume(err),
        };
        // SAFETY: plain syscall; the socket must survive `exec`.
        unsafe { libc::fcntl(self.listener_fd, libc::F_SETFD, 0) };
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&copy).arg("server").arg("--resume").arg(&path).exec();
        // Still here: `exec` failed.
        unsafe { libc::fcntl(self.listener_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        resume(err)
    }

    /// Start from what a previous build handed over (instead of `start`).
    pub(crate) fn adopt(
        &mut self,
        header: handover::Header,
        mut snapshots: HashMap<PaneId, artty_core::vt::term::TermSnapshot>,
    ) {
        self.next_pane = self.next_pane.max(header.next_pane);
        for pane in header.panes {
            let id = pane.session.id;
            self.handed.insert(id, (pane, snapshots.remove(&id)));
        }
        let mut state = header.state;
        let hidden = std::mem::take(&mut state.hidden);
        for saved in state.into_windows() {
            let workspaces: Vec<Workspace> =
                saved.workspaces.into_iter().filter_map(|w| self.restore_workspace(w)).collect();
            if !workspaces.is_empty() {
                self.add_window(workspaces, saved.active, saved.window);
            }
        }
        for saved in hidden {
            if let Some(workspace) = self.restore_workspace(saved) {
                self.add_workspace(workspace);
            }
        }
        // Anything the layout didn't place still runs: keep it reachable.
        let left: Vec<PaneId> = self.handed.keys().copied().collect();
        let mut tabs = Vec::new();
        for id in left {
            let (handed, snapshot) = self.handed.remove(&id).unwrap();
            if let Some(pane) = self.adopt_pane(handed, snapshot, SavedPane::default()) {
                tabs.push(Tab::new(pane));
            }
        }
        if !tabs.is_empty() {
            self.add_workspace(Workspace::new(Some("Recovered".into()), tabs, 0));
        }
        log::warn!("took over {} panes from the previous server", self.panes.len());
    }

    fn adopt_pane(
        &mut self,
        handed: HandedPane,
        snapshot: Option<artty_core::vt::term::TermSnapshot>,
        saved: SavedPane,
    ) -> Option<PaneId> {
        let id = handed.session.id;
        let options = SessionOptions {
            scrollback: Some(self.config.scrollback),
            cursor_shape: self.cursor.shape(),
            ..Default::default()
        };
        let session = Session::adopt(
            handed.session,
            snapshot,
            options,
            self.theme.clone(),
            self.sink.clone(),
        )
        .inspect_err(|err| log::error!("taking over pane {id}: {err}"))
        .ok()?;
        let mut pane = Pane::new(session, saved.title, saved.command, self.agents.clone());
        pane.restore_meta(handed.meta);
        self.panes.insert(id, pane);
        self.next_pane = self.next_pane.max(id + 1);
        self.had_panes = true;
        Some(id)
    }

    pub(crate) fn should_exit(&self) -> bool {
        self.stopping
            || (self.panes.is_empty() && self.had_panes)
            || (self.panes.is_empty()
                && self.clients.is_empty()
                && self.idle_since.elapsed() >= IDLE_EXIT)
    }

    pub(crate) fn shutdown(&mut self) {
        if self.stopping && !self.restarting {
            // Ended on purpose: nothing to reopen.
            self.panes.clear();
            self.windows.clear();
            self.workspaces.clear();
        }
        self.keep(Instant::now());
        self.save();
        // Dropping sessions hangs up their shells.
        self.panes.clear();
        // Tell apps whether to reconnect; dropping the senders lets the
        // writers finish.
        for client in std::mem::take(&mut self.clients).into_values() {
            let _ = client.tx.send(ServerMessage::Stopping { restart: self.restarting });
        }
    }
}
