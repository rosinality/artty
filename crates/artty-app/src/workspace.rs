//! The main window: a sidebar of tabs, each running a terminal session.

use std::ops::Range;
use std::sync::Arc;

use artty_core::vt::event::Event;
use artty_core::vt::term::TermMode;
use artty_core::{FrameCursor, Notice, Session, SessionId, SessionOptions, TerminalSize, Theme};
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    FontWeight, Hsla, KeyDownEvent, MouseButton, Pixels, ScrollDelta, ScrollWheelEvent,
    SharedString, Task, UTF16Selection, Window, actions, canvas, div, prelude::*, px, rgba,
};

use crate::keys;
use crate::terminal::{self, Metrics, hsla};

actions!(artty, [NewTab, CloseTab, NextTab, PreviousTab, Paste]);

pub const SIDEBAR_WIDTH: f32 = 232.;
const TAB_LABEL_WIDTH: f32 = 28.;
const TITLEBAR_HEIGHT: f32 = 32.;
const PADDING: f32 = 12.;
const BACKGROUND_ALPHA: f32 = 0.72;

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
    theme: Arc<Theme>,
    metrics: Metrics,
    tabs: Vec<Tab>,
    active: usize,
    next_id: SessionId,
    sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    /// IME composition in progress, shown at the cursor until committed.
    marked_text: Option<String>,
    /// Cursor position from the last render, for placing the IME window.
    cursor: Option<FrameCursor>,
    /// Sub-line remainder of trackpad scrolling.
    scroll_remainder: f32,
    _events: Task<()>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        let focus = cx.focus_handle();
        focus.focus(window);
        let mut this = Self {
            focus,
            theme: Arc::new(Theme::default()),
            metrics: Metrics::measure(window),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            sink,
            marked_text: None,
            cursor: None,
            scroll_remainder: 0.,
            _events: events,
        };
        this.new_tab(window, cx);
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

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        let size = self.terminal_size(window);
        match Session::spawn(id, SessionOptions::default(), size, self.theme.clone(), self.sink.clone()) {
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
        if let Some(bytes) = keys::to_esc(keystroke, session.mode()) {
            session.write(bytes);
            cx.stop_propagation();
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
        let Some(session) = self.active_session() else { return };
        if lines == 0 {
            return;
        }
        let mode = session.mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs (less, vim) get arrow keys instead.
            let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA".as_slice(),
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            session.write(key.repeat(lines.unsigned_abs() as usize));
        } else {
            session.scroll(lines);
        }
        cx.notify();
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
        session.write(bytes.into_bytes());
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
                            .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
                    ),
            )
            .child(div().flex().flex_col().gap(px(3.)).children(rows))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.terminal_size(window);
        let frame = self.tabs.get_mut(self.active).map(|tab| {
            tab.session.resize(size);
            tab.session.frame()
        });
        self.cursor = frame.as_ref().and_then(|frame| frame.cursor);

        let title = self.tabs.get(self.active).map(Tab::title).unwrap_or_default();
        window.set_window_title(&title);

        let tint = Hsla { a: BACKGROUND_ALPHA, ..hsla(self.theme.background) };
        let focused = self.focus.is_focused(window);
        let metrics = self.metrics.clone();
        let marked_text = self.marked_text.clone();
        let focus = self.focus.clone();
        let entity = cx.entity();

        div()
            .track_focus(&self.focus)
            .key_context("Workspace")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
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
            .on_action(cx.listener(Self::paste))
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
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, _| this.focus.focus(window)))
                            .on_scroll_wheel(cx.listener(Self::scroll))
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, window, cx| {
                                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                                        if let Some(frame) = frame {
                                            terminal::paint(bounds, &frame, &metrics, focused, marked_text.as_deref(), window, cx);
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
        if let Some(session) = self.active_session() {
            session.write(text.as_bytes().to_vec());
        }
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
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-t", NewTab, None),
        gpui::KeyBinding::new("cmd-w", CloseTab, None),
        gpui::KeyBinding::new("cmd-shift-]", NextTab, None),
        gpui::KeyBinding::new("cmd-shift-[", PreviousTab, None),
        gpui::KeyBinding::new("cmd-v", Paste, None),
    ]);
}
