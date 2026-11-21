//! The sidebar grouped by agent: instead of tabs by workspace, every
//! agent (each machine's, each workspace's), those an agent started under
//! it, its state on the left and what it has for you on the right; then
//! the tabs with no agent. A click goes to the agent's terminal: the
//! terminal is where you talk to an agent, this is where you see them all.

use artty_core::control::{AgentInfo, AgentState};
use artty_core::protocol::SessionHit;

use super::*;

/// Every agent, as last heard (for the sidebar, grouped by them or not:
/// those an agent started go under it).
#[derive(Default)]
pub(super) struct Known {
    /// Every connected server's agents, pane ids tagged, in the order shown
    /// (each followed by those it started), with their server's name.
    pub(super) agents: Vec<(AgentInfo, String)>,
    loading: bool,
    /// Machines away (reconnecting): their agents as last heard, dimmed.
    away: HashSet<String>,
}

/// The sidebar, grouped by agent: what's folded in it.
#[derive(Default)]
pub(super) struct Grouped {
    /// The tabs with no agent, folded away; the past conversations too;
    /// and the places (machine · workspace) folded.
    others_folded: bool,
    past_folded: bool,
    folded: HashSet<String>,
}

/// Where each agent is listed: before row `i`, the place (machine ·
/// workspace) beginning there, if one does. Only when agents are in more
/// than one place; one an agent started is under it, wherever it is.
fn places(agents: &[(AgentInfo, String)], servers: usize) -> Vec<Option<String>> {
    let listed: HashSet<PaneId> = agents.iter().map(|(a, _)| a.pane).collect();
    let place = |(agent, server): &(AgentInfo, String)| match servers > 1 {
        true => format!("{server} · {}", agent.workspace),
        false => agent.workspace.clone(),
    };
    let roots: Vec<Option<String>> = agents
        .iter()
        .map(|entry| match entry.0.parent.filter(|p| listed.contains(p)) {
            Some(_) => None,
            None => Some(place(entry)),
        })
        .collect();
    let distinct: HashSet<&String> = roots.iter().flatten().collect();
    if distinct.len() < 2 {
        return vec![None; agents.len()];
    }
    let mut last: Option<&String> = None;
    roots
        .iter()
        .map(|root| {
            let root = root.as_ref()?;
            if last == Some(root) {
                return None;
            }
            last = Some(root);
            Some(root.clone())
        })
        .collect()
}

impl WindowView {
    /// Group the sidebar by agent, or back by workspace: kept as the
    /// setting (`sidebar-by-agent`), which every window follows.
    pub(super) fn toggle_grouping(&mut self, cx: &mut Context<Self>) {
        let on = self.grouped.is_none();
        self.grouped = on.then(Grouped::default);
        let saved = artty_core::config_file::ConfigFile::open().and_then(|mut file| {
            file.set_bool("sidebar-by-agent", on);
            file.save()
        });
        if let Err(err) = saved {
            self.show_toast(format!("Couldn't save the setting: {err}"), cx);
        }
        self.refresh_known(cx);
        cx.notify();
    }

    /// The agents asked for again (the sidebar shows who started whom),
    /// and the past conversations when the sidebar shows them.
    pub(super) fn refresh_known(&mut self, cx: &mut Context<Self>) {
        self.refresh_past(false, cx);
        let known = &mut self.known;
        if known.loading {
            return;
        }
        known.loading = true;
        let servers = app_state::servers(cx);
        // Those away: their agents stay, as last heard.
        let away: HashSet<String> = servers
            .iter()
            .filter(|(_, _, status)| *status != app_state::Status::Connected)
            .map(|(_, name, _)| name.clone())
            .collect();
        let kept: Vec<(AgentInfo, String)> =
            known.agents.iter().filter(|(_, s)| away.contains(s)).cloned().collect();
        let asks: Vec<_> = servers
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, name, _)| {
                (server, name, app_state::control_on(server, Request::Agents, cx))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let mut found = kept;
            for (server, name, answer) in asks {
                if let Ok(Response::Agents { agents }) = answer.await {
                    found.extend(agents.into_iter().map(|mut agent| {
                        agent.pane = ids::tag(server, agent.pane);
                        agent.parent = agent.parent.map(|p| ids::tag(server, p));
                        (agent, name.clone())
                    }));
                }
            }
            let _ = this.update(cx, |this, cx| {
                found.sort_by_cached_key(|(agent, server)| this.agent_order(agent, server));
                let found = agents_view::nested(found);
                let known = &mut this.known;
                known.loading = false;
                if known.agents != found || known.away != away {
                    known.agents = found;
                    known.away = away;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// AGENTS (grouped) over the list, with the way back to tabs.
    pub(super) fn grouped_heading(&self, cx: &mut Context<Self>) -> gpui::Div {
        let count = self.known.agents.len();
        div()
            .flex()
            .items_center()
            .px_2()
            .pb_1()
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui(0xffffff70))
            .child(div().flex_1().child(format!("AGENTS · {count}")))
            .child(
                div()
                    .id("group-by-workspace")
                    .px(px(5.))
                    .rounded(px(4.))
                    .font_weight(FontWeight::NORMAL)
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffd0)))
                    .child("Tabs")
                    .tooltip(|_, cx| cx.new(|_| Tooltip("Group by workspace".into())).into())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_grouping(cx))),
            )
    }

    /// The sidebar's rows, grouped: the agents, then the tabs with none.
    pub(super) fn grouped_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(grouped) = self.grouped.as_ref() else { return Vec::new() };
        let servers: HashSet<&String> = self.known.agents.iter().map(|(_, s)| s).collect();
        let starts = places(&self.known.agents, servers.len());
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut folded = false;
        for (i, ((agent, server), start)) in self.known.agents.iter().zip(&starts).enumerate() {
            if let Some(place) = start {
                folded = grouped.folded.contains(place);
                let away = self.known.away.contains(server);
                rows.push(self.place_header(i, place, folded, away, cx));
            }
            if !folded {
                rows.push(self.grouped_agent(agent, self.known.away.contains(server), cx));
            }
        }
        if self.known.agents.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .py(px(4.))
                    .text_size(px(11.5))
                    .text_color(ui(0xffffff60))
                    .child("No agents running")
                    .into_any_element(),
            );
        }
        rows.extend(self.grouped_past(grouped, cx));
        // The tabs with no agent, in their workspaces' order.
        let agent_panes: HashSet<PaneId> = self.known.agents.iter().map(|(a, _)| a.pane).collect();
        let mut others = Vec::new();
        let mut index = 0;
        for workspace in &self.layout.workspaces {
            for tab in &workspace.tabs {
                if !tab.root.panes().iter().any(|p| agent_panes.contains(p)) {
                    others.push(index);
                }
                index += 1;
            }
        }
        if !others.is_empty() {
            let folded = grouped.others_folded;
            rows.push(
                div()
                    .id("other-tabs")
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px_2()
                    .pt(px(10.))
                    .pb_1()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70))
                    .hover(|d| d.text_color(ui(0xffffffb0)))
                    .child(if folded { "▸" } else { "▾" })
                    .child(format!("OTHER TABS · {}", others.len()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(grouped) = this.grouped.as_mut() {
                            grouped.others_folded = !grouped.others_folded;
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if !folded {
                let tabs: Vec<_> =
                    self.layout.workspaces.iter().flat_map(|w| w.tabs.iter()).collect();
                for index in others {
                    rows.push(self.tab_row(index, tabs[index], cx).into_any_element());
                }
            }
        }
        rows
    }

    /// A place's header (machine · workspace), folding its agents; a
    /// machine away says so.
    fn place_header(
        &self,
        i: usize,
        place: &str,
        folded: bool,
        away: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = place.to_owned();
        div()
            .id(("place", i))
            .flex()
            .items_center()
            .gap(px(4.))
            .px_2()
            .pt(px(if i == 0 { 2. } else { 8. }))
            .pb(px(2.))
            .text_size(px(11.))
            .text_color(ui(if away { 0xffffff45 } else { 0xffffff70 }))
            .hover(|d| d.text_color(ui(0xffffffb0)))
            .child(div().flex_none().text_size(px(9.)).child(if folded { "▸" } else { "▾" }))
            .child(Self::one_line(place.to_owned()).flex_1().min_w_0())
            .when(away, |d| d.child(div().flex_none().child("reconnecting")))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(grouped) = this.grouped.as_mut()
                    && !grouped.folded.remove(&key)
                {
                    grouped.folded.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// An agent's row: its state, its tab's title (under the agent that
    /// started it), and what it has for you, else its name (dimmed, its
    /// machine `away`).
    fn grouped_agent(
        &self,
        agent: &AgentInfo,
        away: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pane = agent.pane;
        let chosen = !self.showing_view() && self.focused_id() == Some(pane);
        let icon = match agent.state {
            AgentState::Working => "busy",
            AgentState::Waiting => "waiting",
            AgentState::Done => "done",
            AgentState::Idle => "",
        };
        let (end, color) = match (agent.pending, agent.unread) {
            (n, _) if n > 0 => ("permission".to_owned(), 0xe0af68ff),
            (_, n) if n > 0 => ("reply".to_owned(), 0x9ece6aff),
            _ => (agent.agent.clone(), 0xffffff60),
        };
        let depth = agents_view::depth(&self.known.agents, agent);
        let waiting = agent.state == AgentState::Waiting;
        // What it's doing, under its title, while it's at it.
        let doing = agent
            .message
            .as_deref()
            .filter(|_| matches!(agent.state, AgentState::Working | AgentState::Waiting))
            .and_then(|m| m.lines().next())
            .map(str::to_owned);
        div()
            .id(SharedString::from(format!("grouped-{pane}")))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .min_h(px(28.))
            .py(px(2.))
            .pl(px(4. + 14. * depth as f32))
            .pr(px(8.))
            .rounded(px(7.))
            .when(away, |d| d.opacity(0.45))
            .when(chosen, |d| d.bg(ui(0xffffff1c)))
            .when(!chosen && waiting, |d| d.bg(ui(0xe0af6814)))
            .hover(|d| d.bg(ui(0xffffff10)))
            .child(self.icon(icon))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        Self::one_line(agent.title.clone())
                            .text_color(if chosen { ui(0xffffffee) } else { ui(0xffffffb0) })
                            .when(waiting, |d| d.font_weight(FontWeight::SEMIBOLD)),
                    )
                    .when_some(doing, |d, doing| {
                        d.child(Self::one_line(doing).text_size(px(11.)).text_color(ui(0xffffff70)))
                    }),
            )
            .child(div().flex_none().text_size(px(11.5)).text_color(ui(color)).child(end))
            .on_click(cx.listener(move |this, _, _, cx| this.go_to_agent(pane, cx)))
            .into_any_element()
    }

    /// Past conversations (not going on in a pane): one opens in the panel
    /// beside the terminal, read-only, to ask a copy of or carry on.
    fn grouped_past(&self, grouped: &Grouped, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.past.list.is_empty() {
            return Vec::new();
        }
        let folded = grouped.past_folded;
        let mut rows = vec![
            div()
                .id("past-heading")
                .flex()
                .items_center()
                .gap(px(4.))
                .px_2()
                .pt(px(10.))
                .pb_1()
                .text_size(px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui(0xffffff70))
                .hover(|d| d.text_color(ui(0xffffffb0)))
                .child(if folded { "▸" } else { "▾" })
                .child("PAST CONVERSATIONS")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(grouped) = this.grouped.as_mut() {
                        grouped.past_folded = !grouped.past_folded;
                    }
                    cx.notify();
                }))
                .into_any_element(),
        ];
        if folded {
            return rows;
        }
        let shown = self.thread_panel.as_ref().filter(|_| self.panel_shown());
        let open = shown.and_then(|p| p.agents.past_shown()).map(|(s, h)| (s, h.session.clone()));
        for (i, (server, hit)) in self.past.list.iter().enumerate() {
            let chosen = open.as_ref().is_some_and(|(s, id)| s == server && *id == hit.session);
            let (mark, color) = search_view::agent_mark(&hit.agent);
            let (server, hit) = (*server, hit.clone());
            rows.push(
                div()
                    .id(("grouped-past", i))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(26.))
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
                        if this.showing_view() {
                            this.show_terminals(cx);
                        }
                        this.show_past(agents_view::Shown::Panel, server, hit.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    /// A workspace's tabs in the sidebar's order: each followed by those
    /// whose agent its agent started (in this workspace), and how deep.
    pub(super) fn nested_tabs(&self, tabs: &[TabLayout]) -> Vec<(usize, usize)> {
        let parent_of: HashMap<PaneId, PaneId> =
            self.known.agents.iter().filter_map(|(a, _)| Some((a.pane, a.parent?))).collect();
        let tab_of = |pane: PaneId| tabs.iter().position(|t| t.root.panes().contains(&pane));
        let parents: Vec<Option<usize>> = tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                tab.root.panes().iter().filter_map(|p| tab_of(*parent_of.get(p)?)).find(|&t| t != i)
            })
            .collect();
        // Agents' tabs first (each with those under it), then the rest.
        let agents: HashSet<PaneId> = self.known.agents.iter().map(|(a, _)| a.pane).collect();
        let agent_tab: Vec<bool> =
            tabs.iter().map(|t| t.root.panes().iter().any(|p| agents.contains(p))).collect();
        agents_first(nest_order(&parents), &agent_tab)
    }

    /// Past conversations on `server` (folded unless opened: kept, by
    /// server, as `sidebar-past-open`): one opens in the panel beside the
    /// terminal.
    pub(super) fn server_past(
        &self,
        server: ServerId,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let key = match server {
            LOCAL => String::new(),
            _ => app_state::servers(cx)
                .into_iter()
                .find(|(id, _, _)| *id == server)
                .map(|(_, name, _)| name)
                .unwrap_or_default(),
        };
        let list: Vec<&(ServerId, SessionHit)> =
            self.past.list.iter().filter(|(s, _)| *s == server).collect();
        if list.is_empty() {
            return Vec::new();
        }
        let open = self.past_open.contains(&key);
        let toggle = key.clone();
        let mut rows = vec![
            div()
                .id(("server-past", server as usize))
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(24.))
                .px_2()
                .rounded(px(6.))
                .text_size(px(11.5))
                .text_color(ui(0xffffff60))
                .hover(|d| d.bg(ui(0xffffff0a)).text_color(ui(0xffffffb0)))
                .child(div().flex_none().text_size(px(9.)).child(if open { "▾" } else { "▸" }))
                .child(format!("Past conversations · {}", list.len()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_past(&toggle, cx)))
                .into_any_element(),
        ];
        if !open {
            return rows;
        }
        let shown = self.thread_panel.as_ref().filter(|_| self.panel_shown());
        let chosen = shown.and_then(|p| p.agents.past_shown()).map(|(s, h)| (s, h.session.clone()));
        for (i, (server, hit)) in list.into_iter().enumerate() {
            let here = chosen.as_ref().is_some_and(|(s, id)| s == server && *id == hit.session);
            let (mark, color) = search_view::agent_mark(&hit.agent);
            let (server, hit) = (*server, hit.clone());
            rows.push(
                div()
                    .id(SharedString::from(format!("server-past-{server}-{i}")))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(26.))
                    .pl(px(18.))
                    .pr(px(8.))
                    .rounded(px(7.))
                    .when(here, |d| d.bg(ui(0xffffff1c)))
                    .hover(|d| d.bg(ui(0xffffff10)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(ui(color << 8 | 0xb0))
                            .child(mark),
                    )
                    .child(
                        Self::one_line(hit.title.clone()).flex_1().min_w_0().text_color(if here {
                            ui(0xffffffee)
                        } else {
                            ui(0xffffff90)
                        }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.showing_view() {
                            this.show_terminals(cx);
                        }
                        this.show_past(agents_view::Shown::Panel, server, hit.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    /// Open (or fold) a server's past conversations, kept as the setting.
    fn toggle_past(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.past_open.remove(key) {
            self.past_open.insert(key.to_owned());
        }
        let mut open: Vec<String> = self.past_open.iter().cloned().collect();
        open.sort();
        let saved = artty_core::config_file::ConfigFile::open().and_then(|mut file| {
            file.set_strings("sidebar-past-open", &open);
            file.save()
        });
        if let Err(err) = saved {
            self.show_toast(format!("Couldn't save the setting: {err}"), cx);
        }
        self.refresh_past(true, cx);
        cx.notify();
    }

    /// To an agent's terminal: here, or (another window's) there.
    pub(super) fn go_to_agent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        // (The panel, showing a past conversation, goes back to threads.)
        if let Some(panel) = self.thread_panel.as_mut() {
            panel.agents.close_past();
        }
        if self.showing_view() {
            self.show_terminals(cx);
        }
        if self.tabs.iter().any(|t| t.root.panes().contains(&pane)) {
            self.focus_pane(pane, cx);
        } else {
            let request = Request::Focus { tab: ids::local_id(pane) };
            drop(app_state::control_on(ids::server_of(pane), request, cx));
        }
        cx.notify();
    }
}

/// The order to show items whose parents are `parents` (by index): each
/// followed by its own, at once; with how deep each is. A loop, or a
/// parent that isn't there, leaves one where it is.
fn nest_order(parents: &[Option<usize>]) -> Vec<(usize, usize)> {
    let n = parents.len();
    let parent = |i: usize| parents[i].filter(|&p| p < n && p != i);
    // (A loop: its members are roots, in place.)
    let rooted = |i: usize| {
        let mut seen = HashSet::from([i]);
        let mut at = i;
        while let Some(p) = parent(at) {
            if !seen.insert(p) {
                return false;
            }
            at = p;
        }
        true
    };
    let mut out = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    fn place(
        i: usize,
        depth: usize,
        children: &[Vec<usize>],
        placed: &mut [bool],
        out: &mut Vec<(usize, usize)>,
    ) {
        if std::mem::replace(&mut placed[i], true) {
            return;
        }
        out.push((i, depth));
        for &c in &children[i] {
            place(c, depth + 1, children, placed, out);
        }
    }
    let mut children = vec![Vec::new(); n];
    for i in 0..n {
        if let Some(p) = parent(i).filter(|_| rooted(i)) {
            children[p].push(i);
        }
    }
    for i in 0..n {
        let root = parent(i).is_none() || !rooted(i);
        if root {
            place(i, 0, &children, &mut placed, &mut out);
        }
    }
    out
}

/// `order` (from [`nest_order`]) with the items whose root is an agent's
/// (`agent[i]`) first, each root's together, otherwise as it was.
fn agents_first(order: Vec<(usize, usize)>, agent: &[bool]) -> Vec<(usize, usize)> {
    let mut blocks: Vec<Vec<(usize, usize)>> = Vec::new();
    for item in order {
        match blocks.last_mut() {
            Some(block) if item.1 > 0 => block.push(item),
            _ => blocks.push(vec![item]),
        }
    }
    // (Stable: agents' in their order, then the rest in theirs.)
    blocks.sort_by_key(|block| !agent.get(block[0].0).copied().unwrap_or(false));
    blocks.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use artty_core::control::AgentInfo;

    fn agent(pane: u64, parent: Option<u64>, workspace: &str, server: &str) -> (AgentInfo, String) {
        let json = serde_json::json!({
            "pane": pane, "agent": "claude", "title": "", "workspace": workspace, "state": "idle",
            "pending": 0, "unread": 0, "parent": parent,
        });
        (serde_json::from_value(json).unwrap(), server.to_owned())
    }

    #[test]
    fn agents_tabs_first() {
        // Tabs: 0 shell, 1 agent (started 3), 2 shell, 3 worker, 4 agent.
        let order = super::nest_order(&[None, None, None, Some(1), None]);
        let agent = [false, true, false, true, true];
        assert_eq!(super::agents_first(order, &agent), [(1, 0), (3, 1), (4, 0), (0, 0), (2, 0)]);
    }

    #[test]
    fn tabs_under_their_starters() {
        // 0 started 3, which started 1; 2 alone.
        let order = super::nest_order(&[None, Some(3), None, Some(0)]);
        assert_eq!(order, [(0, 0), (3, 1), (1, 2), (2, 0)]);
        // A loop doesn't lose anyone.
        let order = super::nest_order(&[Some(1), Some(0), None]);
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn places_only_when_more_than_one() {
        // One place: no headers.
        let one = [agent(1, None, "api", "mac"), agent(2, None, "api", "mac")];
        assert_eq!(super::places(&one, 1), [None, None]);
        // Two: a header where each begins; one started by another is under
        // it, though elsewhere.
        let two = [
            agent(1, None, "api", "mac"),
            agent(5, Some(1), "docs", "mac"),
            agent(2, None, "docs", "mac"),
            agent(3, None, "docs", "mac"),
        ];
        let starts = super::places(&two, 1);
        assert_eq!(starts, [Some("api".into()), None, Some("docs".into()), None]);
        // Machines named when there are several.
        let machines = [agent(1, None, "api", "mac"), agent(2, None, "train", "gpu")];
        assert_eq!(
            super::places(&machines, 2),
            [Some("mac · api".into()), Some("gpu · train".into())]
        );
    }
}
