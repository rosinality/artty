//! The main window: a sidebar of tabs, each running a terminal session.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{AgentState, ReadSource, Request, Response, SESSION_ENV, SOCKET_ENV};
use artty_core::vt::ansi::Progress;
use artty_core::vt::event::Event;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::term::TermMode;
use artty_core::{
    Config, FrameCursor, Notice, Session, SessionId, SessionOptions, ShellPhase, TerminalSize,
    Theme,
};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontWeight, Hsla, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, WindowBackgroundAppearance, actions, canvas, div, point, prelude::*,
    px, rgba,
};

use crate::control::{ControlServer, Pending, cli_dir};
use crate::keys;
use crate::mouse::{self, Action, Button};
use crate::tab::{Status, Tab};
use crate::terminal::{self, Metrics, hsla};

actions!(
    artty,
    [
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        JumpToAttention,
        Copy,
        Paste,
        Find,
        FindNext,
        FindPrevious,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize
    ]
);

pub const SIDEBAR_WIDTH: f32 = 232.;
const TAB_LABEL_WIDTH: f32 = 28.;
const TITLEBAR_HEIGHT: f32 = 32.;
const PADDING: f32 = 12.;
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Background commands running at least this long notify when they finish.
const LONG_COMMAND: Duration = Duration::from_secs(10);
const MIN_FONT_SIZE: f32 = 6.;
const MAX_FONT_SIZE: f32 = 72.;

/// `artty run` waiting for its command to finish.
struct PendingRun {
    tab: SessionId,
    reply: oneshot::Sender<Response>,
    deadline: Option<Instant>,
}

struct Search {
    query: String,
    /// `Some(false)`: no match; `Some(true)`: showing a match.
    found: Option<bool>,
    error: Option<String>,
}

pub struct Workspace {
    focus: FocusHandle,
    config: Arc<Config>,
    config_error: Option<String>,
    config_modified: Option<SystemTime>,
    reload_config: bool,
    theme: Arc<Theme>,
    agents: Arc<[String]>,
    font_size: f32,
    metrics: Metrics,
    tabs: Vec<Tab>,
    active: usize,
    next_id: SessionId,
    /// Terminal size from the last layout, for tabs opened between renders.
    size: TerminalSize,
    sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    control: Option<ControlServer>,
    cli_dir: Option<PathBuf>,
    runs: Vec<PendingRun>,
    search: Option<Search>,
    /// IME composition in progress, shown at the cursor until committed.
    marked_text: Option<String>,
    /// Cursor position from the last render, for placing the IME window.
    cursor: Option<FrameCursor>,
    cursor_visible: bool,
    /// Whether the cursor should blink, as of the last render.
    blink_enabled: bool,
    /// Skip one blink so the cursor stays solid while typing.
    blink_hold: bool,
    spinner_frame: usize,
    window_active: bool,
    /// A mouse drag is extending the selection.
    selecting: bool,
    /// Button held down while the program receives mouse reports.
    reporting_button: Option<Button>,
    /// Last cell reported for motion, to avoid duplicate reports.
    last_mouse_cell: Option<(usize, usize)>,
    /// Sub-line remainder of trackpad scrolling.
    scroll_remainder: f32,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

fn agents_of(config: &Config) -> Arc<[String]> {
    config.agents.clone().into()
}

/// Post a macOS notification.
fn notify_desktop(title: &str, body: &str) {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

impl Workspace {
    pub fn new(
        config: Config,
        config_error: Option<String>,
        command: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded::<(SessionId, Event)>();
        let sink: Arc<dyn Fn(SessionId, Event) + Send + Sync> =
            Arc::new(move |id, event| {
                let _ = tx.unbounded_send((id, event));
            });

        // Terminal events arrive from PTY threads; drain them in batches so a
        // burst of output costs one redraw.
        let events = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                if this.update(cx, |this, cx| this.handle_events(batch, cx)).is_err() {
                    break;
                }
            }
        });

        let (request_tx, mut request_rx) = mpsc::unbounded::<Pending>();
        let control = ControlServer::start(request_tx)
            .inspect_err(|err| log::error!("control socket: {err}"))
            .ok();
        let requests = cx.spawn(async move |this, cx| {
            while let Some((request, reply)) = request_rx.next().await {
                if this.update(cx, |this, cx| this.handle_request(request, reply, cx)).is_err() {
                    break;
                }
            }
        });

        let every = |interval: Duration, f: fn(&mut Self, &mut Context<Self>)| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(interval).await;
                    if this.update(cx, f).is_err() {
                        break;
                    }
                }
            })
        };
        let blink = every(BLINK_INTERVAL, Self::blink);
        let poll = every(POLL_INTERVAL, Self::poll);
        let spinner = every(SPINNER_INTERVAL, Self::spin);

        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.report_focus(this.window_active);
            cx.notify();
        });

        if let Some(error) = &config_error {
            log::error!("config: {error}");
        }

        let focus = cx.focus_handle();
        focus.focus(window);
        let font_size = config.font_size;
        let metrics = Metrics::measure(window, &config, font_size);
        let mut this = Self {
            focus,
            theme: Arc::new(config.theme()),
            agents: agents_of(&config),
            metrics,
            font_size,
            config: Arc::new(config),
            config_error,
            config_modified: config_modified(),
            reload_config: false,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            size: TerminalSize { columns: 80, lines: 24, cell_width: 8, cell_height: 16 },
            sink,
            control,
            cli_dir: cli_dir(),
            runs: Vec::new(),
            search: None,
            marked_text: None,
            cursor: None,
            cursor_visible: true,
            blink_enabled: false,
            blink_hold: false,
            spinner_frame: 0,
            window_active: true,
            selecting: false,
            reporting_button: None,
            last_mouse_cell: None,
            scroll_remainder: 0.,
            _tasks: vec![events, requests, blink, poll, spinner],
            _subscriptions: vec![activation],
        };
        this.size = this.terminal_size(window);
        let _ = this.open_tab(command, None, None, true, cx);
        this
    }

    fn terminal_size(&self, window: &Window) -> TerminalSize {
        let viewport = window.viewport_size();
        let width = viewport.width - px(SIDEBAR_WIDTH + 2. * PADDING);
        let height = viewport.height - px(TITLEBAR_HEIGHT + PADDING);
        TerminalSize {
            columns: (width / self.metrics.cell_width).floor().max(2.) as u16,
            lines: (height / self.metrics.line_height).floor().max(1.) as u16,
            cell_width: f32::from(self.metrics.cell_width).round() as u16,
            cell_height: f32::from(self.metrics.line_height).round() as u16,
        }
    }

    /// Top-left of the cell grid in window coordinates; matches the layout in
    /// `render`.
    fn grid_origin() -> Point<Pixels> {
        point(px(SIDEBAR_WIDTH + PADDING), px(TITLEBAR_HEIGHT))
    }

    /// The viewport cell under a window position, and which half of it.
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize, Side) {
        let size = self.active_session().map(Session::size);
        let (columns, lines) = size.map_or((1, 1), |s| (s.columns as usize, s.lines as usize));
        let offset = position - Self::grid_origin();
        let x = (offset.x / self.metrics.cell_width).max(0.);
        let y = (offset.y / self.metrics.line_height).max(0.);
        let column = (x as usize).min(columns - 1);
        let side = if x.fract() < 0.5 || x as usize >= columns { Side::Left } else { Side::Right };
        ((y as usize).min(lines - 1), column, side)
    }

    /// Open a tab running `command` (default: the configured shell) in `cwd`
    /// (default: the active tab's directory).
    fn open_tab(
        &mut self,
        command: Option<Vec<String>>,
        cwd: Option<PathBuf>,
        title: Option<String>,
        focus: bool,
        cx: &mut Context<Self>,
    ) -> Result<SessionId, String> {
        let id = self.next_id;
        self.next_id += 1;
        let command = command.or_else(|| Some(self.config.shell.clone())).filter(|c| !c.is_empty());
        let cwd = cwd.or_else(|| self.active_session().and_then(Session::cwd));

        let mut env = std::collections::HashMap::new();
        env.insert(SESSION_ENV.to_owned(), id.to_string());
        if let Some(control) = &self.control {
            env.insert(SOCKET_ENV.to_owned(), control.path().display().to_string());
        }
        if let Some(dir) = &self.cli_dir {
            let path = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".to_owned(), format!("{}:{path}", dir.display()));
        }
        let options = SessionOptions {
            command: command.map(|mut c| (c.remove(0), c)),
            working_directory: cwd.filter(|dir| dir.is_dir()),
            env,
            scrollback: Some(self.config.scrollback),
        };
        let session = Session::spawn(id, options, self.size, self.theme.clone(), self.sink.clone())
            .map_err(|err| format!("failed to start: {err}"))?;
        self.tabs.push(Tab::new(session, title, self.agents.clone()));
        if focus {
            self.select(self.tabs.len() - 1, cx);
        }
        cx.notify();
        Ok(id)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.active = index;
            self.marked_text = None;
            self.selecting = false;
            self.reporting_button = None;
            self.close_search();
            cx.notify();
        }
    }

    fn index_of(&self, id: SessionId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.session.id() == id)
    }

    fn close(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else { return };
        self.tabs.remove(index);
        for run in self.runs.extract_if(.., |run| run.tab == id) {
            let _ = run.reply.send(Response::error("the tab closed"));
        }
        if self.tabs.is_empty() {
            cx.quit();
            return;
        }
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        cx.notify();
    }

    /// Tell the user about a tab that needs them, unless they're looking at it.
    fn notify_user(&self, index: usize, title: Option<&str>, body: &str) {
        if !self.config.notifications || (self.window_active && index == self.active) {
            return;
        }
        let tab_title = self.tabs[index].title();
        let title = title.map_or_else(|| tab_title.to_string(), |t| format!("{t} — {tab_title}"));
        notify_desktop(&title, if body.is_empty() { "Needs your attention" } else { body });
    }

    fn handle_events(&mut self, events: Vec<(SessionId, Event)>, cx: &mut Context<Self>) {
        for (id, event) in events {
            let Some(index) = self.index_of(id) else { continue };
            let Some(notice) = self.tabs[index].session.handle(event) else { continue };
            let tab = &mut self.tabs[index];
            match notice {
                Notice::Redraw => {}
                Notice::Title(title) => tab.set_title(Some(title)),
                Notice::ResetTitle => tab.set_title(None),
                Notice::Bell => {
                    if index != self.active || !self.window_active {
                        tab.alert(None);
                        self.notify_user(index, None, "Bell");
                    }
                }
                Notice::Notification { title, body } => {
                    tab.alert(Some(body.clone()).filter(|b| !b.is_empty()).or(title.clone()));
                    self.notify_user(index, title.as_deref(), &body);
                }
                Notice::Progress(progress) => {
                    tab.progress = (progress != Progress::Remove).then_some(progress);
                }
                Notice::CommandStarted(command) => tab.command_started(command),
                Notice::CommandFinished(record) => {
                    tab.command_finished(record.exit);
                    let long = record.finished.is_some_and(|end| end - record.started >= LONG_COMMAND);
                    let response = Response::Command {
                        command: record.command.clone(),
                        output: record.output.clone(),
                        exit: record.exit,
                        truncated: record.truncated,
                    };
                    for run in self.runs.extract_if(.., |run| run.tab == id) {
                        let _ = run.reply.send(response.clone());
                    }
                    if long && (index != self.active || !self.window_active) && tab.agent_name().is_none() {
                        let what = record.command.clone().unwrap_or_else(|| "Command".to_owned());
                        let result = match record.exit {
                            Some(0) | None => "finished".to_owned(),
                            Some(code) => format!("failed (exit {code})"),
                        };
                        let message = format!("{what} {result}");
                        self.tabs[index].alert(Some(message.clone()));
                        self.notify_user(index, None, &message);
                    }
                }
                Notice::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Notice::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
                    tab.session.reply_clipboard(&format, &text);
                }
                Notice::Exited => {
                    self.close(id, cx);
                    continue;
                }
            }
            self.tabs[index].refresh();
            cx.notify();
        }
    }

    fn resolve(&self, tab: Option<SessionId>) -> Result<usize, String> {
        match tab {
            Some(id) => self.index_of(id).ok_or_else(|| format!("no tab {id}")),
            None if self.tabs.is_empty() => Err("no tabs".to_owned()),
            None => Ok(self.active),
        }
    }

    fn handle_request(&mut self, request: Request, reply: oneshot::Sender<Response>, cx: &mut Context<Self>) {
        let response = match request {
            // Answered when the command finishes.
            Request::Run { tab, command, timeout_secs } => match self.start_run(tab, &command) {
                Ok(tab) => {
                    let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
                    self.runs.push(PendingRun { tab, reply, deadline });
                    return;
                }
                Err(message) => Response::error(message),
            },
            request => self.respond(request, cx).unwrap_or_else(Response::error),
        };
        let _ = reply.send(response);
        cx.notify();
    }

    /// Type `command` into a tab's shell; returns the tab to wait on.
    fn start_run(&mut self, tab: Option<SessionId>, command: &str) -> Result<SessionId, String> {
        let session = &self.tabs[self.resolve(tab)?].session;
        if !session.shell_integration() {
            return Err("this tab's shell doesn't report commands (no shell integration)".into());
        }
        let id = session.id();
        if session.shell_phase() != ShellPhase::Prompt || self.runs.iter().any(|run| run.tab == id) {
            return Err("the tab is busy".into());
        }
        session.write(format!("{command}\r").into_bytes());
        Ok(id)
    }

    fn respond(&mut self, request: Request, cx: &mut Context<Self>) -> Result<Response, String> {
        Ok(match request {
            Request::List => Response::Tabs {
                tabs: self.tabs.iter().enumerate().map(|(i, tab)| tab.info(i == self.active)).collect(),
            },
            Request::Read { tab, source } => {
                let session = &self.tabs[self.resolve(tab)?].session;
                let text = match source {
                    ReadSource::Screen => session.screen_text(),
                    ReadSource::Scrollback { lines } => session.scrollback_text(lines),
                    ReadSource::LastCommand => {
                        if !session.shell_integration() {
                            return Err("this tab's shell doesn't report commands (no shell integration)".into());
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
                let session = &self.tabs[self.resolve(tab)?].session;
                let mut bytes = text.into_bytes();
                if enter {
                    bytes.push(b'\r');
                }
                session.write(bytes);
                Response::Ok
            }
            Request::New { cwd, command, title, focus } => {
                let command = (!command.is_empty()).then_some(command);
                let id = self.open_tab(command, cwd, title, focus, cx)?;
                Response::Tab { id }
            }
            Request::Focus { tab } => {
                let index = self.resolve(Some(tab))?;
                self.select(index, cx);
                Response::Ok
            }
            Request::Close { tab } => {
                self.resolve(Some(tab))?;
                self.close(tab, cx);
                Response::Ok
            }
            Request::Run { .. } => unreachable!("handled in handle_request"),
            Request::Notify { tab, title, body } => {
                let index = self.resolve(tab)?;
                self.tabs[index].alert(Some(body.clone()));
                self.notify_user(index, title.as_deref(), &body);
                Response::Ok
            }
            Request::Status { tab, state, message } => {
                let index = self.resolve(tab)?;
                let was_waiting = matches!(self.tabs[index].status, Status::Agent { state: AgentState::Waiting, .. });
                self.tabs[index].report(state, message.clone());
                match state {
                    AgentState::Waiting if !was_waiting => {
                        self.notify_user(index, Some("Needs input"), message.as_deref().unwrap_or(""));
                    }
                    AgentState::Done => self.notify_user(index, Some("Done"), message.as_deref().unwrap_or("")),
                    _ => {}
                }
                Response::Ok
            }
        })
    }

    fn active_session(&self) -> Option<&Session> {
        self.tabs.get(self.active).map(|tab| &tab.session)
    }

    /// Send user input to the active session, keeping the cursor solid.
    fn input(&mut self, bytes: Vec<u8>) {
        if let Some(session) = self.active_session() {
            session.write(bytes);
        }
        self.cursor_visible = true;
        self.blink_hold = true;
    }

    fn blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_hold {
            self.blink_hold = false;
        } else if self.blink_enabled {
            self.cursor_visible = !self.cursor_visible;
            cx.notify();
        } else if !self.cursor_visible {
            self.cursor_visible = true;
            cx.notify();
        }
    }

    fn spin(&mut self, cx: &mut Context<Self>) {
        if self.tabs.iter().any(|tab| tab.status.busy()) {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
            cx.notify();
        }
    }

    /// Once a second: what's running in each tab, config changes, timeouts.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for tab in &mut self.tabs {
            let process = tab.session.process();
            let before = (tab.status.clone(), tab.process.clone());
            tab.process = process;
            tab.refresh();
            changed |= before != (tab.status.clone(), tab.process.clone()) || tab.status.busy();
        }

        let now = Instant::now();
        for run in self.runs.extract_if(.., |run| run.deadline.is_some_and(|d| d <= now) || run.reply.is_canceled()) {
            let _ = run.reply.send(Response::error("timed out"));
        }

        let modified = config_modified();
        if modified != self.config_modified {
            self.config_modified = modified;
            self.reload_config = true;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn apply_config(&mut self, window: &mut Window) {
        self.reload_config = false;
        let (config, error) = Config::load();
        self.config_error = error;
        if self.config_error.is_some() {
            return;
        }
        self.theme = Arc::new(config.theme());
        self.agents = agents_of(&config);
        for tab in &mut self.tabs {
            tab.session.set_theme(self.theme.clone());
            tab.set_agents(self.agents.clone());
        }
        if config.font_size != self.config.font_size {
            self.font_size = config.font_size;
        }
        self.metrics = Metrics::measure(window, &config, self.font_size);
        window.set_background_appearance(background_appearance(&config));
        self.config = Arc::new(config);
    }

    fn report_focus(&self, focused: bool) {
        if let Some(session) = self.active_session()
            && session.mode().contains(TermMode::FOCUS_IN_OUT)
        {
            session.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }

    fn jump_to_attention(&mut self, cx: &mut Context<Self>) {
        let count = self.tabs.len();
        let order = (1..=count).map(|offset| (self.active + offset) % count);
        let waiting = |tab: &Tab| matches!(tab.status, Status::Agent { state: AgentState::Waiting, .. });
        let target = order.clone().find(|&i| waiting(&self.tabs[i])).or_else(|| order.clone().find(|&i| self.tabs[i].attention));
        if let Some(index) = target {
            self.select(index, cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform {
            if let Some(digit) = keystroke.key.parse::<usize>().ok().filter(|d| (1..=9).contains(d)) {
                self.select(digit - 1, cx);
                cx.stop_propagation();
            }
            return;
        }
        if self.search.is_some() {
            self.search_key(event, cx);
            return;
        }
        let Some(session) = self.active_session() else { return };
        if let Some(bytes) = keys::to_esc(keystroke, session.mode(), self.config.option_as_meta) {
            self.input(bytes);
            cx.stop_propagation();
        }
    }

    // Search (⌘F).

    fn open_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search = Some(Search { query: String::new(), found: None, error: None });
        }
        cx.notify();
    }

    fn close_search(&mut self) {
        if self.search.take().is_some()
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.session.end_search();
        }
    }

    fn run_search(&mut self, older: bool, restart: bool, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else { return };
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if restart {
            tab.session.clear_selection();
        }
        if search.query.is_empty() {
            search.found = None;
            search.error = None;
        } else {
            match tab.session.search(&search.query, older) {
                Ok(found) => {
                    search.found = Some(found);
                    search.error = None;
                }
                Err(error) => search.error = Some(error),
            }
        }
        cx.notify();
    }

    fn search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" => {
                self.close_search();
                cx.notify();
            }
            "enter" => self.run_search(!keystroke.modifiers.shift, false, cx),
            "backspace" => {
                if let Some(search) = self.search.as_mut() {
                    search.query.pop();
                }
                self.run_search(true, true, cx);
            }
            // Text arrives through the input handler.
            _ if !keystroke.modifiers.control && !keystroke.modifiers.alt => return,
            _ => {}
        }
        cx.stop_propagation();
    }

    /// Whether mouse events go to the program instead of selecting text.
    /// Holding Shift always selects, as in other terminals.
    fn mouse_reporting(&self, modifiers: &Modifiers) -> Option<TermMode> {
        let mode = self.active_session()?.mode();
        (mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift).then_some(mode)
    }

    fn report_mouse(&mut self, button: Button, action: Action, position: Point<Pixels>, modifiers: &Modifiers) {
        let Some(mode) = self.mouse_reporting(modifiers) else { return };
        let (line, column, _) = self.cell_at(position);
        if action == Action::Motion && self.last_mouse_cell == Some((line, column)) {
            return;
        }
        self.last_mouse_cell = Some((line, column));
        if let Some(bytes) = mouse::report(button, action, line, column, modifiers, mode) {
            self.input(bytes);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if event.button == MouseButton::Left && event.modifiers.platform {
            let (line, column, _) = self.cell_at(event.position);
            if let Some(link) = self.active_session().and_then(|s| s.link_at(line, column)) {
                let _ = std::process::Command::new("open").arg(&link).spawn();
            }
            return;
        }
        if self.mouse_reporting(&event.modifiers).is_some() {
            if let Some(button) = Button::from_gpui(event.button) {
                self.reporting_button = Some(button);
                self.last_mouse_cell = None;
                self.report_mouse(button, Action::Press, event.position, &event.modifiers);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let (line, column, side) = self.cell_at(event.position);
        let Some(session) = self.active_session() else { return };
        if event.modifiers.shift && session.selection_text().is_some() {
            session.update_selection(line, column, side);
        } else {
            let kind = match event.click_count {
                0 | 1 => SelectionType::Simple,
                2 => SelectionType::Semantic,
                _ => SelectionType::Lines,
            };
            session.start_selection(kind, line, column, side);
        }
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(button) = self.reporting_button {
            self.report_mouse(button, Action::Motion, event.position, &event.modifiers);
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            let (line, column, side) = self.cell_at(event.position);
            if let Some(session) = self.active_session() {
                session.update_selection(line, column, side);
            }
            cx.notify();
        } else if event.pressed_button.is_none() && event.position.x > Self::grid_origin().x {
            self.report_mouse(Button::None, Action::Motion, event.position, &event.modifiers);
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(button) = self.reporting_button.take() {
            self.report_mouse(button, Action::Release, event.position, &event.modifiers);
            return;
        }
        if self.selecting {
            self.selecting = false;
            // A plain click selects nothing; drop the empty selection.
            if let Some(session) = self.active_session()
                && session.selection_text().is_none()
            {
                session.clear_selection();
            }
            cx.notify();
        }
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => {
                self.scroll_remainder += delta.y / self.metrics.line_height;
                let whole = self.scroll_remainder.trunc();
                self.scroll_remainder -= whole;
                whole
            }
        } as i32;
        if lines == 0 {
            return;
        }
        let Some(session) = self.active_session() else { return };
        let mode = session.mode();
        if self.mouse_reporting(&event.modifiers).is_some() {
            let button = if lines > 0 { Button::WheelUp } else { Button::WheelDown };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(button, Action::Press, event.position, &event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs (less, vim) get arrow keys instead.
            let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA".as_slice(),
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.input(key.repeat(lines.unsigned_abs() as usize));
        } else {
            session.scroll(lines);
        }
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.active_session().and_then(Session::selection_text) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if let Some(search) = self.search.as_mut() {
            search.query.push_str(text.lines().next().unwrap_or_default());
            self.run_search(true, true, cx);
            return;
        }
        let Some(session) = self.active_session() else { return };
        let bytes = if session.mode().contains(TermMode::BRACKETED_PASTE) {
            // Strip any embedded end marker so pasted text can't escape the bracket.
            format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.input(bytes.into_bytes());
    }

    fn set_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.metrics = Metrics::measure(window, &self.config, self.font_size);
        cx.notify();
    }

    fn status_icon(&self, tab: &Tab) -> Option<(SharedString, Hsla)> {
        let color = |hex: u32| -> Hsla { rgba(hex).into() };
        Some(match &tab.status {
            status if status.busy() => (SPINNER[self.spinner_frame].into(), color(0x8fb4ffff)),
            Status::Agent { state: AgentState::Waiting, .. } => ("●".into(), color(0xe0af68ff)),
            Status::Agent { state: AgentState::Done, .. } => ("✓".into(), color(0x9ece6aff)),
            Status::Failed { .. } => ("✕".into(), color(0xf7768eff)),
            _ if tab.attention => ("●".into(), color(0xe0af68ff)),
            _ => return None,
        })
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.tabs.iter().enumerate().map(|(i, tab)| {
            let selected = i == self.active;
            let label: SharedString = if i < 9 { format!("⌘{}", i + 1).into() } else { "".into() };
            let mut detail = tab.detail();
            if let Some(Progress::Normal(percent) | Progress::Paused(Some(percent)) | Progress::Error(Some(percent))) = tab.progress {
                detail = Some(format!("{} · {percent}%", detail.unwrap_or_default()).trim_start_matches(" · ").to_owned());
            }
            let icon = self.status_icon(tab);
            let text_color = if selected { rgba(0xffffffee) } else { rgba(0xffffffb0) };
            div()
                .id(i)
                .flex()
                .items_center()
                .gap(px(6.))
                .min_h(px(30.))
                .py(px(4.))
                .px_2()
                .rounded(px(7.))
                .when(selected, |d| d.bg(rgba(0xffffff1c)))
                .when(tab.attention && !selected, |d| d.bg(rgba(0xe0af6814)))
                .hover(|d| d.bg(rgba(0xffffff10)))
                .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx)))
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .when_some(icon, |d, (icon, color)| {
                            // Braille spinner frames are tiny at text size.
                            let size = if tab.status.busy() { 16. } else { 12. };
                            d.text_color(color).text_size(px(size)).child(icon)
                        }),
                )
                .child(
                    // GPUI 0.2 caches the first measurement of `nowrap` text, which
                    // happens before the width is known, so `.truncate()` only clips.
                    // A one-line clamp re-measures at the real width and ellipsizes.
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .overflow_hidden()
                                .line_clamp(1)
                                .text_ellipsis()
                                .text_color(text_color)
                                .when(tab.attention, |d| d.font_weight(FontWeight::SEMIBOLD))
                                .child(tab.title()),
                        )
                        .when_some(detail, |d, detail| {
                            d.child(
                                div()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .text_size(px(11.))
                                    .text_color(rgba(0xffffff70))
                                    .child(detail),
                            )
                        }),
                )
                .child(
                    div()
                        .w(px(TAB_LABEL_WIDTH))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .text_xs()
                        .text_color(rgba(0xffffff66))
                        .child(label),
                )
        });

        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(rgba(0xffffff0a))
            .border_r_1()
            .border_color(rgba(0xffffff12))
            .pt(px(44.))
            .px(px(8.))
            .pb(px(10.))
            .text_size(px(12.5))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .pb_2()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(0xffffff70))
                    .child("TABS")
                    .child(
                        div()
                            .id("new-tab")
                            .px_1()
                            .rounded(px(4.))
                            .hover(|d| d.bg(rgba(0xffffff14)))
                            .text_size(px(14.))
                            .child("+")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let _ = this.open_tab(None, None, None, true, cx);
                            })),
                    ),
            )
            .child(div().id("tab-list").flex().flex_col().flex_1().gap(px(3.)).overflow_y_scroll().children(rows))
            .when_some(self.config_error.clone(), |d, error| {
                d.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(rgba(0xf7768eff))
                        .child(format!("Config error — using previous settings. {error}")),
                )
            })
    }

    fn search_bar(&self) -> Option<impl IntoElement> {
        let search = self.search.as_ref()?;
        let mut query = search.query.clone();
        if let Some(marked) = &self.marked_text {
            query.push_str(marked);
        }
        let (status, status_color) = match (&search.error, search.found) {
            (Some(_), _) => ("invalid pattern".to_owned(), rgba(0xf7768eff)),
            (None, Some(false)) => ("no match".to_owned(), rgba(0xf7768eff)),
            _ => ("⏎ older · ⇧⏎ newer · esc".to_owned(), rgba(0xffffff60)),
        };
        Some(
            div()
                .absolute()
                .top(px(6.))
                .right(px(PADDING + 4.))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(5.))
                .rounded(px(8.))
                .bg(rgba(0x1e2430f2))
                .border_1()
                .border_color(rgba(0xffffff24))
                .shadow_md()
                .text_size(px(12.))
                .child(div().text_color(rgba(0xffffff70)).child("Find"))
                .child(
                    div()
                        .min_w(px(160.))
                        .font_family(self.metrics.font.family.clone())
                        .text_color(rgba(0xffffffee))
                        .child(format!("{query}▏")),
                )
                .child(div().text_color(status_color).child(status)),
        )
    }
}

pub fn background_appearance(config: &Config) -> WindowBackgroundAppearance {
    if config.background_opacity >= 1. {
        WindowBackgroundAppearance::Opaque
    } else if config.background_blur {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Transparent
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.reload_config {
            self.apply_config(window);
        }
        self.size = self.terminal_size(window);
        for tab in &mut self.tabs {
            tab.session.resize(self.size);
        }
        self.window_active = window.is_window_active();
        if self.window_active
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.attention = false;
        }
        let frame = self.active_session().map(Session::frame);
        self.cursor = frame.as_ref().and_then(|frame| frame.cursor);

        let focused = self.focus.is_focused(window) && self.window_active && self.search.is_none();
        self.blink_enabled =
            focused && (self.config.cursor_blink || self.cursor.is_some_and(|c| c.blinking));

        let title = self.tabs.get(self.active).map(Tab::title).unwrap_or_default();
        window.set_window_title(&title);

        let tint = Hsla { a: self.config.background_opacity.clamp(0., 1.), ..hsla(self.theme.background) };
        let metrics = self.metrics.clone();
        let marked_text = self.marked_text.clone().filter(|_| self.search.is_none());
        let cursor_visible = self.cursor_visible;
        let focus = self.focus.clone();
        let entity = cx.entity();

        div()
            .track_focus(&self.focus)
            .key_context("Workspace")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                if let Err(err) = this.open_tab(None, None, None, true, cx) {
                    log::error!("{err}");
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(id) = this.active_session().map(Session::id) {
                    this.close(id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| {
                let next = (this.active + 1) % this.tabs.len().max(1);
                this.select(next, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| {
                let count = this.tabs.len().max(1);
                this.select((this.active + count - 1) % count, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToAttention, _, cx| this.jump_to_attention(cx)))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(|this, _: &Find, _, cx| this.open_search(cx)))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| {
                this.open_search(cx);
                this.run_search(true, false, cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| {
                this.open_search(cx);
                this.run_search(false, false, cx);
            }))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, window, cx| {
                this.set_font_size(this.font_size + 1., window, cx)
            }))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, window, cx| {
                this.set_font_size(this.font_size - 1., window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResetFontSize, window, cx| {
                this.set_font_size(this.config.font_size, window, cx)
            }))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .flex()
            .size_full()
            .bg(tint)
            .font_family(".SystemUIFont")
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .h(px(TITLEBAR_HEIGHT))
                            .px_4()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(rgba(0xffffffb8))
                            .child(div().min_w_0().overflow_hidden().line_clamp(1).text_ellipsis().child(title)),
                    )
                    .child(
                        div()
                            .id("terminal")
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .px(px(PADDING))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
                            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
                            .on_scroll_wheel(cx.listener(Self::scroll))
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, window, cx| {
                                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                                        if let Some(frame) = frame {
                                            let cursor = terminal::CursorState {
                                                focused,
                                                visible: cursor_visible,
                                                marked_text: marked_text.as_deref(),
                                            };
                                            terminal::paint(bounds, &frame, &metrics, cursor, window, cx);
                                        }
                                    },
                                )
                                .size_full(),
                            )
                            .children(self.search_bar()),
                    ),
            )
    }
}

/// IME support: composition text is held here and only sent to the program
/// (or the search query) once committed, so typing Hangul or kana works as in
/// any Mac app.
impl EntityInputHandler for Workspace {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let marked: Vec<u16> = self.marked_text.as_deref().unwrap_or_default().encode_utf16().collect();
        let range = range.start.min(marked.len())..range.end.min(marked.len());
        *adjusted_range = Some(range.clone());
        Some(String::from_utf16_lossy(&marked[range]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked_text.as_deref().map_or(0, |text| text.encode_utf16().count());
        Some(UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text.as_deref().map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if let Some(search) = self.search.as_mut() {
            search.query.push_str(text);
            self.run_search(true, true, cx);
            return;
        }
        self.input(text.as_bytes().to_vec());
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cursor = self.cursor?;
        Some(self.metrics.cell_bounds(element_bounds.origin, cursor.line, cursor.column, 1))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-shift-]", NextTab, None),
        KeyBinding::new("cmd-shift-[", PreviousTab, None),
        KeyBinding::new("cmd-j", JumpToAttention, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-f", Find, None),
        KeyBinding::new("cmd-g", FindNext, None),
        KeyBinding::new("cmd-shift-g", FindPrevious, None),
        KeyBinding::new("cmd-=", IncreaseFontSize, None),
        KeyBinding::new("cmd-+", IncreaseFontSize, None),
        KeyBinding::new("cmd--", DecreaseFontSize, None),
        KeyBinding::new("cmd-0", ResetFontSize, None),
    ]);
}
