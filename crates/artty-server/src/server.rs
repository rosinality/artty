//! The server's state: windows of tabs of panes, the apps attached to them,
//! and everything that happens to the sessions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{
    AgentState, PaneProcesses, ProcessRow, ReadSource, Request, Response, SESSION_ENV, SOCKET_ENV,
    ServerInfo, SplitDirection, TabInfo,
};
use artty_core::layout::{Axis, Node, PaneId};
use artty_core::pane::{Pane, Status};
use artty_core::protocol::{
    AppCommand, ClientMessage, PROTOCOL_VERSION, PaneSpec, PaneState, Query, Reply, SelectionOp,
    ServerMessage, TabLayout, WindowId, WindowLayout, WindowSummary,
};
use artty_core::vt::ansi::Progress;
use artty_core::vt::event::Event;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::term::TermMode;
use artty_core::{
    Config, Frame, Notice, SavedLayout, SavedState, SavedTab, SavedWindow, SavedWorkspace, Session,
    SessionOptions, ShellPhase, TerminalSize, Theme, process_tree, processes,
};

use crate::{ClientId, Input};

/// Each visible pane gets at most one screen update per frame at 120 Hz.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// A server with no sessions and no app exits after this long.
const IDLE_EXIT: Duration = Duration::from_secs(20);
/// Background commands running at least this long notify when they finish.
const LONG_COMMAND: Duration = Duration::from_secs(10);
/// Shown under a restored pane's old output.
const RESTORED_BANNER: &str = "\r\n\x1b[2m── restored ──\x1b[0m\r\n";
const DEFAULT_SIZE: TerminalSize = TerminalSize { columns: 100, lines: 30, cell_width: 8, cell_height: 18 };

struct Tab {
    title: Option<String>,
    root: Node,
    focused: PaneId,
    zoomed: bool,
}

impl Tab {
    fn visible(&self) -> Vec<PaneId> {
        if self.zoomed { vec![self.focused] } else { self.root.panes() }
    }
}

struct Window {
    tabs: Vec<Tab>,
    active: usize,
    bounds: Option<SavedWindow>,
    /// The app showing this window, if any.
    client: Option<ClientId>,
    focused: bool,
    /// When it last had focus, to find the frontmost window.
    last_focus: Instant,
}

impl Window {
    fn visible(&self) -> Vec<PaneId> {
        self.tabs.get(self.active).map(Tab::visible).unwrap_or_default()
    }

    fn focused_pane(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|tab| tab.focused)
    }

    fn layout(&self) -> WindowLayout {
        WindowLayout {
            tabs: self
                .tabs
                .iter()
                .map(|tab| TabLayout { title: tab.title.clone(), root: tab.root.clone(), focused: tab.focused, zoomed: tab.zoomed })
                .collect(),
            active: self.active,
        }
    }
}

struct Client {
    tx: Sender<ServerMessage>,
    /// Last screen sent per visible pane, for sending only changes.
    sent: HashMap<PaneId, (Frame, Instant)>,
    dirty: HashSet<PaneId>,
    panes_sent: Vec<PaneState>,
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

pub(crate) struct Server {
    socket: PathBuf,
    sink: Arc<dyn Fn(PaneId, Event) + Send + Sync>,
    started: Instant,
    config: Config,
    config_modified: Option<SystemTime>,
    theme: Arc<Theme>,
    agents: Arc<[String]>,
    cli_dir: Option<PathBuf>,
    integration_dir: Option<PathBuf>,
    panes: HashMap<PaneId, Pane>,
    windows: BTreeMap<WindowId, Window>,
    clients: HashMap<ClientId, Client>,
    runs: Vec<PendingRun>,
    next_pane: PaneId,
    next_window: WindowId,
    had_panes: bool,
    idle_since: Instant,
    stopping: bool,
    /// Stopping, but save the sessions for the next server to reopen.
    restarting: bool,
    last_poll: Instant,
    last_save: Instant,
    last_saved: Option<String>,
    /// Previous CPU samples (pid → time used, when), for CPU percentages.
    cpu_samples: HashMap<i32, (u64, Instant)>,
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

/// Put an `artty` command on the PATH of shells: a directory holding only a
/// link to this executable.
fn cli_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = std::env::temp_dir().join("artty-bin");
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join("artty");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(exe, &link).ok()?;
    Some(dir)
}

fn osascript_notify(title: &str, body: &str) {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn pbcopy(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn() {
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
    pub(crate) fn new(socket: PathBuf, tx: Sender<Input>) -> Self {
        let (config, error) = Config::load();
        if let Some(error) = error {
            log::error!("config: {error}");
        }
        let sink: Arc<dyn Fn(PaneId, Event) + Send + Sync> = Arc::new(move |id, event| {
            let _ = tx.send(Input::Session(id, event));
        });
        let now = Instant::now();
        Self {
            socket,
            sink,
            started: now,
            theme: Arc::new(config.theme()),
            agents: config.agents.clone().into(),
            config,
            config_modified: config_modified(),
            cli_dir: cli_dir(),
            integration_dir: artty_core::integration::install().ok(),
            panes: HashMap::new(),
            windows: BTreeMap::new(),
            clients: HashMap::new(),
            runs: Vec::new(),
            next_pane: 1,
            next_window: 1,
            had_panes: false,
            idle_since: now,
            stopping: false,
            restarting: false,
            last_poll: now,
            last_save: now,
            last_saved: None,
            cpu_samples: HashMap::new(),
        }
    }

    /// Reopen saved windows (detached until the app attaches).
    pub(crate) fn start(&mut self) {
        if !self.config.restore_sessions {
            return;
        }
        let Some(state) = SavedState::load() else { return };
        for saved in state.into_windows() {
            let mut tabs = Vec::new();
            for tab in saved.tabs {
                let Some(root) = self.restore_layout(tab.layout) else { continue };
                let panes = root.panes();
                let focused = panes[tab.focused.min(panes.len() - 1)];
                tabs.push(Tab { title: tab.title, root, focused, zoomed: tab.zoomed && panes.len() > 1 });
            }
            if tabs.is_empty() {
                continue;
            }
            let active = saved.active.min(tabs.len() - 1);
            self.add_window(tabs, active, saved.window);
        }
    }

    fn add_window(&mut self, tabs: Vec<Tab>, active: usize, bounds: Option<SavedWindow>) -> WindowId {
        let id = self.next_window;
        self.next_window += 1;
        self.windows.insert(id, Window { tabs, active, bounds, client: None, focused: false, last_focus: Instant::now() });
        id
    }

    fn restore_layout(&mut self, layout: SavedLayout) -> Option<Node> {
        match layout {
            SavedLayout::Pane(saved) => {
                let initial_output = (!saved.scrollback.is_empty())
                    .then(|| format!("{}{RESTORED_BANNER}", saved.scrollback).into_bytes());
                let spec = PaneSpec { command: saved.command, cwd: saved.cwd, title: saved.title };
                let id = self.spawn_pane(spec, None, initial_output).inspect_err(|err| log::error!("restoring: {err}")).ok()?;
                let resume = saved.agent.filter(|_| self.config.resume_agents).and_then(|agent| {
                    self.config.resume.get(&agent.name)?.command(agent.session.as_deref())
                });
                if let (Some(resume), Some(pane)) = (resume, self.panes.get_mut(&id)) {
                    pane.type_when_ready(resume);
                }
                Some(Node::Pane(id))
            }
            SavedLayout::Split { vertical, ratio, first, second } => {
                match (self.restore_layout(*first), self.restore_layout(*second)) {
                    (Some(first), Some(second)) => Some(Node::Split {
                        axis: if vertical { Axis::Vertical } else { Axis::Horizontal },
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    // Panes, tabs, windows.

    /// Start a session for a new pane (not yet placed in a tab). `near` is a
    /// pane whose directory to start in.
    fn spawn_pane(&mut self, spec: PaneSpec, near: Option<PaneId>, initial_output: Option<Vec<u8>>) -> Result<PaneId, String> {
        let id = self.next_pane;
        self.next_pane += 1;
        let explicit = spec.command.clone().filter(|c| !c.is_empty());
        let mut command = explicit.clone().or_else(|| Some(self.config.shell.clone())).filter(|c| !c.is_empty());
        let cwd = spec.cwd.or_else(|| self.panes.get(&near?)?.session.cwd());

        let mut env = HashMap::new();
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
        }
        let size = near.and_then(|p| self.panes.get(&p)).map_or(DEFAULT_SIZE, |pane| pane.session.size());
        let options = SessionOptions {
            command: command.map(|mut c| (c.remove(0), c)),
            working_directory: cwd.filter(|dir| dir.is_dir()),
            env,
            scrollback: Some(self.config.scrollback),
            initial_output,
            cursor_shape: self.config.cursor_style.shape(),
        };
        let session = Session::spawn(id, options, size, self.theme.clone(), self.sink.clone())
            .map_err(|err| format!("failed to start: {err}"))?;
        self.panes.insert(id, Pane::new(session, spec.title, explicit, self.agents.clone()));
        self.had_panes = true;
        Ok(id)
    }

    fn window_of(&self, pane: PaneId) -> Option<(WindowId, usize)> {
        self.windows.iter().find_map(|(&id, window)| {
            window.tabs.iter().position(|tab| tab.root.contains(pane)).map(|tab| (id, tab))
        })
    }

    /// The window the user last used.
    fn frontmost(&self) -> Option<WindowId> {
        let attached = self.windows.iter().filter(|(_, w)| w.client.is_some());
        attached.max_by_key(|(_, w)| w.last_focus).or_else(|| self.windows.iter().max_by_key(|(_, w)| w.last_focus)).map(|(&id, _)| id)
    }

    fn focused_pane(&self) -> Option<PaneId> {
        self.windows.get(&self.frontmost()?)?.focused_pane()
    }

    /// Whether the user can see a pane right now.
    fn visible(&self, pane: PaneId) -> bool {
        self.windows.values().any(|w| w.client.is_some() && w.focused && w.visible().contains(&pane))
    }

    fn open_tab(&mut self, window: WindowId, spec: PaneSpec, focus: bool) -> Result<PaneId, String> {
        let near = self.windows.get(&window).and_then(Window::focused_pane);
        let id = self.spawn_pane(spec, near, None)?;
        let window = self.windows.get_mut(&window).ok_or("no such window")?;
        window.tabs.push(Tab { title: None, root: Node::Pane(id), focused: id, zoomed: false });
        if focus {
            window.active = window.tabs.len() - 1;
        }
        Ok(id)
    }

    fn split(&mut self, target: PaneId, axis: Axis, spec: PaneSpec, focus: bool) -> Result<PaneId, String> {
        let (window, tab) = self.window_of(target).ok_or_else(|| format!("no pane {target}"))?;
        let id = self.spawn_pane(spec, Some(target), None)?;
        let window = self.windows.get_mut(&window).unwrap();
        let tab_state = &mut window.tabs[tab];
        tab_state.root.split(target, id, axis);
        tab_state.zoomed = false;
        if focus {
            tab_state.focused = id;
            window.active = tab;
        }
        Ok(id)
    }

    fn focus_pane(&mut self, pane: PaneId) {
        let Some((window_id, tab)) = self.window_of(pane) else { return };
        let before = self.focused_pane();
        let window = self.windows.get_mut(&window_id).unwrap();
        window.active = tab;
        let tab = &mut window.tabs[tab];
        if tab.zoomed && tab.focused != pane {
            tab.zoomed = false;
        }
        tab.focused = pane;
        self.report_focus_change(before);
    }

    /// Tell programs that track focus (vim, etc.) that it moved.
    fn report_focus_change(&self, before: Option<PaneId>) {
        let after = self.focused_pane();
        let window_focused = self.frontmost().and_then(|w| self.windows.get(&w)).is_some_and(|w| w.focused);
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

    /// Close a pane; its tab closes with its last pane, and its window with
    /// its last tab.
    fn close_pane(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
        self.fail_runs(pane, "the pane closed");
        let Some((window_id, tab)) = self.window_of(pane) else { return };
        let window = self.windows.get_mut(&window_id).unwrap();
        let tab_state = &mut window.tabs[tab];
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
            }
            None => self.remove_tab(window_id, tab),
        }
    }

    fn close_tab(&mut self, window: WindowId, tab: usize) {
        let Some(panes) = self.windows.get(&window).and_then(|w| w.tabs.get(tab)).map(|t| t.root.panes()) else { return };
        for pane in panes {
            self.panes.remove(&pane);
            self.fail_runs(pane, "the pane closed");
        }
        self.remove_tab(window, tab);
    }

    fn remove_tab(&mut self, window_id: WindowId, tab: usize) {
        let Some(window) = self.windows.get_mut(&window_id) else { return };
        window.tabs.remove(tab);
        if window.tabs.is_empty() {
            let client = window.client;
            self.windows.remove(&window_id);
            if let Some(client) = client {
                self.send(client, ServerMessage::WindowClosed { window: window_id });
            }
            return;
        }
        if tab < window.active || window.active >= window.tabs.len() {
            window.active = window.active.saturating_sub(1);
        }
    }

    fn fail_runs(&mut self, pane: PaneId, message: &str) {
        for run in self.runs.extract_if(.., |run| run.pane == pane) {
            Self::answer(&self.clients, run.waiter, Response::error(message));
        }
    }

    fn answer(clients: &HashMap<ClientId, Client>, waiter: Waiter, response: Response) {
        match waiter {
            Waiter::Command(reply) => {
                let _ = reply.send(response);
            }
            Waiter::App(client, id) => {
                if let Some(client) = clients.get(&client) {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    let _ = client.tx.send(ServerMessage::Control { id, response: json });
                }
            }
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
        let body = if body.is_empty() { "Needs your attention".to_owned() } else { body.to_owned() };
        // Attached apps post native notifications; otherwise use osascript.
        let client = self.frontmost().and_then(|w| self.windows.get(&w)?.client).or_else(|| self.clients.keys().next().copied());
        match client {
            Some(client) => self.send(client, ServerMessage::Notify { title, body }),
            None => osascript_notify(&title, &body),
        }
    }

    // Inputs.

    pub(crate) fn handle(&mut self, input: Input) {
        match input {
            Input::Session(id, event) => self.handle_event(id, event),
            Input::Control(request, reply) => match request {
                Request::Run { tab, command, timeout_secs } => match self.start_run(tab, &command) {
                    Ok(pane) => {
                        let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
                        self.runs.push(PendingRun { pane, waiter: Waiter::Command(reply), deadline });
                    }
                    Err(message) => {
                        let _ = reply.send(Response::error(message));
                    }
                },
                request => {
                    let _ = reply.send(self.respond(request).unwrap_or_else(Response::error));
                }
            },
            Input::Connected(id, tx) => {
                let _ = tx.send(ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    server_version: env!("CARGO_PKG_VERSION").to_owned(),
                });
                self.clients.insert(id, Client { tx, sent: HashMap::new(), dirty: HashSet::new(), panes_sent: Vec::new() });
            }
            Input::Client(id, message) => self.handle_client(id, message),
            Input::Disconnected(id) => {
                self.clients.remove(&id);
                // Its windows keep running, detached.
                for window in self.windows.values_mut().filter(|w| w.client == Some(id)) {
                    window.client = None;
                    window.focused = false;
                }
                for run in self.runs.extract_if(.., |run| matches!(run.waiter, Waiter::App(client, _) if client == id)) {
                    drop(run);
                }
            }
        }
    }

    fn handle_event(&mut self, id: PaneId, event: Event) {
        let visible = self.visible(id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let Some(notice) = pane.session.handle(event) else { return };
        match notice {
            Notice::Redraw => {}
            Notice::Title(title) => pane.set_title(Some(title)),
            Notice::ResetTitle => pane.set_title(None),
            Notice::Bell => {
                if visible {
                    let client = self.windows.values().find(|w| w.focused && w.visible().contains(&id)).and_then(|w| w.client);
                    if let Some(client) = client {
                        self.send(client, ServerMessage::Bell { pane: id });
                    }
                } else {
                    pane.alert(None);
                    self.notify_user(id, None, "Bell");
                }
            }
            Notice::Notification { title, body } => {
                pane.alert(Some(body.clone()).filter(|b| !b.is_empty()).or(title.clone()));
                self.notify_user(id, title.as_deref(), &body);
            }
            Notice::Progress(progress) => pane.progress = (progress != Progress::Remove).then_some(progress),
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
            }
            Notice::ClipboardStore(_, text) => pbcopy(&text),
            Notice::ClipboardLoad(_, format) => pane.session.reply_clipboard(&format, &pbpaste()),
            Notice::Exited => {
                self.close_pane(id);
                return;
            }
        }
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.refresh();
        }
        self.mark_dirty(id);
    }

    fn handle_client(&mut self, client: ClientId, message: ClientMessage) {
        match message {
            ClientMessage::AttachWindow { window, spec, bounds } => {
                let window = match window.filter(|w| self.windows.get(w).is_some_and(|w| w.client.is_none())) {
                    Some(window) => window,
                    None => {
                        let near = self.focused_pane();
                        match self.spawn_pane(spec, near, None) {
                            Ok(pane) => {
                                let tab = Tab { title: None, root: Node::Pane(pane), focused: pane, zoomed: false };
                                self.add_window(vec![tab], 0, bounds)
                            }
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            }
                        }
                    }
                };
                let state = self.windows.get_mut(&window).unwrap();
                state.client = Some(client);
                state.last_focus = Instant::now();
                let bounds = state.bounds;
                self.send(client, ServerMessage::Attached { window, bounds });
                if let Some(c) = self.clients.get_mut(&client) {
                    c.panes_sent.clear();
                }
            }
            ClientMessage::DetachWindow { window } => {
                if let Some(state) = self.windows.get_mut(&window).filter(|w| w.client == Some(client)) {
                    state.client = None;
                    state.focused = false;
                }
            }
            ClientMessage::WindowBounds { window, bounds } => {
                if let Some(state) = self.windows.get_mut(&window) {
                    state.bounds = Some(bounds);
                }
            }
            ClientMessage::WindowFocused { window, focused } => {
                let before = self.focused_pane();
                if let Some(state) = self.windows.get_mut(&window) {
                    state.focused = focused;
                    if focused {
                        state.last_focus = Instant::now();
                    }
                }
                if focused {
                    self.report_focus_change(before);
                    self.report_focus(self.focused_pane(), true);
                } else {
                    self.report_focus(self.windows.get(&window).and_then(Window::focused_pane), false);
                }
            }
            ClientMessage::NewTab { window, spec } => {
                if let Err(err) = self.open_tab(window, spec, true) {
                    log::error!("{err}");
                }
            }
            ClientMessage::Split { pane, axis, spec } => {
                if let Err(err) = self.split(pane, axis, spec, true) {
                    log::error!("{err}");
                }
            }
            ClientMessage::ClosePane { pane } => self.close_pane(pane),
            ClientMessage::CloseTab { window, tab } => self.close_tab(window, tab),
            ClientMessage::SelectTab { window, tab } => {
                let before = self.focused_pane();
                if let Some(state) = self.windows.get_mut(&window).filter(|w| tab < w.tabs.len()) {
                    state.active = tab;
                }
                self.report_focus_change(before);
            }
            ClientMessage::FocusPane { pane } => self.focus_pane(pane),
            ClientMessage::MoveTab { window, from, to } => {
                if let Some(state) = self.windows.get_mut(&window)
                    && from < state.tabs.len()
                    && to < state.tabs.len()
                    && from != to
                {
                    let active = state.tabs[state.active].focused;
                    let tab = state.tabs.remove(from);
                    state.tabs.insert(to, tab);
                    state.active = state.tabs.iter().position(|t| t.focused == active).unwrap_or(0);
                }
            }
            ClientMessage::RenameTab { window, tab, title } => {
                if let Some(tab) = self.windows.get_mut(&window).and_then(|w| w.tabs.get_mut(tab)) {
                    tab.title = title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
                }
            }
            ClientMessage::ToggleZoom { window } => {
                if let Some(tab) = self.windows.get_mut(&window).and_then(|w| w.tabs.get_mut(w.active))
                    && matches!(tab.root, Node::Split { .. })
                {
                    tab.zoomed = !tab.zoomed;
                }
            }
            ClientMessage::SetRatio { window, path, ratio } => {
                if let Some(tab) = self.windows.get_mut(&window).and_then(|w| w.tabs.get_mut(w.active)) {
                    tab.root.set_ratio(&path, ratio);
                }
            }
            ClientMessage::Input { pane, bytes } => {
                if let Some(pane) = self.panes.get(&pane) {
                    pane.session.write(bytes);
                }
            }
            ClientMessage::Resize { pane, size } => {
                if let Some(pane) = self.panes.get_mut(&pane) {
                    pane.session.resize(size);
                }
                self.mark_dirty(pane);
            }
            ClientMessage::Scroll { pane, lines } => {
                if let Some(pane) = self.panes.get(&pane) {
                    pane.session.scroll(lines);
                }
                self.mark_dirty(pane);
            }
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
                        }
                        SelectionOp::Update { line, column, right } => state.session.update_selection(line, column, side(right)),
                        SelectionOp::Clear => state.session.clear_selection(),
                    }
                }
                self.mark_dirty(pane);
            }
            ClientMessage::Search { id, pane, query, older, restart } => {
                let result = match self.panes.get_mut(&pane) {
                    Some(state) => {
                        if restart {
                            state.session.clear_selection();
                        }
                        if query.is_empty() { Ok(false) } else { state.session.search(&query, older) }
                    }
                    None => Err("no such pane".into()),
                };
                self.send(client, ServerMessage::Reply { id, reply: Reply::Search(result) });
                self.mark_dirty(pane);
            }
            ClientMessage::EndSearch { pane } => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.session.end_search();
                }
                self.mark_dirty(pane);
            }
            ClientMessage::ClearBuffer { pane } => {
                if let Some(state) = self.panes.get(&pane) {
                    state.session.clear_buffer();
                }
                self.mark_dirty(pane);
            }
            ClientMessage::Query { id, query } => {
                let reply = match query {
                    Query::DetachedWindows => Reply::Windows(
                        self.windows
                            .iter()
                            .filter(|(_, w)| w.client.is_none())
                            .map(|(&window, w)| WindowSummary { window, tabs: w.tabs.len(), bounds: w.bounds })
                            .collect(),
                    ),
                    Query::LinkSpan { pane, line, column } => Reply::Link(
                        self.panes
                            .get(&pane)
                            .and_then(|p| p.session.link_span(line, column))
                            .map(|(url, span)| (url, span.start, span.end)),
                    ),
                    Query::SelectionText { pane } => Reply::Text(self.panes.get(&pane).and_then(|p| p.session.selection_text())),
                    Query::Cwd { pane } => Reply::Path(self.panes.get(&pane).and_then(|p| p.session.cwd())),
                };
                self.send(client, ServerMessage::Reply { id, reply });
            }
            ClientMessage::Control { id, request } => {
                let request = match serde_json::from_str::<Request>(&request) {
                    Ok(request) => request,
                    Err(err) => {
                        let response = Response::error(format!("bad request: {err}"));
                        self.send(client, ServerMessage::Control { id, response: serde_json::to_string(&response).unwrap_or_default() });
                        return;
                    }
                };
                match request {
                    Request::Run { tab, command, timeout_secs } => match self.start_run(tab, &command) {
                        Ok(pane) => {
                            let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
                            self.runs.push(PendingRun { pane, waiter: Waiter::App(client, id), deadline });
                        }
                        Err(message) => Self::answer(&self.clients, Waiter::App(client, id), Response::error(message)),
                    },
                    request => {
                        let response = self.respond(request).unwrap_or_else(Response::error);
                        Self::answer(&self.clients, Waiter::App(client, id), response);
                    }
                }
            }
        }
    }

    // Control requests (the `artty` command and agent hooks).

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
        if session.shell_phase() != ShellPhase::Prompt || self.runs.iter().any(|run| run.pane == id) {
            return Err("the pane is busy".into());
        }
        session.write(format!("{command}\r").into_bytes());
        Ok(id)
    }

    /// Window numbers as shown to users: 1, 2, ... in opening order.
    fn window_number(&self, window: WindowId) -> usize {
        self.windows.keys().position(|&w| w == window).map_or(0, |i| i + 1)
    }

    fn pane_infos(&self) -> Vec<TabInfo> {
        let front = self.frontmost();
        let mut infos = Vec::new();
        for (&window_id, window) in &self.windows {
            for (index, tab) in window.tabs.iter().enumerate() {
                for id in tab.root.panes() {
                    let focused = tab.focused == id;
                    let active = Some(window_id) == front && focused && index == window.active;
                    if let Some(pane) = self.panes.get(&id) {
                        infos.push(pane.info(self.window_number(window_id), index + 1, active, focused));
                    }
                }
            }
        }
        infos
    }

    fn app_client(&self) -> Option<ClientId> {
        self.frontmost().and_then(|w| self.windows.get(&w)?.client).or_else(|| self.clients.keys().next().copied())
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
                            return Err("this pane's shell doesn't report commands (no shell integration)".into());
                        }
                        let last = session.last_command().ok_or("no command has finished yet")?;
                        return Ok(Response::Command {
                            command: last.command.clone(),
                            output: last.output.clone(),
                            exit: last.exit,
                            truncated: last.truncated,
                        });
                    }
                };
                Response::Text { text }
            }
            Request::Send { tab, text, enter } => {
                let session = &self.panes[&self.resolve(tab)?].session;
                let mut bytes = text.into_bytes();
                if enter {
                    bytes.push(b'\r');
                }
                session.write(bytes);
                Response::Ok
            }
            Request::New { cwd, command, title, focus, split, target } => {
                let command = (!command.is_empty()).then_some(command);
                let spec = PaneSpec { command, cwd, title };
                let id = match split {
                    Some(direction) => {
                        let target = self.resolve(target)?;
                        let axis = match direction {
                            SplitDirection::Right => Axis::Horizontal,
                            SplitDirection::Down => Axis::Vertical,
                        };
                        self.split(target, axis, spec, focus)?
                    }
                    None => match self.frontmost() {
                        Some(window) => self.open_tab(window, spec, focus)?,
                        // No windows: a detached one, shown when the app attaches.
                        None => {
                            let pane = self.spawn_pane(spec, None, None)?;
                            let tab = Tab { title: None, root: Node::Pane(pane), focused: pane, zoomed: false };
                            self.add_window(vec![tab], 0, None);
                            pane
                        }
                    },
                };
                if focus {
                    self.focus_pane(id);
                }
                Response::Tab { id }
            }
            Request::Focus { tab } => {
                let id = self.resolve(Some(tab))?;
                self.focus_pane(id);
                if let Some((window, _)) = self.window_of(id)
                    && let Some(client) = self.windows[&window].client
                {
                    self.send(client, ServerMessage::App(AppCommand::Activate { window }));
                }
                Response::Ok
            }
            Request::Close { tab } => {
                let id = self.resolve(Some(tab))?;
                self.close_pane(id);
                Response::Ok
            }
            Request::Rename { tab, title } => {
                let id = self.resolve(tab)?;
                let (window, tab) = self.window_of(id).ok_or("the pane closed")?;
                let title = title.trim();
                self.windows.get_mut(&window).unwrap().tabs[tab].title = (!title.is_empty()).then(|| title.to_owned());
                Response::Ok
            }
            Request::Run { .. } => unreachable!("handled before"),
            Request::Notify { tab, title, body } => {
                let id = self.resolve(tab)?;
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.alert(Some(body.clone()));
                }
                self.notify_user(id, title.as_deref(), &body);
                Response::Ok
            }
            Request::Status { tab, state, message, agent_session, agent } => {
                let id = self.resolve(tab)?;
                let pane = self.panes.get_mut(&id).ok_or("the pane closed")?;
                let was_waiting = matches!(pane.status, Status::Agent { state: AgentState::Waiting, .. });
                pane.report(state, message.clone(), agent_session, agent);
                match state {
                    AgentState::Waiting if !was_waiting => {
                        self.notify_user(id, Some("Needs input"), message.as_deref().unwrap_or(""));
                    }
                    AgentState::Done => self.notify_user(id, Some("Done"), message.as_deref().unwrap_or("")),
                    _ => {}
                }
                Response::Ok
            }
            Request::Settings => {
                let client = self.app_client().ok_or("the artty app isn't running")?;
                self.send(client, ServerMessage::App(AppCommand::OpenSettings));
                Response::Ok
            }
            Request::Restart => {
                let client = self.app_client().ok_or("the artty app isn't running")?;
                self.send(client, ServerMessage::App(AppCommand::Restart));
                Response::Ok
            }
            Request::ServerStatus => Response::Server {
                info: ServerInfo {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    pid: std::process::id(),
                    uptime_secs: self.started.elapsed().as_secs(),
                    windows: self.windows.len(),
                    attached_windows: self.windows.values().filter(|w| w.client.is_some()).count(),
                    panes: self.panes.len(),
                    clients: self.clients.len(),
                },
            },
            Request::StopServer => {
                self.stopping = true;
                Response::Ok
            }
            Request::RestartServer => {
                self.stopping = true;
                self.restarting = true;
                Response::Ok
            }
            Request::Processes { tab } => Response::Processes { panes: self.processes(tab)? },
            Request::Kill { tab, signal, pane } => {
                let id = self.resolve(Some(tab))?;
                if pane {
                    self.close_pane(id);
                } else {
                    let group = self.panes[&id].process.foreground_group.ok_or("no foreground process")?;
                    if self.panes[&id].process.at_shell {
                        return Err("only the shell is running; use --pane to close the pane".into());
                    }
                    // SAFETY: plain syscall.
                    if unsafe { libc::killpg(group, signal.unwrap_or(libc::SIGTERM)) } != 0 {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                }
                Response::Ok
            }
        })
    }

    /// Processes in each pane, with CPU and memory use.
    fn processes(&mut self, only: Option<PaneId>) -> Result<Vec<PaneProcesses>, String> {
        let samples = processes();
        let now = Instant::now();
        let unix_now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let cpu = |previous: &HashMap<i32, (u64, Instant)>, sample: &artty_core::ProcessSample| -> f32 {
            match previous.get(&sample.pid) {
                // Since the last look, if recent; else the lifetime average.
                Some(&(used, at)) if now.duration_since(at) <= Duration::from_secs(10) && sample.cpu_ns >= used => {
                    let wall = now.duration_since(at).as_nanos().max(1) as f64;
                    ((sample.cpu_ns - used) as f64 / wall * 100.) as f32
                }
                _ => {
                    let alive = unix_now.saturating_sub(sample.started).max(1) as f64 * 1e9;
                    (sample.cpu_ns as f64 / alive * 100.) as f32
                }
            }
        };
        let infos = self.pane_infos();
        let mut result = Vec::new();
        for info in infos.into_iter().filter(|i| only.is_none_or(|id| id == i.id)) {
            let Some(pane) = self.panes.get(&info.id) else { continue };
            let Some(shell) = pane.session.shell_pid() else { continue };
            let foreground = pane.process.foreground_group;
            let rows: Vec<ProcessRow> = process_tree(&samples, shell)
                .into_iter()
                .map(|p| ProcessRow {
                    pid: p.pid,
                    ppid: p.ppid,
                    name: p.name.clone(),
                    cpu_percent: cpu(&self.cpu_samples, p),
                    memory_bytes: p.memory,
                    foreground: Some(p.pgid) == foreground,
                    elapsed_secs: unix_now.saturating_sub(p.started),
                })
                .collect();
            let attached = self.window_of(info.id).is_some_and(|(w, _)| self.windows[&w].client.is_some());
            result.push(PaneProcesses {
                id: info.id,
                window: info.window,
                tab: info.tab,
                title: info.title,
                status: info.status,
                attached,
                cwd: info.cwd,
                cpu_percent: rows.iter().map(|r| r.cpu_percent).fold(0., |a, b| a + b),
                memory_bytes: rows.iter().map(|r| r.memory_bytes).sum(),
                processes: rows,
            });
        }
        if only.is_some() && result.is_empty() {
            return Err("no such pane".into());
        }
        self.cpu_samples = samples.iter().map(|s| (s.pid, (s.cpu_ns, now))).collect();
        Ok(result)
    }

    // Per-batch work.

    /// When the main loop must wake up next.
    pub(crate) fn next_deadline(&self) -> Instant {
        let mut deadline = (self.last_poll + POLL_INTERVAL).min(self.last_save + SAVE_INTERVAL);
        for client in self.clients.values() {
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
        self.clear_seen();
        self.push_updates(now);
        if now.duration_since(self.last_save) >= SAVE_INTERVAL {
            self.last_save = now;
            self.save();
        }
        if !self.panes.is_empty() || !self.clients.is_empty() {
            self.idle_since = now;
        }
    }

    /// Once a second: processes, statuses, `artty run` timeouts, config.
    fn poll(&mut self) {
        for pane in self.panes.values_mut() {
            pane.process = pane.session.process();
            pane.refresh();
            pane.flush_pending_input();
        }
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
                self.theme = Arc::new(config.theme());
                self.agents = config.agents.clone().into();
                for pane in self.panes.values_mut() {
                    pane.session.set_theme(self.theme.clone());
                    pane.session.set_cursor_shape(config.cursor_style.shape());
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
        let seen: Vec<PaneId> = self.windows.values().filter(|w| w.client.is_some() && w.focused).flat_map(Window::visible).collect();
        for pane in seen {
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.attention = false;
            }
        }
    }

    fn pane_state(&self, pane: &Pane, info: TabInfo) -> PaneState {
        let icon = match &pane.status {
            status if status.busy() => "busy",
            Status::Agent { state: AgentState::Waiting, .. } => "waiting",
            Status::Agent { state: AgentState::Done, .. } => "done",
            Status::Failed { .. } => "failed",
            _ if pane.attention => "attention",
            _ => "",
        };
        let progress = match pane.progress {
            Some(Progress::Normal(p) | Progress::Paused(Some(p)) | Progress::Error(Some(p))) => Some(p),
            _ => None,
        };
        PaneState { info, icon: icon.to_owned(), progress, agent: pane.agent_name() }
    }

    /// Send each app its windows' layouts, pane states and screen updates.
    fn push_updates(&mut self, now: Instant) {
        let infos = self.pane_infos();
        let client_ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for client_id in client_ids {
            let windows: Vec<(WindowId, WindowLayout, Vec<PaneId>)> = self
                .windows
                .iter()
                .filter(|(_, w)| w.client == Some(client_id))
                .map(|(&id, w)| (id, w.layout(), w.visible()))
                .collect();
            let window_panes: HashSet<PaneId> = self
                .windows
                .values()
                .filter(|w| w.client == Some(client_id))
                .flat_map(|w| w.tabs.iter().flat_map(|t| t.root.panes()))
                .collect();
            let states: Vec<PaneState> = infos
                .iter()
                .filter(|info| window_panes.contains(&info.id))
                .filter_map(|info| Some(self.pane_state(self.panes.get(&info.id)?, info.clone())))
                .collect();
            let visible: Vec<PaneId> = windows.iter().flat_map(|(_, _, v)| v.iter().copied()).collect();

            let mut frames = Vec::new();
            {
                let client = self.clients.get_mut(&client_id).unwrap();
                // Forget panes no longer shown, so they get a full update if
                // they come back.
                client.sent.retain(|pane, _| visible.contains(pane));
                for &pane in &visible {
                    let due = match client.sent.get(&pane) {
                        None => true,
                        Some((_, at)) => client.dirty.contains(&pane) && now.duration_since(*at) >= FRAME_INTERVAL,
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
                let previous = client.sent.get(&pane).map(|(f, _)| f);
                let update = artty_core::protocol::FrameUpdate::diff(previous, &frame, mode);
                client.dirty.remove(&pane);
                client.sent.insert(pane, (frame, now));
                messages.push(ServerMessage::Frame { pane, update });
            }
            let client = self.clients.get_mut(&client_id).unwrap();
            if client.panes_sent != states {
                client.panes_sent = states.clone();
                messages.push(ServerMessage::Panes { panes: states });
            }
            for message in messages {
                let _ = client.tx.send(message);
            }
        }
    }

    fn saved_layout(&self, node: &Node) -> SavedLayout {
        match node {
            Node::Pane(id) => SavedLayout::Pane(self.panes.get(id).map(|p| p.saved(self.config.restore_scrollback)).unwrap_or_default()),
            Node::Split { axis, ratio, first, second } => SavedLayout::Split {
                vertical: *axis == Axis::Vertical,
                ratio: *ratio,
                first: Box::new(self.saved_layout(first)),
                second: Box::new(self.saved_layout(second)),
            },
        }
    }

    /// Save every window for the next start (after a reboot or crash).
    fn save(&mut self) {
        if !self.config.restore_sessions {
            return;
        }
        let windows = self
            .windows
            .values()
            .map(|window| SavedWorkspace {
                window: window.bounds,
                active: window.active,
                tabs: window
                    .tabs
                    .iter()
                    .map(|tab| SavedTab {
                        title: tab.title.clone(),
                        layout: self.saved_layout(&tab.root),
                        focused: tab.root.panes().iter().position(|&id| id == tab.focused).unwrap_or(0),
                        zoomed: tab.zoomed,
                    })
                    .collect(),
            })
            .collect();
        let state = SavedState::new(windows);
        let Ok(json) = serde_json::to_string(&state) else { return };
        if self.last_saved.as_ref() == Some(&json) {
            return;
        }
        match state.save() {
            Ok(()) => self.last_saved = Some(json),
            Err(err) => log::error!("saving: {err}"),
        }
    }

    pub(crate) fn should_exit(&self) -> bool {
        self.stopping
            || (self.panes.is_empty() && self.had_panes)
            || (self.panes.is_empty() && self.clients.is_empty() && self.idle_since.elapsed() >= IDLE_EXIT)
    }

    pub(crate) fn shutdown(&mut self) {
        if self.stopping && !self.restarting {
            // Ended on purpose: nothing to reopen.
            self.panes.clear();
            self.windows.clear();
        }
        self.save();
        // Dropping sessions hangs up their shells.
        self.panes.clear();
    }
}
