//! The Agents view: every agent on every connected server, the chosen one's
//! thread (what it was given and said, its permission requests, its
//! sessions), and what's known about it. Refreshed while on screen; like
//! Activity, it belongs to no workspace and runs nothing itself. Its
//! terminal is a click away.

use artty_core::control::{AgentInfo, AgentState, ApprovalState, Said, ThreadEntry, ThreadItem};
use artty_core::protocol::{QueueKind, SessionHit};

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

/// A thread's list: held to the end (the newest), as a chat is.
pub(super) struct ThreadList(pub gpui::ListState);

impl Default for ThreadList {
    fn default() -> Self {
        ThreadList(gpui::ListState::new(1, gpui::ListAlignment::Bottom, px(800.)))
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
    parsed: std::cell::RefCell<HashMap<String, (usize, std::rc::Rc<crate::markdown::Doc>)>>,
    /// The chosen agent's terminal shown, live (else folded away, and not
    /// sent).
    pub(super) peek_open: bool,
    /// An entry to scroll to once the thread's in (one searched for), and
    /// the one shown standing out, since when.
    reveal: Option<(PaneId, u64)>,
    flash: Option<(u64, Instant)>,
    /// The thread as a list (its entries, then what's queued): only those
    /// in view are drawn.
    pub(super) list: ThreadList,
    loading: bool,
    pub(super) loaded: bool,
    /// The past conversation shown (instead of an agent's thread), and
    /// what was said in it once read.
    archived: Option<(ServerId, SessionHit)>,
    said: Option<Result<Vec<Said>, String>>,
}

/// Agents' past conversations on each machine (not going on in a pane),
/// newest first, for Agents and the sidebar.
#[derive(Default)]
pub(super) struct Past {
    pub(super) list: Vec<(ServerId, SessionHit)>,
    /// Those of them that are copies (`artty new --fork`).
    pub(super) copies: HashSet<String>,
    /// When they were last asked for.
    at: Option<Instant>,
}

impl Agents {
    /// A past conversation is shown, not an agent's thread.
    pub(super) fn showing_past(&self) -> bool {
        self.archived.is_some()
    }

    /// The past conversation shown, and its machine.
    pub(super) fn past_shown(&self) -> Option<(ServerId, &SessionHit)> {
        self.archived.as_ref().map(|(server, hit)| (*server, hit))
    }

    /// Back to the agent's thread.
    pub(super) fn close_past(&mut self) {
        if self.archived.take().is_some() {
            self.said = None;
            self.text_sel = None;
            self.pieces.borrow_mut().clear();
        }
    }
}

/// The conversation of `agent` a copy can be made of: Claude Code's.
fn copyable(agent: &AgentInfo) -> Option<String> {
    agent.session.clone().filter(|_| agent.agent == "claude" || agent.agent == "claude-acp")
}

/// Past conversations listed, at most (each machine's).
const PAST: usize = 8;
/// How often the list of past conversations is read again.
const PAST_EVERY: Duration = Duration::from_secs(30);

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
    pub(super) fn agent_order(
        &self,
        agent: &AgentInfo,
        server: &str,
    ) -> (u8, String, usize, usize, usize) {
        for (w, workspace) in self.layout.workspaces.iter().enumerate() {
            for (t, tab) in workspace.tabs.iter().enumerate() {
                if let Some(p) = tab.root.panes().iter().position(|&p| p == agent.pane) {
                    return (0, String::new(), w, t, p);
                }
            }
        }
        // (A workspace's together, by machine.)
        (1, format!("{server}\u{0}{}", agent.workspace), 0, 0, agent.pane as usize)
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
    /// (`right`: over a message on the right, Copy on the left of the line,
    /// so the line ends at the message's edge.)
    fn copyable(
        &self,
        label: String,
        text: &str,
        seq: u64,
        right: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.copyable_from(label, text, seq, right, None, cx)
    }

    /// [`Self::copyable`], and (`from`: the agent whose thread it's in)
    /// "Send to…": it, or what's selected in it, to another agent or a copy
    /// of this one's conversation.
    fn copyable_from(
        &self,
        label: String,
        text: &str,
        seq: u64,
        right: bool,
        from: Option<&AgentInfo>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let text = text.to_owned();
        let send = from.map(|agent| {
            let (pane, title, session) = (agent.pane, agent.title.clone(), copyable(agent));
            let whole = text.clone();
            div()
                .id(SharedString::from(format!("send-{seq}")))
                .px(px(5.))
                .rounded(px(4.))
                .opacity(0.)
                .group_hover(SharedString::from(format!("entry-{seq}")), |s| s.opacity(1.))
                .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                .child("Send to…")
                .on_click(cx.listener(move |this, _, _, cx| {
                    // What's selected in it, else all of it.
                    let selected = [Shown::View, Shown::Panel]
                        .into_iter()
                        .filter_map(|w| this.agents_in(w))
                        .filter_map(|a| a.text_sel.as_ref())
                        .any(|sel| sel.message == seq.to_string() && sel.anchor != sel.head);
                    let text = match selected {
                        true => this.selected_text(false).unwrap_or_else(|| whole.clone()),
                        false => whole.clone(),
                    };
                    let sending = super::palette::Sending {
                        from: Some(pane),
                        server: ids::server_of(pane),
                        title: title.clone(),
                        session: session.clone(),
                        text,
                    };
                    this.open_send(sending, cx);
                }))
        });
        div()
            .flex()
            .when(right, |d| d.flex_row_reverse())
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
            .children(send)
    }

    /// What's selected in a message shown (Agents, or the panel):
    /// `as_shown`: as it reads; else as written (its markdown, the markup
    /// of what's selected whole with it).
    pub(super) fn selected_text(&self, as_shown: bool) -> Option<String> {
        [Shown::View, Shown::Panel].into_iter().find_map(|which| {
            let agents = self.agents_in(which)?;
            let sel = agents.text_sel.as_ref().filter(|s| s.anchor != s.head)?;
            let pieces = agents.pieces.borrow().get(&sel.message)?.clone();
            let text = match as_shown {
                true => pieces.text(sel.range()),
                false => pieces.markdown(sel.range()),
            };
            Some(text).filter(|t| !t.is_empty())
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
        // Parsed once a message (by its id; its length, should it change).
        let blocks = {
            let mut parsed = agents.parsed.borrow_mut();
            // (A long-running view: start over now and then.)
            if parsed.len() > 2000 {
                parsed.clear();
            }
            let known =
                parsed.get(id).filter(|(len, _)| *len == text.len()).map(|(_, d)| d.clone());
            match known {
                Some(doc) => doc,
                None => {
                    let doc = std::rc::Rc::new(crate::markdown::parse_doc(&clip(text)));
                    parsed.insert(id.to_owned(), (text.len(), doc.clone()));
                    doc
                },
            }
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
            colors: code_color,
        };
        let pieces = agents.pieces.borrow_mut().entry(id.to_owned()).or_default().clone();
        let selected = agents
            .text_sel
            .as_ref()
            .filter(|s| s.message == id && s.anchor != s.head)
            .map(TextSel::range);
        let source = clip(text);
        let view =
            crate::markdown::view(&blocks, &source, &look, &format!("md-{id}"), &pieces, selected);
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
        if which == Shown::View {
            self.refresh_past(false, cx);
        }
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
                        agent.parent = agent.parent.map(|p| ids::tag(server, p));
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
                // Each followed by those it started.
                let found = nested(found);
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
                    // The list told what changed: another agent's thread,
                    // all of it; else from the first entry that isn't as it
                    // was (what's queued, at the end, each time).
                    let list = &agents.list.0;
                    let old = agents.thread.len();
                    if agents.thread_of != Some(pane) || list.item_count() != old + 1 {
                        list.reset(entries.len() + 1);
                    } else if entries != agents.thread || asks != agents.asks {
                        let same =
                            agents.thread.iter().zip(&entries).take_while(|(a, b)| a == b).count();
                        list.splice(same..old + 1, entries.len() + 1 - same);
                    }
                    agents.thread_of = Some(pane);
                    agents.thread = entries;
                    agents.asks = asks;
                    // To the entry searched for, else (anything new) the end.
                    // (Once: one older than the entries shown isn't there.)
                    // (Asked of this pane's thread: one for another waits.)
                    let wanted = match agents.reveal {
                        Some((p, seq)) if p == pane => agents.reveal.take().map(|_| seq),
                        _ => None,
                    };
                    let found = wanted.and_then(|seq| {
                        let index = agents.thread.iter().position(|e| e.seq == seq);
                        Some(seq).zip(index)
                    });
                    if let Some((seq, index)) = found {
                        agents
                            .list
                            .0
                            .scroll_to(gpui::ListOffset { item_ix: index, offset_in_item: px(0.) });
                        agents.flash = Some((seq, Instant::now()));
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
    /// `pane`'s agent's thread, at its entry `seq`: in Agents when it's
    /// shown, else in the panel beside the agent's terminal.
    pub(super) fn reveal_in_thread(&mut self, pane: PaneId, seq: u64, cx: &mut Context<Self>) {
        if self.showing_agents() {
            self.select_agent(pane, cx);
            if let Some(agents) = self.agents.as_mut() {
                agents.reveal = Some((pane, seq));
                // Already in: the next refresh scrolls there.
                agents.thread_of = None;
            }
            return self.refresh_in(Shown::View, cx);
        }
        self.go_to_agent(pane, cx);
        if !self.panel_shown() {
            self.open_thread_panel(cx);
        }
        // (The panel follows the focus there: the thread comes once it has.)
        if let Some(panel) = self.thread_panel.as_mut() {
            panel.agents.reveal = Some((pane, seq));
            if panel.agents.selected == Some(pane) {
                panel.agents.thread_of = None;
            }
        }
        self.refresh_in(Shown::Panel, cx);
    }

    /// ⇧⌘K: agents where the terminals are: the sidebar grouped by agent
    /// (kept so: the setting), the thread panel open, at an agent: the one
    /// focused, else the one that most needs you.
    pub(super) fn show_agents_here(&mut self, cx: &mut Context<Self>) {
        if self.showing_agents() {
            self.leave_agents(cx);
        }
        if self.showing_activity() {
            self.toggle_activity(cx);
        }
        if self.grouped.is_none() {
            self.toggle_grouping(cx);
        }
        let shared = &cx.global::<Shared>().panes;
        let agent_here =
            self.focused_id().and_then(|p| shared.get(&p)).is_some_and(|s| s.agent.is_some());
        if !agent_here {
            // Waiting for you, else finished, else any: this window's first.
            let here: Vec<PaneId> = self.tabs.iter().flat_map(|t| t.root.panes()).collect();
            let rank = |icon: &str| match icon {
                "waiting" | "attention" => 0,
                "done" => 1,
                _ => 2,
            };
            let target = here
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    Some((rank(&shared.get(p)?.icon), i, *p)).filter(|_| shared[p].agent.is_some())
                })
                .min()
                .map(|(_, _, p)| p);
            match target {
                Some(pane) => self.go_to_agent(pane, cx),
                None => {
                    self.show_toast("No agent in this window (⇧⌘K shows them once one runs)", cx)
                },
            }
        }
        if !self.panel_shown() {
            self.open_thread_panel(cx);
        }
        cx.notify();
    }

    pub(super) fn select_agent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(agents) = self.agents.as_mut() else { return };
        if agents.archived.take().is_some() {
            agents.text_sel = None;
        }
        if agents.selected != Some(pane) {
            agents.selected = Some(pane);
            agents.pieces.borrow_mut().clear();
            agents.text_sel = None;
            agents.thread.clear();
            agents.list.0.reset(1);
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
        // The agents, then the past conversations, as the list has them.
        let panes: Vec<PaneId> = agents.agents.iter().map(|(a, _)| a.pane).collect();
        let count = panes.len() + self.past.list.len();
        let at = match &agents.archived {
            Some((server, hit)) => self
                .past
                .list
                .iter()
                .position(|(s, h)| s == server && h.session == hit.session)
                .map(|i| panes.len() + i),
            None => agents.selected.and_then(|p| panes.iter().position(|&q| q == p)),
        };
        match event.keystroke.key.as_str() {
            "escape" => self.leave_agents(cx),
            "up" | "down" if count > 0 && !writing => {
                let next = match (event.keystroke.key.as_str(), at) {
                    ("up", Some(i)) => i.saturating_sub(1),
                    ("down", Some(i)) => (i + 1).min(count - 1),
                    _ => 0,
                };
                match panes.get(next) {
                    Some(&pane) => self.select_agent(pane, cx),
                    None => {
                        let (server, hit) = self.past.list[next - panes.len()].clone();
                        self.show_past(Shown::View, server, hit, cx);
                    },
                }
            },
            // A past conversation: a copy of it asked.
            "enter" if agents.archived.is_some() => {
                if agents.archived.as_ref().is_some_and(|(_, h)| h.agent == "claude") {
                    self.ask_copy(Shown::View, None, cx);
                }
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
        // (ARTTY_PROFILE: from here to its painting done.)
        let started = Instant::now();
        let timing = crate::profile::enabled().then(|| {
            canvas(
                |_, _, _| {},
                move |_, _, _, _| crate::profile::record_view("agents", started.elapsed()),
            )
            .absolute()
            .size_0()
        });
        self.agents_view_inner(cx).children(timing)
    }

    fn agents_view_inner(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(agents) = self.agents.as_ref() else { return div() };
        let several =
            agents.agents.iter().map(|(_, server)| server).collect::<HashSet<_>>().len() > 1;
        if let Some((server, hit)) = agents.archived.as_ref() {
            let list = self.sidebar_hidden.then(|| self.folded_list(cx));
            return div().size_full().flex().min_h_0().children(list).child(self.past_view(
                agents,
                Shown::View,
                *server,
                hit,
                cx,
            ));
        }
        if agents.loaded && agents.agents.is_empty() && self.past.list.is_empty() {
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
        let list = self.sidebar_hidden.then(|| self.folded_list(cx));
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

    /// The agents' list, beside the thread when the sidebar's folded away.
    fn folded_list(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
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
    }

    /// The past conversations asked for again (`now`, or once it's been a
    /// while), from each connected machine.
    pub(super) fn refresh_past(&mut self, now: bool, cx: &mut Context<Self>) {
        if !now && self.past.at.is_some_and(|at| at.elapsed() < PAST_EVERY) {
            return;
        }
        self.past.at = Some(Instant::now());
        let asks: Vec<_> = app_state::servers(cx)
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, _, _)| {
                let request = Request::Sessions { text: None, limit: Some(PAST) };
                (server, app_state::control_on(server, request, cx))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let (mut past, mut copies) = (Vec::new(), HashSet::new());
            for (server, answer) in asks {
                if let Ok(Response::Sessions { sessions }) = answer.await {
                    for found in sessions {
                        if found.forked_from.is_some() {
                            copies.insert(found.hit.session.clone());
                        }
                        past.push((server, found.hit));
                    }
                }
            }
            past.sort_by_key(|(_, hit)| std::cmp::Reverse(hit.modified));
            let _ = this.update(cx, |this, cx| {
                this.past.list = past;
                this.past.copies = copies;
                cx.notify();
            });
        })
        .detach();
    }

    /// Show a past conversation (in Agents, or the panel beside the
    /// terminal): what was said in it, read from its machine.
    pub(super) fn show_past(
        &mut self,
        which: Shown,
        server: ServerId,
        hit: SessionHit,
        cx: &mut Context<Self>,
    ) {
        if which == Shown::Panel && !self.panel_shown() {
            self.open_thread_panel(cx);
        }
        let Some(agents) = self.agents_in_mut(which) else { return };
        let session = hit.session.clone();
        agents.archived = Some((server, hit));
        agents.said = None;
        agents.text_sel = None;
        agents.pieces.borrow_mut().clear();
        let answer = app_state::control_on(
            server,
            Request::Conversation { session: session.clone(), limit: Some(60) },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let said = match answer.await {
                Ok(Response::Conversation { messages }) => Ok(messages),
                Ok(Response::Error { message }) => Err(message),
                _ => Err("its machine didn't answer".to_owned()),
            };
            let _ = this.update(cx, |this, cx| {
                let Some(agents) = this.agents_in_mut(which) else { return };
                if agents.archived.as_ref().is_some_and(|(_, h)| h.session == session) {
                    agents.said = Some(said);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// "Send to…" from the keyboard (or the palette): what's selected in
    /// the thread shown, else its last reply (a past conversation's: its
    /// last message).
    pub(super) fn send_to(&mut self, cx: &mut Context<Self>) {
        // Agents', else the thread panel's (beside the terminal).
        let which = match (self.showing_agents(), self.panel_shown()) {
            (true, _) => Shown::View,
            (false, true) => Shown::Panel,
            (false, false) => {
                return self.show_toast("Send to…: from an agent's thread (the panel, ⇧⌘J)", cx);
            },
        };
        self.send_from(which, cx);
    }

    /// "Send to…" from Agents or the panel: what's selected there, else
    /// its last reply.
    fn send_from(&mut self, which: Shown, cx: &mut Context<Self>) {
        let selected = self.selected_text(false);
        let Some(agents) = self.agents_in(which) else { return };
        if agents.archived.is_some() {
            let last = match &agents.said {
                Some(Ok(said)) => said.iter().rev().find(|s| s.role == "assistant"),
                _ => None,
            };
            let text = selected.or_else(|| last.map(|s| s.text.clone())).unwrap_or_default();
            return self.ask_copy(which, Some(text), cx);
        }
        let Some(agent) =
            agents.selected.and_then(|p| agents.agents.iter().find(|(a, _)| a.pane == p))
        else {
            return;
        };
        let last = agents.thread.iter().rev().find_map(|e| match &e.item {
            ThreadItem::Reply { text } => Some(text.clone()),
            _ => None,
        });
        let Some(text) = selected.or(last) else {
            return self.show_toast("Nothing to send yet", cx);
        };
        let agent = &agent.0;
        let sending = super::palette::Sending {
            from: Some(agent.pane),
            server: ids::server_of(agent.pane),
            title: agent.title.clone(),
            session: copyable(agent),
            text,
        };
        self.open_send(sending, cx);
    }

    /// A copy of `agent`'s conversation started, given a task (typed in
    /// the palette).
    fn start_copy(&mut self, agent: &AgentInfo, cx: &mut Context<Self>) {
        let Some(session) = copyable(agent) else {
            return self.show_toast("Only Claude Code's conversations are copied", cx);
        };
        let sending = super::palette::Sending {
            from: Some(agent.pane),
            server: ids::server_of(agent.pane),
            title: agent.title.clone(),
            session: Some(session),
            text: String::new(),
        };
        self.open_send(sending, cx);
    }

    /// Under an agent's name (in the panel): the agent that started it and
    /// those it started (a click goes to each), and what can be done with
    /// it: send on, a copy, its thread cleared.
    fn agent_bar(
        &self,
        agents: &Agents,
        agent: &AgentInfo,
        which: Shown,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let link = |id: String, label: String, pane: PaneId| {
            div()
                .id(SharedString::from(id))
                .px(px(5.))
                .rounded(px(4.))
                .bg(ui(0xffffff0c))
                .text_color(ui(0xffffffc0))
                .hover(|d| d.bg(ui(0xffffff18)).text_color(ui(0xffffffee)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.go_to_agent(pane, cx)))
        };
        let parent = agent
            .parent
            .and_then(|p| agents.agents.iter().find(|(a, _)| a.pane == p))
            .map(|(a, _)| link(format!("parent-{}", a.pane), format!("↑ {}", a.title), a.pane));
        let workers: Vec<_> = agents
            .agents
            .iter()
            .filter(|(a, _)| a.parent == Some(agent.pane))
            .map(|(a, _)| link(format!("worker-{}", a.pane), format!("↳ {}", a.title), a.pane))
            .collect();
        let action = |id: &str, label: &str| {
            div()
                .id(SharedString::from(id.to_owned()))
                .px(px(6.))
                .py(px(1.))
                .rounded(px(4.))
                .text_color(ui(0xffffff90))
                .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffee)))
                .child(label.to_owned())
        };
        let (pane, copy_of) = (agent.pane, agent.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(16.))
            .py(px(6.))
            .border_b_1()
            .border_color(ui(0xffffff12))
            .text_size(px(11.5))
            .when(parent.is_some() || !workers.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(4.))
                        .text_color(ui(0xffffff70))
                        .children(parent)
                        .children(workers),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .child(
                        action("bar-send", "Send to…")
                            .on_click(cx.listener(move |this, _, _, cx| this.send_from(which, cx))),
                    )
                    .when(copyable(agent).is_some(), |d| {
                        d.child(action("bar-copy", "Start a copy…").on_click(
                            cx.listener(move |this, _, _, cx| this.start_copy(&copy_of, cx)),
                        ))
                    })
                    .child(div().flex_1())
                    .child(action("bar-clear", "Clear thread…").on_click(
                        cx.listener(move |this, _, window, cx| this.clear_thread(pane, window, cx)),
                    )),
            )
    }

    /// The past conversation shown, sent on (`text`), or (none) a copy of
    /// it started.
    fn ask_copy(&mut self, which: Shown, text: Option<String>, cx: &mut Context<Self>) {
        let Some((server, hit)) = self.agents_in(which).and_then(|a| a.archived.clone()) else {
            return;
        };
        let sending = super::palette::Sending {
            from: None,
            server,
            title: hit.title.clone(),
            session: (hit.agent == "claude").then(|| hit.session.clone()),
            text: text.unwrap_or_default(),
        };
        self.open_send(sending, cx);
    }

    /// A past conversation, read-only: what was said, and what can be done
    /// with it (a copy asked, or carried on in a tab).
    pub(super) fn past_view(
        &self,
        agents: &Agents,
        which: Shown,
        server: ServerId,
        hit: &SessionHit,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut detail = vec![hit.agent.clone()];
        detail.extend(hit.cwd.as_deref().map(short_home));
        detail.push(super::search_view::ago(hit.modified));
        if self.past.copies.contains(&hit.session) {
            detail.push("a copy".into());
        }
        let copyable = hit.agent == "claude";
        let ask_copy =
            // (Enter asks one in Agents; in the panel it's the terminal's.)
            Self::small_button(
                "past-copy".into(),
                if which == Shown::View { "Ask a copy… ↩" } else { "Ask a copy…" },
                0xffffffee,
                0x3d59a1ff,
            )
                .on_click(cx.listener(move |this, _, _, cx| this.ask_copy(which, None, cx)));
        let resume = hit.clone();
        let carry_on =
            Self::small_button("past-resume".into(), "Continue in a tab", 0xffffffd0, 0xffffff14)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let Some(window) = this.window_on(server) else { return };
                    this.send(
                        ClientMessage::ResumeSession {
                            window,
                            agent: resume.agent.clone(),
                            session: resume.session.clone(),
                            cwd: resume.cwd.clone(),
                        },
                        cx,
                    );
                }));
        let bubble = |mine: bool| {
            div()
                .px(px(11.))
                .py(px(8.))
                .rounded(px(10.))
                .bg(if mine { ui(0x3d59a1aa) } else { ui(0xffffff0e) })
                .text_color(ui(0xffffffe0))
        };
        let body: Vec<gpui::AnyElement> = match &agents.said {
            None => vec![div().text_color(ui(0xffffff70)).child("Reading…").into_any_element()],
            Some(Err(message)) => {
                vec![div().text_color(ui(0xffffff70)).child(message.clone()).into_any_element()]
            },
            Some(Ok(said)) => said
                .iter()
                .enumerate()
                .map(|(i, said)| {
                    let mine = said.role == "user";
                    let message =
                        self.markdown(agents, &said.text, &format!("past-{i}"), which, cx);
                    div()
                        .flex()
                        .when(mine, |d| d.justify_end())
                        .child(div().max_w(relative_width(0.86)).child(bubble(mine).child(message)))
                        .into_any_element()
                })
                .collect(),
        };
        // Typing (into the palette over it) comes here: in Agents it has no
        // draft whose input handler would take it (in the panel, the
        // terminal's does).
        let (focus, entity) = (self.focus.clone(), cx.entity());
        let typing = (which == Shown::View).then(|| {
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    window.handle_input(&focus, gpui::ElementInputHandler::new(bounds, entity), cx);
                },
            )
            .absolute()
            .size_full()
        });
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .min_h_0()
            .children(typing)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(20.))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(ui(0xffffff12))
                    .child(
                        Self::one_line(hit.title.clone())
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui(0xffffffee)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(ui(0xffffff80))
                            .child(format!("Past conversation · {}", detail.join(" · "))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .when(copyable, |d| d.child(ask_copy))
                            .child(carry_on),
                    )
                    .child(div().text_size(px(11.5)).text_color(ui(0xffffff60)).child(
                        "Read-only. A copy starts a new agent with this whole conversation; this \
                         one isn't changed.",
                    )),
            )
            .child(
                div()
                    .id("past-said")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .px(px(20.))
                    .py(px(14.))
                    .text_size(px(13.))
                    .children(body),
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
            // (One started by another is under it, wherever its tab is.)
            let under =
                agent.parent.is_some_and(|p| agents.agents.iter().any(|(a, _)| a.pane == p));
            if !under && places.len() > 1 && last_place.as_ref() != Some(&here) {
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
            if !under {
                last_place = Some(here);
            }
            rows.push(self.sidebar_agent(agents, agent, cx));
        }
        if !self.past.list.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .pt(px(12.))
                    .pb_1()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70))
                    .child("PAST CONVERSATIONS")
                    .into_any_element(),
            );
        }
        for (i, (server, hit)) in self.past.list.iter().enumerate() {
            let chosen = agents
                .archived
                .as_ref()
                .is_some_and(|(s, h)| *s == *server && h.session == hit.session);
            let (mark, color) = super::search_view::agent_mark(&hit.agent);
            let (server, open) = (*server, hit.clone());
            rows.push(
                div()
                    .id(("past", i))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .when(chosen, |d| d.bg(ui(0xffffff1c)))
                    .hover(|d| d.bg(ui(0xffffff10)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(ui(color << 8 | 0xb0))
                            .child(mark),
                    )
                    .child(
                        Self::one_line(hit.title.clone())
                            .flex_1()
                            .min_w_0()
                            .text_color(if chosen { ui(0xffffffee) } else { ui(0xffffff90) }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_past(Shown::View, server, open.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }
        if !self.past.list.is_empty() {
            rows.push(
                div()
                    .id("past-search")
                    .flex_none()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .text_size(px(11.5))
                    .text_color(ui(0xffffff60))
                    .hover(|d| d.bg(ui(0xffffff10)).text_color(ui(0xffffffb0)))
                    .child("Search conversations… ⌘⇧F")
                    .on_click(cx.listener(|this, _, _, cx| this.open_search_all(cx)))
                    .into_any_element(),
            );
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
        let chosen = agents.selected == Some(pane) && agents.archived.is_none();
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
        let depth = depth(&agents.agents, agent);
        div()
            .id(SharedString::from(format!("agent-{pane}")))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(32.))
            .pl(px(4. + 14. * depth as f32))
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

    /// The thread's entry `ix`, as the list draws it (only those in view);
    /// past the last, what's queued for the agent.
    fn thread_item(&self, which: Shown, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(agents) = self.agents_in(which) else { return div().into_any_element() };
        let chosen = agents.selected.and_then(|p| agents.agents.iter().find(|(a, _)| a.pane == p));
        let Some((agent, _)) = chosen else { return div().into_any_element() };
        let Some(entry) = agents.thread.get(ix) else {
            return div()
                .px(px(16.))
                .pt(px(12.))
                .pb(px(14.))
                .when(agents.thread.is_empty(), |d| {
                    d.child(
                        div()
                            .text_color(ui(0xffffff60))
                            .child("Nothing yet: what it's given and says from now on shows here."),
                    )
                })
                .child(self.queued_asks(&agents.asks))
                .into_any_element();
        };
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
        let time = clock(entry.at_ms);
        let seq = entry.seq;
        let element: gpui::AnyElement = match &entry.item {
            ThreadItem::Prompt { text, ask, from, .. } => div()
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
                        .child(self.copyable_from(
                            match (ask, from) {
                                (_, Some(from)) => format!("{} · {time}", from_label(from)),
                                (_, None) => format!("you · {time}"),
                            },
                            text,
                            seq,
                            true,
                            Some(agent),
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
                .child(self.copyable_from(
                    format!("{} · {time}", agent.agent),
                    text,
                    seq,
                    false,
                    Some(agent),
                    cx,
                ))
                .child(bubble(false).child(self.markdown(
                    agents,
                    text,
                    &seq.to_string(),
                    which,
                    cx,
                )))
                .into_any_element(),
            ThreadItem::Permission { id, tool_name, summary, state, input } => {
                let diff = edit_diff(tool_name, input);
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
                    // What it would change, for an edit.
                    .when_some(diff, |d, diff| {
                        d.child(self.markdown(agents, &diff, &format!("{seq}-diff"), which, cx))
                    })
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
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.decide(key, true, None, cx),
                                    )),
                                )
                                .child(
                                    Self::small_button(
                                        format!("thread-deny-{id}"),
                                        "Deny",
                                        DENY,
                                        0xffffff12,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.decide(key, false, None, cx),
                                    )),
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
                        false,
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
            // What the agent put in itself: its summary; Copy, all of it.
            ThreadItem::Notice { tag, summary, text } => div()
                .max_w(relative_width(0.86))
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .border_1()
                .border_color(ui(0xffffff14))
                .bg(ui(0xffffff05))
                .child(self.copyable(
                    format!("⚙ {} · {time}", tag.replace('-', " ")),
                    text,
                    seq,
                    false,
                    cx,
                ))
                .child(div().text_size(px(12.5)).text_color(ui(0xffffffb0)).child(self.markdown(
                    agents,
                    summary,
                    &seq.to_string(),
                    which,
                    cx,
                )))
                .into_any_element(),
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
        // Undone in the agent (rewound): faded, its prompt saying so.
        let rewound = entry.rewound;
        let starts_rewound = rewound && matches!(entry.item, ThreadItem::Prompt { .. });
        div()
            .group(SharedString::from(format!("entry-{seq}")))
            .flex_none()
            .flex()
            .flex_col()
            .rounded(px(8.))
            .when(found, |d| d.bg(ui(0xe0af6826)).border_1().border_color(ui(0xe0af6888)))
            .when(starts_rewound, |d| {
                d.child(
                    div()
                        .flex()
                        .justify_center()
                        .pb(px(4.))
                        .text_size(px(11.))
                        .text_color(ui(0xffffff70))
                        .child("↶ rewound in the agent: not in its conversation any more"),
                )
            })
            .child(div().when(rewound, |d| d.opacity(0.4)).child(element))
            .px(px(16.))
            .pt(px(if ix == 0 { 14. } else { 12. }))
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
            // In the panel (Agents has a column for it): who it's with, and
            // what can be done.
            .when(which == Shown::Panel, |d| d.child(self.agent_bar(agents, agent, which, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .text_size(px(13.))
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
                    // Only the entries in view are drawn (and measured once).
                    .child(
                        gpui::list(
                            agents.list.0.clone(),
                            cx.processor(move |this, ix, _, cx| this.thread_item(which, ix, cx)),
                        )
                        .size_full(),
                    ),
            )
            // What it's doing now (a tool in use), while it works.
            .when_some(
                agent
                    .message
                    .as_deref()
                    .filter(|m| agent.state == AgentState::Working && !m.is_empty()),
                |d, doing| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(18.))
                            .pb(px(2.))
                            .text_size(px(11.5))
                            .text_color(ui(0xffffff80))
                            .child(div().flex_none().text_color(ui(color)).child("●"))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(doing.lines().next().unwrap_or_default().to_owned()),
                            ),
                    )
                },
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
            .child({
                let pane = agent.pane;
                div()
                    .id("clear-thread")
                    .text_size(px(11.))
                    .text_color(ui(0xffffff50))
                    .hover(|d| d.text_color(ui(0xffffffb0)))
                    .cursor_pointer()
                    .child("Clear thread…")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.clear_thread(pane, window, cx)
                    }))
            })
    }

    /// Forget an agent's thread (asked first): its terminal isn't touched.
    fn clear_thread(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Clear this agent's thread?",
            Some("Its messages here are forgotten. Its terminal and conversation aren't touched."),
            &["Clear", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let request = Request::ThreadClear { tab: ids::local_id(pane) };
            let Ok(answer) =
                this.update(cx, |_, cx| app_state::control_on(ids::server_of(pane), request, cx))
            else {
                return;
            };
            if let Ok(Response::Error { message }) = answer.await {
                let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
            }
            let _ = this.update(cx, |this, cx| this.refresh_agents(cx));
        })
        .detach();
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
/// The agents in their order, each followed (at once) by those it started,
/// in theirs; one whose starter isn't listed stays where it is.
pub(super) fn nested(agents: Vec<(AgentInfo, String)>) -> Vec<(AgentInfo, String)> {
    let listed: HashSet<PaneId> = agents.iter().map(|(a, _)| a.pane).collect();
    let parent = |a: &AgentInfo| a.parent.filter(|p| listed.contains(p) && *p != a.pane);
    let mut out = Vec::with_capacity(agents.len());
    let mut placed = HashSet::new();
    fn place(
        at: usize,
        agents: &[(AgentInfo, String)],
        parent: &dyn Fn(&AgentInfo) -> Option<PaneId>,
        placed: &mut HashSet<PaneId>,
        out: &mut Vec<(AgentInfo, String)>,
    ) {
        if !placed.insert(agents[at].0.pane) {
            return;
        }
        out.push(agents[at].clone());
        let pane = agents[at].0.pane;
        for (i, (child, _)) in agents.iter().enumerate() {
            if parent(child) == Some(pane) {
                place(i, agents, parent, placed, out);
            }
        }
    }
    for (i, (agent, _)) in agents.iter().enumerate() {
        // From the top of each line of starters (a loop: from anywhere).
        let mut top = agent;
        let mut seen = HashSet::from([top.pane]);
        while let Some(p) = parent(top) {
            let Some((up, _)) = agents.iter().find(|(a, _)| a.pane == p) else { break };
            if !seen.insert(up.pane) {
                break;
            }
            top = up;
        }
        let top = agents.iter().position(|(a, _)| a.pane == top.pane).unwrap_or(i);
        place(top, &agents, &parent, &mut placed, &mut out);
        place(i, &agents, &parent, &mut placed, &mut out);
    }
    out
}

/// How many starters up `agent` is listed under.
pub(super) fn depth(agents: &[(AgentInfo, String)], agent: &AgentInfo) -> usize {
    let mut depth = 0;
    let mut at = agent;
    while let Some(p) = at.parent
        && depth < 6
        && let Some((up, _)) = agents.iter().find(|(a, _)| a.pane == p && a.pane != at.pane)
    {
        depth += 1;
        at = up;
    }
    depth
}

pub(super) fn from_label(from: &artty_core::control::AskFrom) -> String {
    match &from.agent {
        Some(agent) => format!("from {} · {agent}", from.title),
        None => format!("from {}", from.title),
    }
}

/// Code's colors, in the app's palette.
fn code_color(kind: crate::highlight::Kind) -> Hsla {
    use crate::highlight::Kind;
    ui(match kind {
        Kind::Keyword => 0xbb9af7ff,
        Kind::String => 0x9ece6aff,
        Kind::Comment => 0x7f86a8ff,
        Kind::Number => 0xff9e64ff,
        Kind::Type => 0x2ac3deff,
        Kind::Call => 0x7aa2f7ff,
        Kind::Added => 0x9ece6aff,
        Kind::Removed => 0xf7768eff,
        Kind::Hunk => 0x7dcfffff,
    })
    .into()
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

/// What an edit tool (Claude Code's Edit, MultiEdit, Write) would change,
/// as a markdown diff block: its lines removed and added, the first few.
fn edit_diff(tool: &str, input: &serde_json::Value) -> Option<String> {
    const MOST: usize = 24;
    let text = |v: &serde_json::Value, key: &str| v.get(key)?.as_str().map(str::to_owned);
    let mut lines: Vec<String> = Vec::new();
    let mut change = |old: &str, new: &str| {
        lines.extend(old.lines().map(|l| format!("-{l}")));
        lines.extend(new.lines().map(|l| format!("+{l}")));
    };
    match tool {
        "Edit" => change(&text(input, "old_string")?, &text(input, "new_string")?),
        "MultiEdit" => {
            for edit in input.get("edits")?.as_array()? {
                change(&text(edit, "old_string")?, &text(edit, "new_string")?);
            }
        },
        "Write" => change("", &text(input, "content")?),
        _ => return None,
    }
    if lines.is_empty() {
        return None;
    }
    let more = lines.len().saturating_sub(MOST);
    lines.truncate(MOST);
    if more > 0 {
        lines.push(format!("… {more} more lines"));
    }
    // (A fence longer than any run of backticks in it.)
    let fence = "`".repeat(3.max(lines.iter().map(|l| longest_run(l, '`')).max().unwrap_or(0) + 1));
    Some(format!("{fence}diff\n{}\n{fence}", lines.join("\n")))
}

fn longest_run(text: &str, c: char) -> usize {
    let (mut most, mut run) = (0, 0);
    for ch in text.chars() {
        run = if ch == c { run + 1 } else { 0 };
        most = most.max(run);
    }
    most
}

#[cfg(test)]
mod tests {
    #[test]
    fn edits_as_diffs() {
        let edit =
            serde_json::json!({ "file_path": "a.rs", "old_string": "x\ny", "new_string": "z" });
        assert_eq!(super::edit_diff("Edit", &edit).unwrap(), "```diff\n-x\n-y\n+z\n```");
        let write = serde_json::json!({ "file_path": "a.md", "content": "```\ncode\n```" });
        assert_eq!(super::edit_diff("Write", &write).unwrap(), "````diff\n+```\n+code\n+```\n````");
        let long = serde_json::json!({ "content": "l\n".repeat(30) });
        assert!(super::edit_diff("Write", &long).unwrap().contains("… 6 more lines"));
        assert_eq!(super::edit_diff("Bash", &serde_json::json!({ "command": "ls" })), None);
    }

    #[test]
    fn started_ones_under_their_starter() {
        let agent = |pane: u64, parent: Option<u64>| {
            let json = serde_json::json!({
                "pane": pane, "agent": "claude", "title": "", "workspace": "", "state": "idle",
                "pending": 0, "unread": 0, "parent": parent,
            });
            (serde_json::from_value::<artty_core::control::AgentInfo>(json).unwrap(), String::new())
        };
        let order = |list: Vec<(artty_core::control::AgentInfo, String)>| {
            let nested = super::nested(list);
            let panes: Vec<u64> = nested.iter().map(|(a, _)| a.pane).collect();
            let depths: Vec<usize> = nested.iter().map(|(a, _)| super::depth(&nested, a)).collect();
            (panes, depths)
        };
        // 4 started 5, which started 2; 1 started by one not listed.
        let list = vec![
            agent(1, Some(9)),
            agent(2, Some(5)),
            agent(3, None),
            agent(4, None),
            agent(5, Some(4)),
        ];
        assert_eq!(order(list), (vec![1, 4, 5, 2, 3], vec![0, 0, 1, 2, 0]));
        // A loop doesn't lose anyone.
        let (panes, _) = order(vec![agent(1, Some(2)), agent(2, Some(1))]);
        assert_eq!(panes.len(), 2);
    }

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
