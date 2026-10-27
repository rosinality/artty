//! Search everything (⌘⇧F): what every pane on every server has printed,
//! history too, and coding agents' conversations on those machines (Claude
//! Code's, Codex's). Each server searches its own, so only matches travel.
//! A line found goes to its pane, selected; a conversation found carries on
//! in a new tab, in its directory.

use artty_core::protocol::{OutputHit, SessionHit};

use super::*;

/// How long typing pauses before a search.
const PAUSE: Duration = Duration::from_millis(300);

pub(super) struct SearchAll {
    query: String,
    /// Which search the results are of (searches overtaken are dropped).
    generation: u64,
    searching: usize,
    lines: Vec<OutputHit>,
    sessions: Vec<(ServerId, SessionHit)>,
    selected: usize,
}

enum Found<'a> {
    Line(&'a OutputHit),
    Session(ServerId, &'a SessionHit),
}

impl SearchAll {
    fn rows(&self) -> Vec<Found<'_>> {
        self.lines
            .iter()
            .map(Found::Line)
            .chain(self.sessions.iter().map(|(server, hit)| Found::Session(*server, hit)))
            .collect()
    }
}

/// "3 hours ago", for a time in seconds since the epoch.
fn ago(secs: u64) -> String {
    let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match now.saturating_sub(secs) {
        s if s < 3600 => format!("{} min ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

impl WindowView {
    pub(super) fn open_search_all(&mut self, cx: &mut Context<Self>) {
        self.palette = None;
        self.picker = None;
        self.marked_text = None;
        if self.search_all.is_none() {
            self.search_all = Some(SearchAll {
                query: String::new(),
                generation: 0,
                searching: 0,
                lines: Vec::new(),
                sessions: Vec::new(),
                selected: 0,
            });
        }
        cx.notify();
    }

    /// Search a moment after typing stops (or at once).
    fn schedule_search_all(&mut self, now: bool, cx: &mut Context<Self>) {
        let Some(search) = self.search_all.as_mut() else { return };
        search.generation += 1;
        let generation = search.generation;
        cx.spawn(async move |this, cx| {
            if !now {
                cx.background_executor().timer(PAUSE).await;
            }
            let _ = this.update(cx, |this, cx| {
                if this.search_all.as_ref().is_some_and(|s| s.generation == generation) {
                    this.run_search_all(cx);
                }
            });
        })
        .detach();
    }

    fn run_search_all(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_all.as_mut() else { return };
        let text = search.query.trim().to_owned();
        search.lines.clear();
        search.sessions.clear();
        search.selected = 0;
        if text.is_empty() {
            search.searching = 0;
            return cx.notify();
        }
        let generation = search.generation;
        let servers: Vec<ServerId> = app_state::servers(cx)
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, _, _)| server)
            .collect();
        search.searching = servers.len() * 2;
        for server in servers {
            let lines = app_state::query_on(server, Query::SearchOutput { text: text.clone() }, cx);
            let sessions =
                app_state::query_on(server, Query::SearchSessions { text: text.clone() }, cx);
            cx.spawn(async move |this, cx| {
                let found = lines.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(search) =
                        this.search_all.as_mut().filter(|s| s.generation == generation)
                    else {
                        return;
                    };
                    search.searching -= 1;
                    if let Ok(Reply::Found(hits)) = found {
                        search.lines.extend(hits);
                    }
                    cx.notify();
                });
                let found = sessions.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(search) =
                        this.search_all.as_mut().filter(|s| s.generation == generation)
                    else {
                        return;
                    };
                    search.searching -= 1;
                    if let Ok(Reply::Sessions(hits)) = found {
                        search.sessions.extend(hits.into_iter().map(|hit| (server, hit)));
                        search.sessions.sort_by_key(|(_, hit)| std::cmp::Reverse(hit.modified));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn search_all_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(search) = self.search_all.as_mut() else { return };
        let count = search.rows().len();
        match event.keystroke.key.as_str() {
            "escape" => self.search_all = None,
            // A result chosen; or, none yet, search now.
            "enter" if count > 0 => return self.choose_found(cx),
            "enter" => self.schedule_search_all(true, cx),
            "up" => search.selected = search.selected.saturating_sub(1),
            "down" => search.selected = (search.selected + 1).min(count.saturating_sub(1)),
            "backspace" => {
                search.query.pop();
                self.schedule_search_all(false, cx);
            },
            // Text arrives through the input handler.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn search_all_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(search) = self.search_all.as_mut() {
            search.query.push_str(text);
            self.schedule_search_all(false, cx);
            cx.notify();
        }
    }

    fn choose_found(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_all.as_ref() else { return };
        match search.rows().get(search.selected) {
            Some(Found::Line(hit)) => {
                let (pane, line, column, length) = (hit.pane, hit.line, hit.column, hit.length);
                // Out of sight: its server brings it into view first.
                if !self.shows(pane) {
                    let request = Request::Focus { tab: ids::local_id(pane) };
                    drop(app_state::control_on(ids::server_of(pane), request, cx));
                }
                self.send(ClientMessage::Reveal { pane, line, column, length }, cx);
                self.search_all = None;
            },
            Some(Found::Session(server, hit)) => {
                let Some(window) = self.window_on(*server) else { return };
                let message = ClientMessage::ResumeSession {
                    window,
                    agent: hit.agent.clone(),
                    session: hit.session.clone(),
                    cwd: hit.cwd.clone(),
                };
                self.send(message, cx);
                self.search_all = None;
            },
            None => {},
        }
        cx.notify();
    }

    pub(super) fn search_all_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let search = self.search_all.as_ref()?;
        let marked = self.marked_text.as_deref().unwrap_or_default();
        let names: HashMap<ServerId, String> =
            app_state::servers(cx).into_iter().map(|(id, name, _)| (id, name)).collect();
        let several = names.len() > 1;
        let section = |label: String| {
            div()
                .px(px(10.))
                .pt(px(8.))
                .pb(px(2.))
                .text_size(px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0xffffff60))
                .child(label)
        };
        let row = |i: usize| {
            div()
                .id(("found", i))
                .flex()
                .flex_col()
                .px(px(10.))
                .py(px(4.))
                .rounded(px(6.))
                .when(i == search.selected, |d| d.bg(rgba(0x3478f6ff)))
                .when(i != search.selected, |d| d.hover(|d| d.bg(rgba(0xffffff10))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(search) = this.search_all.as_mut() {
                        search.selected = i;
                    }
                    this.choose_found(cx);
                }))
        };
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        if !search.lines.is_empty() {
            items
                .push(section(format!("IN TERMINALS · {}", search.lines.len())).into_any_element());
        }
        for (i, hit) in search.lines.iter().enumerate() {
            let state = self.pane_state(hit.pane, cx);
            let mut place: Vec<String> = Vec::new();
            if several {
                place.extend(names.get(&ids::server_of(hit.pane)).cloned());
            }
            place.extend(state.and_then(|s| s.info.workspace_name.clone()));
            place.extend(state.map(|s| s.info.title.clone()));
            items.push(
                row(i)
                    .child(
                        Self::one_line(hit.text.trim().to_owned())
                            .font_family(
                                self.config.font_family.first().cloned().unwrap_or_default(),
                            )
                            .text_color(rgba(0xffffffe0)),
                    )
                    .child(
                        Self::one_line(place.join(" · "))
                            .text_size(px(11.))
                            .text_color(rgba(0xffffff70)),
                    )
                    .into_any_element(),
            );
        }
        if !search.sessions.is_empty() {
            items.push(
                section(format!("IN AGENT SESSIONS · {}", search.sessions.len()))
                    .into_any_element(),
            );
        }
        for (j, (server, hit)) in search.sessions.iter().enumerate() {
            let i = search.lines.len() + j;
            let mut place = vec![hit.agent.clone()];
            if several {
                place.extend(names.get(server).cloned());
            }
            place.extend(hit.cwd.as_ref().map(|c| c.display().to_string()));
            place.push(ago(hit.modified));
            items.push(
                row(i)
                    .child(
                        Self::one_line(if hit.title.is_empty() {
                            "(untitled)".to_owned()
                        } else {
                            hit.title.clone()
                        })
                        .text_color(rgba(0xffffffee)),
                    )
                    .child(
                        Self::one_line(hit.snippet.clone())
                            .text_size(px(11.5))
                            .text_color(rgba(0xffffffb0)),
                    )
                    .child(
                        Self::one_line(place.join(" · "))
                            .text_size(px(11.))
                            .text_color(rgba(0xffffff70)),
                    )
                    .into_any_element(),
            );
        }
        let status = match (search.searching, items.is_empty(), search.query.trim().is_empty()) {
            (_, _, true) => Some("What every pane printed, and agents' conversations"),
            (n, _, _) if n > 0 => Some("Searching…"),
            (_, true, _) => Some("Nothing found"),
            _ => None,
        };
        let panel = div()
            .id("search-all")
            .absolute()
            .top(px(48.))
            .left_0()
            .right_0()
            .mx_auto()
            .w(px(720.))
            .max_h(px(560.))
            .occlude()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(6.))
            .rounded(px(10.))
            .bg(rgba(0x262a33f8))
            .border_1()
            .border_color(rgba(0xffffff26))
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .px(px(10.))
                    .py(px(7.))
                    .mb(px(2.))
                    .rounded(px(6.))
                    .bg(rgba(0x0000004d))
                    .text_color(if search.query.is_empty() && marked.is_empty() {
                        rgba(0xffffff60)
                    } else {
                        rgba(0xffffffee)
                    })
                    .child(if search.query.is_empty() && marked.is_empty() {
                        "Search all terminals and agent sessions…".to_owned()
                    } else {
                        format!("{}{marked}▏", search.query)
                    }),
            )
            .when_some(status, |d, status| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(4.))
                        .text_size(px(12.))
                        .text_color(rgba(0xffffff70))
                        .child(status),
                )
            })
            .child(
                div()
                    .id("found-rows")
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .overflow_y_scroll()
                    .children(items),
            );
        let backdrop =
            div().id("search-all-backdrop").absolute().top_0().left_0().size_full().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.search_all = None;
                    cx.notify();
                }),
            );
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(panel))
    }
}
