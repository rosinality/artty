//! A window: a sidebar of tabs, each a layout of split panes. The sessions
//! live in the artty server; this view draws what it sends and forwards
//! input, so closing the window leaves them running.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::layout::{self, Axis, Direction, Divider, PaneId, Rect};
use artty_core::protocol::{
    ClientMessage, PaneSpec, PaneState, Query, Reply, SelectionOp, TabLayout, WindowId,
    WindowLayout, WorkspaceId,
};
use artty_core::vt::term::TermMode;
use artty_core::{Config, FrameCursor, SavedWindow, TerminalSize, Theme};
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontWeight, Hsla, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, WindowBackgroundAppearance, WindowBounds, actions, canvas, div, fill,
    point, prelude::*, px, rgba, size,
};

use crate::app_state::{self, Shared};
use crate::keys;
use crate::mouse::{self, Action, Button};
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
/// How long the visual bell flash lasts.
const FLASH: Duration = Duration::from_millis(220);
const MIN_FONT_SIZE: f32 = 6.;
const MAX_FONT_SIZE: f32 = 72.;

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

struct Search {
    query: String,
    /// `Some(false)`: no match; `Some(true)`: showing a match.
    found: Option<bool>,
    error: Option<String>,
}

pub struct WindowView {
    focus: FocusHandle,
    config: Arc<Config>,
    config_error: Option<String>,
    config_modified: Option<SystemTime>,
    reload_config: bool,
    theme: Arc<Theme>,
    font_size: f32,
    metrics: Metrics,
    window_handle: gpui::AnyWindowHandle,
    /// The server's window this view shows, once attached.
    window: Option<WindowId>,
    /// Detach from the server's window when this view goes away.
    detach: bool,
    layout: WindowLayout,
    /// Every tab of the layout, workspace by workspace; most of the view
    /// numbers tabs this way.
    tabs: Vec<TabLayout>,
    /// Where each of `tabs` is: its workspace and index there.
    places: Vec<(WorkspaceId, usize)>,
    /// The shown tab, in `tabs`.
    active: usize,
    /// Area the active tab's panes fill, from the last render.
    area: Rect,
    /// Pane rectangles and dividers of the active tab, from the last render.
    pane_rects: Vec<(PaneId, Rect)>,
    dividers: Vec<Divider>,
    /// A divider being dragged.
    drag: Option<Divider>,
    /// Sizes sent to the server, to send only changes.
    sizes: HashMap<PaneId, TerminalSize>,
    bounds_sent: Option<SavedWindow>,
    search: Option<Search>,
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
    /// The cell last asked about for a link, to avoid repeat queries.
    hover_cell: Option<(PaneId, usize, usize)>,
    context_menu: Option<ContextMenu>,
    renaming: Option<Renaming>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

fn bounds(rect: Rect) -> Bounds<Pixels> {
    Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height)))
}

fn layout_of(tab: &TabLayout, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
    if tab.zoomed { (vec![(tab.focused, area)], Vec::new()) } else { tab.root.layout(area, SPLIT_GAP) }
}

impl WindowView {
    pub fn new(config: Config, config_error: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            if let Some(id) = this.window {
                app_state::send(ClientMessage::WindowFocused { window: id, focused: this.window_active }, cx);
            }
            cx.notify();
        });

        let focus = cx.focus_handle();
        focus.focus(window);
        let font_size = config.font_size;
        let metrics = Metrics::measure(window, &config, font_size);
        Self {
            focus,
            theme: Arc::new(config.theme()),
            metrics,
            font_size,
            config: Arc::new(config),
            config_error,
            config_modified: config_modified(),
            reload_config: false,
            window_handle: window.window_handle(),
            window: None,
            detach: true,
            layout: WindowLayout::default(),
            tabs: Vec::new(),
            places: Vec::new(),
            active: 0,
            area: Self::area_rect(window),
            pane_rects: Vec::new(),
            dividers: Vec::new(),
            drag: None,
            sizes: HashMap::new(),
            bounds_sent: None,
            search: None,
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
            hover_cell: None,
            context_menu: None,
            renaming: None,
            _tasks: vec![blink, poll, spinner],
            _subscriptions: vec![activation],
        }
    }

    // The server's side.

    pub fn window_handle(&self) -> gpui::AnyWindowHandle {
        self.window_handle
    }

    /// The server attached this view to its window `window`.
    pub fn attached(&mut self, window: WindowId, _bounds: Option<SavedWindow>, cx: &mut Context<Self>) {
        self.window = Some(window);
        self.sizes.clear();
        app_state::send(ClientMessage::WindowFocused { window, focused: self.window_active }, cx);
        cx.notify();
    }

    pub fn set_layout(&mut self, layout: WindowLayout, cx: &mut Context<Self>) {
        if layout != self.layout {
            let shown = |layout: &WindowLayout| (layout.active_workspace().map(|w| (w.id, w.active)), layout.active_tab().map(|t| t.focused));
            if shown(&layout) != shown(&self.layout) {
                self.reset_input_state();
            }
            self.tabs = layout.tabs().map(|(_, _, tab)| tab.clone()).collect();
            self.places = layout.tabs().map(|(workspace, index, _)| (workspace, index)).collect();
            let before: usize = layout.workspaces.iter().take(layout.active).map(|w| w.tabs.len()).sum();
            self.active = before + layout.active_workspace().map_or(0, |w| w.active);
            self.layout = layout;
            cx.notify();
        }
    }

    /// The server is gone or closed this window: don't tell it we detached.
    pub fn forget_server(&mut self) {
        self.detach = false;
    }

    /// Whether this window shows `pane` right now.
    pub fn shows(&self, pane: PaneId) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| layout_of(tab, self.area).0.iter().any(|(id, _)| *id == pane))
    }

    fn send(&self, message: ClientMessage, cx: &App) {
        app_state::send(message, cx);
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

    /// The cell of pane `id` under a window position, and whether it's the
    /// right half.
    fn cell_at(&self, id: PaneId, position: Point<Pixels>) -> (usize, usize, bool) {
        let rect = self.rect_of(id).unwrap_or(self.area);
        let size = self.size_for(rect);
        let (columns, lines) = (size.columns as usize, size.lines as usize);
        let x = ((position.x - px(rect.x)) / self.metrics.cell_width).max(0.);
        let y = ((position.y - px(rect.y)) / self.metrics.line_height).max(0.);
        let column = (x as usize).min(columns - 1);
        let right = x.fract() >= 0.5 && (x as usize) < columns;
        ((y as usize).min(lines - 1), column, right)
    }

    // Panes and tabs.

    fn focused_id(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|tab| tab.focused)
    }

    fn mode(&self, pane: PaneId, cx: &App) -> TermMode {
        cx.global::<Shared>().screens.get(&pane).map_or(TermMode::empty(), |s| s.mode)
    }

    fn pane_state<'a>(&self, pane: PaneId, cx: &'a App) -> Option<&'a PaneState> {
        cx.global::<Shared>().panes.get(&pane)
    }

    /// A tab's name: what the user called it, or its focused pane's title.
    fn tab_title(&self, tab: &TabLayout, cx: &App) -> SharedString {
        match &tab.title {
            Some(title) => title.clone().into(),
            None => self.pane_state(tab.focused, cx).map(|p| p.info.title.clone()).unwrap_or_else(|| "shell".into()).into(),
        }
    }

    fn open_tab(&self, cx: &App) {
        if let Some(window) = self.window {
            self.send(ClientMessage::NewTab { window, workspace: None, spec: PaneSpec::default() }, cx);
        }
    }

    fn split(&self, pane: Option<PaneId>, axis: Axis, cx: &App) {
        if let Some(pane) = pane.or(self.focused_id()) {
            self.send(ClientMessage::Split { pane, axis, spec: PaneSpec::default() }, cx);
        }
    }

    fn select(&mut self, tab: usize, cx: &mut Context<Self>) {
        if let Some(&(workspace, tab)) = self.places.get(tab) {
            self.close_search(cx);
            self.send(ClientMessage::SelectTab { workspace, tab }, cx);
        }
    }

    fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if Some(pane) != self.focused_id() {
            self.close_search(cx);
        }
        self.send(ClientMessage::FocusPane { pane }, cx);
    }

    fn reset_input_state(&mut self) {
        self.marked_text = None;
        self.selecting = false;
        self.reporting_button = None;
        self.mouse_pane = None;
        self.cursor_visible = true;
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

    /// Move a tab to where tab `to` is; across a section boundary, it joins
    /// the neighboring workspace at the near end.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let (Some(&(workspace, from_index)), Some(&(to_workspace, to_index))) = (self.places.get(from), self.places.get(to)) else { return };
        let to = if to_workspace != workspace && to < from { to_index + 1 } else { to_index };
        self.renaming = None;
        self.send(ClientMessage::MoveTab { workspace, from: from_index, to_workspace, to }, cx);
    }

    fn start_rename(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        let text = self.tab_title(tab, cx).to_string();
        self.close_search(cx);
        self.marked_text = None;
        self.renaming = Some(Renaming { tab: index, text });
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(Renaming { tab, text }) = self.renaming.take()
            && commit
            && let Some(&(workspace, tab)) = self.places.get(tab)
        {
            let title = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
            self.send(ClientMessage::RenameTab { workspace, tab, title }, cx);
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

    fn open_context_menu(&mut self, pane: PaneId, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (line, column, _) = self.cell_at(pane, position);
        let link = app_state::query(Query::LinkSpan { pane, line, column }, cx);
        let selection = app_state::query(Query::SelectionText { pane }, cx);
        cx.spawn(async move |this, cx| {
            let link = match link.await {
                Ok(Reply::Link(link)) => link.map(|(url, _, _)| url),
                _ => None,
            };
            let has_selection = matches!(selection.await, Ok(Reply::Text(Some(_))));
            let _ = this.update(cx, |this, cx| {
                this.context_menu = Some(ContextMenu { position, pane, link, has_selection });
                cx.notify();
            });
        })
        .detach();
    }

    fn run_menu_command(&mut self, command: MenuCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.take() else { return };
        let pane = menu.pane;
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
            MenuCommand::SplitRight => self.split(Some(pane), Axis::Horizontal, cx),
            MenuCommand::SplitDown => self.split(Some(pane), Axis::Vertical, cx),
            MenuCommand::RenameTab => {
                if let Some(index) = self.tabs.iter().position(|t| t.root.contains(pane)) {
                    self.start_rename(index, cx);
                }
            }
            MenuCommand::ClearBuffer => self.send(ClientMessage::ClearBuffer { pane }, cx),
            MenuCommand::ClosePane => self.send(ClientMessage::ClosePane { pane }, cx),
        }
        cx.notify();
    }

    fn jump_to_attention(&mut self, cx: &mut Context<Self>) {
        // Every pane in tab order, starting after the focused one.
        let all: Vec<PaneId> = self.tabs.iter().flat_map(|tab| tab.root.panes()).collect();
        let start = self.focused_id().and_then(|id| all.iter().position(|&p| p == id)).map_or(0, |i| i + 1);
        let order: Vec<PaneId> = all.iter().cycle().skip(start).take(all.len()).copied().collect();
        let state = |id: &PaneId| self.pane_state(*id, cx);
        let target = order
            .iter()
            .find(|id| state(id).is_some_and(|s| s.icon == "waiting"))
            .or_else(|| order.iter().find(|id| state(id).is_some_and(|s| s.info.needs_attention)))
            .copied();
        if let Some(id) = target {
            self.focus_pane(id, cx);
        }
    }

    // Timers.

    /// Send user input to the focused pane, keeping the cursor solid.
    fn input(&mut self, bytes: Vec<u8>, cx: &App) {
        if let Some(pane) = self.focused_id() {
            self.send(ClientMessage::Input { pane, bytes }, cx);
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
        let panes: Vec<PaneId> = self.tabs.iter().flat_map(|t| t.root.panes()).collect();
        let shared = cx.global::<Shared>();
        let busy = panes.iter().any(|id| shared.panes.get(id).is_some_and(|s| s.icon == "busy"));
        let flashing = panes.iter().any(|id| shared.flashes.get(id).is_some_and(|at| at.elapsed() <= FLASH));
        if busy {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
        }
        if busy || flashing {
            cx.notify();
        }
    }

    /// Once a second: notice config changes (fonts, opacity are drawn here).
    fn poll(&mut self, cx: &mut Context<Self>) {
        let modified = config_modified();
        if modified != self.config_modified {
            self.config_modified = modified;
            self.reload_config = true;
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
        if config.font_size != self.config.font_size {
            self.font_size = config.font_size;
        }
        self.metrics = Metrics::measure(window, &config, self.font_size);
        window.set_background_appearance(background_appearance(&config));
        self.config = Arc::new(config);
    }

    /// Underline the link under the pointer while ⌘ is held.
    fn update_hover_link(&mut self, platform: bool, cx: &mut Context<Self>) {
        let cell = platform.then_some(self.mouse_position).flatten().and_then(|position| {
            let (pane, _) = self.pane_at(position)?;
            let (line, column, _) = self.cell_at(pane, position);
            Some((pane, line, column))
        });
        if cell == self.hover_cell {
            return;
        }
        self.hover_cell = cell;
        let Some((pane, line, column)) = cell else {
            if self.hover_link.take().is_some() {
                cx.notify();
            }
            return;
        };
        let answer = app_state::query(Query::LinkSpan { pane, line, column }, cx);
        cx.spawn(async move |this, cx| {
            let link = match answer.await {
                Ok(Reply::Link(link)) => link,
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                if this.hover_cell == Some((pane, line, column)) {
                    this.hover_link = link.map(|(_, start, end)| (pane, line, start..end));
                    cx.notify();
                }
            });
        })
        .detach();
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
        let Some(pane) = self.focused_id() else { return };
        if let Some(bytes) = keys::to_esc(keystroke, self.mode(pane, cx), self.config.option_as_meta) {
            self.input(bytes, cx);
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

    fn close_search(&mut self, cx: &App) {
        if self.search.take().is_some()
            && let Some(pane) = self.focused_id()
        {
            self.send(ClientMessage::EndSearch { pane }, cx);
        }
    }

    fn run_search(&mut self, older: bool, restart: bool, cx: &mut Context<Self>) {
        let (Some(pane), Some(search)) = (self.focused_id(), self.search.as_ref()) else { return };
        let answer = app_state::search(pane, search.query.clone(), older, restart, cx);
        cx.spawn(async move |this, cx| {
            let Ok(Reply::Search(result)) = answer.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(search) = this.search.as_mut() {
                    match result {
                        Ok(found) => {
                            search.found = (!search.query.is_empty()).then_some(found);
                            search.error = None;
                        }
                        Err(error) => search.error = Some(error),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" => {
                self.close_search(cx);
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
    fn mouse_reporting(&self, id: PaneId, modifiers: &Modifiers, cx: &App) -> Option<TermMode> {
        let mode = self.mode(id, cx);
        (mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift).then_some(mode)
    }

    fn report_mouse(&mut self, id: PaneId, button: Button, action: Action, position: Point<Pixels>, modifiers: &Modifiers, cx: &App) {
        let Some(mode) = self.mouse_reporting(id, modifiers, cx) else { return };
        let (line, column, _) = self.cell_at(id, position);
        if action == Action::Motion && self.last_mouse_cell == Some((line, column)) {
            return;
        }
        self.last_mouse_cell = Some((line, column));
        if let Some(bytes) = mouse::report(button, action, line, column, modifiers, mode) {
            self.send(ClientMessage::Input { pane: id, bytes }, cx);
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

        if event.button == MouseButton::Left && event.modifiers.platform {
            let (line, column, _) = self.cell_at(id, event.position);
            let answer = app_state::query(Query::LinkSpan { pane: id, line, column }, cx);
            cx.spawn(async move |_, _| {
                if let Ok(Reply::Link(Some((link, _, _)))) = answer.await {
                    let _ = std::process::Command::new("open").arg(&link).spawn();
                }
            })
            .detach();
            return;
        }
        let menu_click = event.button == MouseButton::Right || (event.button == MouseButton::Left && event.modifiers.control);
        if menu_click && (self.mouse_reporting(id, &event.modifiers, cx).is_none() || event.modifiers.control) {
            self.open_context_menu(id, event.position, cx);
            return;
        }
        if self.mouse_reporting(id, &event.modifiers, cx).is_some() {
            if let Some(button) = Button::from_gpui(event.button) {
                self.reporting_button = Some(button);
                self.last_mouse_cell = None;
                self.report_mouse(id, button, Action::Press, event.position, &event.modifiers, cx);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let (line, column, right) = self.cell_at(id, event.position);
        let op = if event.modifiers.shift {
            SelectionOp::Update { line, column, right }
        } else {
            let kind = match event.click_count {
                0 | 1 => 0,
                2 => 1,
                _ => 2,
            };
            SelectionOp::Start { kind, line, column, right }
        };
        self.send(ClientMessage::Selection { pane: id, op }, cx);
        self.selecting = true;
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.mouse_position = Some(event.position);
        self.update_hover_link(event.modifiers.platform, cx);
        if let Some(divider) = &self.drag {
            let ratio = layout::drag_ratio(divider, event.position.x.into(), event.position.y.into(), SPLIT_GAP);
            let path = divider.path.clone();
            // Move the divider right away; the server confirms with a layout.
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.root.set_ratio(&path, ratio);
            }
            if let Some(tab) = self.layout.active_tab_mut() {
                tab.root.set_ratio(&path, ratio);
            }
            if let Some(window) = self.window {
                self.send(ClientMessage::SetRatio { window, path, ratio }, cx);
            }
            cx.notify();
            return;
        }
        if let (Some(button), Some(id)) = (self.reporting_button, self.mouse_pane) {
            self.report_mouse(id, button, Action::Motion, event.position, &event.modifiers, cx);
            return;
        }
        if self.selecting && event.pressed_button == Some(MouseButton::Left) {
            if let Some(id) = self.mouse_pane {
                let (line, column, right) = self.cell_at(id, event.position);
                self.send(ClientMessage::Selection { pane: id, op: SelectionOp::Update { line, column, right } }, cx);
            }
        } else if event.pressed_button.is_none()
            && let Some((id, _)) = self.pane_at(event.position)
            && Some(id) == self.focused_id()
        {
            self.report_mouse(id, Button::None, Action::Motion, event.position, &event.modifiers, cx);
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
            return;
        }
        let id = self.mouse_pane.take();
        if let (Some(button), Some(id)) = (self.reporting_button.take(), id) {
            self.report_mouse(id, button, Action::Release, event.position, &event.modifiers, cx);
            return;
        }
        if self.selecting {
            self.selecting = false;
            let Some(pane) = id else { return };
            let answer = app_state::query(Query::SelectionText { pane }, cx);
            let copy = self.config.copy_on_select;
            cx.spawn(async move |this, cx| {
                let text = match answer.await {
                    Ok(Reply::Text(text)) => text,
                    _ => None,
                };
                let _ = this.update(cx, |this, cx| match text {
                    Some(text) if copy => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                    Some(_) => {}
                    // A plain click selects nothing; drop the empty selection.
                    None => this.send(ClientMessage::Selection { pane, op: SelectionOp::Clear }, cx),
                });
            })
            .detach();
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
        let mode = self.mode(id, cx);
        if self.mouse_reporting(id, &event.modifiers, cx).is_some() {
            let button = if lines > 0 { Button::WheelUp } else { Button::WheelDown };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(id, button, Action::Press, event.position, &event.modifiers, cx);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs (less, vim) get arrow keys instead.
            let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA".as_slice(),
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.send(ClientMessage::Input { pane: id, bytes: key.repeat(lines.unsigned_abs() as usize) }, cx);
        } else {
            self.send(ClientMessage::Scroll { pane: id, lines }, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.focused_id() else { return };
        let answer = app_state::query(Query::SelectionText { pane }, cx);
        cx.spawn(async move |_, cx| {
            if let Ok(Reply::Text(Some(text))) = answer.await {
                let _ = cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
            }
        })
        .detach();
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if let Some(search) = self.search.as_mut() {
            search.query.push_str(text.lines().next().unwrap_or_default());
            self.run_search(true, true, cx);
            return;
        }
        let Some(pane) = self.focused_id() else { return };
        let bytes = if self.mode(pane, cx).contains(TermMode::BRACKETED_PASTE) {
            // Strip any embedded end marker so pasted text can't escape the bracket.
            format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.input(bytes.into_bytes(), cx);
    }

    fn set_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.metrics = Metrics::measure(window, &self.config, self.font_size);
        cx.notify();
    }

    // Sidebar.

    fn icon_cell(&self, state: Option<&PaneState>) -> gpui::Div {
        let color = |hex: u32| -> Hsla { rgba(hex).into() };
        // Braille spinner frames are tiny at text size.
        let icon: Option<(SharedString, Hsla, f32)> = match state.map(|s| s.icon.as_str()) {
            Some("busy") => Some((SPINNER[self.spinner_frame].into(), color(0x8fb4ffff), 16.)),
            Some("waiting" | "attention") => Some(("●".into(), color(0xe0af68ff), 12.)),
            Some("done") => Some(("✓".into(), color(0x9ece6aff), 12.)),
            Some("failed") => Some(("✕".into(), color(0xf7768eff), 12.)),
            _ => None,
        };
        div()
            .w(px(12.))
            .flex_none()
            .flex()
            .justify_center()
            .when_some(icon, |d, (icon, color, size)| d.text_color(color).text_size(px(size)).child(icon))
    }

    fn pane_detail(state: &PaneState) -> Option<String> {
        match (state.info.detail.clone(), state.progress) {
            (Some(detail), Some(percent)) => Some(format!("{detail} · {percent}%")),
            (None, Some(percent)) => Some(format!("{percent}%")),
            (detail, None) => detail,
        }
    }

    fn urgency(state: Option<&PaneState>) -> u8 {
        match state.map(|s| s.icon.as_str()) {
            Some("waiting") => 5,
            Some("busy") => 4,
            Some("failed") => 3,
            Some("done") => 2,
            Some("attention") => 1,
            _ => 0,
        }
    }

    fn one_line(text: impl Into<SharedString>) -> gpui::Div {
        // GPUI 0.2 caches the first measurement of `nowrap` text, which happens
        // before the width is known, so `.truncate()` only clips. A one-line
        // clamp re-measures at the real width and ellipsizes.
        div().overflow_hidden().line_clamp(1).text_ellipsis().child(text.into())
    }

    fn tab_row(&self, index: usize, tab: &TabLayout, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let selected = index == self.active;
        let ids = tab.root.panes();
        let split = ids.len() > 1;
        let states: Vec<Option<&PaneState>> = ids.iter().map(|id| self.pane_state(*id, cx)).collect();
        let focused = self.pane_state(tab.focused, cx);
        // The row shows the most urgent pane, preferring the focused one.
        let summary = ids
            .iter()
            .zip(&states)
            .max_by_key(|(id, state)| (Self::urgency(**state), **id == tab.focused))
            .and_then(|(_, state)| *state)
            .or(focused);
        let attention = states.iter().flatten().any(|s| s.info.needs_attention);
        let label: SharedString = if index < 9 { format!("⌘{}", index + 1).into() } else { "".into() };
        let text_color = if selected { rgba(0xffffffee) } else { rgba(0xffffffb0) };
        let detail = if split { None } else { focused.and_then(Self::pane_detail) };

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
                        None => Self::one_line(self.tab_title(tab, cx))
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
            ids.iter().zip(&states).map(|(&id, state)| {
                let is_focused = id == tab.focused;
                let title = state.map(|s| s.info.title.clone()).unwrap_or_else(|| "shell".into());
                let line = match state.and_then(Self::pane_detail) {
                    Some(detail) => format!("{title} · {detail}"),
                    None => title,
                };
                let pane_attention = state.is_some_and(|s| s.info.needs_attention);
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
                    .child(self.icon_cell(*state))
                    .child(
                        Self::one_line(line)
                            .flex_1()
                            .min_w_0()
                            .text_color(if is_focused && selected { rgba(0xffffffd0) } else { rgba(0xffffff80) })
                            .when(pane_attention, |d| d.font_weight(FontWeight::SEMIBOLD)),
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
            .on_drag(DraggedTab { index, title: self.tab_title(tab, cx) }, |dragged, _, _, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedTab>(|style, _, _, _| style.bg(rgba(0x8fb4ff26)))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| this.move_tab(dragged.index, index, cx)))
            .child(header)
            .children(pane_rows.into_iter().flatten())
    }

    fn banner(id: &'static str, title: &'static str, hint: &'static str, button: &'static str, action: fn(&mut App)) -> gpui::Div {
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
                    .child(div().text_color(rgba(0xffffffee)).child(title))
                    .child(div().text_size(px(11.)).text_color(rgba(0xffffff80)).child(hint)),
            )
            .child(
                div()
                    .id(id)
                    .flex_none()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(rgba(0x3478f6ff))
                    .hover(|d| d.bg(rgba(0x4a88ffff)))
                    .text_color(rgba(0xffffffff))
                    .child(button)
                    .on_click(move |_, _, cx| action(cx)),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // With several workspaces, each starts a section.
        let sections = self.layout.workspaces.len() > 1;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let (workspace, index) = self.places[i];
            if sections && index == 0 {
                let position = self.layout.workspaces.iter().position(|w| w.id == workspace).unwrap_or(0);
                let name = self.layout.workspaces[position].name.clone().unwrap_or_else(|| format!("Workspace {}", position + 1));
                rows.push(
                    div()
                        .px_2()
                        .pt(px(if position == 0 { 0. } else { 8. }))
                        .pb(px(2.))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgba(0xffffff70))
                        .child(Self::one_line(name))
                        .into_any_element(),
                );
            }
            rows.push(self.tab_row(i, tab, cx).into_any_element());
        }
        let mismatch = cx.global::<Shared>().server_mismatch.is_some();
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
                            .on_click(cx.listener(|this, _, _, cx| this.open_tab(cx))),
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
                d.child(Self::banner("restart", "New build ready", "Sessions keep running", "Restart", app_state::restart))
            })
            .when(mismatch, |d| {
                d.child(Self::banner(
                    "restart-server",
                    "Server is a different build",
                    "Restarting it reopens your tabs",
                    "Restart",
                    app_state::restart_server,
                ))
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

impl Drop for WindowView {
    fn drop(&mut self) {
        // The window closed: its tabs keep running in the server, detached.
        // (There's no `App` here; the server also notices a lost connection.)
        if self.detach
            && let Some(window) = self.window
        {
            DETACHED.with(|detached| detached.borrow_mut().push(window));
        }
    }
}

thread_local! {
    /// Windows closed since the last render, to detach from the server.
    static DETACHED: std::cell::RefCell<Vec<WindowId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tell the server about windows that closed.
pub fn flush_detached(cx: &App) {
    for window in DETACHED.with(|detached| std::mem::take(&mut *detached.borrow_mut())) {
        app_state::send(ClientMessage::DetachWindow { window }, cx);
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

impl Render for WindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        flush_detached(cx);
        if self.reload_config {
            self.apply_config(window);
        }
        self.area = Self::area_rect(window);
        self.window_active = window.is_window_active();
        if let Some(id) = self.window {
            let bounds = saved_window(window.window_bounds());
            if self.bounds_sent != Some(bounds) {
                self.bounds_sent = Some(bounds);
                self.send(ClientMessage::WindowBounds { window: id, bounds }, cx);
            }
        }

        // Lay out the active tab and keep the server's pane sizes in step.
        let (rects, dividers) = self.tabs.get(self.active).map(|tab| layout_of(tab, self.area)).unwrap_or_default();
        for (id, rect) in &rects {
            let size = self.size_for(*rect);
            if self.sizes.get(id) != Some(&size) {
                self.sizes.insert(*id, size);
                self.send(ClientMessage::Resize { pane: *id, size }, cx);
            }
        }
        self.pane_rects = rects;
        self.dividers = dividers;

        let focused_id = self.focused_id();
        let split = self.pane_rects.len() > 1;
        let shared = cx.global::<Shared>();
        let views: Vec<PaneView> = self
            .pane_rects
            .iter()
            .filter_map(|&(id, rect)| {
                let screen = shared.screens.get(&id)?;
                let link = self.hover_link.as_ref().filter(|(pane, ..)| *pane == id).map(|(_, line, span)| (*line, span.clone()));
                let flash = shared.flashes.get(&id).map_or(0., |at| 1. - (at.elapsed().as_secs_f32() / FLASH.as_secs_f32()).min(1.));
                Some(PaneView { rect, frame: screen.frame.clone(), focused: Some(id) == focused_id, link, flash })
            })
            .collect();
        self.cursor = views.iter().find(|view| view.focused).and_then(|view| view.frame.cursor);

        let window_focused = self.focus.is_focused(window) && self.window_active;
        let typing = window_focused && self.search.is_none() && self.renaming.is_none();
        self.blink_enabled = typing && (self.config.cursor_blink || self.cursor.is_some_and(|c| c.blinking));

        let title = self.tabs.get(self.active).map(|tab| self.tab_title(tab, cx)).unwrap_or_default();
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
        // The terminal container starts right of the sidebar, below the title bar.
        let container = point(px(SIDEBAR_WIDTH), px(TITLEBAR_HEIGHT));

        div()
            .track_focus(&self.focus)
            .key_context("WindowView")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.open_tab(cx)))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| {
                if let Some(pane) = this.focused_id() {
                    this.send(ClientMessage::ClosePane { pane }, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(&(workspace, tab)) = this.places.get(this.active) {
                    this.send(ClientMessage::CloseTab { workspace, tab }, cx);
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
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| this.split(None, Axis::Horizontal, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| this.split(None, Axis::Vertical, cx)))
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| this.focus_direction(Direction::Left, cx)))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| this.focus_direction(Direction::Right, cx)))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| this.focus_direction(Direction::Up, cx)))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| this.focus_direction(Direction::Down, cx)))
            .on_action(cx.listener(|this, _: &NextPane, _, cx| this.cycle_pane(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousPane, _, cx| this.cycle_pane(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleZoom, _, cx| {
                if let Some(window) = this.window {
                    this.send(ClientMessage::ToggleZoom { window }, cx);
                }
            }))
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
                if active + 1 < this.tabs.len() {
                    this.move_tab(active, active + 1, cx);
                }
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
                                        let paint_started = Instant::now();
                                        let build_time = Duration::ZERO;
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
/// (or the search query, or a tab name) once committed, so typing Hangul or
/// kana works as in any Mac app.
impl EntityInputHandler for WindowView {
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
        self.input(text.as_bytes().to_vec(), cx);
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
