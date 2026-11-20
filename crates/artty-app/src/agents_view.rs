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
const DONE: u32 = 0x9ece6aff;
const BUSY: u32 = 0x8fb4ffff;
const DENY: u32 = 0xf7768eff;

#[derive(Default)]
pub(super) struct Agents {
    /// Every connected server's agents, pane ids tagged (see `ids`), with
    /// their server's name.
    agents: Vec<(AgentInfo, String)>,
    /// The agent shown (its pane), and its thread.
    selected: Option<PaneId>,
    thread: Vec<ThreadEntry>,
    thread_of: Option<PaneId>,
    scroll: gpui::ScrollHandle,
    loading: bool,
    loaded: bool,
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
            self.refresh_agents(cx);
        }
        cx.notify();
    }

    /// Ask every connected server for its agents, and the chosen one's
    /// thread.
    pub(super) fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        let Some(agents) = self.agents.as_mut().filter(|a| !a.loading) else { return };
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
            (pane, app_state::control_on(ids::server_of(pane), request, cx))
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
                Some((pane, answer)) => match answer.await {
                    Ok(Response::Thread { entries, .. }) => Some((pane, entries)),
                    _ => Some((pane, Vec::new())),
                },
                None => None,
            };
            let _ = this.update(cx, |this, cx| {
                let Some(agents) = this.agents.as_mut() else { return };
                agents.loading = false;
                agents.loaded = true;
                agents.agents = found;
                // The first, if none is chosen (or the chosen one is gone).
                let chosen =
                    agents.selected.filter(|p| agents.agents.iter().any(|(a, _)| a.pane == *p));
                agents.selected = chosen.or_else(|| agents.agents.first().map(|(a, _)| a.pane));
                // (Only the thread of the agent still chosen.)
                if let Some((pane, entries)) = thread.filter(|(p, _)| agents.selected == Some(*p)) {
                    let grew =
                        agents.thread_of != Some(pane) || entries.len() != agents.thread.len();
                    agents.thread_of = Some(pane);
                    agents.thread = entries;
                    if grew {
                        agents.scroll.scroll_to_bottom();
                    }
                }
                cx.notify();
            });
            // A new choice gets its thread at once.
            let _ = this.update(cx, |this, cx| {
                if this
                    .agents
                    .as_ref()
                    .is_some_and(|a| a.thread_of.is_none() && a.selected.is_some())
                {
                    this.refresh_agents(cx);
                }
            });
        })
        .detach();
    }

    fn select_agent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(agents) = self.agents.as_mut() else { return };
        if agents.selected != Some(pane) {
            agents.selected = Some(pane);
            agents.thread.clear();
            agents.thread_of = None;
            self.refresh_agents(cx);
        }
        cx.notify();
    }

    /// Keys while Agents is shown: ↑↓ choose an agent, Enter shows its
    /// terminal, Escape leaves. Returns whether the key was used.
    pub(super) fn agents_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(agents) = self.agents.as_ref() else { return false };
        let panes: Vec<PaneId> = agents.agents.iter().map(|(a, _)| a.pane).collect();
        let at = agents.selected.and_then(|p| panes.iter().position(|&q| q == p));
        match event.keystroke.key.as_str() {
            "escape" => {
                self.agents = None;
                cx.notify();
            },
            "up" | "down" if !panes.is_empty() => {
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
    fn show_agent_terminal(&mut self, pane: PaneId, cx: &mut Context<Self>) {
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
            .child(self.agent_thread(agents, selected.map(|(a, _)| a), cx))
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
                .py(px(6.))
                .rounded(px(7.))
                .when(chosen, |d| d.bg(ui(0xffffff1c)))
                .hover(|d| d.bg(ui(0xffffff10)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().size(px(7.)).flex_none().rounded_full().bg(ui(color)))
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
                        .pl(px(13.))
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

    fn agent_thread(
        &self,
        agents: &Agents,
        agent: Option<&AgentInfo>,
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
        let entries = agents.thread.iter().map(|entry| {
            let time = clock(entry.at_ms);
            let element: gpui::AnyElement = match &entry.item {
                ThreadItem::Prompt { text, ask } => div()
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
                                match ask {
                                    Some(_) => format!("you, by artty ask · {time}"),
                                    None => format!("you · {time}"),
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
            element
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
                            .text_size(px(14.))
                            .text_color(ui(0xffffffee))
                            .child(format!("{} · {}", agent.title, agent.agent)),
                    )
                    .child(div().text_size(px(12.)).text_color(ui(color)).child(label))
                    .child(div().flex_1())
                    .child(
                        Self::small_button(
                            format!("agent-terminal-{pane}"),
                            "Open terminal  ↩",
                            0xffffffd0,
                            0xffffff14,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.show_agent_terminal(pane, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .id("agent-thread")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&agents.scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .px(px(16.))
                            .py(px(14.))
                            .text_size(px(13.))
                            .when(agents.thread.is_empty(), |d| {
                                d.child(div().text_color(ui(0xffffff60)).child(
                                    "Nothing yet: what it's given and says from now on shows here.",
                                ))
                            })
                            .children(entries),
                    ),
            )
    }

    fn agent_details(
        &self,
        agent: &AgentInfo,
        server: &str,
        several: bool,
        _cx: &mut Context<Self>,
    ) -> gpui::Div {
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
            .w(px(240.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(14.))
            .border_l_1()
            .border_color(ui(0xffffff12))
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

fn relative_width(fraction: f32) -> gpui::DefiniteLength {
    gpui::relative(fraction)
}
