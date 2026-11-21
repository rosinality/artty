//! The sidebar grouped by agent: instead of tabs by workspace, every
//! agent (each machine's, each workspace's), those an agent started under
//! it, its state on the left and what it has for you on the right; then
//! the tabs with no agent. A click goes to the agent's terminal: the
//! terminal is where you talk to an agent, this is where you see them all.

use artty_core::control::{AgentInfo, AgentState};

use super::*;

/// The sidebar's agents, while it's grouped by them.
#[derive(Default)]
pub(super) struct Grouped {
    /// Every connected server's agents, pane ids tagged, in the order shown
    /// (each followed by those it started), with their server's name.
    agents: Vec<(AgentInfo, String)>,
    loading: bool,
    /// The tabs with no agent, folded away.
    others_folded: bool,
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
        self.refresh_grouped(cx);
        cx.notify();
    }

    /// The agents asked for again (while the sidebar's grouped by them).
    pub(super) fn refresh_grouped(&mut self, cx: &mut Context<Self>) {
        let Some(grouped) = self.grouped.as_mut().filter(|g| !g.loading) else { return };
        grouped.loading = true;
        let asks: Vec<_> = app_state::servers(cx)
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, name, _)| {
                (server, name, app_state::control_on(server, Request::Agents, cx))
            })
            .collect();
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
            let _ = this.update(cx, |this, cx| {
                found.sort_by_cached_key(|(agent, server)| this.agent_order(agent, server));
                let found = agents_view::nested(found);
                let Some(grouped) = this.grouped.as_mut() else { return };
                grouped.loading = false;
                if grouped.agents != found {
                    grouped.agents = found;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// AGENTS (grouped) over the list, with the way back to tabs.
    pub(super) fn grouped_heading(&self, cx: &mut Context<Self>) -> gpui::Div {
        let count = self.grouped.as_ref().map_or(0, |g| g.agents.len());
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
        let mut rows: Vec<gpui::AnyElement> = grouped
            .agents
            .iter()
            .map(|(agent, _)| self.grouped_agent(grouped, agent, cx))
            .collect();
        if grouped.agents.is_empty() {
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
        // The tabs with no agent, in their workspaces' order.
        let agent_panes: HashSet<PaneId> = grouped.agents.iter().map(|(a, _)| a.pane).collect();
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

    /// An agent's row: its state, its tab's title (under the agent that
    /// started it), and what it has for you, else its name.
    fn grouped_agent(
        &self,
        grouped: &Grouped,
        agent: &AgentInfo,
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
        let depth = agents_view::depth(&grouped.agents, agent);
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

    /// To an agent's terminal: here, or (another window's) there.
    pub(super) fn go_to_agent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
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
