//! The palette, as in IDEs: ⌘P goes anywhere — every pane on every server,
//! by its title, workspace, folder, program or agent (one out of sight is
//! brought into view) — and ⌘⇧P (or `>` first) runs any command, with its
//! keys shown. Matching is fuzzy (see `fuzzy`).
//!
//! It also picks where a message from a thread goes ("Send to…"): another
//! agent (an ask, saying where it's from), or a copy of the conversation it
//! came from, given what's typed as its task.

use std::collections::HashSet;

use gpui::Action;

use super::*;

/// The most rows shown.
const SHOWN: usize = 50;

pub(super) struct Palette {
    query: String,
    places: Vec<Place>,
    commands: Vec<Command>,
    selected: usize,
    sending: Option<Sending>,
}

/// A message from an agent's thread, to send on.
pub(super) struct Sending {
    /// The agent whose thread it's from (none: a past conversation's), its
    /// machine, its title, and its conversation's id (a copy of it can be
    /// started).
    pub from: Option<PaneId>,
    pub server: ServerId,
    pub title: String,
    pub session: Option<String>,
    pub text: String,
}

/// A pane to go to.
struct Place {
    pane: PaneId,
    icon: String,
    title: String,
    detail: String,
    /// What a query is matched against.
    text: String,
    /// Shown in this window now.
    here: bool,
    attention: bool,
    /// An agent runs in it.
    agent: bool,
}

struct Command {
    action: Box<dyn Action>,
    name: String,
    keys: String,
}

/// What a row is.
enum Row<'a> {
    Place(&'a Place),
    Command(&'a Command),
    /// A copy of the conversation a message is sent from.
    Copy,
}

/// "artty::ToggleFiles" as "Toggle Files".
fn command_name(action: &str) -> String {
    let name = action.rsplit("::").next().unwrap_or(action);
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

impl Palette {
    fn commanding(&self) -> bool {
        self.sending.is_none() && self.query.starts_with('>')
    }

    /// The rows matching the query, best first.
    fn rows(&self) -> Vec<Row<'_>> {
        if self.commanding() {
            let query = self.query[1..].trim();
            let mut scored: Vec<(i32, &Command)> = self
                .commands
                .iter()
                .filter_map(|c| Some((crate::fuzzy::score(query, &c.name)?, c)))
                .collect();
            if !query.is_empty() {
                scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            }
            return scored.into_iter().take(SHOWN).map(|(_, c)| Row::Command(c)).collect();
        }
        let query = self.query.trim();
        let mut scored: Vec<(i32, &Place)> = self
            .places
            .iter()
            .filter_map(|p| Some((crate::fuzzy::score(query, &p.text)?, p)))
            .collect();
        if query.is_empty() {
            // Nothing typed: what needs you, then this window's, then the rest.
            scored.sort_by_key(|(_, p)| (!p.attention, !p.here));
        } else {
            scored.sort_by_key(|(score, p)| (std::cmp::Reverse(*score), !p.here));
        }
        let mut rows: Vec<Row> =
            scored.into_iter().take(SHOWN).map(|(_, p)| Row::Place(p)).collect();
        if self.sending.as_ref().is_some_and(|s| s.session.is_some()) {
            rows.push(Row::Copy);
        }
        rows
    }
}

/// What an agent is given, sent on from another's thread: where from, and
/// how to ask there for more.
fn sent(sending: &Sending) -> String {
    let (title, text) = (&sending.title, sending.text.trim());
    let mut out = match sending.from.map(ids::local_id) {
        Some(from) => format!(
            "From {title} (pane {from}), sent on by the user:\n\n{text}\n\n(To ask it more: \
             `artty ask --tab {from} \"...\"`"
        ),
        None => format!(
            "From a past conversation, {title}, sent on by the user:\n\n{text}\n\n(It's over"
        ),
    };
    if let Some(session) = &sending.session {
        out.push_str(&format!(
            "; to ask a copy of its conversation, which leaves it as it is: `artty new \
             --background --agent claude --fork {session} --task \"...\"`"
        ));
    }
    out.push_str(".)");
    out
}

impl WindowView {
    /// ⌘P (`commands`: ⌘⇧P, starting at `>`).
    pub(super) fn open_palette(
        &mut self,
        commanding: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        self.picker = None;
        let places = self.palette_places(cx);
        let mut commands: Vec<Command> = window
            .available_actions(cx)
            .into_iter()
            .filter(|action| {
                action.name().starts_with("artty::") && action.name() != "artty::SendText"
            })
            .map(|action| {
                let keys = window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .map(|binding| binding.keystrokes().iter().map(|k| k.to_string()).collect())
                    .unwrap_or_default();
                Command { name: command_name(action.name()), action, keys }
            })
            .collect();
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        commands.dedup_by(|a, b| a.name == b.name);
        self.marked_text = None;
        self.palette = Some(Palette {
            query: if commanding { ">".to_owned() } else { String::new() },
            places,
            commands,
            selected: 0,
            sending: None,
        });
        cx.notify();
    }

    /// "Send to…": the palette, listing the agents `sending` can go to (on
    /// its machine: an ask is), and a copy of its conversation.
    pub(super) fn open_send(&mut self, sending: Sending, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            self.finish_rename(true, cx);
        }
        self.picker = None;
        let places = self
            .palette_places(cx)
            .into_iter()
            // (Nothing to send: only a copy.)
            .filter(|p| {
                p.agent
                    && Some(p.pane) != sending.from
                    && ids::server_of(p.pane) == sending.server
                    && !sending.text.trim().is_empty()
            })
            .collect();
        self.marked_text = None;
        self.palette = Some(Palette {
            query: String::new(),
            places,
            commands: Vec::new(),
            selected: 0,
            sending: Some(sending),
        });
        cx.notify();
    }

    /// Every pane, as the palette lists them.
    fn palette_places(&self, cx: &mut Context<Self>) -> Vec<Place> {
        let here: HashSet<PaneId> = self.tabs.iter().flat_map(|t| t.root.panes()).collect();
        let names: HashMap<ServerId, String> =
            app_state::servers(cx).into_iter().map(|(id, name, _)| (id, name)).collect();
        let several = names.len() > 1;
        let home = std::env::var("HOME").ok();
        let mut places: Vec<Place> = cx
            .global::<Shared>()
            .panes
            .iter()
            .map(|(&pane, state)| {
                let info = &state.info;
                let server = ids::server_of(pane);
                let cwd = info.cwd.as_ref().map(|cwd| {
                    let cwd = cwd.display().to_string();
                    match home.as_deref().filter(|_| server == LOCAL) {
                        Some(home) if cwd.starts_with(home) => format!("~{}", &cwd[home.len()..]),
                        _ => cwd,
                    }
                });
                let mut detail: Vec<String> = Vec::new();
                if several {
                    detail.extend(names.get(&server).cloned());
                }
                detail.extend(info.workspace_name.clone());
                detail.extend(cwd);
                if let Some(agent) = &state.agent {
                    detail.push(agent.clone());
                } else if let Some(process) = &info.process {
                    detail.push(process.clone());
                }
                let detail = detail.join(" · ");
                Place {
                    pane,
                    icon: state.icon.clone(),
                    text: format!("{} {detail}", info.title),
                    title: info.title.clone(),
                    detail,
                    here: here.contains(&pane),
                    attention: matches!(state.icon.as_str(), "waiting" | "attention")
                        || info.needs_attention,
                    agent: state.agent.is_some(),
                }
            })
            .collect();
        places.sort_by_key(|p| p.pane);
        places
    }

    pub(super) fn palette_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(palette) = self.palette.as_mut() else { return };
        let count = palette.rows().len();
        match event.keystroke.key.as_str() {
            "escape" => self.palette = None,
            "enter" => return self.choose_palette(window, cx),
            "up" => palette.selected = palette.selected.saturating_sub(1),
            "down" => palette.selected = (palette.selected + 1).min(count.saturating_sub(1)),
            "backspace" => {
                palette.query.pop();
                palette.selected = 0;
            },
            // Text arrives through the input handler.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn palette_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.as_mut() {
            palette.query.push_str(text);
            palette.selected = 0;
            cx.notify();
        }
    }

    fn choose_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.as_ref().is_some_and(|p| p.sending.is_some()) {
            return self.choose_send(cx);
        }
        let Some(palette) = self.palette.take() else { return };
        cx.notify();
        match palette.rows().get(palette.selected) {
            Some(Row::Command(command)) => {
                let action = command.action.boxed_clone();
                // After this: the action may need this view.
                window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
            },
            Some(Row::Place(place)) if place.here => self.focus_pane(place.pane, cx),
            // Out of sight: its server brings it into view (and its window
            // forward).
            Some(Row::Place(place)) => {
                let server = ids::server_of(place.pane);
                let request = Request::Focus { tab: ids::local_id(place.pane) };
                // (Sent now; no need of the answer.)
                drop(app_state::control_on(server, request, cx));
            },
            Some(Row::Copy) | None => {},
        }
    }

    /// The message sent to the agent chosen, or a copy of its conversation
    /// started with the task typed.
    fn choose_send(&mut self, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.as_ref() else { return };
        let Some(sending) = palette.sending.as_ref() else { return };
        let server = sending.server;
        let from = sending.from.map(ids::local_id);
        let task = palette.query.trim().to_owned();
        let (request, to) = match palette.rows().get(palette.selected) {
            Some(Row::Place(place)) => (
                Request::Ask {
                    tab: ids::local_id(place.pane),
                    text: sent(sending),
                    raw: false,
                    wait: false,
                    timeout_secs: None,
                    thread: from,
                    back: false,
                },
                place.title.clone(),
            ),
            Some(Row::Copy) if task.is_empty() => {
                return self.show_toast("Type the copy's task first", cx);
            },
            Some(Row::Copy) => (
                Request::New {
                    cwd: None,
                    command: vec!["claude".into()],
                    title: None,
                    focus: false,
                    split: None,
                    target: None,
                    workspace: None,
                    worktree: None,
                    task: Some(task),
                    thread: from,
                    back: false,
                    fork: sending.session.clone(),
                },
                format!("a copy of {}", sending.title),
            ),
            _ => return,
        };
        self.palette = None;
        cx.notify();
        let answer = app_state::control_on(server, request, cx);
        cx.spawn(async move |this, cx| {
            let result = answer.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Response::Error { message }) => {
                        this.show_toast(format!("Not sent: {message}"), cx)
                    },
                    Err(_) => this.show_toast("Not sent: no answer from its server", cx),
                    Ok(Response::Started { id, .. }) | Ok(Response::Tab { id }) => {
                        this.show_toast(format!("Started {to}"), cx);
                        if let Some(agents) = this.agents_in_mut(Shown::View) {
                            agents.selected = Some(ids::tag(server, id));
                        }
                    },
                    Ok(_) => this.show_toast(format!("Sent to {to}"), cx),
                }
                this.refresh_agents(cx);
            });
        })
        .detach();
    }

    pub(super) fn palette_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let palette = self.palette.as_ref()?;
        let rows = palette.rows();
        let marked = self.marked_text.as_deref().unwrap_or_default();
        let items: Vec<gpui::AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let (icon, title, detail, here) = match row {
                    Row::Place(p) => (p.icon.as_str(), p.title.clone(), p.detail.clone(), p.here),
                    Row::Command(c) => ("", c.name.clone(), c.keys.clone(), true),
                    Row::Copy => {
                        let from = palette.sending.as_ref().map_or("", |s| s.title.as_str());
                        let detail = match palette.query.trim() {
                            "" => "type its task: the conversation comes with it".to_owned(),
                            task => format!("task: {task}"),
                        };
                        ("", format!("A copy of {from}"), detail, true)
                    },
                };
                div()
                    .id(("palette-item", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .when(i == palette.selected, |d| d.bg(ui(0x3478f6ff)))
                    .when(i != palette.selected, |d| d.hover(|d| d.bg(ui(0xffffff10))))
                    .when(matches!(row, Row::Place(_)), |d| d.child(self.icon(icon)))
                    .child(
                        Self::one_line(title).flex_shrink_0().max_w(px(260.)).text_color(if here {
                            ui(0xffffffee)
                        } else {
                            ui(0xffffffb0)
                        }),
                    )
                    .child(
                        Self::one_line(detail)
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.5))
                            .text_color(ui(0xffffff80))
                            .when(matches!(row, Row::Command(_)), |d| d.flex().justify_end()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(palette) = this.palette.as_mut() {
                            palette.selected = i;
                        }
                        this.choose_palette(window, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let empty = items.is_empty();
        let hint = if palette.sending.as_ref().is_some_and(|s| s.text.trim().is_empty()) {
            "What should the copy do? (Enter starts it)"
        } else if palette.sending.is_some() {
            "Send to an agent… (or type a task for a copy)"
        } else if palette.commanding() {
            "Run a command…"
        } else {
            "Go to a pane: title, workspace, folder, program… (> for commands)"
        };
        let panel = div()
            .id("palette")
            .absolute()
            .top(px(48.))
            .left_0()
            .right_0()
            .mx_auto()
            .w(px(600.))
            .max_h(px(480.))
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
                    .text_color(if palette.query.is_empty() && marked.is_empty() {
                        ui(0xffffff60)
                    } else {
                        ui(0xffffffee)
                    })
                    .child(if palette.query.is_empty() && marked.is_empty() {
                        hint.to_owned()
                    } else {
                        format!("{}{marked}▏", palette.query)
                    }),
            )
            .when_some(
                palette.sending.as_ref().filter(|s| !s.text.trim().is_empty()),
                |d, sending| {
                    let text: String =
                        sending.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let text = match text.char_indices().nth(160) {
                        Some((at, _)) => format!("{}…", &text[..at]),
                        None => text,
                    };
                    d.child(
                        Self::one_line(format!("From {}: {text}", sending.title))
                            .px(px(10.))
                            .pb(px(4.))
                            .text_size(px(11.5))
                            .text_color(ui(0xffffff80)),
                    )
                },
            )
            .child(
                div()
                    .id("palette-rows")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .overflow_y_scroll()
                    .children(items),
            )
            .when(empty, |d| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .text_color(ui(0xffffff70))
                        .child("Nothing matches"),
                )
            });
        let backdrop =
            div().id("palette-backdrop").absolute().top_0().left_0().size_full().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.palette = None;
                    cx.notify();
                }),
            );
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn what_is_sent_says_where_from() {
        let mut sending = super::Sending {
            from: Some(7),
            server: 0,
            title: "parser".into(),
            session: Some("abc-1".into()),
            text: "  the method  ".into(),
        };
        let text = super::sent(&sending);
        assert!(text.starts_with("From parser (pane 7), sent on by the user:\n\nthe method\n\n"));
        assert!(text.contains("artty ask --tab 7"), "{text}");
        assert!(text.contains("--fork abc-1"), "{text}");
        sending.session = None;
        assert!(!super::sent(&sending).contains("--fork"));
        sending.from = None;
        assert!(super::sent(&sending).starts_with("From a past conversation, parser,"));
    }

    #[test]
    fn command_names_read_as_words() {
        assert_eq!(super::command_name("artty::ToggleFiles"), "Toggle Files");
        assert_eq!(super::command_name("artty::NewTab"), "New Tab");
    }
}
