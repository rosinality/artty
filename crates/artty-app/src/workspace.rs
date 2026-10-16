//! The main window: a sidebar of tabs, each running a terminal session.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use artty_core::vt::event::Event;
use artty_core::vt::index::Side;
use artty_core::vt::selection::SelectionType;
use artty_core::vt::term::TermMode;
use artty_core::{
    Config, FrameCursor, Notice, Session, SessionId, SessionOptions, TerminalSize, Theme,
};
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontWeight, Hsla, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    UTF16Selection, Window, actions, canvas, div, point, prelude::*, px, rgba,
};

use crate::keys;
use crate::mouse::{self, Action, Button};
use crate::terminal::{self, Metrics, hsla};

actions!(
    artty,
    [
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        Copy,
        Paste,
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
const MIN_FONT_SIZE: f32 = 6.;
const MAX_FONT_SIZE: f32 = 72.;

struct Tab {
    session: Session,
    title: Option<String>,
    /// The program rang the bell while this tab was in the background.
    bell: bool,
}

impl Tab {
    fn title(&self) -> SharedString {
        self.title.clone().unwrap_or_else(|| "shell".to_owned()).into()
    }
}

pub struct Workspace {
    focus: FocusHandle,
    config: Arc<Config>,
    config_error: Option<String>,
    theme: Arc<Theme>,
    font_size: f32,
    metrics: Metrics,
    tabs: Vec<Tab>,
    active: usize,
    next_id: SessionId,
    sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    /// IME composition in progress, shown at the cursor until committed.
    marked_text: Option<String>,
    /// Cursor position from the last render, for placing the IME window.
    cursor: Option<FrameCursor>,
    cursor_visible: bool,
    /// Whether the cursor should blink, as of the last render.
    blink_enabled: bool,
    /// Skip one blink so the cursor stays solid while typing.
    blink_hold: bool,
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
        let blink = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                if this.update(cx, |this, cx| this.blink(cx)).is_err() {
                    break;
                }
            }
        });
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.report_focus(window.is_window_active());
            cx.notify();
        });

        if let Some(error) = &config_error {
            log::error!("config: {error}");
        }

        let focus = cx.focus_handle();
        focus.focus(window);
        let font_size = config.font_size;
        let mut this = Self {
            focus,
            theme: Arc::new(config.theme()),
            metrics: Metrics::measure(window, &config, font_size),
            font_size,
            config: Arc::new(config),
            config_error,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            sink,
            marked_text: None,
            cursor: None,
            cursor_visible: true,
            blink_enabled: false,
            blink_hold: false,
            selecting: false,
            reporting_button: None,
            last_mouse_cell: None,
            scroll_remainder: 0.,
            _tasks: vec![events, blink],
            _subscriptions: vec![activation],
        };
        this.open_tab(command, window, cx);
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

    fn open_tab(&mut self, command: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        let command = command.or_else(|| Some(self.config.shell.clone())).filter(|c| !c.is_empty());
        let options = SessionOptions {
            command: command.map(|mut c| (c.remove(0), c)),
            scrollback: Some(self.config.scrollback),
            ..Default::default()
        };
        let size = self.terminal_size(window);
        match Session::spawn(id, options, size, self.theme.clone(), self.sink.clone()) {
            Ok(session) => {
                self.tabs.push(Tab { session, title: None, bell: false });
                self.select(self.tabs.len() - 1, cx);
            }
            Err(err) => log::error!("failed to start shell: {err}"),
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.bell = false;
            self.active = index;
            self.marked_text = None;
            self.selecting = false;
            self.reporting_button = None;
            cx.notify();
        }
    }

    fn close(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.session.id() == id) else { return };
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            cx.quit();
            return;
        }
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        cx.notify();
    }

    fn handle_events(&mut self, events: Vec<(SessionId, Event)>, cx: &mut Context<Self>) {
        for (id, event) in events {
            let Some(index) = self.tabs.iter().position(|tab| tab.session.id() == id) else { continue };
            let Some(notice) = self.tabs[index].session.handle(event) else { continue };
            match notice {
                Notice::Redraw => {}
                Notice::Title(title) => self.tabs[index].title = Some(title),
                Notice::ResetTitle => self.tabs[index].title = None,
                Notice::Bell => {
                    if index != self.active {
                        self.tabs[index].bell = true;
                    }
                }
                Notice::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Notice::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
                    self.tabs[index].session.reply_clipboard(&format, &text);
                }
                Notice::Exited => {
                    self.close(id, cx);
                    continue;
                }
            }
            cx.notify();
        }
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

    fn report_focus(&self, focused: bool) {
        if let Some(session) = self.active_session()
            && session.mode().contains(TermMode::FOCUS_IN_OUT)
        {
            session.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
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
        let Some(session) = self.active_session() else { return };
        if let Some(bytes) = keys::to_esc(keystroke, session.mode(), self.config.option_as_meta) {
            self.input(bytes);
            cx.stop_propagation();
        }
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

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.tabs.iter().enumerate().map(|(i, tab)| {
            let selected = i == self.active;
            let label: SharedString = if i < 9 { format!("⌘{}", i + 1).into() } else { "".into() };
            div()
                .id(i)
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_2()
                .rounded(px(7.))
                .when(selected, |d| d.bg(rgba(0xffffff1c)))
                .hover(|d| d.bg(rgba(0xffffff10)))
                .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx)))
                .child(
                    // GPUI 0.2 caches the first measurement of `nowrap` text, which
                    // happens before the width is known, so `.truncate()` only clips.
                    // A one-line clamp re-measures at the real width and ellipsizes.
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .line_clamp(1)
                        .text_ellipsis()
                        .text_color(if selected { rgba(0xffffffee) } else { rgba(0xffffffb0) })
                        .child(tab.title()),
                )
                .when(tab.bell, |d| d.child(div().size(px(6.)).rounded_full().bg(rgba(0xe0af68ff))))
                .child(
                    div()
                        .w(px(TAB_LABEL_WIDTH))
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
                            .on_click(cx.listener(|this, _, window, cx| this.open_tab(None, window, cx))),
                    ),
            )
            .child(div().flex().flex_col().flex_1().gap(px(3.)).children(rows))
            .when_some(self.config_error.clone(), |d, error| {
                d.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(rgba(0xf7768eff))
                        .child(format!("Config error — using defaults. {error}")),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.terminal_size(window);
        for tab in &mut self.tabs {
            tab.session.resize(size);
        }
        let frame = self.active_session().map(Session::frame);
        self.cursor = frame.as_ref().and_then(|frame| frame.cursor);

        let focused = self.focus.is_focused(window) && window.is_window_active();
        self.blink_enabled =
            focused && (self.config.cursor_blink || self.cursor.is_some_and(|c| c.blinking));

        let title = self.tabs.get(self.active).map(Tab::title).unwrap_or_default();
        window.set_window_title(&title);

        let tint = Hsla { a: self.config.background_opacity.clamp(0., 1.), ..hsla(self.theme.background) };
        let metrics = self.metrics.clone();
        let marked_text = self.marked_text.clone();
        let cursor_visible = self.cursor_visible;
        let focus = self.focus.clone();
        let entity = cx.entity();

        div()
            .track_focus(&self.focus)
            .key_context("Workspace")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.open_tab(None, window, cx)))
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
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
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
                            ),
                    ),
            )
    }
}

/// IME support: composition text is held here and only sent to the program
/// once committed, so typing Hangul or kana works as in any Mac app.
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
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-=", IncreaseFontSize, None),
        KeyBinding::new("cmd-+", IncreaseFontSize, None),
        KeyBinding::new("cmd--", DecreaseFontSize, None),
        KeyBinding::new("cmd-0", ResetFontSize, None),
    ]);
}
