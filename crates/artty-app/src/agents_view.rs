//! The Agents view: every agent on every connected server, the chosen one's
//! thread (what it was given and said, its permission requests, its
//! sessions), and what's known about it. Refreshed while on screen; like
//! Activity, it belongs to no workspace and runs nothing itself. Its
//! terminal is a click away.

use artty_core::control::{AgentInfo, AgentState, ApprovalState, ThreadEntry, ThreadItem};
use artty_core::protocol::QueueKind;

use super::*;

/// Thread entries shown, at most (the latest).
const THREAD: usize = 400;
/// Characters of a long message shown.
const LONG: usize = 6000;

const WAITING: u32 = 0xe0af68ff;
/// How long an entry searched for stands out.
const FLASH_FOUND: Duration = Duration::from_secs(3);
const DONE: u32 = 0x9ece6aff;
const BUSY: u32 = 0x8fb4ffff;
const DENY: u32 = 0xf7768eff;

/// Where agents are shown: the Agents view, or the thread panel beside the
/// terminal (see `thread_panel`), which follows the focused pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    View,
    Panel,
}

/// Text selected in a message (offsets in its text as drawn), while
/// dragging or after.
pub(super) struct TextSel {
    pub message: String,
    pub anchor: usize,
    pub head: usize,
    pub dragging: bool,
}

impl TextSel {
    fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

#[derive(Default)]
pub(super) struct Agents {
    /// Every connected server's agents, pane ids tagged (see `ids`), with
    /// their server's name.
    pub(super) agents: Vec<(AgentInfo, String)>,
    /// The agent shown (its pane), and its thread.
    pub(super) selected: Option<PaneId>,
    /// The chosen agent's asks not yet over (some still waiting to be
    /// given to it).
    pub(super) asks: Vec<artty_core::control::AskInfo>,
    pub(super) thread: Vec<ThreadEntry>,
    pub(super) thread_of: Option<PaneId>,
    /// Edited files opened to show their changes: (entry's seq, file).
    expanded: HashSet<(u64, usize)>,
    /// Each message's pieces of text, as last drawn (by its id), to find
    /// what's under the pointer; and what's selected in one, if anything.
    pub(super) pieces: std::cell::RefCell<HashMap<String, crate::markdown::Pieces>>,
    pub(super) text_sel: Option<TextSel>,
    /// Messages as markdown, parsed once (by their text's hash).
    parsed: std::cell::RefCell<HashMap<u64, std::rc::Rc<Vec<crate::markdown::Block>>>>,
    /// The chosen agent's terminal shown, live (else folded away, and not
    /// sent).
    pub(super) peek_open: bool,
    /// An entry to scroll to once the thread's in (one searched for), and
    /// the one shown standing out, since when.
    reveal: Option<u64>,
    flash: Option<(u64, Instant)>,
    scroll: gpui::ScrollHandle,
    loading: bool,
    pub(super) loaded: bool,
}

fn state_label(state: AgentState) -> (&'static str, u32) {
    match state {
        AgentState::Working => ("working", BUSY),
        AgentState::Waiting => ("waiting for you", WAITING),
        AgentState::Done => ("done", DONE),
        AgentState::Idle => ("idle", 0xffffff70),
    }
}

/// A Unix time in milliseconds as local "HH:MM".
fn clock(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: `tm` is written by localtime_r before it's read.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn clip(text: &str) -> String {
    let mut short: String = text.chars().take(LONG).collect();
    if short.len() < text.len() {
        short.push_str("\n…");
    }
    short
}

impl WindowView {
    pub(super) fn showing_agents(&self) -> bool {
        self.agents.is_some()
    }

    /// Show Agents, the agent of the tab you're on chosen (else the first);
    /// or leave it (see `leave_agents`).
    pub(super) fn toggle_agents(&mut self, cx: &mut Context<Self>) {
        if self.showing_agents() {
            return self.leave_agents(cx);
        }
        self.close_search(cx);
        self.picker = None;
        self.activity = None;
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        self.agents = Some(Agents { selected: self.focused_id(), ..Agents::default() });
        self.refresh_in(Shown::View, cx);
        cx.notify();
    }

    /// Back to the tabs, at the tab of the agent you were looking at.
    pub(super) fn leave_agents(&mut self, cx: &mut Context<Self>) {
        let Some(agents) = self.agents.take() else { return };
        if let Some(pane) =
            agents.selected.filter(|p| agents.agents.iter().any(|(a, _)| a.pane == *p))
        {
            self.open_queue_pane(pane, cx);
        }
        cx.notify();
    }

    /// Where an agent goes in the list: as its tab is in this window's
    /// sidebar (workspace, tab, pane), then those elsewhere (another
    /// window's, another machine's) by machine and workspace.
    fn agent_order(&self, agent: &AgentInfo, server: &str) -> (u8, usize, usize, usize, String) {
        for (w, workspace) in self.layout.workspaces.iter().enumerate() {
            for (t, tab) in workspace.tabs.iter().enumerate() {
                if let Some(p) = tab.root.panes().iter().position(|&p| p == agent.pane) {
                    return (0, w, t, p, String::new());
                }
            }
        }
        (1, 0, 0, agent.pane as usize, format!("{server}\u{0}{}", agent.workspace))
    }

    /// The chosen agent's terminal, live, while Agents shows it: its
    /// server sends its screens though no window shows it (`peek`).
    pub(super) fn follow_peek(&mut self, cx: &mut Context<Self>) {
        let want = self.agents.as_ref().filter(|a| a.peek_open).and_then(|a| a.selected);
        // Still asked of the same connection (a new one forgot it).
        let current = self.peeking.filter(|&(pane, connection)| {
            app_state::connection(ids::server_of(pane), cx) == Some(connection)
        });
        if want == current.map(|(pane, _)| pane) {
            return;
        }
        if let Some((old, _)) = self.peeking.take()
            && Some(old) != want
        {
            app_state::peek(ids::server_of(old), None, cx);
        }
        self.peeking = want.and_then(|pane| {
            let connection = app_state::peek(ids::server_of(pane), Some(ids::local_id(pane)), cx)?;
            Some((pane, connection))
        });
    }

    /// A message's line over it (who, when), and on hover, Copy: the
    /// message as written (its markdown).
    fn copyable(&self, label: String, text: &str, seq: u64, cx: &mut Context<Self>) -> gpui::Div {
        let text = text.to_owned();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.))
            .text_color(ui(0xffffff60))
            .child(label)
            .child(
                div()
                    .id(SharedString::from(format!("copy-{seq}")))
                    .px(px(5.))
                    .rounded(px(4.))
                    .opacity(0.)
                    .group_hover(SharedString::from(format!("entry-{seq}")), |s| s.opacity(1.))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                    .child("Copy")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                        this.show_toast("Copied", cx);
                    })),
            )
    }

    /// What's selected in a message shown (Agents, or the panel), as it
    /// reads.
    pub(super) fn selected_text(&self) -> Option<String> {
        [Shown::View, Shown::Panel].into_iter().find_map(|which| {
            let agents = self.agents_in(which)?;
            let sel = agents.text_sel.as_ref().filter(|s| s.anchor != s.head)?;
            let pieces = agents.pieces.borrow().get(&sel.message)?.clone();
            Some(pieces.text(sel.range())).filter(|t| !t.is_empty())
        })
    }

    /// The pointer pressed in a message: a selection starts there (a
    /// double click: its word; a triple: all of it).
    fn press_text(
        &mut self,
        which: Shown,
        message: &str,
        pieces: &crate::markdown::Pieces,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = pieces.offset_at(event.position) else { return };
        let (anchor, head) = match event.click_count {
            1 => (at, at),
            2 => {
                let text = pieces.text(0..pieces.len());
                word_at(&text, at)
            },
            _ => (0, pieces.len()),
        };
        if let Some(agents) = self.agents_in_mut(which) {
            let message = message.to_owned();
            agents.text_sel = Some(TextSel { message, anchor, head, dragging: true });
        }
        cx.notify();
    }

    /// A message, drawn as markdown (`id`: unique in the thread), its text
    /// selectable.
    fn markdown(
        &self,
        agents: &Agents,
        text: &str,
        id: &str,
        which: Shown,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let key = hasher.finish();
        let blocks = {
            let mut parsed = agents.parsed.borrow_mut();
            // (A long-running view: start over now and then.)
            if parsed.len() > 2000 {
                parsed.clear();
            }
            parsed
                .entry(key)
                .or_insert_with(|| std::rc::Rc::new(crate::markdown::parse(&clip(text))))
                .clone()
        };
        let look = crate::markdown::Look {
            text: ui(0xffffffe0).into(),
            dim: ui(0xffffff90).into(),
            link: ui(0x8fb4ffff).into(),
            code_bg: ui(0x00000040).into(),
            rule: ui(0xffffff22).into(),
            font: ".SystemUIFont".into(),
            mono: self.metrics.font.family.clone(),
            selection: ui(0x5c8ae6aa).into(),
        };
        let pieces = agents.pieces.borrow_mut().entry(id.to_owned()).or_default().clone();
        let selected = agents
            .text_sel
            .as_ref()
            .filter(|s| s.message == id && s.anchor != s.head)
            .map(TextSel::range);
        let view = crate::markdown::view(&blocks, &look, &format!("md-{id}"), &pieces, selected);
        let message = id.to_owned();
        let dragging = agents.text_sel.as_ref().is_some_and(|s| s.dragging && s.message == message);
        let down = pieces.clone();
        let here = message.clone();
        let entity = cx.entity();
        div()
            .id(SharedString::from(format!("msg-{id}")))
            .cursor_text()
            .child(view)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.press_text(which, &here, &down, event, cx);
                    cx.stop_propagation();
                }),
            )
            // While dragging in it: the pointer anywhere in the window moves
            // the selection's end, and letting go ends it.
            .when(dragging, |d| {
                d.child(
                    canvas(
                        |_, _, _| {},
                        move |_, _, window, _| {
                            let (moved, up) = (entity.clone(), entity);
                            let pieces = pieces.clone();
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                                if !phase.bubble() {
                                    return;
                                }
                                moved.update(cx, |this, cx| {
                                    let Some(sel) = this
                                        .agents_in_mut(which)
                                        .and_then(|a| a.text_sel.as_mut())
                                        .filter(|s| s.dragging)
                                    else {
                                        return;
                                    };
                                    if let Some(at) = pieces.offset_at(event.position)
                                        && sel.head != at
                                    {
                                        sel.head = at;
                                        cx.notify();
                                    }
                                });
                            });
                            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                                if !phase.bubble() {
                                    return;
                                }
                                up.update(cx, |this, cx| {
                                    if let Some(sel) =
                                        this.agents_in_mut(which).and_then(|a| a.text_sel.as_mut())
                                    {
                                        sel.dragging = false;
                                        cx.notify();
                                    }
                                });
                            });
                        },
                    )
                    .absolute()
                    .size_0(),
                )
            })
    }

    /// Show (or fold away) the chosen agent's terminal.
    fn toggle_peek(&mut self, cx: &mut Context<Self>) {
        if let Some(agents) = self.agents.as_mut() {
            agents.peek_open = !agents.peek_open;
        }
        cx.notify();
    }

    /// The agent's terminal as it is now, shrunk to `width` (not grown).
    fn terminal_peek(
        &self,
        pane: PaneId,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let frame = app_state::screen_of(pane, cx)?.frame.clone();
        if frame.columns == 0 || frame.lines.is_empty() {
            return None;
        }
        let natural = frame.columns as f32 * f32::from(self.metrics.cell_width);
        let scale = (width / natural).min(1.);
        let mut metrics = self.metrics.clone();
        metrics.font_size *= scale;
        metrics.cell_width *= scale;
        metrics.line_height *= scale;
        let height = frame.lines.len() as f32 * f32::from(metrics.line_height);
        let background = crate::terminal::hsla(frame.background);
        Some(
            div()
                .id(SharedString::from(format!("peek-{pane}")))
                .w(px(width))
                .h(px(height))
                .flex_none()
                .rounded(px(6.))
                .overflow_hidden()
                .bg(background)
                .border_1()
                .border_color(ui(0xffffff1a))
                .cursor_pointer()
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, cx| {
                            let state = crate::terminal::PaintState {
                                focused: false,
                                cursor_visible: false,
                                marked_text: None,
                                fade: 0.,
                                link: None,
                                flash: 0.,
                            };
                            crate::terminal::paint(bounds, &frame, &metrics, state, window, cx);
                        },
                    )
                    .size_full(),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.show_agent_terminal(pane, cx))),
        )
    }

    pub(super) fn agents_in(&self, which: Shown) -> Option<&Agents> {
        match which {
            Shown::View => self.agents.as_ref(),
            Shown::Panel => self.thread_panel.as_ref().map(|p| &p.agents),
        }
    }

    pub(super) fn agents_in_mut(&mut self, which: Shown) -> Option<&mut Agents> {
        match which {
            Shown::View => self.agents.as_mut(),
            Shown::Panel => self.thread_panel.as_mut().map(|p| &mut p.agents),
        }
    }

    /// Bring what's shown of agents up to date: the Agents view, the thread
    /// panel.
    pub(super) fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        if self.showing_agents() {
            self.refresh_in(Shown::View, cx);
        }
        if self.panel_shown() {
            self.refresh_in(Shown::Panel, cx);
        }
    }

    /// Ask every connected server for its agents, and the chosen one's
    /// thread.
    pub(super) fn refresh_in(&mut self, which: Shown, cx: &mut Context<Self>) {
        let Some(agents) = self.agents_in_mut(which).filter(|a| !a.loading) else { return };
        agents.loading = true;
        let selected = agents.selected;
        let servers: Vec<(ServerId, String)> = app_state::servers(cx)
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, name, _)| (server, name))
            .collect();
        let asks: Vec<_> = servers
            .into_iter()
            .map(|(server, name)| {
                (server, name, app_state::control_on(server, Request::Agents, cx))
            })
            .collect();
        let thread = selected.map(|pane| {
            let request =
                Request::Thread { tab: ids::local_id(pane), after: None, limit: Some(THREAD) };
            let server = ids::server_of(pane);
            let waiting = Request::AskList { tab: Some(ids::local_id(pane)) };
            (
                pane,
                app_state::control_on(server, request, cx),
                app_state::control_on(server, waiting, cx),
            )
        });
        cx.spawn(async move |this, cx| {
            let mut found = Vec::new();
            for (server, name, answer) in asks {
                if let Ok(Response::Agents { agents }) = answer.await {
                    found.extend(agents.into_iter().map(|mut agent| {
                        agent.pane = ids::tag(server, agent.pane);
                        (agent, name.clone())
                    }));
                }
            }
            let thread = match thread {
                Some((pane, answer, waiting)) => {
                    let entries = match answer.await {
                        Ok(Response::Thread { entries, .. }) => entries,
                        _ => Vec::new(),
                    };
                    let asks = match waiting.await {
                        Ok(Response::Asks { asks }) => asks,
                        _ => Vec::new(),
                    };
                    Some((pane, entries, asks))
                },
                None => None,
            };
            let _ = this.update(cx, |this, cx| {
                // As the sidebar has their tabs (↑↓ go that way too).
                let mut found = found;
                found.sort_by_cached_key(|(agent, server)| this.agent_order(agent, server));
                let Some(agents) = this.agents_in_mut(which) else { return };
                agents.loading = false;
                agents.loaded = true;
                agents.agents = found;
                // The first, if none is chosen (or the chosen one is gone);
                // the panel's is the focused pane's, agent or not.
                if which == Shown::View {
                    let chosen =
                        agents.selected.filter(|p| agents.agents.iter().any(|(a, _)| a.pane == *p));
                    agents.selected = chosen.or_else(|| agents.agents.first().map(|(a, _)| a.pane));
                }
                // (Only the thread of the agent still chosen.)
                if let Some((pane, entries, asks)) =
                    thread.filter(|(p, _, _)| agents.selected == Some(*p))
                {
                    // (Also a card at the end that changed: an answer came.)
                    let grew = agents.thread_of != Some(pane)
                        || entries.len() != agents.thread.len()
                        || entries.last() != agents.thread.last()
                        || asks.len() != agents.asks.len();
                    agents.thread_of = Some(pane);
                    agents.thread = entries;
                    agents.asks = asks;
                    // To the entry searched for, else (anything new) the end.
                    // (Once: one older than the entries shown isn't there.)
                    let wanted = agents.reveal.take();
                    let found = wanted.and_then(|seq| {
                        let index = agents.thread.iter().position(|e| e.seq == seq);
                        Some(seq).zip(index)
                    });
                    if let Some((seq, index)) = found {
                        // Once laid out as it is now (else the entries'
                        // places are those of what was shown before).
                        let scroll = agents.scroll.clone();
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(Duration::from_millis(50)).await;
                            scroll.scroll_to_top_of_item(index);
                            let _ = this.update(cx, |_, cx| cx.notify());
                        })
                        .detach();
                        agents.flash = Some((seq, Instant::now()));
                    } else if grew {
                        agents.scroll.scroll_to_bottom();
                    }
                }
                if which == Shown::Panel {
                    this.panel_refreshed();
                }
                cx.notify();
            });
            // A new choice gets its thread at once.
            let _ = this.update(cx, |this, cx| {
                if this
                    .agents_in(which)
                    .is_some_and(|a| a.thread_of.is_none() && a.selected.is_some())
                {
                    this.refresh_in(which, cx);
                }
            });
        })
        .detach();
    }

    /// Show Agents with `pane`'s agent chosen, scrolled to its entry `seq`.
    pub(super) fn reveal_in_thread(&mut self, pane: PaneId, seq: u64, cx: &mut Context<Self>) {
        if !self.showing_agents() {
            self.toggle_agents(cx);
        }
        self.select_agent(pane, cx);
        if let Some(agents) = self.agents.as_mut() {
            agents.reveal = Some(seq);
            // Already in: the next refresh scrolls there.
            agents.thread_of = None;
        }
        self.refresh_in(Shown::View, cx);
    }

    fn select_agent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(agents) = self.agents.as_mut() else { return };
        if agents.selected != Some(pane) {
            agents.selected = Some(pane);
            agents.pieces.borrow_mut().clear();
            agents.text_sel = None;
            agents.thread.clear();
            agents.thread_of = None;
            self.refresh_in(Shown::View, cx);
        }
        cx.notify();
    }

    /// Keys while Agents is shown: ↑↓ choose an agent, Enter shows its
    /// terminal, Escape leaves. Returns whether the key was used.
    pub(super) fn agents_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        // The draft's keys first: editing, sending.
        if self.compose_key(event, cx) {
            return true;
        }
        let Some(agents) = self.agents.as_ref() else { return false };
        let writing =
            agents.selected.and_then(|p| self.drafts.get(&p)).is_some_and(|d| !d.text.is_empty());
        let panes: Vec<PaneId> = agents.agents.iter().map(|(a, _)| a.pane).collect();
        let at = agents.selected.and_then(|p| panes.iter().position(|&q| q == p));
        match event.keystroke.key.as_str() {
            "escape" => self.leave_agents(cx),
            "up" | "down" if !panes.is_empty() && !writing => {
                let next = match (event.keystroke.key.as_str(), at) {
                    ("up", Some(i)) => i.saturating_sub(1),
                    ("down", Some(i)) => (i + 1).min(panes.len() - 1),
                    _ => 0,
                };
                self.select_agent(panes[next], cx);
            },
            "enter" => {
                if let Some(pane) = agents.selected {
                    self.show_agent_terminal(pane, cx);
                }
            },
            _ => return false,
        }
        true
    }

    /// Leave Agents and show the agent's terminal, wherever it is.
    pub(super) fn show_agent_terminal(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        self.agents = None;
        self.open_queue_pane(pane, cx);
        cx.notify();
    }

    pub(super) fn agents_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(agents) = self.agents.as_ref() else { return div() };
        let several =
            agents.agents.iter().map(|(_, server)| server).collect::<HashSet<_>>().len() > 1;
        if agents.loaded && agents.agents.is_empty() {
            return div().size_full().flex().items_center().justify_center().child(
                div()
                    .max_w(px(420.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .text_color(ui(0xffffffa0))
                    .child(
                        div()
                            .text_size(px(15.))
                            .text_color(ui(0xffffffe0))
                            .child("No agents running"),
                    )
                    .child(
                        "Start Claude Code, Codex or Gemini CLI in a tab. With artty's hooks \
                         installed (artty hooks install), each shows up here with what it was \
                         asked, what it said, and what it needs from you.",
                    ),
            );
        }
        let selected =
            agents.selected.and_then(|p| agents.agents.iter().find(|(a, _)| a.pane == p));
        // The list is the sidebar's; with the sidebar folded away, here.
        let list = self.sidebar_hidden.then(|| {
            div()
                .id("agent-list")
                .w(px(232.))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(1.))
                .p(px(6.))
                .text_size(px(12.5))
                .border_r_1()
                .border_color(ui(0xffffff12))
                .overflow_y_scroll()
                .child(self.agents_heading(cx))
                .children(self.sidebar_agents(cx))
        });
        div()
            .size_full()
            .flex()
            .min_h_0()
            .children(list)
            .child(self.agent_thread(agents, selected.map(|(a, _)| a), Shown::View, cx))
            .children(
                selected.map(|(agent, server)| self.agent_details(agent, server, several, cx)),
            )
    }

    /// AGENTS, over the sidebar's list of them (in place of its tabs).
    pub(super) fn agents_heading(&self, _cx: &mut Context<Self>) -> gpui::Div {
        let count = self.agents.as_ref().map_or(0, |a| a.agents.len());
        div()
            .flex()
            .items_center()
            .px_2()
            .pb_1()
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui(0xffffff70))
            .child(format!("AGENTS · {count}"))
    }

    /// The agents, as the sidebar lists tabs: its state's icon, its tab's
    /// title, and what it has for you (else what it is) on the right.
    pub(super) fn sidebar_agents(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(agents) = self.agents.as_ref() else { return Vec::new() };
        let several =
            agents.agents.iter().map(|(_, server)| server).collect::<HashSet<_>>().len() > 1;
        // A workspace's (machine's) name over its agents, when they're of
        // more than one.
        let place = |agent: &AgentInfo, server: &str| match several {
            true => format!("{server} · {}", agent.workspace),
            false => agent.workspace.clone(),
        };
        let places: HashSet<String> = agents.agents.iter().map(|(a, s)| place(a, s)).collect();
        let mut last_place = None;
        let mut rows = Vec::new();
        for (agent, server) in &agents.agents {
            let here = place(agent, server);
            if places.len() > 1 && last_place.as_ref() != Some(&here) {
                rows.push(
                    Self::one_line(here.clone())
                        .px_2()
                        .pt(px(6.))
                        .pb(px(2.))
                        .text_size(px(11.))
                        .text_color(ui(0xffffff60))
                        .into_any_element(),
                );
            }
            last_place = Some(here);
            rows.push(self.sidebar_agent(agents, agent, cx));
        }
        rows
    }

    fn sidebar_agent(
        &self,
        agents: &Agents,
        agent: &AgentInfo,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane = agent.pane;
        let chosen = agents.selected == Some(pane);
        let icon = match agent.state {
            AgentState::Working => "busy",
            AgentState::Waiting => "waiting",
            AgentState::Done => "done",
            AgentState::Idle => "",
        };
        let (end, color) = match (agent.pending, agent.unread) {
            (n, _) if n > 0 => ("permission".to_owned(), WAITING),
            (_, n) if n > 0 => ("reply".to_owned(), DONE),
            _ => (agent.agent.clone(), 0xffffff60),
        };
        let title = agent.title.clone();
        let text = if chosen { ui(0xffffffee) } else { ui(0xffffffb0) };
        div()
            .id(SharedString::from(format!("agent-{pane}")))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(32.))
            .pl(px(4.))
            .pr(px(8.))
            .rounded(px(7.))
            .when(chosen, |d| d.bg(ui(0xffffff1c)))
            .when(!chosen && agent.state == AgentState::Waiting, |d| d.bg(ui(0xe0af6814)))
            .hover(|d| d.bg(ui(0xffffff10)))
            .child(self.icon(icon))
            .child(
                Self::one_line(title)
                    .flex_1()
                    .min_w_0()
                    .text_color(text)
                    .when(agent.state == AgentState::Waiting, |d| {
                        d.font_weight(FontWeight::SEMIBOLD)
                    }),
            )
            .child(div().flex_none().text_size(px(11.5)).text_color(ui(color)).child(end))
            .on_click(cx.listener(move |this, _, _, cx| this.select_agent(pane, cx)))
            .into_any_element()
    }

    /// An agent's thread, with its header and the box to write to it in.
    pub(super) fn agent_thread(
        &self,
        agents: &Agents,
        agent: Option<&AgentInfo>,
        which: Shown,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(agent) = agent else { return div().flex_1() };
        let server = ids::server_of(agent.pane);
        let mono = self.metrics.font.family.clone();
        let bubble = |mine: bool| {
            div()
                .px(px(11.))
                .py(px(8.))
                .rounded(px(10.))
                .bg(if mine { ui(0x3d59a1aa) } else { ui(0xffffff0e) })
                .text_color(ui(0xffffffe0))
        };
        let note = |text: String, color: u32| {
            let pill = div()
                .px(px(10.))
                .py(px(1.))
                .rounded(px(10.))
                .border_1()
                .border_color(ui(0xffffff1a))
                .text_size(px(11.))
                .text_color(ui(color))
                .child(text);
            div().flex().justify_center().child(pill)
        };
        let cwd = agent.cwd.clone();
        let shown_path = move |path: &str| -> String {
            let path = std::path::Path::new(path);
            cwd.as_deref()
                .and_then(|cwd| path.strip_prefix(cwd).ok())
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let entries = agents.thread.iter().map(|entry| {
            let time = clock(entry.at_ms);
            let seq = entry.seq;
            let element: gpui::AnyElement = match &entry.item {
                ThreadItem::Prompt { text, ask, from } => div()
                    .flex()
                    .justify_end()
                    .child(
                        // The width is the column's: its bubble wraps in it.
                        div()
                            .max_w(relative_width(0.86))
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(3.))
                            .child(self.copyable(
                                match (ask, from) {
                                    (_, Some(from)) => format!("{} · {time}", from_label(from)),
                                    (_, None) => format!("you · {time}"),
                                },
                                text,
                                seq,
                                cx,
                            ))
                            .child(bubble(true).child(self.markdown(
                                agents,
                                text,
                                &seq.to_string(),
                                which,
                                cx,
                            ))),
                    )
                    .into_any_element(),
                ThreadItem::Reply { text } => div()
                    .max_w(relative_width(0.86))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(self.copyable(format!("{} · {time}", agent.agent), text, seq, cx))
                    .child(bubble(false).child(self.markdown(agents, text, &seq.to_string(), which, cx)))
                    .into_any_element(),
                ThreadItem::Permission { id, summary, state, .. } => {
                    let pending = *state == ApprovalState::Pending;
                    let (word, color) = match state {
                        ApprovalState::Pending => ("asks permission", WAITING),
                        ApprovalState::Allowed => ("allowed in artty", DONE),
                        ApprovalState::AllowedThere => ("allowed at its prompt", DONE),
                        ApprovalState::Denied => ("denied in artty", DENY),
                        ApprovalState::Dismissed => ("turned down at its prompt", DENY),
                        ApprovalState::Ended => ("ended with its turn", 0xffffff80),
                        ApprovalState::Expired => ("left to its prompt", 0xffffff80),
                    };
                    let key = (server, QueueKind::Approval, *id);
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .border_l_2()
                        .border_color(ui(color))
                        .bg(ui(0xffffff08))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(ui(color))
                                .child(format!("{word} · {time}")),
                        )
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .text_color(ui(0xffffffe0))
                                .child(summary.clone()),
                        )
                        .when(pending, |d| {
                            d.child(
                                div()
                                    .flex()
                                    .gap(px(6.))
                                    .child(
                                        Self::small_button(
                                            format!("thread-allow-{id}"),
                                            "Allow",
                                            0x1a1b26ff,
                                            DONE,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.decide(key, true, None, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::small_button(
                                            format!("thread-deny-{id}"),
                                            "Deny",
                                            DENY,
                                            0xffffff12,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.decide(key, false, None, cx)
                                            }),
                                        ),
                                    ),
                            )
                        })
                        .into_any_element()
                },
                ThreadItem::Handoff {
                    ask: id,
                    to,
                    text,
                    by_agent,
                    state,
                    reply,
                    error,
                    forwarded,
                    back,
                } => {
                    use artty_core::control::AskState;
                    let (word, color) = match state {
                        AskState::Queued => ("waiting for its turn", 0xffffff80),
                        AskState::Delivering => ("being given to it", BUSY),
                        AskState::Running => ("working on it", BUSY),
                        AskState::Answered => ("answered", DONE),
                        AskState::Failed => ("failed", DENY),
                        AskState::Cancelled => ("cancelled", 0xffffff80),
                    };
                    let who = if *by_agent { agent.agent.as_str() } else { "you" };
                    let offer =
                        !by_agent && !back && forwarded.is_none() && *state == AskState::Answered;
                    let id = *id;
                    let here = agent.title.clone();
                    let target = match &to.agent {
                        Some(a) => format!("{} · {a}", to.title),
                        None => to.title.clone(),
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .border_l_2()
                        .border_color(ui(0x7aa2f7ff))
                        .bg(ui(0xffffff08))
                        // (Copy: its answer, once there's one.)
                        .child(self.copyable(
                            format!("{who} → {target} · {time}"),
                            reply.as_deref().unwrap_or(text),
                            seq,
                            cx,
                        ))
                        .child(self.markdown(agents, text, &seq.to_string(), which, cx))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(ui(color))
                                .child(format!("{} {word}", to.title)),
                        )
                        .children(reply.as_ref().map(|reply| {
                            div()
                                .px(px(9.))
                                .py(px(6.))
                                .rounded(px(7.))
                                .bg(ui(0xffffff0c))
                                .text_color(ui(0xffffffd8))
                                .child(self.markdown(agents, reply, &format!("{seq}-reply"), which, cx))
                        }))
                        .children(error.as_ref().map(|error| {
                            div().text_size(px(11.)).text_color(ui(DENY)).child(error.clone())
                        }))
                        .when(forwarded.is_some(), |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(ui(0xffffff70))
                                    .child(format!("↩ brought back to {here}")),
                            )
                        })
                        .when(*back && forwarded.is_none() && error.is_none(), |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(ui(0xffffff70))
                                    .child(format!("↩ its answer comes back to {here}")),
                            )
                        })
                        .when(offer, |d| {
                            d.child(
                                div().flex().child(
                                    Self::small_button(
                                        format!("thread-bring-{id}"),
                                        &format!("↩ Send to {here}"),
                                        0xffffffd0,
                                        0xffffff14,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.bring_back(server, id, cx),
                                    )),
                                ),
                            )
                        })
                        .into_any_element()
                },
                ThreadItem::Edited { files } => {
                    let (added, removed) =
                        files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
                    let count = match files.len() {
                        1 => "edited a file".to_owned(),
                        n => format!("edited {n} files"),
                    };
                    let rows = files.iter().enumerate().map(|(i, file)| {
                        let open = agents.expanded.contains(&(seq, i));
                        let counts = match file.written {
                            true => format!("written · {} lines", file.added),
                            false => format!("+{} −{}", file.added, file.removed),
                        };
                        let hunks = file.hunks.iter().map(|(old, new)| {
                            let line = |mark: &str, text: &str, color: u32| {
                                div()
                                    .text_color(ui(color))
                                    .whitespace_normal()
                                    .child(format!("{mark} {text}"))
                            };
                            div()
                                .flex()
                                .flex_col()
                                .py(px(3.))
                                .border_t_1()
                                .border_color(ui(0xffffff10))
                                .children(old.lines().map(|l| line("−", l, 0xf7768ecc)))
                                .children(new.lines().map(|l| line("+", l, 0x9ece6acc)))
                        });
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .id(SharedString::from(format!("edit-{seq}-{i}")))
                                    .flex()
                                    .gap(px(8.))
                                    .px(px(4.))
                                    .rounded(px(4.))
                                    .hover(|d| d.bg(ui(0xffffff0c)))
                                    .child(div().text_color(ui(0xffffff60)).child(if open {
                                        "▾"
                                    } else {
                                        "▸"
                                    }))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .font_family(mono.clone())
                                            .text_size(px(12.))
                                            .text_color(ui(0xffffffd8))
                                            .child(shown_path(&file.path)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(11.))
                                            .text_color(ui(0xffffff70))
                                            .child(counts),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_edit(which, seq, i, cx)
                                    })),
                            )
                            .when(open, |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .ml(px(18.))
                                        .font_family(mono.clone())
                                        .text_size(px(11.5))
                                        .children(hunks),
                                )
                            })
                    });
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .bg(ui(0xffffff06))
                        .border_1()
                        .border_color(ui(0xffffff12))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(ui(0xffffff70))
                                .child(format!("✎ {count} · +{added} −{removed} · {time}")),
                        )
                        .children(rows)
                        .into_any_element()
                },
                ThreadItem::Overlap { file, other } => note(
                    format!("⚠ {} also changed {} · {time}", other.title, shown_path(file)),
                    WAITING,
                )
                .into_any_element(),
                ThreadItem::Waiting { message } => {
                    note(format!("waiting for you · {message}"), WAITING).into_any_element()
                },
                ThreadItem::Session { event } => {
                    note(format!("session {event} · {time}"), 0xffffff70).into_any_element()
                },
                ThreadItem::Interrupted { reason } => {
                    note(format!("stopped · {reason}"), DENY).into_any_element()
                },
            };
            // An entry just searched for stands out a moment.
            let found = agents.flash.is_some_and(|(s, at)| s == seq && at.elapsed() < FLASH_FOUND);
            div()
                .group(SharedString::from(format!("entry-{seq}")))
                .flex_none()
                .rounded(px(8.))
                .when(found, |d| d.bg(ui(0xe0af6826)).border_1().border_color(ui(0xe0af6888)))
                .child(element)
        });
        // (Built now: their buttons' listeners hold the context.)
        let entries: Vec<gpui::Div> = entries.collect();
        let (label, color) = state_label(agent.state);
        let pane = agent.pane;
        let live =
            agents.peek_open && app_state::screen_of(pane, cx).is_some_and(|s| s.frame.columns > 0);
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(ui(0xffffff12))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(14.))
                            .text_color(ui(0xffffffee))
                            .child(format!("{} · {}", agent.title, agent.agent)),
                    )
                    .child(div().flex_none().text_size(px(12.)).text_color(ui(color)).child(label))
                    .child(div().flex_1())
                    // (Its live terminal, beside, opens it with a click: only
                    // without one, a button.)
                    .when(which == Shown::View && !live, |d| {
                        d.child(
                            Self::small_button(
                                format!("agent-terminal-{pane}"),
                                "Open terminal  ↩",
                                0xffffffd0,
                                0xffffff14,
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.show_agent_terminal(pane, cx)
                                }),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .id(match which {
                        Shown::View => "agent-thread",
                        Shown::Panel => "panel-thread",
                    })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&agents.scroll)
                    // A press outside a message: what was selected isn't.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(agents) = this.agents_in_mut(which)
                                && agents.text_sel.take().is_some()
                            {
                                cx.notify();
                            }
                        }),
                    )
                    // The entries are its own children: one can be scrolled to.
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(14.))
                    .text_size(px(13.))
                    .children(entries)
                    .when(agents.thread.is_empty(), |d| {
                        d.child(div().text_color(ui(0xffffff60)).child(
                            "Nothing yet: what it's given and says from now on shows here.",
                        ))
                    })
                    .child(self.queued_asks(&agents.asks)),
            )
            .child(self.compose_box(&agent.title, which, cx))
    }

    fn agent_details(
        &self,
        agent: &AgentInfo,
        server: &str,
        several: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // Its terminal, live, above what's known of it: a third of the view
        // (within reason), the thread keeping the most.
        const PADDING: f32 = 14.;
        let wide = (self.area.width * 0.3).clamp(260., 440.);
        let open = self.agents.as_ref().is_some_and(|a| a.peek_open);
        let peek = open.then(|| self.terminal_peek(agent.pane, wide - 2. * PADDING, cx)).flatten();
        let width = if peek.is_some() { wide } else { 240. };
        // Folded away (at first), or open: a click on its line either way.
        let fold = div()
            .id("peek-fold")
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(10.5))
            .text_color(ui(0xffffff60))
            .hover(|d| d.text_color(ui(0xffffffb0)))
            .cursor_pointer()
            .child(if open { "▾ TERMINAL" } else { "▸ TERMINAL" })
            .when(peek.is_some(), |d| d.child("· click it to open"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_peek(cx)));
        let row = |label: &str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(ui(0xffffff60))
                        .child(label.to_uppercase()),
                )
                .child(div().text_size(px(12.5)).text_color(ui(0xffffffd8)).child(value))
        };
        let (label, _) = state_label(agent.state);
        div()
            .id("agent-details")
            .w(px(width))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(PADDING))
            .border_l_1()
            .border_color(ui(0xffffff12))
            .overflow_y_scroll()
            .child(div().flex().flex_col().gap(px(4.)).child(fold).children(peek))
            .child(row(
                "Status",
                match &agent.message {
                    Some(message) if !message.is_empty() => format!("{label}: {message}"),
                    _ => label.to_owned(),
                },
            ))
            .when(agent.pending > 0, |d| {
                let requests = match agent.pending {
                    1 => "1 permission request".to_owned(),
                    n => format!("{n} permission requests"),
                };
                d.child(row("Needs you", requests))
            })
            // The rest, small: where it is, what it's on, when it last did.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .text_size(px(11.5))
                    .text_color(ui(0xffffff80))
                    .child(Self::one_line(match several {
                        true => format!("{server} · {}", agent.workspace),
                        false => agent.workspace.clone(),
                    }))
                    .children(agent.cwd.as_ref().map(|cwd| Self::one_line(short_home(cwd))))
                    .child(Self::one_line(
                        [
                            agent.ask.map(|id| format!("on ask {id}")),
                            agent.last_ms.map(|ms| format!("last at {}", clock(ms))),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                    )),
            )
    }

    /// Show (or hide) what an edited file's changes were.
    fn toggle_edit(&mut self, which: Shown, seq: u64, file: usize, cx: &mut Context<Self>) {
        if let Some(agents) = self.agents_in_mut(which)
            && !agents.expanded.remove(&(seq, file))
        {
            agents.expanded.insert((seq, file));
        }
        cx.notify();
    }

    /// Give an answer from another agent to the agent whose thread asked.
    fn bring_back(&mut self, server: ServerId, id: u64, cx: &mut Context<Self>) {
        let answer = app_state::control_on(server, Request::AskForward { id }, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Response::Error { message }) = answer.await {
                let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
            }
            let _ = this.update(cx, |this, cx| this.refresh_agents(cx));
        })
        .detach();
    }

    fn small_button(id: String, label: &str, fg: u32, bg: u32) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(id))
            .px(px(8.))
            .py(px(2.))
            .rounded(px(5.))
            .text_size(px(11.5))
            .bg(ui(bg))
            .text_color(ui(fg))
            .hover(|d| d.bg(ui(0xffffff28)))
            .child(label.to_owned())
    }
}

/// An agent that asked another, as its thread shows it: "from api (claude)".
pub(super) fn from_label(from: &artty_core::control::AskFrom) -> String {
    match &from.agent {
        Some(agent) => format!("from {} · {agent}", from.title),
        None => format!("from {}", from.title),
    }
}

/// A folder as shown: the home folder as ~.
fn short_home(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.as_deref().and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The word around `at` in `text` (letters, digits, `_`), as offsets; at a
/// space or mark, the one character.
fn word_at(text: &str, at: usize) -> (usize, usize) {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let start =
        text[..at].char_indices().rev().take_while(|(_, c)| word(*c)).last().map_or(at, |(i, _)| i);
    let end =
        text[at..].char_indices().find(|(_, c)| !word(*c)).map_or(text.len(), |(i, _)| at + i);
    if start == end {
        let next = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
        return (at, next);
    }
    (start, end)
}

fn relative_width(fraction: f32) -> gpui::DefiniteLength {
    gpui::relative(fraction)
}

#[cfg(test)]
mod tests {
    use super::word_at;

    #[test]
    fn words() {
        let text = "fix next_url() now, 한글 ok";
        let word = |at: usize| {
            let (a, b) = word_at(text, at);
            &text[a..b]
        };
        assert_eq!(word(5), "next_url");
        assert_eq!(word(0), "fix");
        // Just after a word: that word; at a mark: the mark.
        assert_eq!(word(12), "next_url");
        assert_eq!(word(13), ")");
        assert_eq!(word(text.find('한').unwrap() + 1), "한글");
        assert_eq!(word(text.len()), "ok");
    }
}
