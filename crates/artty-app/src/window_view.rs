//! A window: a sidebar of tabs, each a layout of split panes. The sessions
//! live in the artty server; this view draws what it sends and forwards
//! input, so closing the window leaves them running.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{Request, Response, WorkspaceInfo};
use artty_core::layout::{self, Axis, Direction, Divider, PaneId, Rect};
use artty_core::protocol::{
    ClientMessage, PaneSpec, PaneState, Query, Reply, SelectionOp, TabLayout, WORKSPACE_COLORS,
    WindowId, WindowLayout, WorkspaceId,
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
use crate::chrome::ui;
use crate::ids::{self, LOCAL, ServerId};
use crate::keys;
use crate::mouse::{self, Action, Button};
use crate::terminal::{self, Metrics, hsla};

#[path = "activity.rs"]
mod activity;
#[path = "ask_sheet.rs"]
mod ask_sheet;
#[path = "copy_mode.rs"]
mod copy_mode;
#[path = "files_drawer.rs"]
mod files_drawer;
#[path = "palette.rs"]
mod palette;
#[path = "pane_actions.rs"]
mod pane_actions;
#[path = "search_view.rs"]
mod search_view;

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
        NewWorkspace,
        OpenWorkspace,
        ShowActivity,
        NextWorkspace,
        PreviousWorkspace,
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
        ResetFontSize,
        JumpToPreviousPrompt,
        JumpToNextPrompt,
        CopyCommandOutput,
        CopyMode,
        ResizePaneLeft,
        ResizePaneRight,
        ResizePaneUp,
        ResizePaneDown,
        SwapPaneLeft,
        SwapPaneRight,
        SwapPaneUp,
        SwapPaneDown,
        EqualizePanes,
        PaneToNewTab,
        ToggleBroadcast,
        ReopenClosedTab,
        ToggleFiles,
        ToggleSidebar,
        GoTo,
        RunCommand,
        SearchEverything
    ]
);

pub const SIDEBAR_WIDTH: f32 = 232.;
/// The last column of a tab row: its ⌘N label, or the close button.
const TAB_LABEL_WIDTH: f32 = 22.;
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

/// Type text into the focused pane: what `{ send = "..." }` in `[keys]`
/// binds to.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = artty, no_json)]
pub struct SendText {
    pub text: String,
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
    CloseTab,
    TabToNewWorkspace,
    NewTabInWorkspace,
    RenameWorkspace,
    ToggleCollapsed,
    CloseWorkspace,
    HideWorkspace,
    MoveToNewWindow,
    MoveToWindow(WindowId),
    /// An index into `WORKSPACE_COLORS`, or none.
    SetColor(Option<usize>),
    RootFromCurrentDirectory,
    ClearRoot,
    ReconnectFromScratch,
    Connect,
    Disconnect,
}

/// The shade of a workspace color (see `WORKSPACE_COLORS`).
fn workspace_color(name: &str) -> Option<u32> {
    Some(match name {
        "red" => 0xf7768e,
        "orange" => 0xff9e64,
        "yellow" => 0xe0af68,
        "green" => 0x9ece6a,
        "teal" => 0x73daca,
        "blue" => 0x7aa2f7,
        "purple" => 0xbb9af7,
        "pink" => 0xff7eb6,
        _ => return None,
    })
}

/// What was right-clicked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuTarget {
    Pane(PaneId),
    /// A tab in the sidebar, as an index into `tabs`.
    Tab(usize),
    Workspace(WorkspaceId),
    /// Another machine's server, by its header.
    Server(ServerId),
}

struct ContextMenu {
    position: Point<Pixels>,
    target: MenuTarget,
    link: Option<String>,
    has_selection: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RenameTarget {
    /// An index into `tabs`.
    Tab(usize),
    Workspace(WorkspaceId),
}

/// A tab title or workspace name being edited in the sidebar.
struct Renaming {
    target: RenameTarget,
    text: String,
}

/// A tab being dragged in the sidebar; also its drag preview.
#[derive(Clone)]
struct DraggedTab {
    index: usize,
    title: SharedString,
}

/// A workspace section being dragged by its header.
#[derive(Clone)]
struct DraggedWorkspace {
    workspace: WorkspaceId,
    title: SharedString,
}

impl Render for DraggedWorkspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.title.clone())
    }
}

/// A split tab's pane being dragged by its sidebar line, to another tab.
#[derive(Clone)]
struct DraggedPane {
    pane: PaneId,
    title: SharedString,
}

impl Render for DraggedPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.title.clone())
    }
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.title.clone())
    }
}

/// A small hover label.
struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        drag_preview(self.0.clone()).text_size(px(11.5))
    }
}

/// What follows the pointer while dragging in the sidebar.
fn drag_preview(title: SharedString) -> gpui::Div {
    div()
        .px_3()
        .py_1()
        .rounded(px(7.))
        .bg(ui(0x2a3140f0))
        .border_1()
        .border_color(ui(0xffffff30))
        .text_size(px(12.5))
        .text_color(ui(0xffffffee))
        .font_family(".SystemUIFont")
        .child(title)
}

/// The Open Workspace picker: workspaces this window doesn't show.
struct Picker {
    query: String,
    items: Vec<WorkspaceInfo>,
    selected: usize,
}

impl Picker {
    /// Items whose name contains the query, ignoring case.
    fn matches(&self) -> Vec<&WorkspaceInfo> {
        let query = self.query.to_lowercase();
        self.items.iter().filter(|w| w.title.to_lowercase().contains(&query)).collect()
    }
}

struct Search {
    query: String,
    /// `Some(false)`: no match; `Some(true)`: showing a match.
    found: Option<bool>,
    /// Which match, of how many; and whether the pane has history at all.
    counts: Option<artty_core::protocol::SearchFound>,
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
    /// The front server's window, once attached: the one the shown tab is in.
    window: Option<WindowId>,
    /// The server windows this view shows, one per server, with their
    /// layouts; the sidebar lists their workspaces in server order.
    servers: std::collections::BTreeMap<ServerId, (WindowId, WindowLayout)>,
    /// The server whose active tab is shown.
    front: ServerId,
    /// Each server's window last shown here, to show again after a reconnect.
    remembered: HashMap<ServerId, WindowId>,
    /// Asked a server for a new window here: show it when it comes.
    front_on_attach: Option<ServerId>,
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
    /// Scrolling the selection's pane while the drag is past its top or bottom.
    autoscrolling: bool,
    /// ⌘ is held: the sidebar shows tabs' ⌘N shortcuts.
    command_held: bool,
    /// Screen pixels per point, as of the last render.
    scale_factor: f32,
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
    /// Asked for a new workspace: name it when it shows up.
    naming_new_workspace: bool,
    /// A workspace whose close button was clicked once; a second click closes it.
    closing_workspace: Option<WorkspaceId>,
    picker: Option<Picker>,
    /// The Activity view, when it's shown instead of the active tab.
    activity: Option<activity::Activity>,
    /// The pane in copy mode, if any.
    copy_mode: Option<PaneId>,
    /// A short message over the terminal, and when it appeared.
    toast: Option<(SharedString, Instant)>,
    /// Questions ssh asks while logging in, the first shown.
    asks: Vec<ask_sheet::AskSheet>,
    /// The files drawer (⌘⇧E), when open.
    files: Option<files_drawer::Files>,
    /// The sidebar folded away (⌘B): the terminal takes its width.
    sidebar_hidden: bool,
    /// The palette (⌘P, ⌘⇧P), when open.
    palette: Option<palette::Palette>,
    /// Search everything (⌘⇧F), when open.
    search_all: Option<search_view::SearchAll>,
    /// The workspace switcher's list is open (sidebar style `switcher`).
    switcher_open: bool,
    /// A key sequence's keys so far (`cmd-e 1` waiting after ⌘E), shown.
    pending_keys: Option<String>,
    /// Servers whose workspaces are folded away in the sidebar.
    folded_servers: HashSet<ServerId>,
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
    if tab.zoomed {
        (vec![(tab.focused, area)], Vec::new())
    } else {
        tab.root.layout(area, SPLIT_GAP)
    }
}

impl WindowView {
    pub fn new(
        config: Config,
        config_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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

        // A key sequence under way: its keys so far, shown until it ends.
        let pending = cx.observe_pending_input(window, |this, window, cx| {
            this.pending_keys = window
                .pending_input_keystrokes()
                .map(|keys| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(" "));
            cx.notify();
        });
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.update_secure_input(cx);
            // Away to another window or app: a name being typed is saved.
            if !this.window_active && this.renaming.is_some() {
                this.finish_rename(true, cx);
            }
            // ⌘-Tab away and back: the key-up went elsewhere.
            this.command_held = false;
            for (&server, &(id, _)) in &this.servers {
                let focused = this.window_active && server == this.front;
                app_state::send(ClientMessage::WindowFocused { window: id, focused }, cx);
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
            servers: std::collections::BTreeMap::new(),
            front: LOCAL,
            front_on_attach: None,
            remembered: HashMap::new(),
            detach: true,
            layout: WindowLayout::default(),
            tabs: Vec::new(),
            places: Vec::new(),
            active: 0,
            area: Self::area_rect(window, SIDEBAR_WIDTH),
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
            autoscrolling: false,
            command_held: false,
            scale_factor: window.scale_factor(),
            reporting_button: None,
            last_mouse_cell: None,
            scroll_remainder: 0.,
            mouse_position: None,
            hover_link: None,
            hover_cell: None,
            context_menu: None,
            renaming: None,
            naming_new_workspace: false,
            closing_workspace: None,
            picker: None,
            activity: None,
            copy_mode: None,
            toast: None,
            asks: Vec::new(),
            files: None,
            sidebar_hidden: false,
            palette: None,
            search_all: None,
            switcher_open: false,
            pending_keys: None,
            folded_servers: HashSet::new(),
            _tasks: vec![blink, poll, spinner],
            _subscriptions: vec![activation, pending],
        }
    }

    // The server's side.

    pub fn window_handle(&self) -> gpui::AnyWindowHandle {
        self.window_handle
    }

    /// A server attached this view to its window `window`.
    pub fn attached(
        &mut self,
        window: WindowId,
        _bounds: Option<SavedWindow>,
        cx: &mut Context<Self>,
    ) {
        let server = ids::server_of(window);
        self.servers.insert(server, (window, WindowLayout::default()));
        self.remembered.insert(server, window);
        if self.window.is_none() {
            self.front = server;
            self.window = Some(window);
        }
        self.sizes.clear();
        let focused = self.window_active && server == self.front;
        app_state::send(ClientMessage::WindowFocused { window, focused }, cx);
        if self.front_on_attach == Some(server) {
            self.front_on_attach = None;
            self.set_front(server, cx);
        }
        cx.notify();
    }

    /// A server's layout of its window here.
    pub fn set_layout(&mut self, window: WindowId, layout: WindowLayout, cx: &mut Context<Self>) {
        match self.servers.get_mut(&ids::server_of(window)) {
            Some(entry) if entry.0 == window => entry.1 = layout,
            _ => return,
        }
        let merged = self.merged_layout();
        self.apply_layout(merged, cx);
    }

    /// Every server's workspaces, in server order; the front server's
    /// active one is active.
    fn merged_layout(&self) -> WindowLayout {
        let mut merged = WindowLayout::default();
        for (&server, (_, layout)) in &self.servers {
            if server == self.front {
                merged.active = merged.workspaces.len() + layout.active;
            }
            merged.workspaces.extend(layout.workspaces.iter().cloned());
            merged.hidden += layout.hidden;
        }
        merged
    }

    /// Show `server`'s active tab: the user went to something there.
    fn set_front(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let Some(&(window, _)) = self.servers.get(&server) else { return };
        if server == self.front && self.window == Some(window) {
            return;
        }
        let before = self.window;
        self.front = server;
        self.window = Some(window);
        // Only the front server's tab is seen.
        if let Some(before) = before {
            self.send(ClientMessage::WindowFocused { window: before, focused: false }, cx);
        }
        self.send(ClientMessage::WindowFocused { window, focused: self.window_active }, cx);
        let merged = self.merged_layout();
        self.apply_layout(merged, cx);
    }

    /// This view's window on `server`, if it shows one.
    fn window_on(&self, server: ServerId) -> Option<WindowId> {
        self.servers.get(&server).map(|(window, _)| *window)
    }

    /// The server windows this view shows.
    pub fn server_windows(&self) -> Vec<WindowId> {
        self.servers.values().map(|(window, _)| *window).collect()
    }

    /// The window last shown here from `server`.
    pub fn remembered(&self, server: ServerId) -> Option<WindowId> {
        self.remembered.get(&server).copied()
    }

    /// A server closed its window here. Returns whether nothing is left.
    pub fn server_window_gone(&mut self, window: WindowId, cx: &mut Context<Self>) -> bool {
        let server = ids::server_of(window);
        self.remembered.remove(&server);
        if self.window_on(server) == Some(window) {
            self.server_gone(server, cx)
        } else {
            self.servers.is_empty()
        }
    }

    /// A server's window went from here (its connection was lost, say).
    /// Returns whether nothing is left.
    pub fn server_gone(&mut self, server: ServerId, cx: &mut Context<Self>) -> bool {
        if self.servers.remove(&server).is_none() {
            return self.servers.is_empty();
        }
        if self.copy_mode.is_some_and(|pane| ids::server_of(pane) == server) {
            self.copy_mode = None;
        }
        if self.front == server {
            self.front = self.servers.keys().next().copied().unwrap_or(LOCAL);
            self.window = self.window_on(self.front);
        }
        let merged = self.merged_layout();
        self.apply_layout(merged, cx);
        self.servers.is_empty()
    }

    fn apply_layout(&mut self, layout: WindowLayout, cx: &mut Context<Self>) {
        if layout != self.layout {
            let shown = |layout: &WindowLayout| {
                (
                    layout.active_workspace().map(|w| (w.id, w.active)),
                    layout.active_tab().map(|t| t.focused),
                )
            };
            if shown(&layout) != shown(&self.layout) {
                self.reset_input_state();
                // Something opened or switched tabs: show it.
                if !self.layout.workspaces.is_empty() {
                    self.activity = None;
                }
            }
            self.tabs = layout.tabs().map(|(_, _, tab)| tab.clone()).collect();
            self.places = layout.tabs().map(|(workspace, index, _)| (workspace, index)).collect();
            let before: usize =
                layout.workspaces.iter().take(layout.active).map(|w| w.tabs.len()).sum();
            self.active = before + layout.active_workspace().map_or(0, |w| w.active);
            let new = layout
                .workspaces
                .iter()
                .map(|w| w.id)
                .find(|id| !self.layout.workspaces.iter().any(|w| w.id == *id));
            let first = self.layout.workspaces.is_empty();
            self.layout = layout;
            if let Some(id) = new.filter(|_| self.naming_new_workspace && !first) {
                self.naming_new_workspace = false;
                self.start_workspace_rename(id, cx);
            }
            cx.notify();
        }
    }

    /// The servers are gone or let go already: don't detach when closing.
    pub fn forget_servers(&mut self) {
        self.detach = false;
    }

    /// What this window shows: its workspace's name, or its tab's title.
    pub fn title(&self, cx: &App) -> String {
        match self.layout.active_workspace() {
            Some(workspace) if workspace.name.is_some() => {
                workspace.name.clone().unwrap_or_default()
            },
            _ => self
                .layout
                .active_tab()
                .map(|tab| self.tab_title(tab, cx).to_string())
                .unwrap_or_default(),
        }
    }

    /// Whether this window shows `pane` right now.
    pub fn shows(&self, pane: PaneId) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| layout_of(tab, self.area).0.iter().any(|(id, _)| *id == pane))
    }

    fn send(&self, message: ClientMessage, cx: &App) {
        app_state::send(message, cx);
    }

    // Geometry.

    /// The area panes fill, in window coordinates; matches `render`.
    fn area_rect(window: &Window, sidebar: f32) -> Rect {
        let viewport = window.viewport_size();
        Rect {
            x: sidebar + PADDING,
            y: TITLEBAR_HEIGHT,
            width: (f32::from(viewport.width) - sidebar - 2. * PADDING).max(1.),
            height: (f32::from(viewport.height) - TITLEBAR_HEIGHT - PADDING).max(1.),
        }
    }

    /// The sidebar's width: none while folded away.
    fn sidebar_width(&self) -> f32 {
        if self.sidebar_hidden { 0. } else { SIDEBAR_WIDTH }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        cx.notify();
    }

    /// While the sidebar is folded away, what it would show of tabs that
    /// matter: how many need you (waiting for an answer, or asking for
    /// attention), how many are working.
    fn away_counts(&self, cx: &App) -> (usize, usize) {
        let panes = self.tabs.iter().flat_map(|tab| tab.root.panes());
        let (mut need, mut working) = (0, 0);
        for pane in panes {
            match self.pane_state(pane, cx) {
                Some(s) if s.icon == "waiting" || s.info.needs_attention => need += 1,
                Some(s) if s.icon == "busy" => working += 1,
                _ => {},
            }
        }
        (need, working)
    }

    fn size_for(&self, rect: Rect) -> TerminalSize {
        TerminalSize {
            columns: (px(rect.width) / self.metrics.cell_width).floor().max(2.) as u16,
            lines: (px(rect.height) / self.metrics.line_height).floor().max(1.) as u16,
            // Screen pixels, as other terminals report them: programs that
            // size images to the window send them sharp on Retina displays.
            cell_width: (f32::from(self.metrics.cell_width) * self.scale_factor).round() as u16,
            cell_height: (f32::from(self.metrics.line_height) * self.scale_factor).round() as u16,
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
            None => self
                .pane_state(tab.focused, cx)
                .map(|p| p.info.title.clone())
                .unwrap_or_else(|| "shell".into())
                .into(),
        }
    }

    fn open_tab(&self, cx: &App) {
        self.open_tab_in(None, cx);
    }

    /// A new tab at the end of `workspace`, or of the active one.
    fn open_tab_in(&self, workspace: Option<WorkspaceId>, cx: &App) {
        let window = match workspace {
            Some(workspace) => self.window_on(ids::server_of(workspace)),
            None => self.window,
        };
        if let Some(window) = window {
            self.send(ClientMessage::NewTab { window, workspace, spec: PaneSpec::default() }, cx);
        }
    }

    /// A new workspace, named as soon as it appears.
    fn new_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.window {
            self.naming_new_workspace = true;
            self.send(
                ClientMessage::NewWorkspace { window, name: None, spec: PaneSpec::default() },
                cx,
            );
        }
    }

    /// Show the workspace `offset` sections away (wrapping), at its active tab.
    fn cycle_workspace(&mut self, offset: isize, cx: &mut Context<Self>) {
        let count = self.layout.workspaces.len() as isize;
        if count < 2 {
            return;
        }
        let target = (self.layout.active as isize + offset).rem_euclid(count) as usize;
        self.show_workspace_at(target, cx);
    }

    /// Show the workspace at `position` (its current tab).
    fn show_workspace_at(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(workspace) = self.layout.workspaces.get(position) else { return };
        if let Some(index) = self.places.iter().position(|&p| p == (workspace.id, workspace.active))
        {
            self.select(index, cx);
        }
    }

    /// What needs you most in the workspaces at `positions`: that pane's
    /// state, and how many are in the same state.
    fn rollup<'a>(
        &self,
        positions: impl Iterator<Item = usize>,
        cx: &'a App,
    ) -> Option<(&'a PaneState, usize)> {
        let states: Vec<&PaneState> = positions
            .filter_map(|p| self.layout.workspaces.get(p))
            .flat_map(|w| w.tabs.iter().flat_map(|t| t.root.panes()))
            .filter_map(|pane| self.pane_state(pane, cx))
            .collect();
        let top = states
            .iter()
            .copied()
            .max_by_key(|s| Self::urgency(Some(s)))
            .filter(|s| Self::urgency(Some(s)) > 0)?;
        Some((top, states.iter().filter(|s| s.icon == top.icon).count()))
    }

    fn toggle_collapsed(&self, workspace: WorkspaceId, cx: &App) {
        if let Some(state) = self.layout.workspaces.iter().find(|w| w.id == workspace) {
            self.send(ClientMessage::SetCollapsed { workspace, collapsed: !state.collapsed }, cx);
        }
    }

    /// A workspace's name: what the user called it, else its repository or
    /// directory, else its position.
    fn workspace_title(&self, position: usize) -> SharedString {
        let workspace = &self.layout.workspaces[position];
        let automatic = Some(workspace.title.clone()).filter(|t| !t.is_empty());
        workspace
            .name
            .clone()
            .or(automatic)
            .unwrap_or_else(|| format!("Workspace {}", position + 1))
            .into()
    }

    /// The color of the workspace holding tab `index`, as RGB.
    fn tab_color(&self, index: usize) -> Option<u32> {
        let (workspace, _) = *self.places.get(index)?;
        let workspace = self.layout.workspaces.iter().find(|w| w.id == workspace)?;
        workspace_color(workspace.color.as_deref()?)
    }

    /// Make the directory of a workspace's current pane its root.
    fn root_from_current_directory(&self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        let Some(state) = self.layout.workspaces.iter().find(|w| w.id == workspace) else { return };
        let Some(pane) = state.tabs.get(state.active).map(|t| t.focused) else { return };
        let answer = app_state::query(Query::Cwd { pane }, cx);
        cx.spawn(async move |_, cx| {
            if let Ok(Reply::Path(Some(root))) = answer.await {
                let _ = cx.update(|cx| {
                    app_state::send(
                        ClientMessage::SetWorkspaceRoot { workspace, root: Some(root) },
                        cx,
                    )
                });
            }
        })
        .detach();
    }

    /// Open the workspace picker, listing workspaces this window doesn't show.
    fn open_picker(&mut self, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        self.close_search(cx);
        // Every server shown here: its workspaces not in this window.
        let names: HashMap<ServerId, String> =
            app_state::servers(cx).into_iter().map(|(id, name, _)| (id, name)).collect();
        let several = self.servers.len() > 1;
        let asks: Vec<(ServerId, Option<WindowId>, _)> = self
            .servers
            .iter()
            .map(|(&server, &(window, _))| {
                (server, Some(window), app_state::control_on(server, Request::Workspaces, cx))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let mut items: Vec<WorkspaceInfo> = Vec::new();
            for (server, here, answer) in asks {
                let Ok(Response::Workspaces { workspaces }) = answer.await else { continue };
                let name = names.get(&server).cloned().unwrap_or_default();
                items.extend(workspaces.into_iter().filter_map(|mut workspace| {
                    workspace.id = ids::tag(server, workspace.id);
                    workspace.window = workspace.window.map(|w| ids::tag(server, w));
                    if workspace.window.is_some() && workspace.window == here {
                        return None;
                    }
                    if several && server != LOCAL {
                        workspace.title = format!("{name} · {}", workspace.title);
                    }
                    Some(workspace)
                }));
            }
            let _ = this.update(cx, |this, cx| {
                // Hidden ones first: those are what you're likely after.
                items.sort_by_key(|w| w.attached);
                this.marked_text = None;
                this.picker = Some(Picker { query: String::new(), items, selected: 0 });
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.take() else { return };
        // Into this window's window on the workspace's server.
        if let Some(choice) = picker.matches().get(picker.selected) {
            let server = ids::server_of(choice.id);
            if let Some(window) = self.window_on(server) {
                self.set_front(server, cx);
                self.send(ClientMessage::OpenWorkspace { workspace: choice.id, window }, cx);
            }
        }
        self.marked_text = None;
        cx.notify();
    }

    fn picker_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else { return };
        let count = picker.matches().len();
        match event.keystroke.key.as_str() {
            "escape" => self.picker = None,
            "enter" => return self.choose_workspace(cx),
            "up" => picker.selected = picker.selected.saturating_sub(1),
            "down" => picker.selected = (picker.selected + 1).min(count.saturating_sub(1)),
            "backspace" => {
                picker.query.pop();
                picker.selected = 0;
            },
            // Text arrives through the input handler.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The picker, over a backdrop that dismisses it.
    fn picker_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let picker = self.picker.as_ref()?;
        let matches = picker.matches();
        let marked = self.marked_text.as_deref().unwrap_or_default();
        let rows: Vec<gpui::AnyElement> = matches
            .iter()
            .enumerate()
            .map(|(i, workspace)| {
                let place = match (workspace.window, workspace.attached) {
                    (None, _) => "hidden".to_owned(),
                    (Some(_), false) => "closed window".to_owned(),
                    (Some(_), true) => format!("window {}", workspace.window_number),
                };
                let tabs = if workspace.tabs == 1 {
                    "1 tab".to_owned()
                } else {
                    format!("{} tabs", workspace.tabs)
                };
                // Statuses (`working`, ...) as the sidebar's icons (`busy`, ...).
                let icon = match workspace.status.as_str() {
                    "working" | "running" => "busy",
                    "waiting" | "failed" | "done" => workspace.status.as_str(),
                    _ if workspace.needs_attention => "attention",
                    _ => "",
                };
                div()
                    .id(("picker-item", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .when(i == picker.selected, |d| d.bg(ui(0x3478f6ff)))
                    .when(i != picker.selected, |d| d.hover(|d| d.bg(ui(0xffffff10))))
                    .child(self.icon(icon))
                    .child(
                        Self::one_line(workspace.title.clone())
                            .flex_1()
                            .min_w_0()
                            .text_color(ui(0xffffffee)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(ui(0xffffff80))
                            .child(format!("{tabs} · {place}")),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(picker) = this.picker.as_mut() {
                            picker.selected = i;
                        }
                        this.choose_workspace(cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let panel = div()
            .id("picker")
            .absolute()
            .top(px(70.))
            .left(px(self.sidebar_width() + 40.))
            .w(px(420.))
            .occlude()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(6.))
            .rounded(px(10.))
            .bg(ui(0x262a33f8))
            .border_1()
            .border_color(ui(0xffffff26))
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .px(px(10.))
                    .py(px(7.))
                    .mb(px(4.))
                    .rounded(px(6.))
                    .bg(ui(0x0000004d))
                    .text_color(if picker.query.is_empty() && marked.is_empty() {
                        ui(0xffffff60)
                    } else {
                        ui(0xffffffee)
                    })
                    .child(if picker.query.is_empty() && marked.is_empty() {
                        "Open workspace…".to_owned()
                    } else {
                        format!("{}{marked}▏", picker.query)
                    }),
            )
            .children(rows)
            .when(empty, |d| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .text_color(ui(0xffffff70))
                        .child("No other workspaces"),
                )
            });
        let backdrop = div()
            .id("picker-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                this.picker = None;
                cx.notify();
            }));
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }

    fn open_sidebar_menu(
        &mut self,
        target: MenuTarget,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        self.context_menu =
            Some(ContextMenu { position, target, link: None, has_selection: false });
        cx.notify();
    }

    fn split(&self, pane: Option<PaneId>, axis: Axis, cx: &App) {
        if let Some(pane) = pane.or(self.focused_id()) {
            self.send(ClientMessage::Split { pane, axis, spec: PaneSpec::default() }, cx);
        }
    }

    fn select(&mut self, tab: usize, cx: &mut Context<Self>) {
        if let Some(&(workspace, tab)) = self.places.get(tab) {
            self.close_search(cx);
            self.activity = None;
            self.set_front(ids::server_of(workspace), cx);
            self.send(ClientMessage::SelectTab { workspace, tab }, cx);
            cx.notify();
        }
    }

    fn focus_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if Some(pane) != self.focused_id() {
            self.close_search(cx);
        }
        self.set_front(ids::server_of(pane), cx);
        self.send(ClientMessage::FocusPane { pane }, cx);
    }

    fn reset_input_state(&mut self) {
        self.marked_text = None;
        self.selecting = false;
        self.reporting_button = None;
        self.mouse_pane = None;
        self.cursor_visible = true;
    }

    /// Secure keyboard entry: on while it's set and an artty window is active.
    fn update_secure_input(&self, cx: &App) {
        crate::secure_input::set(
            (self.config.secure_keyboard_entry || self.asking_secret())
                && (self.window_active || cx.active_window().is_some()),
        );
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
        let next = if forward {
            (index + 1) % panes.len()
        } else {
            (index + panes.len() - 1) % panes.len()
        };
        self.focus_pane(panes[next], cx);
    }

    /// Move a tab to where tab `to` is; across a section boundary, it joins
    /// the neighboring workspace at the near end.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let (Some(&(workspace, from_index)), Some(&(to_workspace, to_index))) =
            (self.places.get(from), self.places.get(to))
        else {
            return;
        };
        // A tab's programs run on its server: it can't move to another.
        if ids::server_of(workspace) != ids::server_of(to_workspace) {
            return;
        }
        let to = if to_workspace != workspace && to < from { to_index + 1 } else { to_index };
        self.renaming = None;
        self.send(ClientMessage::MoveTab { workspace, from: from_index, to_workspace, to }, cx);
    }

    fn start_rename(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else { return };
        let text = self.tab_title(tab, cx).to_string();
        self.close_search(cx);
        self.marked_text = None;
        self.renaming = Some(Renaming { target: RenameTarget::Tab(index), text });
        cx.notify();
    }

    fn start_workspace_rename(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        let Some(position) = self.layout.workspaces.iter().position(|w| w.id == workspace) else {
            return;
        };
        let text = self.layout.workspaces[position].name.clone().unwrap_or_default();
        self.close_search(cx);
        self.marked_text = None;
        self.renaming = Some(Renaming { target: RenameTarget::Workspace(workspace), text });
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(Renaming { target, text }) = self.renaming.take()
            && commit
        {
            let name = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
            match target {
                RenameTarget::Tab(index) => {
                    if let Some(&(workspace, tab)) = self.places.get(index) {
                        self.send(ClientMessage::RenameTab { workspace, tab, title: name }, cx);
                    }
                },
                RenameTarget::Workspace(workspace) => {
                    self.send(ClientMessage::RenameWorkspace { workspace, name }, cx)
                },
            }
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
            },
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
                this.context_menu = Some(ContextMenu {
                    position,
                    target: MenuTarget::Pane(pane),
                    link,
                    has_selection,
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn run_menu_command(
        &mut self,
        command: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.take() else { return };
        let place = |index: usize| self.places.get(index).copied();
        match menu.target {
            MenuTarget::Tab(index) => {
                match (command, place(index)) {
                    (MenuCommand::RenameTab, _) => self.start_rename(index, cx),
                    (MenuCommand::CloseTab, Some((workspace, tab))) => {
                        self.send(ClientMessage::CloseTab { workspace, tab }, cx)
                    },
                    (MenuCommand::TabToNewWorkspace, Some((workspace, tab))) => {
                        self.send(ClientMessage::TabToNewWorkspace { workspace, tab }, cx)
                    },
                    _ => {},
                }
                return cx.notify();
            },
            MenuTarget::Server(server) => {
                match command {
                    MenuCommand::ReconnectFromScratch => {
                        app_state::reconnect_from_scratch(server, cx)
                    },
                    MenuCommand::Connect => app_state::connect_remote(server, cx),
                    // (After this view's update: it updates every view.)
                    MenuCommand::Disconnect => {
                        cx.defer(move |cx| app_state::disconnect_remote(server, cx))
                    },
                    _ => {},
                }
                return cx.notify();
            },
            MenuTarget::Workspace(workspace) => {
                match command {
                    MenuCommand::NewTabInWorkspace => self.open_tab_in(Some(workspace), cx),
                    MenuCommand::RenameWorkspace => self.start_workspace_rename(workspace, cx),
                    MenuCommand::ToggleCollapsed => self.toggle_collapsed(workspace, cx),
                    MenuCommand::CloseWorkspace => {
                        self.send(ClientMessage::CloseWorkspace { workspace }, cx)
                    },
                    MenuCommand::HideWorkspace => {
                        self.send(ClientMessage::HideWorkspace { workspace }, cx)
                    },
                    MenuCommand::MoveToNewWindow => {
                        self.send(ClientMessage::WorkspaceToNewWindow { workspace }, cx)
                    },
                    MenuCommand::SetColor(color) => {
                        let color =
                            color.and_then(|i| WORKSPACE_COLORS.get(i)).map(|c| (*c).to_owned());
                        self.send(ClientMessage::SetWorkspaceColor { workspace, color }, cx);
                    },
                    MenuCommand::ClearRoot => {
                        self.send(ClientMessage::SetWorkspaceRoot { workspace, root: None }, cx)
                    },
                    MenuCommand::RootFromCurrentDirectory => {
                        self.root_from_current_directory(workspace, cx)
                    },
                    MenuCommand::MoveToWindow(window) => {
                        self.send(
                            ClientMessage::MoveWorkspace { workspace, window, to: usize::MAX },
                            cx,
                        );
                        app_state::activate(window, cx);
                    },
                    _ => {},
                }
                return cx.notify();
            },
            MenuTarget::Pane(_) => {},
        }
        let MenuTarget::Pane(pane) = menu.target else { return };
        match command {
            MenuCommand::Copy => self.copy(&Copy, window, cx),
            MenuCommand::Paste => self.paste(&Paste, window, cx),
            MenuCommand::OpenLink => {
                if let Some(link) = menu.link {
                    self.open_link(pane, link, cx);
                }
            },
            MenuCommand::CopyLink => {
                if let Some(link) = menu.link {
                    cx.write_to_clipboard(ClipboardItem::new_string(link));
                }
            },
            MenuCommand::SplitRight => self.split(Some(pane), Axis::Horizontal, cx),
            MenuCommand::SplitDown => self.split(Some(pane), Axis::Vertical, cx),
            MenuCommand::RenameTab => {
                if let Some(index) = self.tabs.iter().position(|t| t.root.contains(pane)) {
                    self.start_rename(index, cx);
                }
            },
            MenuCommand::ClearBuffer => self.send(ClientMessage::ClearBuffer { pane }, cx),
            MenuCommand::ClosePane => self.send(ClientMessage::ClosePane { pane }, cx),
            _ => {},
        }
        cx.notify();
    }

    fn jump_to_attention(&mut self, cx: &mut Context<Self>) {
        // Every pane in tab order, starting after the focused one.
        let all: Vec<PaneId> = self.tabs.iter().flat_map(|tab| tab.root.panes()).collect();
        let start =
            self.focused_id().and_then(|id| all.iter().position(|&p| p == id)).map_or(0, |i| i + 1);
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
    fn input(&mut self, bytes: Vec<u8>, cx: &mut App) {
        if let Some(pane) = self.focused_id() {
            self.echo(pane, &bytes, cx);
            self.send(ClientMessage::Input { pane, bytes }, cx);
        }
        self.cursor_visible = true;
        self.blink_hold = true;
    }

    /// Show what's typed into `pane` before its echo comes back, where
    /// that's on (not with typing going to every pane).
    pub(super) fn echo(&self, pane: PaneId, bytes: &[u8], cx: &mut App) {
        let broadcast = self.tabs.get(self.active).is_some_and(|t| t.broadcast);
        if self.config.local_echo && !broadcast {
            app_state::typed(pane, bytes, cx);
        }
    }

    fn blink(&mut self, cx: &mut Context<Self>) {
        if app_state::expire_echoes(cx) {
            cx.notify();
        }
        self.files_tick(cx);
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
        let flashing =
            panes.iter().any(|id| shared.flashes.get(id).is_some_and(|at| at.elapsed() <= FLASH));
        if busy {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
        }
        if busy || flashing {
            cx.notify();
        }
    }

    /// Once a second: notice config changes (fonts, opacity are drawn here).
    fn poll(&mut self, cx: &mut Context<Self>) {
        if self.showing_activity() {
            self.refresh_activity(cx);
        }
        if self.toast.as_ref().is_some_and(|(_, shown)| shown.elapsed() >= pane_actions::TOAST_TIME)
        {
            self.toast = None;
            cx.notify();
        }
        // A server's "again in Ns" counts down.
        if app_state::servers(cx)
            .iter()
            .any(|(_, _, s)| matches!(s, app_state::Status::Waiting { .. }))
        {
            cx.notify();
        }
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
        // The active window follows the setting (inactive ones leave it be).
        if self.window_active {
            crate::secure_input::set(self.config.secure_keyboard_entry);
        }
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

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.asking() {
            self.ask_key(event, cx);
            return;
        }
        if self.switcher_open && keystroke.key == "escape" {
            self.switcher_open = false;
            cx.stop_propagation();
            return cx.notify();
        }
        if self.palette.is_some() {
            self.palette_key(event, window, cx);
            return;
        }
        if self.search_all.is_some() {
            self.search_all_key(event, cx);
            return;
        }
        if self.context_menu.take().is_some() {
            cx.notify();
            if keystroke.key == "escape" {
                cx.stop_propagation();
                return;
            }
        }
        if self.picker.is_some() {
            self.picker_key(event, cx);
            return;
        }
        if self.renaming.is_some() {
            self.rename_key(event, cx);
            return;
        }
        if self.search.is_some() {
            self.search_key(event, cx);
            return;
        }
        if self.showing_activity() {
            // Nothing typed here goes to a terminal.
            if self.activity_key(event, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.in_copy_mode(cx) {
            if self.copy_mode_key(event, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let Some(pane) = self.focused_id() else { return };
        if let Some(bytes) =
            keys::to_esc(keystroke, self.mode(pane, cx), self.config.option_as_meta)
        {
            self.input(bytes, cx);
            cx.stop_propagation();
        }
    }

    // Search (⌘F), in the focused pane.

    fn open_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search =
                Some(Search { query: String::new(), found: None, counts: None, error: None });
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
                            search.found = (!search.query.is_empty()).then_some(found.found);
                            search.counts = Some(found);
                            search.error = None;
                        },
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
            },
            "enter" => self.run_search(!keystroke.modifiers.shift, false, cx),
            "backspace" => {
                if let Some(search) = self.search.as_mut() {
                    search.query.pop();
                }
                self.run_search(true, true, cx);
            },
            // Text arrives through the input handler.
            _ if !keystroke.modifiers.control && !keystroke.modifiers.alt => return,
            _ => {},
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

    fn report_mouse(
        &mut self,
        id: PaneId,
        button: Button,
        action: Action,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        cx: &App,
    ) {
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
            cx.spawn(async move |this, cx| {
                if let Ok(Reply::Link(Some((link, _, _)))) = answer.await {
                    let _ = this.update(cx, |this, cx| this.open_link(id, link, cx));
                }
            })
            .detach();
            return;
        }
        let menu_click = event.button == MouseButton::Right
            || (event.button == MouseButton::Left && event.modifiers.control);
        if menu_click
            && (self.mouse_reporting(id, &event.modifiers, cx).is_none() || event.modifiers.control)
        {
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
            let ratio = layout::drag_ratio(
                divider,
                event.position.x.into(),
                event.position.y.into(),
                SPLIT_GAP,
            );
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
                self.send(
                    ClientMessage::Selection {
                        pane: id,
                        op: SelectionOp::Update { line, column, right },
                    },
                    cx,
                );
                if self.overflow(id, event.position) != 0 {
                    self.start_autoscroll(cx);
                }
            }
        } else if event.pressed_button.is_none()
            && let Some((id, _)) = self.pane_at(event.position)
            && Some(id) == self.focused_id()
        {
            self.report_mouse(
                id,
                Button::None,
                Action::Motion,
                event.position,
                &event.modifiers,
                cx,
            );
        }
    }

    /// How far past the top (positive) or bottom (negative) of a pane a
    /// point is, in lines (rounded up); 0 inside.
    fn overflow(&self, id: PaneId, position: Point<Pixels>) -> i32 {
        let Some(rect) = self.rect_of(id) else { return 0 };
        let y = f32::from(position.y);
        let line = f32::from(self.metrics.line_height);
        if y < rect.y {
            ((rect.y - y) / line).ceil() as i32
        } else if y > rect.y + rect.height {
            -((y - rect.y - rect.height) / line).ceil() as i32
        } else {
            0
        }
    }

    /// While a selection drag stays past the pane's edge, scroll toward it
    /// and extend the selection, faster the farther out it is.
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscrolling {
            return;
        }
        self.autoscrolling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let going = this.update(cx, |this, cx| this.autoscroll_step(cx)).unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();
    }

    fn autoscroll_step(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(id), Some(position)) = (self.mouse_pane, self.mouse_position) else {
            self.autoscrolling = false;
            return false;
        };
        let overflow = self.overflow(id, position);
        if !self.selecting || overflow == 0 {
            self.autoscrolling = false;
            return false;
        }
        let lines = overflow.signum() * overflow.abs().min(10);
        self.send(ClientMessage::Scroll { pane: id, lines }, cx);
        // The edge line, now showing what scrolled in.
        let (line, column, right) = self.cell_at(id, position);
        self.send(
            ClientMessage::Selection { pane: id, op: SelectionOp::Update { line, column, right } },
            cx,
        );
        true
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
                    Some(_) => {},
                    // A plain click selects nothing; drop the empty selection.
                    None => {
                        this.send(ClientMessage::Selection { pane, op: SelectionOp::Clear }, cx)
                    },
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
            },
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
            self.send(
                ClientMessage::Input { pane: id, bytes: key.repeat(lines.unsigned_abs() as usize) },
                cx,
            );
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

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        if let Some(search) = self.search.as_mut() {
            search.query.push_str(text.lines().next().unwrap_or_default());
            self.run_search(true, true, cx);
            return;
        }
        if self.in_copy_mode(cx) {
            return;
        }
        self.paste_text(text, window, cx);
    }

    fn set_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.metrics = Metrics::measure(window, &self.config, self.font_size);
        cx.notify();
    }

    // Sidebar.

    fn icon_cell(&self, state: Option<&PaneState>) -> gpui::Div {
        self.icon(state.map_or("", |s| s.icon.as_str()))
    }

    /// A status icon: `busy`, `waiting`, `attention`, `done`, `failed`.
    fn icon(&self, icon: &str) -> gpui::Div {
        let color = |hex: u32| -> Hsla { ui(hex).into() };
        // Braille spinner frames are tiny at text size.
        let icon: Option<(SharedString, Hsla, f32)> = match icon {
            "busy" => Some((SPINNER[self.spinner_frame].into(), color(0x8fb4ffff), 16.)),
            "waiting" | "attention" => Some(("●".into(), color(0xe0af68ff), 12.)),
            "done" => Some(("✓".into(), color(0x9ece6aff), 12.)),
            "failed" => Some(("✕".into(), color(0xf7768eff), 12.)),
            _ => None,
        };
        div()
            .w(px(12.))
            .flex_none()
            .flex()
            .justify_center()
            .when_some(icon, |d, (icon, color, size)| {
                d.text_color(color).text_size(px(size)).child(icon)
            })
    }

    /// What a sidebar row shows at its right: the command or agent (not
    /// its time or state; the icon tells that), an exit code, progress. Not
    /// what the title already says: shells put the running command in it.
    fn short_detail(title: &str, state: &PaneState) -> Option<String> {
        let detail = state.info.detail.as_deref()?;
        let what = detail.split_once(" · ").map_or(detail, |(what, _)| what);
        let what = (!title.starts_with(what)).then(|| what.to_owned());
        match (what, state.progress) {
            (Some(what), Some(percent)) => Some(format!("{what} · {percent}%")),
            (None, Some(percent)) => Some(format!("{percent}%")),
            (what, None) => what,
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

    /// Part of the titlebar: a double click does what System Settings says
    /// (zoom, minimize, or nothing), as in other apps — content covers the
    /// real titlebar, so macOS doesn't see the clicks itself.
    fn titlebar(id: &'static str) -> gpui::Stateful<gpui::Div> {
        div().id(id).on_click(|event, window, _| {
            if event.click_count() == 2 {
                window.titlebar_double_click();
            }
        })
    }

    fn one_line(text: impl Into<SharedString>) -> gpui::Div {
        // GPUI 0.2 caches the first measurement of `nowrap` text, which happens
        // before the width is known, so `.truncate()` only clips. A one-line
        // clamp re-measures at the real width and ellipsizes.
        div().overflow_hidden().line_clamp(1).text_ellipsis().child(text.into())
    }

    fn tab_row(
        &self,
        index: usize,
        tab: &TabLayout,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let selected = index == self.active && !self.showing_activity();
        let ids = tab.root.panes();
        let split = ids.len() > 1;
        let states: Vec<Option<&PaneState>> =
            ids.iter().map(|id| self.pane_state(*id, cx)).collect();
        let focused = self.pane_state(tab.focused, cx);
        let mut ports: Vec<u16> =
            states.iter().flatten().flat_map(|s| s.ports.iter().copied()).collect();
        ports.sort_unstable();
        ports.dedup();
        // The row shows the most urgent pane, preferring the focused one.
        let summary = ids
            .iter()
            .zip(&states)
            .max_by_key(|(id, state)| (Self::urgency(**state), **id == tab.focused))
            .and_then(|(_, state)| *state)
            .or(focused);
        let attention = states.iter().flatten().any(|s| s.info.needs_attention);
        let label: SharedString =
            if index < 9 { format!("⌘{}", index + 1).into() } else { "".into() };
        let text_color = if selected { ui(0xffffffee) } else { ui(0xffffffb0) };
        let group: SharedString = format!("tab-row-{index}").into();
        let renaming = self.renaming.as_ref().filter(|r| r.target == RenameTarget::Tab(index));
        // A split tab lists its panes, a line each, so every agent's state is
        // visible; a name of its own (or renaming it) keeps a line above them.
        // With `list-panes` off, a split tab is one line: its focused pane.
        let list = split && self.config.list_panes;
        let own_line = !list || tab.title.is_some() || renaming.is_some();
        let detail = if list {
            None
        } else {
            focused.and_then(|state| Self::short_detail(&self.tab_title(tab, cx), state))
        };

        // One line: icon, title, then what it's doing on the right, and a
        // last column for the ⌘N label (while ⌘ is held) or, on hover, a close
        // button.
        let header = own_line.then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(22.))
                .child(self.icon_cell(summary))
                .child(match renaming {
                    Some(renaming) => {
                        let marked = self.marked_text.as_deref().unwrap_or_default();
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .flex_1()
                            .min_w_0()
                            .px_1()
                            .rounded(px(4.))
                            .bg(ui(0x0000004d))
                            .border_1()
                            .border_color(ui(0x8fb4ff99))
                            .text_color(ui(0xffffffee))
                            .child(format!("{}{marked}▏", renaming.text))
                    },
                    None => Self::one_line(self.tab_title(tab, cx))
                        .flex_1()
                        .min_w_0()
                        .text_color(text_color)
                        .when(attention, |d| d.font_weight(FontWeight::SEMIBOLD)),
                })
                .child(self.tab_end(
                    index,
                    &group,
                    Some(label.clone()),
                    detail.filter(|_| renaming.is_none()),
                    cx,
                ))
        });

        let pane_rows = list.then(|| {
            ids.iter()
                .zip(&states)
                .enumerate()
                .map(|(position, (&id, state))| {
                    let is_focused = id == tab.focused;
                    let title =
                        state.map(|s| s.info.title.clone()).unwrap_or_else(|| "shell".into());
                    let detail = state.and_then(|state| Self::short_detail(&title, state));
                    let pane_attention = state.is_some_and(|s| s.info.needs_attention);
                    // Without the tab's own line, the first pane's line has its
                    // ⌘N label and close button.
                    let controls = (position == 0 && !own_line).then(|| label.clone());
                    div()
                        .id(("pane", id as usize))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .rounded(px(5.))
                        .hover(|d| d.bg(ui(0xffffff0c)))
                        // Under the tab's own line, indented and smaller;
                        // otherwise lines as tall as a tab's.
                        .map(|d| if own_line { d.pl(px(14.)).text_size(px(11.5)) } else { d.h(px(22.)) })
                        .on_click(cx.listener(move |this, _, _, cx| this.focus_pane(id, cx)))
                        // Dragged onto another tab, it moves there.
                        .on_drag(
                            DraggedPane { pane: id, title: title.clone().into() },
                            |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                        )
                        .child(self.icon_cell(*state))
                        .child(
                            Self::one_line(title)
                                .flex_1()
                                .min_w_0()
                                .text_color(if is_focused { text_color } else { ui(0xffffff80) })
                                .when(pane_attention, |d| d.font_weight(FontWeight::SEMIBOLD)),
                        )
                        .child(self.tab_end(index, &group, controls, detail, cx))
                })
                .collect::<Vec<_>>()
        });

        div()
            .id(("tab", index))
            .group(group)
            .flex()
            .flex_col()
            .gap(px(1.))
            .py(px(3.))
            .px_2()
            .rounded(px(7.))
            .when(selected, |d| match self.tab_color(index) {
                // A colored workspace tints its selected tab.
                Some(color) => d.bg(rgba(color << 8 | 0x33)),
                None => d.bg(ui(0xffffff1c)),
            })
            .when(attention && !selected, |d| d.bg(ui(0xe0af6814)))
            .hover(|d| d.bg(ui(0xffffff10)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.start_rename(index, cx);
                } else {
                    this.select(index, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(MenuTarget::Tab(index), event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_drag(DraggedTab { index, title: self.tab_title(tab, cx) }, |dragged, _, _, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedTab>(|style, _, _, _| style.bg(ui(0x8fb4ff26)))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                this.move_tab(dragged.index, index, cx)
            }))
            .drag_over::<DraggedPane>(|style, _, _, _| style.bg(ui(0x8fb4ff26)))
            .on_drop(cx.listener(move |this, dragged: &DraggedPane, _, cx| {
                if let Some(&(workspace, tab)) = this.places.get(index)
                    && ids::server_of(workspace) == ids::server_of(dragged.pane)
                {
                    this.send(ClientMessage::MovePane { pane: dragged.pane, workspace, tab }, cx);
                }
            }))
            // Typing into every pane: an amber outline.
            .when(tab.broadcast, |d| d.border_1().border_color(ui(0xe0af6899)))
            .children(header)
            .children(pane_rows.into_iter().flatten())
            .children(self.ports_row(ids::server_of(tab.focused), ports, cx))
    }

    /// The ports a tab's programs listen on, to open (forwarded from another
    /// machine first).
    fn ports_row(
        &self,
        server: ServerId,
        ports: Vec<u16>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        (!ports.is_empty()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .pl(px(20.))
                .pb(px(2.))
                .children(ports.into_iter().map(|port| self.port_chip(server, port, cx)))
        })
    }

    /// A port: a click opens it in the browser, forwarding it here first if
    /// it's another machine's; × stops forwarding it.
    pub(super) fn port_chip(
        &self,
        server: ServerId,
        port: u16,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let remote = server != LOCAL;
        let local = crate::forward::local_port(server, port, cx);
        let failed = remote && local.is_none() && crate::forward::error(server, port, cx).is_some();
        let label = match local {
            Some(local) if remote && local != port => format!("{port} → {local}"),
            _ => format!(":{port}"),
        };
        let color = match (failed, remote && local.is_some()) {
            (true, _) => ui(0xf7768ecc),
            (false, true) => ui(0x9ece6aee),
            (false, false) => ui(0xffffff90),
        };
        let id = format!("port-{server}-{port}");
        div()
            .id(SharedString::from(id.clone()))
            .flex()
            .items_center()
            .gap(px(3.))
            .px(px(5.))
            .rounded(px(4.))
            .text_size(px(10.5))
            .bg(ui(0xffffff0d))
            .hover(|d| d.bg(ui(0xffffff1f)))
            .text_color(color)
            .child(label)
            .when(remote && local.is_some(), |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("{id}-stop")))
                        .text_color(ui(0xffffff70))
                        .hover(|d| d.text_color(ui(0xffffffee)))
                        .child("×")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            crate::forward::stop(server, port, cx);
                            cx.stop_propagation();
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if let Some(error) = crate::forward::error(server, port, cx).filter(|_| failed) {
                    this.show_toast(
                        format!("Port {port} wasn't forwarded: {error}; trying again"),
                        cx,
                    );
                }
                if let Err(err) = crate::forward::open(server, port, cx) {
                    this.show_toast(format!("Couldn't forward port {port}: {err}"), cx);
                }
            }))
    }

    /// The last column of a sidebar line: what it's running and, given the
    /// tab's ⌘N `label`, that label while ⌘ is held and on hover the tab's
    /// close button, in its place. (Fading, not hiding: GPUI can't change
    /// layout on hover.)
    fn tab_end(
        &self,
        index: usize,
        group: &SharedString,
        label: Option<SharedString>,
        detail: Option<String>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let controls = label.is_some();
        div()
            .relative()
            .when(controls, |d| d.min_w(px(TAB_LABEL_WIDTH)))
            .max_w(px(110.))
            .h_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .text_size(px(11.))
            .text_color(ui(0xffffff70))
            .when_some(detail, |d, detail| {
                d.child(Self::one_line(detail).min_w_0().when(controls, |d| {
                    d.opacity(if self.command_held { 0. } else { 1. })
                        .group_hover(group.clone(), |s| s.opacity(0.))
                }))
            })
            .when_some(label, |d, label| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(ui(0xffffff80))
                        .opacity(if self.command_held { 1. } else { 0. })
                        .group_hover(group.clone(), |s| s.opacity(0.))
                        .child(label),
                )
                .child(
                    // Spans the slot's height, so the button sits at its center.
                    div().absolute().top_0().bottom_0().right_0().flex().items_center().child(
                        div()
                            .id(("close-tab", index))
                            .opacity(0.)
                            .group_hover(group.clone(), |s| s.opacity(1.))
                            .flex()
                            .size(px(18.))
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .text_color(ui(0xffffffa0))
                            .hover(|d| d.bg(ui(0xffffff1f)).text_color(ui(0xffffffee)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(&(workspace, tab)) = this.places.get(index) {
                                    this.send(ClientMessage::CloseTab { workspace, tab }, cx);
                                }
                                cx.stop_propagation();
                            })),
                    ),
                )
            })
    }

    /// A server's header, with other machines' servers: its name, how its
    /// connection is, and a + for a new workspace there. A server that's
    /// away reconnects on a click.
    fn server_header(
        &self,
        server: ServerId,
        name: String,
        status: app_state::Status,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use app_state::Status;
        let group: SharedString = format!("server-{server}").into();
        let (dot, note): (u32, Option<String>) = match &status {
            Status::Connected => (0x9ece6aff, None),
            Status::Connecting => (0xe0af68ff, Some("connecting…".to_owned())),
            Status::Waiting { error, retry_at } => {
                let seconds = retry_at.saturating_duration_since(Instant::now()).as_secs();
                (0xe0af68ff, Some(format!("{error} · again in {seconds}s · click to retry")))
            },
            Status::Off(reason) => (0xffffff40, Some(reason.clone())),
            Status::NeedsInstall(reason) => {
                (0xe0af68ff, Some(format!("{reason} · click to install this build")))
            },
            Status::Installing => (0xe0af68ff, Some("installing artty there…".to_owned())),
            Status::NeedsLogin(reason) => (0xe0af68ff, Some(format!("{reason} · click to log in"))),
        };
        let connected = status == Status::Connected;
        let install = matches!(status, Status::NeedsInstall(_));
        let away =
            matches!(status, Status::Waiting { .. } | Status::Off(_) | Status::NeedsLogin(_))
                || install;
        let folded = self.folded_servers.contains(&server);
        let workspaces =
            self.layout.workspaces.iter().filter(|w| ids::server_of(w.id) == server).count();
        div()
            .id(("server", server as usize))
            .group(group.clone())
            .flex()
            .flex_col()
            .mt(px(if server == LOCAL { 0. } else { 4. }))
            // Its line: a box as a tab row's.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px_2()
                    .rounded(px(7.))
                    .hover(|d| d.bg(ui(0xffffff10)))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70))
                    // Its state, where the workspaces' arrows are; a click
                    // folds its workspaces away (a ring while folded).
                    .child(
                        div()
                            .id(("server-fold", server as usize))
                            .w(px(12.))
                            .h(px(14.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .hover(|d| d.bg(ui(0xffffff14)))
                            .child(div().size(px(6.)).rounded_full().map(|d| {
                                if folded { d.border_1().border_color(ui(dot)) } else { d.bg(ui(dot)) }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.folded_servers.remove(&server) {
                                    this.folded_servers.insert(server);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .child(Self::one_line(name.to_uppercase()).flex_1().min_w_0())
                    .when(folded, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(ui(0xffffff50))
                                .child(match workspaces {
                                    1 => "1 workspace".to_owned(),
                                    n => format!("{n} workspaces"),
                                }),
                        )
                    })
                    .when(connected, |d| {
                        d.child(
                            div()
                                .id(("new-workspace-on", server as usize))
                                .opacity(0.)
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .px_1()
                                .rounded(px(4.))
                                .hover(|d| d.bg(ui(0xffffff14)))
                                .text_size(px(14.))
                                .child("+")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.new_workspace_on(server, cx);
                                    cx.stop_propagation();
                                })),
                        )
                    }),
            )
            .when_some(note, |d, note| {
                d.child(
                    // Under its name.
                    Self::one_line(note)
                        .pl(px(26.))
                        .pr_2()
                        .pb(px(4.))
                        .text_size(px(10.5))
                        .text_color(ui(0xffffff50)),
                )
            })
            // Its menu: connect or disconnect; for one reached by ssh, a
            // fresh connection (the host's key checked again).
            .when(server != LOCAL, |d| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_sidebar_menu(MenuTarget::Server(server), event.position, cx);
                        cx.stop_propagation();
                    }),
                )
            })
            .when(away && server != LOCAL, |d| {
                d.cursor_pointer().on_click(cx.listener(move |_, _, _, cx| {
                    if install {
                        app_state::install_remote(server, cx)
                    } else {
                        app_state::connect_remote(server, cx)
                    }
                }))
            })
    }

    /// Whether the workspace at `position` comes first under its server.
    fn first_on_server(&self, position: usize) -> bool {
        let workspaces = &self.layout.workspaces;
        position == 0
            || ids::server_of(workspaces[position - 1].id)
                != ids::server_of(workspaces[position].id)
    }

    /// A new workspace on `server`: in its window here, or in a new window
    /// of it shown here.
    fn new_workspace_on(&mut self, server: ServerId, cx: &mut Context<Self>) {
        match self.window_on(server) {
            Some(window) => {
                self.naming_new_workspace = true;
                self.send(
                    ClientMessage::NewWorkspace { window, name: None, spec: PaneSpec::default() },
                    cx,
                );
            },
            None => {
                self.front_on_attach = Some(server);
                let view = cx.entity();
                app_state::attach_new(server, &view, cx);
            },
        }
    }

    /// A section header: fold arrow, name, what its tabs are doing, and a
    /// button for a new tab there.
    fn workspace_header(
        &self,
        position: usize,
        folded: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let workspace = &self.layout.workspaces[position];
        let id = workspace.id;
        let active = position == self.layout.active;
        let title = self.workspace_title(position);
        let states: Vec<&PaneState> = workspace
            .tabs
            .iter()
            .flat_map(|t| t.root.panes())
            .filter_map(|pane| self.pane_state(pane, cx))
            .collect();
        // The most urgent state, and how many panes share it.
        let top = states
            .iter()
            .copied()
            .max_by_key(|s| Self::urgency(Some(s)))
            .filter(|s| Self::urgency(Some(s)) > 0);
        let count = top.map_or(0, |top| states.iter().filter(|s| s.icon == top.icon).count());
        let attention = states.iter().any(|s| s.info.needs_attention);
        let renaming = self.renaming.as_ref().filter(|r| r.target == RenameTarget::Workspace(id));
        let name = match renaming {
            Some(renaming) => {
                let marked = self.marked_text.as_deref().unwrap_or_default();
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .flex_1()
                    .px_1()
                    .rounded(px(4.))
                    .bg(ui(0x0000004d))
                    .border_1()
                    .border_color(ui(0x8fb4ff99))
                    .text_color(ui(0xffffffee))
                    .child(format!("{}{marked}▏", renaming.text))
            },
            None => Self::one_line(title.clone())
                .flex_1()
                .min_w_0()
                .text_color(if active { ui(0xffffffd8) } else { ui(0xffffff90) })
                .when(attention, |d| d.text_color(ui(0xe0af68ff))),
        };
        let tabs = workspace.tabs.len();
        let color = workspace.color.as_deref().and_then(workspace_color);
        let group: SharedString = format!("workspace-header-{id}").into();
        let closing = self.closing_workspace == Some(id);
        // Hovering tells what it uses (and where it's rooted).
        let mut usage = format!(
            "CPU {:.1}% · Memory {}",
            workspace.cpu_percent,
            crate::cli::bytes(workspace.memory_bytes)
        );
        if let Some(root) = &workspace.root {
            usage.push_str(&format!(" · Root {}", root.display()));
        }
        div()
            .id(("workspace", id as usize))
            .group(group.clone())
            .flex()
            .items_center()
            // As a tab row's, after its icon.
            .gap(px(6.))
            // Apart from the section above, but not from its server's header.
            .mt(px(if self.first_on_server(position) { 0. } else { 8. }))
            // A box as a tab row's.
            .h(px(28.))
            // As a tab's row: the arrow where its icon is, the name where its
            // title is (the WORKSPACES heading's + lines up on the right).
            .px_2()
            .rounded(px(7.))
            .text_size(px(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|d| d.bg(ui(0xffffff10)))
            // Only the arrow folds it.
            .child(
                div()
                    .id(("workspace-fold", id as usize))
                    .w(px(12.))
                    .h(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .text_size(px(9.))
                    .text_color(ui(0xffffff70))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                    .child(if folded { "▶" } else { "▼" })
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        cx.stop_propagation();
                        if event.click_count() > 1 {
                            return;
                        }
                        let accordion =
                            this.config.sidebar_style == artty_core::SidebarStyle::Accordion;
                        if accordion && position != this.layout.active {
                            // An accordion opens the one unfolded: going to it.
                            this.show_workspace_at(position, cx);
                        } else {
                            this.toggle_collapsed(id, cx);
                        }
                    })),
            )
            .child(name)
            .when_some(color, |d, color| d.child(div().flex_none().size(px(7.)).rounded_full().bg(rgba(color << 8 | 0xff))))
            // Folded, it says how many tabs it holds.
            .when(folded && renaming.is_none(), |d| {
                d.child(div().flex_none().text_size(px(11.)).text_color(ui(0xffffff60)).child(tabs.to_string()))
            })
            // Folded, what its tabs would show (open, they show it).
            .when_some(top.filter(|_| folded), |d, top| {
                d.child(self.icon_cell(Some(top))).when(count > 1, |d| {
                    d.child(div().flex_none().text_size(px(10.5)).text_color(ui(0xffffff90)).child(count.to_string()))
                })
            })
            // Closing a whole workspace takes a second click.
            .child(
                div()
                    .id(("workspace-close", id as usize))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(18.))
                    .h(px(18.))
                    .line_height(px(18.))
                    .px_1()
                    .rounded(px(4.))
                    .map(|d| {
                        if closing {
                            d.bg(ui(0xf7768eff)).text_size(px(11.)).text_color(gpui::white()).child("Close?")
                        } else {
                            d.opacity(0.)
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .text_size(px(13.))
                                .text_color(ui(0xffffff70))
                                .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                                .child("×")
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.closing_workspace == Some(id) {
                            this.closing_workspace = None;
                            this.send(ClientMessage::CloseWorkspace { workspace: id }, cx);
                        } else {
                            this.closing_workspace = Some(id);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id(("workspace-new-tab", id as usize))
                    // Shown on hover, like the close button.
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .flex_none()
                    .px_1()
                    .rounded(px(4.))
                    .text_size(px(14.))
                    .text_color(ui(0xffffff70))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                    .child("+")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_tab_in(Some(id), cx);
                        cx.stop_propagation();
                    })),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Moving away takes back a pending close.
                if !hovered && this.closing_workspace == Some(id) {
                    this.closing_workspace = None;
                    cx.notify();
                }
            }))
            // Clicked (not on its arrow): going to it; twice, renaming it.
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.start_workspace_rename(id, cx);
                } else if position != this.layout.active {
                    this.show_workspace_at(position, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_sidebar_menu(MenuTarget::Workspace(id), event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .tooltip(move |_, cx| cx.new(|_| Tooltip(usage.clone().into())).into())
            .on_drag(DraggedWorkspace { workspace: id, title }, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
            .drag_over::<DraggedWorkspace>(|style, _, _, _| style.bg(ui(0x8fb4ff26)))
            .drag_over::<DraggedTab>(|style, _, _, _| style.bg(ui(0x8fb4ff26)))
            .on_drop(cx.listener(move |this, dragged: &DraggedWorkspace, _, cx| {
                // Within one server's sections, at its own position there.
                let server = ids::server_of(id);
                if let Some(window) = this.window_on(server)
                    && dragged.workspace != id
                    && ids::server_of(dragged.workspace) == server
                {
                    let to = this.layout.workspaces[..position]
                        .iter()
                        .filter(|w| ids::server_of(w.id) == server)
                        .count();
                    this.send(ClientMessage::MoveWorkspace { workspace: dragged.workspace, window, to }, cx);
                }
            }))
            // A tab dropped on a header goes to the start of that workspace.
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                if let Some(&(workspace, from)) = this.places.get(dragged.index)
                    && ids::server_of(workspace) == ids::server_of(id)
                {
                    this.renaming = None;
                    this.send(ClientMessage::MoveTab { workspace, from, to_workspace: id, to: 0 }, cx);
                }
            }))
    }

    fn banner(
        id: &'static str,
        title: &'static str,
        hint: &'static str,
        button: &'static str,
        action: fn(&mut App),
    ) -> gpui::Div {
        div()
            .mt_2()
            .mx_1()
            .p_2()
            .rounded(px(8.))
            .bg(ui(0x3478f626))
            .border_1()
            .border_color(ui(0x3478f666))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                // (It wraps, so the button stays inside.)
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_color(ui(0xffffffee)).child(title))
                    .child(div().text_size(px(11.)).text_color(ui(0xffffff80)).child(hint)),
            )
            .child(
                div()
                    .id(id)
                    .flex_none()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(ui(0x3478f6ff))
                    .hover(|d| d.bg(ui(0x4a88ffff)))
                    .text_color(ui(0xffffffff))
                    .child(button)
                    .on_click(move |_, _, cx| action(cx)),
            )
    }

    /// In the titlebar while the sidebar is folded away: a button back, and
    /// how many tabs need you (a click goes to one) or are working.
    fn folded_sidebar_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (need, working) = self.away_counts(cx);
        div()
            .absolute()
            .left(px(76.))
            .top_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.5))
            .child(
                div()
                    .id("show-sidebar")
                    .px(px(5.))
                    .rounded(px(4.))
                    .text_color(ui(0xffffff90))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffee)))
                    .child("⇥")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .when(need > 0, |d| {
                d.child(
                    div()
                        .id("need-you")
                        .px(px(6.))
                        .rounded(px(4.))
                        .text_color(ui(0xe0af68ff))
                        .hover(|d| d.bg(ui(0xe0af6826)))
                        .child(format!("● {need} need you"))
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_attention(cx))),
                )
            })
            .when(working > 0, |d| {
                d.child(div().text_color(ui(0xffffff60)).child(format!("{working} working")))
            })
    }

    /// Sidebar style `switcher`: the current workspace (and its server's
    /// name, with several), and what needs you most in the others; a click
    /// lists them.
    fn switcher_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let active = self.layout.active;
        let workspace = self.layout.workspaces.get(active);
        let several = app_state::servers(cx).len() > 1;
        let mut title = self.layout.workspaces.get(active).map_or_else(
            || "No workspace".to_owned(),
            |_| self.workspace_title(active).to_string(),
        );
        if let Some(workspace) = workspace.filter(|_| several) {
            let server = app_state::server_name(ids::server_of(workspace.id), cx);
            title = format!("{} · {title}", server.to_uppercase());
        }
        let color = workspace.and_then(|w| w.color.as_deref()).and_then(workspace_color);
        let others = self.rollup((0..self.layout.workspaces.len()).filter(|&p| p != active), cx);
        div()
            .id("workspace-switcher")
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .mb(px(4.))
            .px_2()
            .rounded(px(7.))
            .bg(if self.switcher_open { ui(0xffffff1c) } else { ui(0xffffff0a) })
            .hover(|d| d.bg(ui(0xffffff14)))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .when_some(color, |d, color| {
                d.child(div().flex_none().size(px(7.)).rounded_full().bg(rgba(color << 8 | 0xff)))
            })
            .child(Self::one_line(title).flex_1().min_w_0().text_color(ui(0xffffffd8)))
            .when_some(others, |d, (top, count)| {
                d.child(self.icon_cell(Some(top))).when(count > 1, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(ui(0xffffff90))
                            .child(count.to_string()),
                    )
                })
            })
            .child(div().flex_none().text_size(px(9.)).text_color(ui(0xffffff70)).child("▼"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.switcher_open = !this.switcher_open;
                cx.notify();
            }))
    }

    /// The switcher's list: this window's workspaces by server, each with
    /// what needs you there; a new one; hidden ones.
    fn switcher_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if !self.switcher_open || self.sidebar_hidden {
            return None;
        }
        let servers = app_state::servers(cx);
        let several = servers.len() > 1;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (server, name, status) in servers {
            let positions: Vec<usize> = (0..self.layout.workspaces.len())
                .filter(|&p| ids::server_of(self.layout.workspaces[p].id) == server)
                .collect();
            if several {
                let dot =
                    if status == app_state::Status::Connected { 0x9ece6aff } else { 0xe0af68ff };
                rows.push(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .pt(px(6.))
                        .pb(px(2.))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ui(0xffffff70))
                        .child(div().flex_none().size(px(6.)).rounded_full().bg(ui(dot)))
                        .child(name.to_uppercase())
                        .into_any_element(),
                );
            }
            for position in positions {
                let workspace = &self.layout.workspaces[position];
                let current = position == self.layout.active;
                let color = workspace.color.as_deref().and_then(workspace_color);
                let rollup = self.rollup(std::iter::once(position), cx);
                rows.push(
                    div()
                        .id(("switch-to", position))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(26.))
                        .px(px(8.))
                        .rounded(px(6.))
                        .when(current, |d| d.bg(ui(0x3478f6ff)))
                        .when(!current, |d| d.hover(|d| d.bg(ui(0xffffff10))))
                        .child(div().flex_none().size(px(7.)).rounded_full().map(|d| match color {
                            Some(color) => d.bg(rgba(color << 8 | 0xff)),
                            None => d,
                        }))
                        .child(
                            Self::one_line(self.workspace_title(position))
                                .flex_1()
                                .min_w_0()
                                .text_color(ui(0xffffffee)),
                        )
                        .when_some(rollup, |d, (top, count)| {
                            d.child(self.icon_cell(Some(top))).when(count > 1, |d| {
                                d.child(
                                    div().flex_none().text_size(px(10.5)).child(count.to_string()),
                                )
                            })
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(ui(0xffffff60))
                                .child(workspace.tabs.len().to_string()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.switcher_open = false;
                            this.show_workspace_at(position, cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        let action = |id: &'static str, label: String| {
            div()
                .id(id)
                .px(px(8.))
                .h(px(26.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .text_color(ui(0xffffffb0))
                .hover(|d| d.bg(ui(0xffffff10)).text_color(ui(0xffffffee)))
                .child(label)
        };
        let hidden = self.layout.hidden;
        let panel = div()
            .id("switcher")
            .absolute()
            .top(px(102.))
            .left(px(8.))
            .w(px(SIDEBAR_WIDTH + 40.))
            .max_h(px(440.))
            .occlude()
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(6.))
            .rounded(px(10.))
            .bg(ui(0x262a33f8))
            .border_1()
            .border_color(ui(0xffffff26))
            .shadow_lg()
            .text_size(px(12.5))
            .child(
                div()
                    .id("switcher-rows")
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(div().my(px(4.)).h(px(1.)).bg(ui(0xffffff14)))
            .child(action("switcher-new", "New Workspace   ⌘⇧N".to_owned()).on_click(cx.listener(
                |this, _, _, cx| {
                    this.switcher_open = false;
                    this.new_workspace(cx);
                },
            )))
            .when(hidden > 0, |d| {
                let label = if hidden == 1 {
                    "1 hidden workspace…".to_owned()
                } else {
                    format!("{hidden} hidden workspaces…")
                };
                d.child(action("switcher-hidden", label).on_click(cx.listener(|this, _, _, cx| {
                    this.switcher_open = false;
                    this.open_picker(cx);
                })))
            });
        let backdrop =
            div().id("switcher-backdrop").absolute().top_0().left_0().size_full().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.switcher_open = false;
                    cx.notify();
                }),
            );
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Workspaces are sections (there's always one); a folded one still
        // shows the tab on screen.
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut index = 0;
        // With other machines' servers, each server's workspaces go under
        // its header (with nothing under it while it's away).
        let servers = app_state::servers(cx);
        let several = servers.len() > 1;
        let style = self.config.sidebar_style;
        let mut headers = servers
            .into_iter()
            .filter(|_| several && style != artty_core::SidebarStyle::Switcher)
            .peekable();
        if style == artty_core::SidebarStyle::Switcher {
            // The current workspace's tabs, under the way to the others.
            rows.push(self.switcher_button(cx).into_any_element());
        }
        for (position, workspace) in self.layout.workspaces.iter().enumerate() {
            let current = position == self.layout.active;
            if style == artty_core::SidebarStyle::Switcher {
                for tab in &workspace.tabs {
                    if current {
                        rows.push(self.tab_row(index, tab, cx).into_any_element());
                    }
                    index += 1;
                }
                continue;
            }
            let server = ids::server_of(workspace.id);
            while let Some((id, name, status)) = headers.next_if(|(id, _, _)| *id <= server) {
                rows.push(self.server_header(id, name, status, cx).into_any_element());
            }
            // Its server folded: its workspaces out of the way.
            if several && self.folded_servers.contains(&server) {
                index += workspace.tabs.len();
                continue;
            }
            // An accordion has only the current workspace open.
            let folded =
                workspace.collapsed || style == artty_core::SidebarStyle::Accordion && !current;
            rows.push(self.workspace_header(position, folded, cx).into_any_element());
            for tab in &workspace.tabs {
                if !folded || index == self.active {
                    rows.push(self.tab_row(index, tab, cx).into_any_element());
                }
                index += 1;
            }
        }
        for (id, name, status) in headers {
            rows.push(self.server_header(id, name, status, cx).into_any_element());
        }
        if self.layout.hidden > 0 {
            let hidden = self.layout.hidden;
            rows.push(
                div()
                    .id("hidden-workspaces")
                    .mt(px(8.))
                    .px_2()
                    .py(px(4.))
                    .rounded(px(6.))
                    .text_size(px(11.5))
                    .text_color(ui(0xffffff70))
                    .hover(|d| d.bg(ui(0xffffff0a)).text_color(ui(0xffffffc0)))
                    .child(if hidden == 1 {
                        "1 more workspace…".to_owned()
                    } else {
                        format!("{hidden} more workspaces…")
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx)))
                    .into_any_element(),
            );
        }
        let mismatch = cx.global::<Shared>().server_mismatch.is_some();
        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .relative()
            .bg(ui(0xffffff0a))
            .border_r_1()
            .border_color(ui(0xffffff12))
            .pt(px(34.))
            // A click elsewhere in it (another tab, say) saves a name being typed.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.renaming.is_some() {
                        this.finish_rename(true, cx);
                    }
                }),
            )
            // The titlebar goes on above the sidebar, behind the window buttons.
            .child(Self::titlebar("sidebar-titlebar").absolute().top_0().left_0().right_0().h(px(TITLEBAR_HEIGHT)))
            .child(
                div()
                    .id("hide-sidebar")
                    .absolute()
                    .top(px(7.))
                    .right(px(8.))
                    .px(px(5.))
                    .rounded(px(4.))
                    .text_size(px(11.5))
                    .text_color(ui(0xffffff70))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffee)))
                    .child("⇤")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
            )
            .px(px(8.))
            .pb(px(10.))
            .text_size(px(12.5))
            .child(
                div()
                    .id("activity")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    // A box as a tab row's.
                    .h(px(28.))
                    .mb(px(8.))
                    .px_2()
                    .rounded(px(7.))
                    .text_color(if self.showing_activity() { ui(0xffffffee) } else { ui(0xffffffb0) })
                    .when(self.showing_activity(), |d| d.bg(ui(0xffffff1c)))
                    .hover(|d| d.bg(ui(0xffffff10)))
                    .child(div().w(px(12.)).flex_none().flex().justify_center().text_color(ui(0x8fb4ffff)).child("◔"))
                    .child(div().flex_1().child("Activity"))
                    .child(div().text_xs().text_color(ui(0xffffff66)).child("⇧⌘A"))
                    .on_click(cx.listener(|this, _, _, cx| this.show_activity(cx))),
            )
            .when(!app_state::several_servers(cx), |d| d.child(
                div()
                    .id("sidebar-heading")
                    .group("sidebar-heading")
                    .flex()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .pb_2()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70))
                    .child("WORKSPACES")
                    .child(
                        div()
                            .id("new-workspace")
                            // Shown on hover, like the sections' own +.
                            .opacity(0.)
                            .group_hover("sidebar-heading", |s| s.opacity(1.))
                            .px_1()
                            .rounded(px(4.))
                            .hover(|d| d.bg(ui(0xffffff14)))
                            .text_size(px(14.))
                            .child("+")
                            .on_click(cx.listener(|this, _, _, cx| this.new_workspace(cx))),
                    ),
            ))
            .child(div().id("tab-list").flex().flex_col().flex_1().gap(px(3.)).overflow_y_scroll().children(rows))
            .when_some(self.config_error.clone(), |d, error| {
                d.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(ui(0xf7768eff))
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
    fn context_menu_overlay(
        &self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        const WIDTH: f32 = 220.;
        const ITEM: f32 = 24.;
        const SEPARATOR: f32 = 9.;
        let menu = self.context_menu.as_ref()?;
        let item =
            |label: &'static str, keys: &'static str, enabled: bool, command: MenuCommand| {
                Some((SharedString::from(label), keys, enabled, command))
            };
        let items = match menu.target {
            MenuTarget::Pane(_) => {
                let mut items = vec![
                    item("Copy", "⌘C", menu.has_selection, MenuCommand::Copy),
                    item("Paste", "⌘V", true, MenuCommand::Paste),
                    None,
                ];
                if menu.link.is_some() {
                    items.extend([
                        item("Open Link", "", true, MenuCommand::OpenLink),
                        item("Copy Link", "", true, MenuCommand::CopyLink),
                        None,
                    ]);
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
                items
            },
            MenuTarget::Server(server) => {
                let status = app_state::servers(cx)
                    .into_iter()
                    .find(|(id, _, _)| *id == server)
                    .map(|(_, _, status)| status);
                let connected = matches!(
                    status,
                    Some(app_state::Status::Connected | app_state::Status::Connecting)
                );
                let by_ssh = app_state::remote_config(server, cx).is_some_and(|r| r.ssh.is_some());
                let mut items = vec![if connected {
                    item("Disconnect", "", true, MenuCommand::Disconnect)
                } else {
                    item("Connect", "", true, MenuCommand::Connect)
                }];
                if by_ssh {
                    items.push(item(
                        "Reconnect from Scratch",
                        "",
                        true,
                        MenuCommand::ReconnectFromScratch,
                    ));
                }
                items
            },
            MenuTarget::Tab(index) => {
                // A workspace's only tab is a workspace already.
                let alone = self
                    .places
                    .get(index)
                    .and_then(|&(w, _)| self.layout.workspaces.iter().find(|ws| ws.id == w))
                    .is_some_and(|w| w.tabs.len() < 2);
                vec![
                    item("Rename Tab…", "", true, MenuCommand::RenameTab),
                    item("Move to New Workspace", "", !alone, MenuCommand::TabToNewWorkspace),
                    None,
                    item("Close Tab", "⇧⌘W", true, MenuCommand::CloseTab),
                ]
            },
            MenuTarget::Workspace(workspace) => {
                let collapsed = self
                    .layout
                    .workspaces
                    .iter()
                    .find(|w| w.id == workspace)
                    .is_some_and(|w| w.collapsed);
                let alone = self.layout.workspaces.len() < 2;
                let mut items = vec![
                    item("New Tab Here", "", true, MenuCommand::NewTabInWorkspace),
                    item("Rename Workspace…", "", true, MenuCommand::RenameWorkspace),
                    item(
                        if collapsed { "Expand" } else { "Collapse" },
                        "",
                        true,
                        MenuCommand::ToggleCollapsed,
                    ),
                    None,
                    item("Move to New Window", "", !alone, MenuCommand::MoveToNewWindow),
                ];
                for (window, title) in app_state::other_windows(self.window, cx) {
                    let label =
                        format!("Move to “{}”", if title.is_empty() { "window" } else { &title });
                    items.push(Some((label.into(), "", true, MenuCommand::MoveToWindow(window))));
                }
                let has_root = self
                    .layout
                    .workspaces
                    .iter()
                    .find(|w| w.id == workspace)
                    .is_some_and(|w| w.root.is_some());
                items.extend([
                    item("Hide Workspace", "", true, MenuCommand::HideWorkspace),
                    None,
                    item(
                        "Use This Directory as Root",
                        "",
                        true,
                        MenuCommand::RootFromCurrentDirectory,
                    ),
                ]);
                if has_root {
                    items.push(item("Clear Root", "", true, MenuCommand::ClearRoot));
                }
                items
                    .extend([None, item("Close Workspace", "", true, MenuCommand::CloseWorkspace)]);
                items
            },
        };

        // A workspace's menu ends with its colors.
        let color_row = match menu.target {
            MenuTarget::Workspace(workspace) => {
                let current = self
                    .layout
                    .workspaces
                    .iter()
                    .find(|w| w.id == workspace)
                    .and_then(|w| w.color.clone());
                let swatch = |i: usize, fill: Option<u32>, chosen: bool| {
                    div()
                        .id(("menu-color", i))
                        .size(px(16.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_2()
                        .border_color(if chosen { ui(0xffffffee) } else { ui(0x00000000) })
                        .hover(|d| d.border_color(ui(0xffffff90)))
                        .map(|d| match fill {
                            Some(color) => d.bg(rgba(color << 8 | 0xff)),
                            None => d.text_size(px(11.)).text_color(ui(0xffffff90)).child("⊘"),
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let color = (i > 0).then(|| i - 1);
                            this.run_menu_command(MenuCommand::SetColor(color), window, cx);
                        }))
                };
                let mut row = div().flex().items_center().justify_between().h(px(ITEM)).px(px(9.));
                row = row.child(swatch(0, None, current.is_none()));
                for (i, name) in WORKSPACE_COLORS.iter().enumerate() {
                    row = row.child(swatch(
                        i + 1,
                        workspace_color(name),
                        current.as_deref() == Some(*name),
                    ));
                }
                Some(row)
            },
            _ => None,
        };

        // Keep the menu on screen.
        let height = 10.
            + items.iter().map(|i| if i.is_some() { ITEM } else { SEPARATOR }).sum::<f32>()
            + if color_row.is_some() { ITEM + SEPARATOR } else { 0. };
        let (mut x, mut y) = (f32::from(menu.position.x), f32::from(menu.position.y));
        if x + WIDTH > f32::from(viewport.width) - 4. {
            x = (x - WIDTH).max(4.);
        }
        if y + height > f32::from(viewport.height) - 4. {
            y = (f32::from(viewport.height) - height - 4.).max(4.);
        }

        let rows = items.into_iter().enumerate().map(|(i, item)| match item {
            None => div().h(px(1.)).mx(px(9.)).my(px(4.)).bg(ui(0xffffff1a)).into_any_element(),
            Some((label, keys, enabled, command)) => div()
                .id(("menu-item", i))
                .flex()
                .items_center()
                .justify_between()
                .h(px(ITEM))
                .px(px(9.))
                .rounded(px(5.))
                .text_color(if enabled { ui(0xffffffee) } else { ui(0xffffff4d) })
                .when(enabled, |d| {
                    d.hover(|d| d.bg(ui(0x3478f6ff))).on_click(cx.listener(
                        move |this, _, window, cx| this.run_menu_command(command, window, cx),
                    ))
                })
                .child(label)
                .child(div().text_color(ui(0xffffff66)).child(keys))
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
            .bg(ui(0x262a33f5))
            .border_1()
            .border_color(ui(0xffffff26))
            .shadow_lg()
            .text_size(px(13.))
            .children(rows)
            .when_some(color_row, |d, row| {
                d.child(div().h(px(1.)).mx(px(9.)).my(px(4.)).bg(ui(0xffffff1a))).child(row)
            });
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }

    /// The find bar, at the top right of the focused pane. Positions are
    /// relative to the terminal area's container.
    fn search_bar(
        &self,
        origin: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let search = self.search.as_ref()?;
        let pane = self.focused_id()?;
        let rect = self.rect_of(pane)?;
        let mut query = search.query.clone();
        if let Some(marked) = &self.marked_text {
            query.push_str(marked);
        }
        // Which of how many: "3 of 17".
        let counts = search.counts.as_ref().filter(|c| c.found && !search.query.is_empty());
        let position = counts.map(|c| {
            let more = if c.more { "+" } else { "" };
            format!("{} of {}{more}", c.index, c.total)
        });
        let (status, status_color) = match (&search.error, search.found) {
            (Some(_), _) => ("invalid pattern".to_owned(), ui(0xf7768eff)),
            (None, Some(false)) => ("no match".to_owned(), ui(0xf7768eff)),
            _ => ("⏎ older · ⇧⏎ newer · esc".to_owned(), ui(0xffffff60)),
        };
        // A full-screen agent keeps its history itself, out of the
        // terminal's reach: its conversations can be searched instead.
        let agent = self.pane_state(pane, cx).and_then(|s| s.agent.clone());
        let elsewhere = agent.filter(|_| search.counts.as_ref().is_some_and(|c| !c.history));
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
                .bg(ui(0x1e2430f2))
                .border_1()
                .border_color(ui(0xffffff24))
                .shadow_md()
                .text_size(px(12.))
                .child(div().text_color(ui(0xffffff70)).child("Find"))
                .child(
                    div()
                        .min_w(px(160.))
                        .font_family(self.metrics.font.family.clone())
                        .text_color(ui(0xffffffee))
                        .child(format!("{query}▏")),
                )
                .when_some(position, |d, position| {
                    d.child(div().flex_none().text_color(ui(0xffffffd0)).child(position))
                })
                .child(div().text_color(status_color).child(status))
                .when_some(elsewhere, |d, agent| {
                    let (mark, color) = search_view::agent_mark(&agent);
                    d.child(
                        div()
                            .id("search-agent-history")
                            .flex()
                            .gap(px(4.))
                            .px(px(6.))
                            .rounded(px(4.))
                            .text_color(ui(0xffffffb0))
                            .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffee)))
                            .child(div().text_color(rgba(color << 8 | 0xff)).child(mark))
                            .child("No history here: search its conversations ⌘⇧F")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let query = this.search.as_ref().map(|s| s.query.clone());
                                this.close_search(cx);
                                this.open_search_all(cx);
                                if let (Some(query), Some(all)) = (query, this.search_all.as_mut())
                                {
                                    all.set_query(query);
                                }
                                this.search_all_now(cx);
                            })),
                    )
                }),
        )
    }

    /// Hit areas for dragging dividers. Positions are relative to the
    /// terminal area's container at `origin`.
    fn divider_handles(
        &self,
        origin: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement> {
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
        if self.detach {
            let windows = self.server_windows();
            DETACHED.with(|detached| detached.borrow_mut().extend(windows));
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
    let rect =
        Bounds::new(point(px(saved.x), px(saved.y)), size(px(saved.width), px(saved.height)));
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
        // A light theme: the app's own colors too.
        crate::chrome::follow(self.theme.background);
        if self.reload_config {
            self.apply_config(window);
        }
        self.area = Self::area_rect(window, self.sidebar_width());
        self.window_active = window.is_window_active();
        if self.scale_factor != window.scale_factor() {
            // Another display: tell programs the new pixel sizes.
            self.scale_factor = window.scale_factor();
            self.sizes.clear();
        }
        if self.window.is_some() {
            let bounds = saved_window(window.window_bounds());
            if self.bounds_sent != Some(bounds) {
                self.bounds_sent = Some(bounds);
                for id in self.server_windows() {
                    self.send(ClientMessage::WindowBounds { window: id, bounds }, cx);
                }
            }
        }

        // Lay out the active tab and keep the server's pane sizes in step.
        let (rects, dividers) =
            self.tabs.get(self.active).map(|tab| layout_of(tab, self.area)).unwrap_or_default();
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
                let link = self
                    .hover_link
                    .as_ref()
                    .filter(|(pane, ..)| *pane == id)
                    .map(|(_, line, span)| (*line, span.clone()));
                let flash = shared.flashes.get(&id).map_or(0., |at| {
                    1. - (at.elapsed().as_secs_f32() / FLASH.as_secs_f32()).min(1.)
                });
                let mut frame = screen.frame.clone();
                if let Some(echo) = shared.echoes.get(&id) {
                    echo.overlay(&mut frame);
                }
                Some(PaneView { rect, frame, focused: Some(id) == focused_id, link, flash })
            })
            .collect();
        self.cursor = views.iter().find(|view| view.focused).and_then(|view| view.frame.cursor);

        let window_focused = self.focus.is_focused(window) && self.window_active;
        let typing = window_focused
            && self.search.is_none()
            && self.renaming.is_none()
            && self.picker.is_none()
            && !self.showing_activity();
        self.blink_enabled =
            typing && (self.config.cursor_blink || self.cursor.is_some_and(|c| c.blinking));

        let title = match self.showing_activity() {
            true => "Activity".into(),
            false => {
                self.tabs.get(self.active).map(|tab| self.tab_title(tab, cx)).unwrap_or_default()
            },
        };
        window.set_window_title(&title);

        let tint =
            Hsla { a: self.config.background_opacity.clamp(0., 1.), ..hsla(self.theme.background) };
        // A tab typing into every pane: amber dividers, no pane dimmed.
        let broadcast = self.tabs.get(self.active).is_some_and(|t| t.broadcast);
        let divider_color: Hsla =
            if broadcast { ui(0xe0af68cc).into() } else { ui(0xffffff1c).into() };
        let metrics = self.metrics.clone();
        let marked_text = self
            .marked_text
            .clone()
            .filter(|_| self.search.is_none() && self.renaming.is_none() && self.picker.is_none());
        let cursor_visible = self.cursor_visible;
        let focused_rect = focused_id.and_then(|id| self.rect_of(id));
        let divider_lines: Vec<Bounds<Pixels>> = self
            .dividers
            .iter()
            .map(|divider| {
                let r = divider.rect;
                match divider.axis {
                    Axis::Horizontal => {
                        bounds(Rect { x: r.x + (r.width / 2.).floor(), width: 1., ..r })
                    },
                    Axis::Vertical => {
                        bounds(Rect { y: r.y + (r.height / 2.).floor(), height: 1., ..r })
                    },
                }
            })
            .collect();
        let focus = self.focus.clone();
        let entity = cx.entity();
        // The terminal container starts right of the sidebar, below the title bar.
        let container = point(px(self.sidebar_width()), px(TITLEBAR_HEIGHT));

        div()
            .track_focus(&self.focus)
            .key_context("WindowView")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.open_tab(cx)))
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| this.new_workspace(cx)))
            .on_action(cx.listener(|this, _: &OpenWorkspace, _, cx| this.open_picker(cx)))
            .on_action(cx.listener(|this, _: &ShowActivity, _, cx| this.toggle_activity(cx)))
            .on_action(cx.listener(|this, _: &ToggleFiles, _, cx| this.toggle_files(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(
                cx.listener(|this, _: &GoTo, window, cx| this.open_palette(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SearchEverything, _, cx| this.open_search_all(cx)))
            .on_action(
                cx.listener(|this, _: &RunCommand, window, cx| this.open_palette(true, window, cx)),
            )
            .on_action(cx.listener(|this, action: &SendText, _, cx| {
                if !this.showing_activity() {
                    this.input(action.text.clone().into_bytes(), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextWorkspace, _, cx| this.cycle_workspace(1, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousWorkspace, _, cx| this.cycle_workspace(-1, cx)),
            )
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
            .on_action(
                cx.listener(|this, _: &SplitRight, _, cx| this.split(None, Axis::Horizontal, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SplitDown, _, cx| this.split(None, Axis::Vertical, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusLeft, _, cx| this.focus_direction(Direction::Left, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusRight, _, cx| {
                    this.focus_direction(Direction::Right, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &FocusUp, _, cx| this.focus_direction(Direction::Up, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusDown, _, cx| this.focus_direction(Direction::Down, cx)),
            )
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
            .on_action(
                cx.listener(|this, _: &JumpToPreviousPrompt, _, cx| this.jump_to_prompt(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &JumpToNextPrompt, _, cx| this.jump_to_prompt(false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CopyCommandOutput, _, cx| this.copy_command_output(cx)),
            )
            .on_action(cx.listener(|this, _: &CopyMode, _, cx| this.toggle_copy_mode(cx)))
            .on_action(
                cx.listener(|this, _: &ResizePaneLeft, _, cx| {
                    this.resize_pane(Direction::Left, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ResizePaneRight, _, cx| {
                this.resize_pane(Direction::Right, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ResizePaneUp, _, cx| this.resize_pane(Direction::Up, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ResizePaneDown, _, cx| {
                    this.resize_pane(Direction::Down, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SwapPaneLeft, _, cx| this.swap_pane(Direction::Left, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SwapPaneRight, _, cx| this.swap_pane(Direction::Right, cx)),
            )
            .on_action(cx.listener(|this, _: &SwapPaneUp, _, cx| this.swap_pane(Direction::Up, cx)))
            .on_action(
                cx.listener(|this, _: &SwapPaneDown, _, cx| this.swap_pane(Direction::Down, cx)),
            )
            .on_action(cx.listener(|this, _: &EqualizePanes, _, cx| this.equalize_panes(cx)))
            .on_action(cx.listener(|this, _: &PaneToNewTab, _, cx| this.pane_to_new_tab(cx)))
            .on_action(cx.listener(|this, _: &ToggleBroadcast, _, cx| this.toggle_broadcast(cx)))
            .on_action(cx.listener(|this, _: &ReopenClosedTab, _, cx| this.reopen_closed_tab(cx)))
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
            .on_modifiers_changed(cx.listener(
                |this, event: &gpui::ModifiersChangedEvent, _, cx| {
                    if this.command_held != event.modifiers.platform {
                        this.command_held = event.modifiers.platform;
                        cx.notify();
                    }
                    this.update_hover_link(event.modifiers.platform, cx);
                },
            ))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .relative()
            .flex()
            .size_full()
            .bg(tint)
            .font_family(".SystemUIFont")
            .when(!self.sidebar_hidden, |d| d.child(self.sidebar(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Self::titlebar("titlebar")
                            .relative()
                            .flex()
                            .flex_none()
                            .h(px(TITLEBAR_HEIGHT))
                            .px_4()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(ui(0xffffffb8))
                            // Folded away: the window's buttons are here, then
                            // the sidebar's way back and what it would show.
                            .when(self.sidebar_hidden, |d| d.pl(px(170.)).pr(px(170.)))
                            .children(self.sidebar_hidden.then(|| self.folded_sidebar_bar(cx)))
                            .child(Self::one_line(title).min_w_0()),
                    )
                    .when(self.showing_activity(), |d| {
                        d.child(div().flex_1().min_h_0().child(self.activity_view(cx)))
                    })
                    .when(!self.showing_activity(), |d| {
                        d.child(
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
                                                window.handle_input(
                                                    &focus,
                                                    ElementInputHandler::new(bounds(rect), entity),
                                                    cx,
                                                );
                                            }
                                            for view in &views {
                                                let state = terminal::PaintState {
                                                    focused: view.focused && typing,
                                                    cursor_visible,
                                                    marked_text: marked_text
                                                        .as_deref()
                                                        .filter(|_| view.focused),
                                                    fade: if split && !view.focused && !broadcast {
                                                        UNFOCUSED_FADE
                                                    } else {
                                                        0.
                                                    },
                                                    link: view.link.clone(),
                                                    flash: view.flash,
                                                };
                                                terminal::paint(
                                                    bounds(view.rect),
                                                    &view.frame,
                                                    &metrics,
                                                    state,
                                                    window,
                                                    cx,
                                                );
                                            }
                                            for line in &divider_lines {
                                                window.paint_quad(fill(*line, divider_color));
                                            }
                                            if crate::profile::enabled() {
                                                crate::profile::record(
                                                    build_time,
                                                    paint_started.elapsed(),
                                                );
                                            }
                                        },
                                    )
                                    .size_full(),
                                )
                                .children(self.divider_handles(container, cx))
                                .children(self.search_bar(container, cx))
                                .children(self.copy_mode_badge())
                                .children(self.files_drawer(cx))
                                .children(self.toast_overlay())
                                .children(self.pending_keys.clone().map(|keys| {
                                    div()
                                        .absolute()
                                        .bottom(px(16.))
                                        .right(px(16.))
                                        .px_3()
                                        .py(px(4.))
                                        .rounded(px(8.))
                                        .bg(ui(0x1e2430f2))
                                        .border_1()
                                        .border_color(ui(0x8fb4ff66))
                                        .text_size(px(12.))
                                        .text_color(ui(0xffffffd0))
                                        .child(format!("{keys} …"))
                                }))
                                .on_drop(cx.listener(
                                    |this, paths: &gpui::ExternalPaths, window, cx| {
                                        this.drop_paths(paths, window, cx)
                                    },
                                )),
                        )
                    }),
            )
            .children(self.context_menu_overlay(window.viewport_size(), cx))
            .children(self.picker_overlay(cx))
            .children(self.palette_overlay(cx))
            .children(self.search_all_overlay(cx))
            .children(self.switcher_overlay(cx))
            .children(self.ask_overlay(cx))
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
        let marked: Vec<u16> =
            self.marked_text.as_deref().unwrap_or_default().encode_utf16().collect();
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

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
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
        if self.asking() {
            self.ask_text(text, cx);
            return;
        }
        if self.palette.is_some() {
            self.palette_text(text, cx);
            return;
        }
        if self.search_all.is_some() {
            self.search_all_text(text, cx);
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            picker.query.push_str(text);
            picker.selected = 0;
            cx.notify();
            return;
        }
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
        if self.in_copy_mode(cx) {
            self.copy_mode_text(text, cx);
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
        KeyBinding::new("cmd-shift-n", NewWorkspace, None),
        KeyBinding::new("cmd-shift-o", OpenWorkspace, None),
        KeyBinding::new("cmd-shift-a", ShowActivity, None),
        KeyBinding::new("cmd-shift-e", ToggleFiles, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-p", GoTo, None),
        KeyBinding::new("cmd-shift-p", RunCommand, None),
        KeyBinding::new("cmd-shift-f", SearchEverything, None),
        KeyBinding::new("ctrl-cmd-]", NextWorkspace, None),
        KeyBinding::new("ctrl-cmd-[", PreviousWorkspace, None),
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
        KeyBinding::new("cmd-up", JumpToPreviousPrompt, None),
        KeyBinding::new("cmd-down", JumpToNextPrompt, None),
        KeyBinding::new("cmd-alt-c", CopyCommandOutput, None),
        KeyBinding::new("cmd-shift-c", CopyMode, None),
        KeyBinding::new("ctrl-cmd-left", ResizePaneLeft, None),
        KeyBinding::new("ctrl-cmd-right", ResizePaneRight, None),
        KeyBinding::new("ctrl-cmd-up", ResizePaneUp, None),
        KeyBinding::new("ctrl-cmd-down", ResizePaneDown, None),
        KeyBinding::new("ctrl-cmd-shift-left", SwapPaneLeft, None),
        KeyBinding::new("ctrl-cmd-shift-right", SwapPaneRight, None),
        KeyBinding::new("ctrl-cmd-shift-up", SwapPaneUp, None),
        KeyBinding::new("ctrl-cmd-shift-down", SwapPaneDown, None),
        KeyBinding::new("ctrl-cmd-=", EqualizePanes, None),
        KeyBinding::new("cmd-alt-i", ToggleBroadcast, None),
        KeyBinding::new("cmd-shift-t", ReopenClosedTab, None),
    ]);
}
