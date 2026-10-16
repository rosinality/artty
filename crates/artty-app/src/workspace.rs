//! The main window: a sidebar of tabs, each a layout of split panes running
//! terminal sessions.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{
    AgentState, ReadSource, Request, Response, SESSION_ENV, SOCKET_ENV, SplitDirection,
};
use artty_core::vt::ansi::Progress;
use artty_core::vt::event::Event;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::term::TermMode;
use artty_core::{
    Config, FrameCursor, Notice, SavedLayout, SavedTab, SavedWindow, SavedWorkspace, Session,
    SessionId, SessionOptions, ShellPhase, TerminalSize, Theme,
};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontWeight, Hsla, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, WindowBackgroundAppearance, WindowBounds, actions, canvas, div, fill,
    point, prelude::*, px, rgba, size,
};

use crate::app_state::{self, Shared};
use crate::keys;
use artty_core::layout::{self, Axis, Direction, Divider, Node, PaneId, Rect};
use crate::mouse::{self, Action, Button};
use artty_core::pane::{Pane, Status};
use crate::terminal::{self, Metrics, hsla};

actions!(
    artty,
    [
        NewTab,
        CloseTab,
        ClosePane,
        NextTab,
        PreviousTab,
        RenameTab,
        MoveTabUp,
        MoveTabDown,
        SplitRight,
        SplitDown,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        NextPane,
        PreviousPane,
        ToggleZoom,
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
/// Space between split panes; a 1px divider line is drawn in its middle.
const SPLIT_GAP: f32 = 13.;
/// How much unfocused panes in a split fade, so the focused one stands out.
const UNFOCUSED_FADE: f32 = 0.4;
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Shown under a restored pane's old output.
const RESTORED_BANNER: &str = "\r\n\x1b[2m── restored ──\x1b[0m\r\n";
/// How long the visual bell flash lasts.
const FLASH: Duration = Duration::from_millis(220);
/// Background commands running at least this long notify when they finish.
const LONG_COMMAND: Duration = Duration::from_secs(10);
const MIN_FONT_SIZE: f32 = 6.;
const MAX_FONT_SIZE: f32 = 72.;

/// `artty run` waiting for its command to finish.
struct PendingRun {
    pane: PaneId,
    reply: oneshot::Sender<Response>,
    deadline: Option<Instant>,
}

/// What to open in a new pane; everything defaults to a shell in the focused
/// pane's directory.
#[derive(Default)]
pub struct TabSpec {
    pub command: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    /// Output to show before the program starts.
    pub initial_output: Option<Vec<u8>>,
}

struct Tab {
    /// Name given by the user; otherwise the focused pane's title.
    title: Option<String>,
    root: Node,
    focused: PaneId,
    /// Show only the focused pane.
    zoomed: bool,
}

/// Right-click menu commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuCommand {
    Copy,
    Paste,
    OpenLink,
    CopyLink,
    SplitRight,
    SplitDown,
    RenameTab,
    ClearBuffer,
    ClosePane,
}

struct ContextMenu {
    position: Point<Pixels>,
    pane: PaneId,
    link: Option<String>,
    has_selection: bool,
}

/// A tab title being edited in the sidebar.
struct Renaming {
    tab: usize,
    text: String,
}

/// A tab being dragged in the sidebar; also its drag preview.
#[derive(Clone)]
struct DraggedTab {
    index: usize,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded(px(7.))
            .bg(rgba(0x2a3140f0))
            .border_1()
            .border_color(rgba(0xffffff30))
            .text_size(px(12.5))
            .text_color(rgba(0xffffffee))
            .font_family(".SystemUIFont")
            .child(self.title.clone())
    }
}

impl Tab {
    fn layout(&self, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        if self.zoomed {
            (vec![(self.focused, area)], Vec::new())
        } else {
            self.root.layout(area, SPLIT_GAP)
        }
    }
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
    panes: HashMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active: usize,
    /// Area the active tab's panes fill, from the last render.
    area: Rect,
    /// Pane rectangles and dividers of the active tab, from the last render.
    pane_rects: Vec<(PaneId, Rect)>,
    dividers: Vec<Divider>,
    /// A divider being dragged.
    drag: Option<Divider>,
    sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    runs: Vec<PendingRun>,
    search: Option<Search>,
    window_bounds: Option<SavedWindow>,
    window_handle: gpui::AnyWindowHandle,
    /// IME composition in progress, shown at the cursor until committed.
    marked_text: Option<String>,
    /// Focused pane's cursor from the last render, for placing the IME window.
    cursor: Option<FrameCursor>,
    cursor_visible: bool,
    /// Whether the cursor should blink, as of the last render.
    blink_enabled: bool,
    /// Skip one blink so the cursor stays solid while typing.
    blink_hold: bool,
    spinner_frame: usize,
    window_active: bool,
    /// Pane where the current mouse press started.
    mouse_pane: Option<PaneId>,
    /// A mouse drag is extending the selection.
    selecting: bool,
    /// Button held down while the program receives mouse reports.
    reporting_button: Option<Button>,
    /// Last cell reported for motion, to avoid duplicate reports.
    last_mouse_cell: Option<(usize, usize)>,
    /// Sub-line remainder of trackpad scrolling.
    scroll_remainder: f32,
    mouse_position: Option<Point<Pixels>>,
    /// The link under the pointer while ⌘ is held: pane, line, columns.
    hover_link: Option<(PaneId, usize, Range<usize>)>,
    context_menu: Option<ContextMenu>,
    renaming: Option<Renaming>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

fn agents_of(config: &Config) -> Arc<[String]> {
    config.agents.clone().into()
}

fn bounds(rect: Rect) -> Bounds<Pixels> {
    Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height)))
}

/// Play the system alert sound.
fn beep() {
    let _ = std::process::Command::new("osascript")
        .args(["-e", "beep"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}


impl Workspace {
    pub fn new(
        config: Config,
        config_error: Option<String>,
        spec: TabSpec,
        restore: Option<SavedWorkspace>,
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
            if this.window_active {
                app_state::set_frontmost(cx.weak_entity(), cx);
            }
            this.report_focus(this.focused_id(), this.window_active);
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
            panes: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            area: Rect::default(),
            pane_rects: Vec::new(),
            dividers: Vec::new(),
            drag: None,
            sink,
            runs: Vec::new(),
            search: None,
            window_bounds: None,
            window_handle: window.window_handle(),
            marked_text: None,
            cursor: None,
            cursor_visible: true,
            blink_enabled: false,
            blink_hold: false,
            spinner_frame: 0,
            window_active: true,
            mouse_pane: None,
            selecting: false,
            reporting_button: None,
            last_mouse_cell: None,
            scroll_remainder: 0.,
            mouse_position: None,
            hover_link: None,
            context_menu: None,
            renaming: None,
            _tasks: vec![events, blink, poll, spinner],
            _subscriptions: vec![activation],
        };
        this.area = Self::area_rect(window);
        match restore.filter(|state| !state.tabs.is_empty()) {
            Some(state) => this.restore(state, cx),
            None => {
                if let Err(err) = this.open_tab(spec, true, cx) {
                    log::error!("{err}");
                }
            }
        }
        this
    }

    pub fn window_handle(&self) -> gpui::AnyWindowHandle {
        self.window_handle
    }

    pub fn has_pane(&self, id: PaneId) -> bool {
        self.panes.contains_key(&id)
    }

    /// The focused pane's directory, for opening windows next to it.
    pub fn focused_cwd(&self) -> Option<PathBuf> {
        self.focused_session().and_then(Session::cwd)
    }

    /// Every pane, for `artty list`; `window` is this window's number.
    pub fn pane_infos(&self, window: usize, frontmost: bool) -> Vec<artty_core::control::TabInfo> {
        let mut infos = Vec::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            for id in tab.root.panes() {
                let focused = tab.focused == id;
                let active = frontmost && focused && index == self.active;
                infos.push(self.panes[&id].info(window, index + 1, active, focused));
            }
        }
        infos
    }

    // Saving and restoring.

    /// Reopen saved tabs: their layouts, directories and output, and agents.
    fn restore(&mut self, state: SavedWorkspace, cx: &mut Context<Self>) {
        for saved in state.tabs {
            let Some(root) = self.restore_layout(saved.layout, cx) else { continue };
            let panes = root.panes();
            let focused = panes[saved.focused.min(panes.len() - 1)];
            let zoomed = saved.zoomed && panes.len() > 1;
            self.tabs.push(Tab { title: saved.title, root, focused, zoomed });
        }
        if self.tabs.is_empty() {
            let _ = self.open_tab(TabSpec::default(), true, cx);
        }
        self.select(state.active.min(self.tabs.len().saturating_sub(1)), cx);
    }

    fn restore_layout(&mut self, layout: SavedLayout, cx: &mut App) -> Option<Node> {
        match layout {
            SavedLayout::Pane(saved) => {
                let initial_output = (!saved.scrollback.is_empty())
                    .then(|| format!("{}{RESTORED_BANNER}", saved.scrollback).into_bytes());
                let spec = TabSpec { command: saved.command, cwd: saved.cwd, title: saved.title, initial_output };
                let id = self.spawn_pane(spec, cx).inspect_err(|err| log::error!("restoring a pane: {err}")).ok()?;
                let resume = saved.agent.filter(|_| self.config.resume_agents).and_then(|agent| {
                    self.config.resume.get(&agent.name)?.command(agent.session.as_deref())
                });
                if let (Some(resume), Some(pane)) = (resume, self.panes.get_mut(&id)) {
                    pane.type_when_ready(resume);
                }
                Some(Node::Pane(id))
            }
            SavedLayout::Split { vertical, ratio, first, second } => {
                match (self.restore_layout(*first, cx), self.restore_layout(*second, cx)) {
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

    fn saved_layout(&self, node: &Node) -> SavedLayout {
        match node {
            Node::Pane(id) => SavedLayout::Pane(self.panes[id].saved(self.config.restore_scrollback)),
            Node::Split { axis, ratio, first, second } => SavedLayout::Split {
                vertical: *axis == Axis::Vertical,
                ratio: *ratio,
                first: Box::new(self.saved_layout(first)),
                second: Box::new(self.saved_layout(second)),
            },
        }
    }

    /// This window's tabs, for saving.
    pub fn saved(&self) -> SavedWorkspace {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| SavedTab {
                title: tab.title.clone(),
                layout: self.saved_layout(&tab.root),
                focused: tab.root.panes().iter().position(|&id| id == tab.focused).unwrap_or(0),
                zoomed: tab.zoomed,
            })
            .collect();
        SavedWorkspace { window: self.window_bounds, active: self.active, tabs }
    }

    // Geometry.

    /// The area panes fill, in window coordinates; matches `render`.
    fn area_rect(window: &Window) -> Rect {
        let viewport = window.viewport_size();
        Rect {
            x: SIDEBAR_WIDTH + PADDING,
            y: TITLEBAR_HEIGHT,
            width: (f32::from(viewport.width) - SIDEBAR_WIDTH - 2. * PADDING).max(1.),
            height: (f32::from(viewport.height) - TITLEBAR_HEIGHT - PADDING).max(1.),
        }
    }

    fn size_for(&self, rect: Rect) -> TerminalSize {
        TerminalSize {
            columns: (px(rect.width) / self.metrics.cell_width).floor().max(2.) as u16,
            lines: (px(rect.height) / self.metrics.line_height).floor().max(1.) as u16,
            cell_width: f32::from(self.metrics.cell_width).round() as u16,
            cell_height: f32::from(self.metrics.line_height).round() as u16,
        }
    }

    fn pane_at(&self, position: Point<Pixels>) -> Option<(PaneId, Rect)> {
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        self.pane_rects.iter().copied().find(|(_, rect)| rect.contains(x, y))
    }

    fn rect_of(&self, id: PaneId) -> Option<Rect> {
        self.pane_rects.iter().find(|(pane, _)| *pane == id).map(|(_, rect)| *rect)
    }

    /// The cell of pane `id` under a window position, and which half of it.
    fn cell_at(&self, id: PaneId, position: Point<Pixels>) -> (usize, usize, Side) {
        let rect = self.rect_of(id).unwrap_or(self.area);
        let size = self.panes.get(&id).map(|pane| pane.session.size());
        let (columns, lines) = size.map_or((1, 1), |s| (s.columns as usize, s.lines as usize));
        let x = ((position.x - px(rect.x)) / self.metrics.cell_width).max(0.);
        let y = ((position.y - px(rect.y)) / self.metrics.line_height).max(0.);
        let column = (x as usize).min(columns - 1);
        let side = if x.fract() < 0.5 || x as usize >= columns { Side::Left } else { Side::Right };
        ((y as usize).min(lines - 1), column, side)
    }

    // Tabs and panes.

    fn focused_id(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|tab| tab.focused)
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.focused_id()?)
    }

    fn focused_session(&self) -> Option<&Session> {
        self.focused_pane().map(|pane| &pane.session)
    }

    fn tab_of(&self, id: PaneId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.root.contains(id))
    }

    /// Whether the user can see pane `id` right now.
    fn visible(&self, id: PaneId) -> bool {
        let Some(tab) = self.tabs.get(self.active) else { return false };
        self.window_active && tab.root.contains(id) && (!tab.zoomed || tab.focused == id)
    }

    /// Start a session for a new pane (not yet placed in a tab).
    fn spawn_pane(&mut self, spec: TabSpec, cx: &mut App) -> Result<PaneId, String> {
        let id = app_state::next_pane_id(cx);
        let shared = cx.global::<Shared>();
        let explicit = spec.command.clone().filter(|c| !c.is_empty());
        let mut command = explicit.clone().or_else(|| Some(self.config.shell.clone())).filter(|c| !c.is_empty());
        let cwd = spec.cwd.or_else(|| self.focused_session().and_then(Session::cwd));

        let mut env = HashMap::new();
        if self.config.shell_integration
            && let Some(dir) = &shared.integration_dir
            && let Some(injection) = artty_core::integration::inject(dir, command.as_deref())
        {
            env.extend(injection.env);
            command = injection.command.or(command);
        }
        env.insert(SESSION_ENV.to_owned(), id.to_string());
        // Apps launched from Finder get no locale; shells need one for UTF-8.
        if std::env::var_os("LANG").is_none()
            && let Some(lang) = crate::notify::default_lang()
        {
            env.insert("LANG".to_owned(), lang);
        }
        if let Some(path) = &shared.control_path {
            env.insert(SOCKET_ENV.to_owned(), path.display().to_string());
        }
        if let Some(dir) = &shared.cli_dir {
            let path = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".to_owned(), format!("{}:{path}", dir.display()));
        }
        let options = SessionOptions {
            command: command.map(|mut c| (c.remove(0), c)),
            working_directory: cwd.filter(|dir| dir.is_dir()),
            env,
            scrollback: Some(self.config.scrollback),
            initial_output: spec.initial_output,
            cursor_shape: self.config.cursor_style.shape(),
        };
        let session = Session::spawn(id, options, self.size_for(self.area), self.theme.clone(), self.sink.clone())
            .map_err(|err| format!("failed to start: {err}"))?;
        self.panes.insert(id, Pane::new(session, spec.title, explicit, self.agents.clone()));
        Ok(id)
    }

    /// Open a tab with one pane as described by `spec`.
    fn open_tab(&mut self, spec: TabSpec, focus: bool, cx: &mut Context<Self>) -> Result<PaneId, String> {
        let id = self.spawn_pane(spec, cx)?;
        self.tabs.push(Tab { title: None, root: Node::Pane(id), focused: id, zoomed: false });
        if focus {
            self.select(self.tabs.len() - 1, cx);
        }
        cx.notify();
        Ok(id)
    }

    /// Split `target` (default: the focused pane), opening a pane right of or
    /// below it in the same directory.
    fn split(
        &mut self,
        mut spec: TabSpec,
        target: Option<PaneId>,
        axis: Axis,
        focus: bool,
        cx: &mut Context<Self>,
    ) -> Result<PaneId, String> {
        let target = target.or(self.focused_id()).ok_or("no pane to split")?;
        let index = self.tab_of(target).ok_or_else(|| format!("no pane {target}"))?;
        if spec.cwd.is_none() {
            spec.cwd = self.panes.get(&target).and_then(|pane| pane.session.cwd());
        }
        let id = self.spawn_pane(spec, cx)?;
        let tab = &mut self.tabs[index];
        tab.root.split(target, id, axis);
        tab.zoomed = false;
        if focus {
            self.focus_pane(id, cx);
        }
        cx.notify();
        Ok(id)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            let before = self.focused_id();
            self.close_search();
            self.active = index;
            self.reset_input_state();
            self.move_focus_report(before);
            cx.notify();
        }
    }

    /// Focus pane `id`, switching to its tab.
    fn focus_pane(&mut self, id: PaneId, cx: &mut Context<Self>) {
        let Some(index) = self.tab_of(id) else { return };
        let before = self.focused_id();
        if before != Some(id) {
            self.close_search();
        }
        self.active = index;
        let tab = &mut self.tabs[index];
        if tab.zoomed && tab.focused != id {
            tab.zoomed = false;
        }
        tab.focused = id;
        self.reset_input_state();
        self.move_focus_report(before);
        cx.notify();
    }

    fn reset_input_state(&mut self) {
        self.marked_text = None;
        self.selecting = false;
        self.reporting_button = None;
        self.mouse_pane = None;
        self.cursor_visible = true;
    }

    /// Tell programs that track focus (vim, etc.) that it moved.
    fn move_focus_report(&self, before: Option<PaneId>) {
        let after = self.focused_id();
        if before != after && self.window_active {
            self.report_focus(before, false);
            self.report_focus(after, true);
        }
    }

    fn report_focus(&self, id: Option<PaneId>, focused: bool) {
        if let Some(pane) = id.and_then(|id| self.panes.get(&id))
            && pane.session.mode().contains(TermMode::FOCUS_IN_OUT)
        {
            pane.session.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }

    fn focus_direction(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(from) = self.focused_id() else { return };
        if let Some(to) = layout::neighbor(&self.pane_rects, from, direction) {
            self.focus_pane(to, cx);
        }
    }

    fn cycle_pane(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let panes = tab.root.panes();
        let Some(index) = panes.iter().position(|&id| id == tab.focused) else { return };
        let next = if forward { (index + 1) % panes.len() } else { (index + panes.len() - 1) % panes.len() };
        self.focus_pane(panes[next], cx);
    }

    fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && matches!(tab.root, Node::Split { .. })
        {
            tab.zoomed = !tab.zoomed;
            cx.notify();
        }
    }

    /// Close pane `id`; its tab closes with its last pane, and the app with
    /// its last tab.
    fn close_pane(&mut self, id: PaneId, cx: &mut Context<Self>) {
        let Some(index) = self.tab_of(id) else { return };
        let was_focused = self.focused_id() == Some(id);
        if was_focused {
            self.close_search();
        }
        self.panes.remove(&id);
        self.fail_runs(id, "the pane closed");

        let tab = &mut self.tabs[index];
        let order = tab.root.panes();
        let root = std::mem::replace(&mut tab.root, Node::Pane(id));
        match root.without(id) {
            Some(root) => {
                let remaining = root.panes();
                if tab.focused == id {
                    let position = order.iter().position(|&pane| pane == id).unwrap_or(0);
                    tab.focused = remaining[position.min(remaining.len() - 1)];
                }
                if remaining.len() == 1 {
                    tab.zoomed = false;
                }
                tab.root = root;
                if was_focused {
                    self.reset_input_state();
                }
            }
            None => self.remove_tab(index, cx),
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(ids) = self.tabs.get(index).map(|tab| tab.root.panes()) else { return };
        if index == self.active {
            self.close_search();
        }
        for id in ids {
            self.panes.remove(&id);
            self.fail_runs(id, "the pane closed");
        }
        self.remove_tab(index, cx);
    }

    fn remove_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // `render` closes the window.
            cx.notify();
            return;
        }
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.reset_input_state();
        cx.notify();
    }

    fn fail_runs(&mut self, id: PaneId, message: &str) {
        for run in self.runs.extract_if(.., |run| run.pane == id) {
            let _ = run.reply.send(Response::error(message));
        }
    }

    /// A tab's name: what the user called it, or its focused pane's title.
    fn tab_title(&self, tab: &Tab) -> SharedString {
        match &tab.title {
            Some(title) => title.clone().into(),
            None => self.panes.get(&tab.focused).map(|pane| pane.title().into()).unwrap_or_default(),
        }
    }

    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let active = self.focused_id();
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        // The active tab stays active wherever it moved.
        if let Some(index) = active.and_then(|id| self.tab_of(id)) {
            self.active = index;
        }
        self.renaming = None;
        cx.notify();
    }

    fn start_rename(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        let text = self.tab_title(tab).to_string();
        self.close_search();
        self.marked_text = None;
        self.renaming = Some(Renaming { tab: index, text });
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(Renaming { tab, text }) = self.renaming.take()
            && commit
            && let Some(tab) = self.tabs.get_mut(tab)
        {
            let text = text.trim();
            tab.title = (!text.is_empty()).then(|| text.to_owned());
        }
        self.marked_text = None;
        cx.notify();
    }

    fn rename_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.finish_rename(false, cx),
            "enter" => self.finish_rename(true, cx),
            "backspace" => {
                if let Some(renaming) = self.renaming.as_mut() {
                    renaming.text.pop();
                }
                cx.notify();
            }
            // Text arrives through the input handler.
            _ => return,
        }
        cx.stop_propagation();
    }

    fn open_context_menu(&mut self, id: PaneId, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(pane) = self.panes.get(&id) else { return };
        let (line, column, _) = self.cell_at(id, position);
        self.context_menu = Some(ContextMenu {
            position,
            pane: id,
            link: pane.session.link_at(line, column),
            has_selection: pane.session.selection_text().is_some(),
        });
        cx.notify();
    }

    fn run_menu_command(&mut self, command: MenuCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.take() else { return };
        let id = menu.pane;
        match command {
            MenuCommand::Copy => self.copy(&Copy, window, cx),
            MenuCommand::Paste => self.paste(&Paste, window, cx),
            MenuCommand::OpenLink => {
                if let Some(link) = &menu.link {
                    let _ = std::process::Command::new("open").arg(link).spawn();
                }
            }
            MenuCommand::CopyLink => {
                if let Some(link) = menu.link {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
            }
            MenuCommand::SplitRight | MenuCommand::SplitDown => {
                let axis = if command == MenuCommand::SplitRight { Axis::Horizontal } else { Axis::Vertical };
                if let Err(err) = self.split(TabSpec::default(), Some(id), axis, true, cx) {
                    log::error!("{err}");
                }
            }
            MenuCommand::RenameTab => {
                if let Some(index) = self.tab_of(id) {
                    self.start_rename(index, cx);
                }
            }
            MenuCommand::ClearBuffer => {
                if let Some(pane) = self.panes.get(&id) {
                    pane.session.clear_buffer();
                }
            }
            MenuCommand::ClosePane => self.close_pane(id, cx),
        }
        cx.notify();
    }

    // Events from sessions.

    /// Tell the user about a pane that needs them, unless they're looking at it.
    fn notify_user(&self, id: PaneId, title: Option<&str>, body: &str) {
        if !self.config.notifications || self.visible(id) {
            return;
        }
        let Some(pane) = self.panes.get(&id) else { return };
        let pane_title = pane.title();
        let title = title.map_or_else(|| pane_title.to_string(), |t| format!("{t} — {pane_title}"));
        crate::notify::post(&title, if body.is_empty() { "Needs your attention" } else { body });
    }

    fn handle_events(&mut self, events: Vec<(SessionId, Event)>, cx: &mut Context<Self>) {
        for (id, event) in events {
            let visible = self.visible(id);
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            let Some(notice) = pane.session.handle(event) else { continue };
            match notice {
                Notice::Redraw => {}
                Notice::Title(title) => pane.set_title(Some(title)),
                Notice::ResetTitle => pane.set_title(None),
                Notice::Bell => {
                    if visible {
                        if self.config.visual_bell {
                            pane.flash = Some(Instant::now());
                        }
                    } else {
                        pane.alert(None);
                        self.notify_user(id, None, "Bell");
                    }
                    if self.config.bell_sound {
                        beep();
                    }
                }
                Notice::Notification { title, body } => {
                    pane.alert(Some(body.clone()).filter(|b| !b.is_empty()).or(title.clone()));
                    self.notify_user(id, title.as_deref(), &body);
                }
                Notice::Progress(progress) => {
                    pane.progress = (progress != Progress::Remove).then_some(progress);
                }
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
                        let _ = run.reply.send(response.clone());
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
                Notice::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Notice::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
                    pane.session.reply_clipboard(&format, &text);
                }
                Notice::Exited => {
                    self.close_pane(id, cx);
                    continue;
                }
            }
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.refresh();
            }
            cx.notify();
        }
    }

    // Control requests.

    fn resolve(&self, pane: Option<PaneId>) -> Result<PaneId, String> {
        match pane {
            Some(id) if self.panes.contains_key(&id) => Ok(id),
            Some(id) => Err(format!("no pane {id}")),
            None => self.focused_id().ok_or_else(|| "no tabs".to_owned()),
        }
    }

    pub fn handle_request(&mut self, request: Request, reply: oneshot::Sender<Response>, cx: &mut Context<Self>) {
        let response = match request {
            // Answered when the command finishes.
            Request::Run { tab, command, timeout_secs } => match self.start_run(tab, &command) {
                Ok(pane) => {
                    let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
                    self.runs.push(PendingRun { pane, reply, deadline });
                    return;
                }
                Err(message) => Response::error(message),
            },
            request => self.respond(request, cx).unwrap_or_else(Response::error),
        };
        let _ = reply.send(response);
        cx.notify();
    }

    /// Type `command` into a pane's shell; returns the pane to wait on.
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

    fn respond(&mut self, request: Request, cx: &mut Context<Self>) -> Result<Response, String> {
        Ok(match request {
            // Normally answered for all windows by `app_state::route`.
            Request::List => Response::Tabs { tabs: self.pane_infos(1, true) },
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
                let spec = TabSpec { command, cwd, title, initial_output: None };
                let id = match split {
                    None => self.open_tab(spec, focus, cx)?,
                    Some(direction) => {
                        let target = target.map(|t| self.resolve(Some(t))).transpose()?;
                        let axis = match direction {
                            SplitDirection::Right => Axis::Horizontal,
                            SplitDirection::Down => Axis::Vertical,
                        };
                        self.split(spec, target, axis, focus, cx)?
                    }
                };
                Response::Tab { id }
            }
            Request::Focus { tab } => {
                let id = self.resolve(Some(tab))?;
                self.focus_pane(id, cx);
                Response::Ok
            }
            Request::Rename { tab, title } => {
                let id = self.resolve(tab)?;
                let index = self.tab_of(id).ok_or("the pane closed")?;
                let title = title.trim();
                self.tabs[index].title = (!title.is_empty()).then(|| title.to_owned());
                Response::Ok
            }
            Request::Close { tab } => {
                let id = self.resolve(Some(tab))?;
                self.close_pane(id, cx);
                Response::Ok
            }
            Request::Run { .. } => unreachable!("handled in handle_request"),
            Request::Settings | Request::Restart => return Err("handled by the app".into()),
            Request::ServerStatus | Request::StopServer | Request::Processes { .. } | Request::Kill { .. } => {
                return Err("handled by the artty server".into());
            }
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
        })
    }

    // Timers.

    /// Send user input to the focused pane, keeping the cursor solid.
    fn input(&mut self, bytes: Vec<u8>) {
        if let Some(session) = self.focused_session() {
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
        if self.panes.values().any(|pane| pane.status.busy()) {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
            cx.notify();
        }
        // Animate visual bells, and clear finished ones.
        let mut flashing = false;
        for pane in self.panes.values_mut() {
            if pane.flash.is_some_and(|at| at.elapsed() > FLASH) {
                pane.flash = None;
                flashing = true;
            }
            flashing |= pane.flash.is_some();
        }
        if flashing {
            cx.notify();
        }
    }

    /// Underline the link under the pointer while ⌘ is held.
    fn update_hover_link(&mut self, platform: bool, cx: &mut Context<Self>) {
        let hover = platform
            .then_some(self.mouse_position)
            .flatten()
            .and_then(|position| {
                let (id, _) = self.pane_at(position)?;
                let (line, column, _) = self.cell_at(id, position);
                let (_, span) = self.panes.get(&id)?.session.link_span(line, column)?;
                Some((id, line, span))
            });
        if hover != self.hover_link {
            self.hover_link = hover;
            cx.notify();
        }
    }

    /// Once a second: what's running in each pane, config changes, timeouts.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for pane in self.panes.values_mut() {
            let process = pane.session.process();
            let before = (pane.status.clone(), pane.process.clone());
            pane.process = process;
            pane.refresh();
            pane.flush_pending_input();
            changed |= before != (pane.status.clone(), pane.process.clone()) || pane.status.busy();
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
        for pane in self.panes.values_mut() {
            pane.session.set_theme(self.theme.clone());
            pane.session.set_cursor_shape(config.cursor_style.shape());
            pane.set_agents(self.agents.clone());
        }
        if config.font_size != self.config.font_size {
            self.font_size = config.font_size;
        }
        self.metrics = Metrics::measure(window, &config, self.font_size);
        window.set_background_appearance(background_appearance(&config));
        self.config = Arc::new(config);
    }

    fn jump_to_attention(&mut self, cx: &mut Context<Self>) {
        // Every pane in tab order, starting after the focused one.
        let all: Vec<PaneId> = self.tabs.iter().flat_map(|tab| tab.root.panes()).collect();
        let start = self.focused_id().and_then(|id| all.iter().position(|&p| p == id)).map_or(0, |i| i + 1);
        let order: Vec<PaneId> = all.iter().cycle().skip(start).take(all.len()).copied().collect();
        let waiting = |id: &PaneId| matches!(self.panes[id].status, Status::Agent { state: AgentState::Waiting, .. });
        let target = order.iter().find(|id| waiting(id)).or_else(|| order.iter().find(|id| self.panes[*id].attention));
        if let Some(&id) = target {
            self.focus_pane(id, cx);
        }
    }

    // Keyboard.

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform {
            if let Some(digit) = keystroke.key.parse::<usize>().ok().filter(|d| (1..=9).contains(d))
                && !keystroke.modifiers.alt
            {
                self.select(digit - 1, cx);
                cx.stop_propagation();
            }
            return;
        }
        if self.context_menu.take().is_some() {
            cx.notify();
            if keystroke.key == "escape" {
                cx.stop_propagation();
                return;
            }
        }
        if self.renaming.is_some() {
            self.rename_key(event, cx);
            return;
        }
        if self.search.is_some() {
            self.search_key(event, cx);
            return;
        }
        let Some(session) = self.focused_session() else { return };
        if let Some(bytes) = keys::to_esc(keystroke, session.mode(), self.config.option_as_meta) {
            self.input(bytes);
            cx.stop_propagation();
        }
    }

    // Search (⌘F), in the focused pane.

    fn open_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search = Some(Search { query: String::new(), found: None, error: None });
        }
        cx.notify();
    }

    fn close_search(&mut self) {
        if self.search.take().is_some()
            && let Some(id) = self.focused_id()
            && let Some(pane) = self.panes.get_mut(&id)
        {
            pane.session.end_search();
        }
    }

    fn run_search(&mut self, older: bool, restart: bool, cx: &mut Context<Self>) {
        let Some(id) = self.focused_id() else { return };
        let Some(search) = self.search.as_mut() else { return };
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if restart {
            pane.session.clear_selection();
        }
        if search.query.is_empty() {
            search.found = None;
            search.error = None;
        } else {
            match pane.session.search(&search.query, older) {
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

    // Mouse.

    /// Whether mouse events in pane `id` go to its program instead of
    /// selecting text. Holding Shift always selects, as in other terminals.
    fn mouse_reporting(&self, id: PaneId, modifiers: &Modifiers) -> Option<TermMode> {
        let mode = self.panes.get(&id)?.session.mode();
        (mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift).then_some(mode)
    }

    fn report_mouse(
        &mut self,
        id: PaneId,
        button: Button,
        action: Action,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) {
        let Some(mode) = self.mouse_reporting(id, modifiers) else { return };
        let (line, column, _) = self.cell_at(id, position);
        if action == Action::Motion && self.last_mouse_cell == Some((line, column)) {
            return;
        }
        self.last_mouse_cell = Some((line, column));
        if let Some(bytes) = mouse::report(button, action, line, column, modifiers, mode)
            && let Some(pane) = self.panes.get(&id)
        {
            pane.session.write(bytes);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        let Some((id, _)) = self.pane_at(event.position) else { return };
        if self.focused_id() != Some(id) {
            self.focus_pane(id, cx);
        }
        self.mouse_pane = Some(id);
        let Some(session) = self.panes.get(&id).map(|pane| &pane.session) else { return };

        if event.button == MouseButton::Left && event.modifiers.platform {
            let (line, column, _) = self.cell_at(id, event.position);
            if let Some(link) = session.link_at(line, column) {
                let _ = std::process::Command::new("open").arg(&link).spawn();
            }
            return;
        }
        let menu_click = event.button == MouseButton::Right
            || (event.button == MouseButton::Left && event.modifiers.control);
        if menu_click && (self.mouse_reporting(id, &event.modifiers).is_none() || event.modifiers.control) {
            self.open_context_menu(id, event.position, cx);
            return;
        }
        if self.mouse_reporting(id, &event.modifiers).is_some() {
            if let Some(button) = Button::from_gpui(event.button) {
                self.reporting_button = Some(button);
                self.last_mouse_cell = None;
                self.report_mouse(id, button, Action::Press, event.position, &event.modifiers);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let (line, column, side) = self.cell_at(id, event.position);
        let session = &self.panes[&id].session;
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
        self.mouse_position = Some(event.position);
        self.update_hover_link(event.modifiers.platform, cx);
        if let Some(divider) = &self.drag {
            let ratio = layout::drag_ratio(divider, event.position.x.into(), event.position.y.into(), SPLIT_GAP);
            let path = divider.path.clone();
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.root.set_ratio(&path, ratio);
            }
            cx.notify();
            return;
        }
        if let (Some(button), Some(id)) = (self.reporting_button, self.mouse_pane) {
            self.report_mouse(id, button, Action::Motion, event.position, &event.modifiers);
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            if let Some(id) = self.mouse_pane {
                let (line, column, side) = self.cell_at(id, event.position);
                if let Some(pane) = self.panes.get(&id) {
                    pane.session.update_selection(line, column, side);
                }
            }
            cx.notify();
        } else if event.pressed_button.is_none()
            && let Some((id, _)) = self.pane_at(event.position)
            && Some(id) == self.focused_id()
        {
            self.report_mouse(id, Button::None, Action::Motion, event.position, &event.modifiers);
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
            return;
        }
        let id = self.mouse_pane.take();
        if let (Some(button), Some(id)) = (self.reporting_button.take(), id) {
            self.report_mouse(id, button, Action::Release, event.position, &event.modifiers);
            return;
        }
        if self.selecting {
            self.selecting = false;
            if let Some(pane) = id.and_then(|id| self.panes.get(&id)) {
                match pane.session.selection_text() {
                    Some(text) if self.config.copy_on_select => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    Some(_) => {}
                    // A plain click selects nothing; drop the empty selection.
                    None => pane.session.clear_selection(),
                }
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
        // Scroll the pane under the pointer, focused or not.
        let Some((id, _)) = self.pane_at(event.position) else { return };
        if lines == 0 {
            return;
        }
        let Some(pane) = self.panes.get(&id) else { return };
        let mode = pane.session.mode();
        if self.mouse_reporting(id, &event.modifiers).is_some() {
            let button = if lines > 0 { Button::WheelUp } else { Button::WheelDown };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(id, button, Action::Press, event.position, &event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs (less, vim) get arrow keys instead.
            let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA".as_slice(),
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            pane.session.write(key.repeat(lines.unsigned_abs() as usize));
        } else {
            pane.session.scroll(lines);
        }
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.focused_session().and_then(Session::selection_text) {
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
        let Some(session) = self.focused_session() else { return };
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

    // Sidebar.

    fn status_icon(&self, pane: &Pane) -> Option<(SharedString, Hsla, f32)> {
        let color = |hex: u32| -> Hsla { rgba(hex).into() };
        // Braille spinner frames are tiny at text size.
        Some(match &pane.status {
            status if status.busy() => (SPINNER[self.spinner_frame].into(), color(0x8fb4ffff), 16.),
            Status::Agent { state: AgentState::Waiting, .. } => ("●".into(), color(0xe0af68ff), 12.),
            Status::Agent { state: AgentState::Done, .. } => ("✓".into(), color(0x9ece6aff), 12.),
            Status::Failed { .. } => ("✕".into(), color(0xf7768eff), 12.),
            _ if pane.attention => ("●".into(), color(0xe0af68ff), 12.),
            _ => return None,
        })
    }

    fn icon_cell(&self, pane: &Pane) -> gpui::Div {
        let icon = self.status_icon(pane);
        div()
            .w(px(12.))
            .flex_none()
            .flex()
            .justify_center()
            .when_some(icon, |d, (icon, color, size)| d.text_color(color).text_size(px(size)).child(icon))
    }

    fn pane_detail(pane: &Pane) -> Option<String> {
        let detail = pane.detail();
        match pane.progress {
            Some(Progress::Normal(percent) | Progress::Paused(Some(percent)) | Progress::Error(Some(percent))) => {
                Some(match detail {
                    Some(detail) => format!("{detail} · {percent}%"),
                    None => format!("{percent}%"),
                })
            }
            _ => detail,
        }
    }

    fn one_line(text: impl Into<SharedString>) -> gpui::Div {
        // GPUI 0.2 caches the first measurement of `nowrap` text, which happens
        // before the width is known, so `.truncate()` only clips. A one-line
        // clamp re-measures at the real width and ellipsizes.
        div().overflow_hidden().line_clamp(1).text_ellipsis().child(text.into())
    }

    fn tab_row(&self, index: usize, tab: &Tab, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let selected = index == self.active;
        let ids = tab.root.panes();
        let split = ids.len() > 1;
        let panes: Vec<&Pane> = ids.iter().map(|id| &self.panes[id]).collect();
        let focused = &self.panes[&tab.focused];
        // The row shows the most urgent pane, preferring the focused one.
        let summary = panes
            .iter()
            .copied()
            .max_by_key(|pane| (pane.status.urgency(), std::ptr::eq(*pane, focused)))
            .unwrap_or(focused);
        let attention = panes.iter().any(|pane| pane.attention);
        let label: SharedString = if index < 9 { format!("⌘{}", index + 1).into() } else { "".into() };
        let text_color = if selected { rgba(0xffffffee) } else { rgba(0xffffffb0) };
        let detail = if split { None } else { Self::pane_detail(focused) };

        let header = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(self.icon_cell(summary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(match self.renaming.as_ref().filter(|r| r.tab == index) {
                        Some(renaming) => {
                            let marked = self.marked_text.as_deref().unwrap_or_default();
                            div()
                                .px_1()
                                .rounded(px(4.))
                                .bg(rgba(0x0000004d))
                                .border_1()
                                .border_color(rgba(0x8fb4ff99))
                                .text_color(rgba(0xffffffee))
                                .child(format!("{}{marked}▏", renaming.text))
                        }
                        None => Self::one_line(self.tab_title(tab))
                            .text_color(text_color)
                            .when(attention, |d| d.font_weight(FontWeight::SEMIBOLD)),
                    })
                    .when_some(detail, |d, detail| {
                        d.child(Self::one_line(detail).text_size(px(11.)).text_color(rgba(0xffffff70)))
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
            );

        // Split tabs list their panes, so every agent's state is visible.
        let pane_rows = split.then(|| {
            ids.iter().map(|&id| {
                let pane = &self.panes[&id];
                let is_focused = id == tab.focused;
                let line = match Self::pane_detail(pane) {
                    Some(detail) => format!("{} · {detail}", pane.title()),
                    None => pane.title().to_string(),
                };
                div()
                    .id(("pane", id as usize))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(14.))
                    .text_size(px(11.5))
                    .rounded(px(5.))
                    .hover(|d| d.bg(rgba(0xffffff0c)))
                    .on_click(cx.listener(move |this, _, _, cx| this.focus_pane(id, cx)))
                    .child(self.icon_cell(pane))
                    .child(
                        Self::one_line(line)
                            .flex_1()
                            .min_w_0()
                            .text_color(if is_focused && selected { rgba(0xffffffd0) } else { rgba(0xffffff80) })
                            .when(pane.attention, |d| d.font_weight(FontWeight::SEMIBOLD)),
                    )
            })
        });

        div()
            .id(("tab", index))
            .flex()
            .flex_col()
            .gap(px(2.))
            .min_h(px(30.))
            .py(px(4.))
            .px_2()
            .rounded(px(7.))
            .when(selected, |d| d.bg(rgba(0xffffff1c)))
            .when(attention && !selected, |d| d.bg(rgba(0xe0af6814)))
            .hover(|d| d.bg(rgba(0xffffff10)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.start_rename(index, cx);
                } else {
                    this.select(index, cx);
                }
            }))
            .on_drag(DraggedTab { index, title: self.tab_title(tab) }, |dragged, _, _, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedTab>(|style, _, _, _| style.bg(rgba(0x8fb4ff26)))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| this.move_tab(dragged.index, index, cx)))
            .child(header)
            .children(pane_rows.into_iter().flatten())
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self.tabs.iter().enumerate().map(|(i, tab)| self.tab_row(i, tab, cx)).collect();
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
                                let _ = this.open_tab(TabSpec::default(), true, cx);
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
            .when(app_state::update_ready(cx), |d| {
                d.child(
                    div()
                        .mt_2()
                        .mx_1()
                        .p_2()
                        .rounded(px(8.))
                        .bg(rgba(0x3478f626))
                        .border_1()
                        .border_color(rgba(0x3478f666))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_color(rgba(0xffffffee)).child("New build ready"))
                                .child(div().text_size(px(11.)).text_color(rgba(0xffffff80)).child("Tabs reopen after restart")),
                        )
                        .child(
                            div()
                                .id("restart")
                                .px_2()
                                .py(px(3.))
                                .rounded(px(6.))
                                .bg(rgba(0x3478f6ff))
                                .hover(|d| d.bg(rgba(0x4a88ffff)))
                                .text_color(rgba(0xffffffff))
                                .child("Restart")
                                .on_click(|_, _, cx| app_state::restart(cx)),
                        ),
                )
            })
    }

    /// The right-click menu, over a backdrop that dismisses it.
    fn context_menu_overlay(&self, viewport: gpui::Size<Pixels>, cx: &mut Context<Self>) -> Option<gpui::Div> {
        const WIDTH: f32 = 220.;
        const ITEM: f32 = 24.;
        const SEPARATOR: f32 = 9.;
        let menu = self.context_menu.as_ref()?;
        let item = |label: &'static str, keys: &'static str, enabled: bool, command: MenuCommand| {
            Some((label, keys, enabled, command))
        };
        let mut items = vec![
            item("Copy", "⌘C", menu.has_selection, MenuCommand::Copy),
            item("Paste", "⌘V", true, MenuCommand::Paste),
            None,
        ];
        if menu.link.is_some() {
            items.extend([item("Open Link", "", true, MenuCommand::OpenLink), item("Copy Link", "", true, MenuCommand::CopyLink), None]);
        }
        items.extend([
            item("Split Right", "⌘D", true, MenuCommand::SplitRight),
            item("Split Down", "⇧⌘D", true, MenuCommand::SplitDown),
            None,
            item("Rename Tab…", "", true, MenuCommand::RenameTab),
            item("Clear Buffer", "", true, MenuCommand::ClearBuffer),
            None,
            item("Close Pane", "⌘W", true, MenuCommand::ClosePane),
        ]);

        // Keep the menu on screen.
        let height = 10. + items.iter().map(|i| if i.is_some() { ITEM } else { SEPARATOR }).sum::<f32>();
        let (mut x, mut y) = (f32::from(menu.position.x), f32::from(menu.position.y));
        if x + WIDTH > f32::from(viewport.width) - 4. {
            x = (x - WIDTH).max(4.);
        }
        if y + height > f32::from(viewport.height) - 4. {
            y = (f32::from(viewport.height) - height - 4.).max(4.);
        }

        let rows = items.into_iter().enumerate().map(|(i, item)| match item {
            None => div().h(px(1.)).mx(px(9.)).my(px(4.)).bg(rgba(0xffffff1a)).into_any_element(),
            Some((label, keys, enabled, command)) => div()
                .id(("menu-item", i))
                .flex()
                .items_center()
                .justify_between()
                .h(px(ITEM))
                .px(px(9.))
                .rounded(px(5.))
                .text_color(if enabled { rgba(0xffffffee) } else { rgba(0xffffff4d) })
                .when(enabled, |d| {
                    d.hover(|d| d.bg(rgba(0x3478f6ff)))
                        .on_click(cx.listener(move |this, _, window, cx| this.run_menu_command(command, window, cx)))
                })
                .child(label)
                .child(div().text_color(rgba(0xffffff66)).child(keys))
                .into_any_element(),
        });

        let backdrop = div()
            .id("menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                this.context_menu = None;
                cx.notify();
            }));
        let panel = div()
            .id("context-menu")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(WIDTH))
            .occlude()
            .flex()
            .flex_col()
            .p(px(5.))
            .rounded(px(8.))
            .bg(rgba(0x262a33f5))
            .border_1()
            .border_color(rgba(0xffffff26))
            .shadow_lg()
            .text_size(px(13.))
            .children(rows);
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }

    /// The find bar, at the top right of the focused pane. Positions are
    /// relative to the terminal area's container.
    fn search_bar(&self, origin: Point<Pixels>) -> Option<impl IntoElement> {
        let search = self.search.as_ref()?;
        let rect = self.rect_of(self.focused_id()?)?;
        let mut query = search.query.clone();
        if let Some(marked) = &self.marked_text {
            query.push_str(marked);
        }
        let (status, status_color) = match (&search.error, search.found) {
            (Some(_), _) => ("invalid pattern".to_owned(), rgba(0xf7768eff)),
            (None, Some(false)) => ("no match".to_owned(), rgba(0xf7768eff)),
            _ => ("⏎ older · ⇧⏎ newer · esc".to_owned(), rgba(0xffffff60)),
        };
        // The container's right edge is the area's plus its padding.
        let container_right = self.area.x + self.area.width + PADDING;
        Some(
            div()
                .absolute()
                .top(px(rect.y) - origin.y + px(6.))
                .right(px(container_right - (rect.x + rect.width) + 4.))
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

    /// Hit areas for dragging dividers. Positions are relative to the
    /// terminal area's container at `origin`.
    fn divider_handles(&self, origin: Point<Pixels>, cx: &mut Context<Self>) -> Vec<impl IntoElement> {
        self.dividers
            .iter()
            .enumerate()
            .map(|(i, divider)| {
                let rect = divider.rect;
                let handle = divider.clone();
                div()
                    .id(("divider", i))
                    .absolute()
                    .left(px(rect.x) - origin.x)
                    .top(px(rect.y) - origin.y)
                    .w(px(rect.width))
                    .h(px(rect.height))
                    .map(|d| match divider.axis {
                        Axis::Horizontal => d.cursor_col_resize(),
                        Axis::Vertical => d.cursor_row_resize(),
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.drag = Some(handle.clone());
                            cx.stop_propagation();
                        }),
                    )
            })
            .collect()
    }
}

fn saved_window(bounds: WindowBounds) -> SavedWindow {
    let (rect, maximized, fullscreen) = match bounds {
        WindowBounds::Windowed(rect) => (rect, false, false),
        WindowBounds::Maximized(rect) => (rect, true, false),
        WindowBounds::Fullscreen(rect) => (rect, false, true),
    };
    SavedWindow {
        x: rect.origin.x.into(),
        y: rect.origin.y.into(),
        width: rect.size.width.into(),
        height: rect.size.height.into(),
        maximized,
        fullscreen,
    }
}

/// Window bounds to open with, from a saved window.
pub fn window_bounds(saved: &SavedWindow) -> WindowBounds {
    let rect = Bounds::new(point(px(saved.x), px(saved.y)), size(px(saved.width), px(saved.height)));
    if saved.fullscreen {
        WindowBounds::Fullscreen(rect)
    } else if saved.maximized {
        WindowBounds::Maximized(rect)
    } else {
        WindowBounds::Windowed(rect)
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

/// What to paint for one pane.
struct PaneView {
    rect: Rect,
    frame: artty_core::Frame,
    focused: bool,
    link: Option<(usize, Range<usize>)>,
    flash: f32,
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The last tab closed: so does the window.
        if self.tabs.is_empty() {
            window.remove_window();
            return div();
        }
        let build_started = std::time::Instant::now();
        if self.reload_config {
            self.apply_config(window);
        }
        self.area = Self::area_rect(window);
        self.window_bounds = Some(saved_window(window.window_bounds()));
        self.window_active = window.is_window_active();

        // Size every pane of every tab to its place in the layout.
        for (index, tab) in self.tabs.iter().enumerate() {
            let (rects, dividers) = tab.layout(self.area);
            for (id, rect) in &rects {
                let size = self.size_for(*rect);
                if let Some(pane) = self.panes.get_mut(id) {
                    pane.session.resize(size);
                }
            }
            if index == self.active {
                self.pane_rects = rects;
                self.dividers = dividers;
            }
        }
        if self.window_active {
            for (id, _) in &self.pane_rects {
                if let Some(pane) = self.panes.get_mut(id) {
                    pane.attention = false;
                }
            }
        }

        let focused_id = self.focused_id();
        let split = self.pane_rects.len() > 1;
        let mut views = Vec::new();
        for (id, rect) in self.pane_rects.clone() {
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            let link = self.hover_link.as_ref().filter(|(pane, ..)| *pane == id).map(|(_, line, span)| (*line, span.clone()));
            let flash = pane.flash.map_or(0., |at| 1. - (at.elapsed().as_secs_f32() / FLASH.as_secs_f32()).min(1.));
            views.push(PaneView { rect, frame: pane.session.frame(), focused: Some(id) == focused_id, link, flash });
        }
        self.cursor = views.iter().find(|view| view.focused).and_then(|view| view.frame.cursor);

        let window_focused = self.focus.is_focused(window) && self.window_active;
        let typing = window_focused && self.search.is_none() && self.renaming.is_none();
        self.blink_enabled = typing && (self.config.cursor_blink || self.cursor.is_some_and(|c| c.blinking));

        let title = self.tabs.get(self.active).map(|tab| self.tab_title(tab)).unwrap_or_default();
        window.set_window_title(&title);

        let tint = Hsla { a: self.config.background_opacity.clamp(0., 1.), ..hsla(self.theme.background) };
        let divider_color: Hsla = rgba(0xffffff1c).into();
        let metrics = self.metrics.clone();
        let marked_text = self.marked_text.clone().filter(|_| self.search.is_none() && self.renaming.is_none());
        let cursor_visible = self.cursor_visible;
        let focused_rect = focused_id.and_then(|id| self.rect_of(id));
        let divider_lines: Vec<Bounds<Pixels>> = self
            .dividers
            .iter()
            .map(|divider| {
                let r = divider.rect;
                match divider.axis {
                    Axis::Horizontal => bounds(Rect { x: r.x + (r.width / 2.).floor(), width: 1., ..r }),
                    Axis::Vertical => bounds(Rect { y: r.y + (r.height / 2.).floor(), height: 1., ..r }),
                }
            })
            .collect();
        let focus = self.focus.clone();
        let entity = cx.entity();
        let build_time = build_started.elapsed();
        // The terminal container starts right of the sidebar, below the title bar.
        let container = point(px(SIDEBAR_WIDTH), px(TITLEBAR_HEIGHT));

        div()
            .track_focus(&self.focus)
            .key_context("Workspace")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                if let Err(err) = this.open_tab(TabSpec::default(), true, cx) {
                    log::error!("{err}");
                }
            }))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| {
                if let Some(id) = this.focused_id() {
                    this.close_pane(id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_tab(this.active, cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| {
                let next = (this.active + 1) % this.tabs.len().max(1);
                this.select(next, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| {
                let count = this.tabs.len().max(1);
                this.select((this.active + count - 1) % count, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                if let Err(err) = this.split(TabSpec::default(), None, Axis::Horizontal, true, cx) {
                    log::error!("{err}");
                }
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                if let Err(err) = this.split(TabSpec::default(), None, Axis::Vertical, true, cx) {
                    log::error!("{err}");
                }
            }))
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| this.focus_direction(Direction::Left, cx)))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| this.focus_direction(Direction::Right, cx)))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| this.focus_direction(Direction::Up, cx)))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| this.focus_direction(Direction::Down, cx)))
            .on_action(cx.listener(|this, _: &NextPane, _, cx| this.cycle_pane(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousPane, _, cx| this.cycle_pane(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleZoom, _, cx| this.toggle_zoom(cx)))
            .on_action(cx.listener(|this, _: &JumpToAttention, _, cx| this.jump_to_attention(cx)))
            .on_action(cx.listener(|this, _: &RenameTab, _, cx| this.start_rename(this.active, cx)))
            .on_action(cx.listener(|this, _: &MoveTabUp, _, cx| {
                let active = this.active;
                if active > 0 {
                    this.move_tab(active, active - 1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveTabDown, _, cx| {
                let active = this.active;
                this.move_tab(active, active + 1, cx);
            }))
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
            .on_modifiers_changed(cx.listener(|this, event: &gpui::ModifiersChangedEvent, _, cx| {
                this.update_hover_link(event.modifiers.platform, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .relative()
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
                            .child(Self::one_line(title).min_w_0()),
                    )
                    .child(
                        div()
                            .id("terminal")
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
                            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
                            .on_scroll_wheel(cx.listener(Self::scroll))
                            .when(self.hover_link.is_some(), |d| d.cursor_pointer())
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |_, _, window, cx| {
                                        let paint_started = std::time::Instant::now();
                                        if let Some(rect) = focused_rect {
                                            window.handle_input(&focus, ElementInputHandler::new(bounds(rect), entity), cx);
                                        }
                                        for view in &views {
                                            let state = terminal::PaintState {
                                                focused: view.focused && typing,
                                                cursor_visible,
                                                marked_text: marked_text.as_deref().filter(|_| view.focused),
                                                fade: if split && !view.focused { UNFOCUSED_FADE } else { 0. },
                                                link: view.link.clone(),
                                                flash: view.flash,
                                            };
                                            terminal::paint(bounds(view.rect), &view.frame, &metrics, state, window, cx);
                                        }
                                        for line in &divider_lines {
                                            window.paint_quad(fill(*line, divider_color));
                                        }
                                        if crate::profile::enabled() {
                                            crate::profile::record(build_time, paint_started.elapsed());
                                        }
                                    },
                                )
                                .size_full(),
                            )
                            .children(self.divider_handles(container, cx))
                            .children(self.search_bar(container)),
                    ),
            )
            .children(self.context_menu_overlay(window.viewport_size(), cx))
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
        if let Some(renaming) = self.renaming.as_mut() {
            renaming.text.push_str(text);
            cx.notify();
            return;
        }
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
        KeyBinding::new("cmd-w", ClosePane, None),
        KeyBinding::new("cmd-shift-w", CloseTab, None),
        KeyBinding::new("cmd-shift-]", NextTab, None),
        KeyBinding::new("cmd-shift-[", PreviousTab, None),
        KeyBinding::new("cmd-d", SplitRight, None),
        KeyBinding::new("cmd-shift-d", SplitDown, None),
        KeyBinding::new("cmd-alt-left", FocusLeft, None),
        KeyBinding::new("cmd-alt-right", FocusRight, None),
        KeyBinding::new("cmd-alt-up", FocusUp, None),
        KeyBinding::new("cmd-alt-down", FocusDown, None),
        KeyBinding::new("cmd-]", NextPane, None),
        KeyBinding::new("cmd-[", PreviousPane, None),
        KeyBinding::new("cmd-shift-enter", ToggleZoom, None),
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
