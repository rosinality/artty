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

    pub(super) fn toggle_agents(&mut self, cx: &mut Context<Self>) {
        if self.agents.take().is_none() {
            self.close_search(cx);
            self.picker = None;
            self.activity = None;
            if self.renaming.is_some() {
                self.finish_rename(true, cx);
            }
            self.agents = Some(Agents::default());
            self.refresh_in(Shown::View, cx);
        }
        cx.notify();
    }

    /// The chosen agent's terminal, live, while Agents shows it: its
    /// server sends its screens though no window shows it (`peek`).
    pub(super) fn follow_peek(&mut self, cx: &mut Context<Self>) {
        let want = self.agents.as_ref().and_then(|a| a.selected);
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
            "escape" => {
                self.agents = None;
                cx.notify();
            },
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
        div()
            .size_full()
            .flex()
            .min_h_0()
            .child(self.agent_list(agents, several, cx))
            .child(self.agent_thread(agents, selected.map(|(a, _)| a), Shown::View, cx))
            .children(
                selected.map(|(agent, server)| self.agent_details(agent, server, several, cx)),
            )
    }

    fn agent_list(
        &self,
        agents: &Agents,
        several: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let rows = agents.agents.iter().map(|(agent, server)| {
            let pane = agent.pane;
            let chosen = agents.selected == Some(pane);
            let (label, color) = state_label(agent.state);
            let place = if several {
                format!("{server} · {}", agent.workspace)
            } else {
                agent.workspace.clone()
            };
            let mut badges = Vec::new();
            if agent.pending > 0 {
                badges.push(("permission", WAITING));
            }
            if agent.unread > 0 {
                badges.push(("reply", DONE));
            }
            div()
                .id(SharedString::from(format!("agent-{pane}")))
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(8.))
                .py(px(5.))
                .rounded(px(6.))
                .when(chosen, |d| d.bg(ui(0xffffff1c)))
                .hover(|d| d.bg(ui(0xffffff10)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().size(px(6.)).flex_none().rounded_full().bg(ui(color)))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(ui(0xffffffe8))
                                .child(format!("{} · {}", agent.title, agent.agent)),
                        )
                        .children(badges.into_iter().map(|(text, color)| {
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(6.))
                                .text_size(px(10.))
                                .bg(ui(color))
                                .text_color(ui(0x1a1b26ff))
                                .child(text)
                        })),
                )
                .child(
                    div()
                        .pl(px(12.))
                        .text_size(px(11.))
                        .text_color(ui(0xffffff70))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(match &agent.message {
                            Some(message) if !message.is_empty() => format!("{label} · {message}"),
                            _ => format!("{label} · {place}"),
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select_agent(pane, cx)))
        });
        div()
            .id("agent-list")
            .w(px(260.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(8.))
            // As the sidebar's tabs.
            .text_size(px(12.5))
            .border_r_1()
            .border_color(ui(0xffffff12))
            .overflow_y_scroll()
            .child(
                div()
                    .px(px(8.))
                    .pb(px(4.))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70))
                    .child(format!("AGENTS · {}", agents.agents.len())),
            )
            .children(rows)
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
                            .child(div().text_size(px(11.)).text_color(ui(0xffffff60)).child(
                                match (ask, from) {
                                    (_, Some(from)) => format!("{} · {time}", from_label(from)),
                                    (Some(_), None) => format!("you, by artty ask · {time}"),
                                    (None, None) => format!("you · {time}"),
                                },
                            ))
                            .child(bubble(true).child(clip(text))),
                    )
                    .into_any_element(),
                ThreadItem::Reply { text } => div()
                    .max_w(relative_width(0.86))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui(0xffffff60))
                            .child(format!("{} · {time}", agent.agent)),
                    )
                    .child(bubble(false).child(clip(text)))
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
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(ui(0xffffff70))
                                .child(format!("{who} → {target} · {time}")),
                        )
                        .child(div().text_color(ui(0xffffffd0)).child(clip(text)))
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
                                .child(clip(reply))
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
                .flex_none()
                .rounded(px(8.))
                .when(found, |d| d.bg(ui(0xe0af6826)).border_1().border_color(ui(0xe0af6888)))
                .child(element)
        });
        let (label, color) = state_label(agent.state);
        let pane = agent.pane;
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
                    .when(which == Shown::View, |d| {
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
        let peek = self.terminal_peek(agent.pane, wide - 2. * PADDING, cx);
        let width = if peek.is_some() { wide } else { 240. };
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
            .children(peek.map(|peek| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(ui(0xffffff60))
                            .child("TERMINAL · click to open"),
                    )
                    .child(peek)
            }))
            .child(row(
                "Status",
                match &agent.message {
                    Some(message) if !message.is_empty() => format!("{label}: {message}"),
                    _ => label.to_owned(),
                },
            ))
            .when(several, |d| d.child(row("Machine", server.to_owned())))
            .child(row("Workspace", agent.workspace.clone()))
            .children(agent.cwd.as_ref().map(|cwd| row("Folder", cwd.display().to_string())))
            .children(agent.ask.map(|id| row("Working on", format!("ask {id}"))))
            .when(agent.pending > 0, |d| {
                let requests = match agent.pending {
                    1 => "1 permission request".to_owned(),
                    n => format!("{n} permission requests"),
                };
                d.child(row("Needs you", requests))
            })
            .children(agent.last_ms.map(|ms| row("Last activity", clock(ms))))
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

fn relative_width(fraction: f32) -> gpui::DefiniteLength {
    gpui::relative(fraction)
}
