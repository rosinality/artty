//! The Activity view: what runs in every pane and what it costs, grouped by
//! workspace, refreshed while it's on screen. It belongs to no workspace and
//! runs nothing itself.

use std::collections::HashSet;

use artty_core::control::{
    PaneProcesses, ProcessRow, Request, Response, ServerInfo, ServerUsage, WorkspaceInfo,
};

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Sort {
    #[default]
    Cpu,
    Memory,
    Name,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Group {
    Workspace(u64),
    Pane(u64),
}

#[derive(Default)]
pub(super) struct Activity {
    /// Every connected server's panes and workspaces, ids tagged (see `ids`).
    panes: Vec<PaneProcesses>,
    workspaces: Vec<WorkspaceInfo>,
    /// Each connected server: its build, uptime, use and what it holds.
    servers: Vec<ServerActivity>,
    sort: Sort,
    collapsed: HashSet<Group>,
    filter: String,
    /// A process whose stop button was clicked once; a second click stops it.
    armed: Option<i32>,
    loaded: bool,
    /// A refresh is on its way.
    loading: bool,
    error: Option<String>,
}

/// One server, as of the last refresh.
struct ServerActivity {
    server: ServerId,
    name: String,
    usage: Option<ServerUsage>,
    info: Option<ServerInfo>,
    error: Option<String>,
}

/// Samples in a pane's history (one a second).
const HISTORY: usize = 60;
const GRAPH_WIDTH: f32 = 120.;
const PID_WIDTH: f32 = 64.;
const CPU_WIDTH: f32 = 112.;
const MEMORY_WIDTH: f32 = 72.;
const TIME_WIDTH: f32 = 64.;
const ACTION_WIDTH: f32 = 52.;
const INDENT: f32 = 16.;

/// A process row with its depth in its pane's tree.
struct Process<'a> {
    row: &'a ProcessRow,
    depth: usize,
}

/// A pane's processes in tree order, each with its depth.
fn tree(rows: &[ProcessRow]) -> Vec<Process<'_>> {
    let pids: HashSet<i32> = rows.iter().map(|r| r.pid).collect();
    let mut out = Vec::new();
    fn walk<'a>(
        rows: &'a [ProcessRow],
        parent: Option<i32>,
        depth: usize,
        pids: &HashSet<i32>,
        out: &mut Vec<Process<'a>>,
    ) {
        let children = rows.iter().filter(|r| match parent {
            None => !pids.contains(&r.ppid) || r.ppid == r.pid,
            Some(parent) => r.ppid == parent && r.pid != parent,
        });
        for row in children {
            out.push(Process { row, depth });
            walk(rows, Some(row.pid), depth + 1, pids, out);
        }
    }
    walk(rows, None, 0, &pids, &mut out);
    out
}

/// Pane statuses (`working`, ...) as the sidebar's icons (`busy`, ...).
fn status_icon(status: &str) -> &str {
    match status {
        "working" | "running" => "busy",
        "waiting" | "failed" | "done" => status,
        _ => "",
    }
}

/// Panes' histories added up, aligned at the newest sample.
fn total_history(panes: &[&PaneProcesses]) -> Vec<(f32, u64)> {
    let len = panes.iter().map(|p| p.history.len()).max().unwrap_or(0);
    (0..len)
        .map(|i| {
            panes
                .iter()
                .filter_map(|p| p.history.get((p.history.len() + i).checked_sub(len)?))
                .fold((0., 0), |(c, m), (pc, pm)| (c + pc, m + pm))
        })
        .collect()
}

/// A bar graph of the last minute, newest on the right. CPU graphs go to at
/// least 100%; others scale to their peak.
fn sparkline(values: &[f32], floor: f32, width: f32, height: f32, color: u32) -> gpui::Div {
    let max = values.iter().copied().fold(floor, f32::max).max(f32::EPSILON);
    let bar = width / HISTORY as f32;
    div().flex_none().w(px(width)).h(px(height)).flex().items_end().justify_end().children(
        values.iter().map(|&v| {
            let h = (v / max).clamp(0., 1.) * height;
            div().flex_none().w(px(bar)).h(px(if v > 0. { h.max(1.) } else { 0. })).bg(rgba(color))
        }),
    )
}

fn cpu_color(percent: f32) -> u32 {
    match percent {
        p if p >= 80. => 0xf7768eff,
        p if p >= 30. => 0xe0af68ff,
        _ => 0x9ece6aff,
    }
}

impl WindowView {
    pub(super) fn showing_activity(&self) -> bool {
        self.activity.is_some()
    }

    pub fn show_activity(&mut self, cx: &mut Context<Self>) {
        if !self.showing_activity() {
            self.toggle_activity(cx);
        }
    }

    pub(super) fn toggle_activity(&mut self, cx: &mut Context<Self>) {
        if self.activity.take().is_none() {
            self.close_search(cx);
            self.picker = None;
            if self.renaming.is_some() {
                self.finish_rename(true, cx);
            }
            self.activity = Some(Activity::default());
            self.refresh_activity(cx);
        }
        cx.notify();
    }

    /// Ask every connected server for its panes' processes (and workspaces'
    /// names and its own state).
    pub(super) fn refresh_activity(&mut self, cx: &mut Context<Self>) {
        let Some(activity) = self.activity.as_mut().filter(|a| !a.loading) else { return };
        activity.loading = true;
        let servers: Vec<(ServerId, String)> = app_state::servers(cx)
            .into_iter()
            .filter(|(_, _, status)| *status == app_state::Status::Connected)
            .map(|(server, name, _)| (server, name))
            .collect();
        let asks: Vec<_> = servers
            .into_iter()
            .map(|(server, name)| {
                (
                    server,
                    name,
                    app_state::control_on(server, Request::Processes { tab: None }, cx),
                    app_state::control_on(server, Request::Workspaces, cx),
                    app_state::control_on(server, Request::ServerStatus, cx),
                )
            })
            .collect();
        let several = asks.len() > 1;
        cx.spawn(async move |this, cx| {
            let mut panes = Vec::new();
            let mut workspaces = Vec::new();
            let mut servers = Vec::new();
            for (server, name, processes, spaces, status) in asks {
                let mut entry = ServerActivity {
                    server,
                    name: name.clone(),
                    usage: None,
                    info: None,
                    error: None,
                };
                if let Ok(Response::Server { info }) = status.await {
                    entry.info = Some(info);
                }
                match (processes.await, spaces.await) {
                    (
                        Ok(Response::Processes { panes: found, server: usage }),
                        Ok(Response::Workspaces { workspaces: spaces }),
                    ) => {
                        entry.usage = usage;
                        let tag = |id: u64| ids::tag(server, id);
                        panes.extend(found.into_iter().map(|mut pane| {
                            pane.id = tag(pane.id);
                            pane.workspace = tag(pane.workspace as u64) as usize;
                            pane
                        }));
                        // Another machine's workspaces say whose they are.
                        workspaces.extend(spaces.into_iter().map(|mut workspace| {
                            workspace.id = tag(workspace.id);
                            if several && server != LOCAL {
                                workspace.title = format!("{name} · {}", workspace.title);
                                workspace.name = workspace.name.map(|n| format!("{name} · {n}"));
                            }
                            workspace
                        }));
                    },
                    (Ok(Response::Error { message }), _) | (_, Ok(Response::Error { message })) => {
                        entry.error = Some(message)
                    },
                    _ => entry.error = Some("not answering".into()),
                }
                servers.push(entry);
            }
            let _ = this.update(cx, |this, cx| {
                let Some(activity) = this.activity.as_mut() else { return };
                activity.loading = false;
                activity.panes = panes;
                activity.workspaces = workspaces;
                activity.error = servers.is_empty().then(|| "lost the artty server".to_owned());
                activity.servers = servers;
                activity.loaded = true;
                cx.notify();
            });
        })
        .detach();
    }

    /// Keys while Activity is shown: typing filters, Escape clears the filter
    /// or leaves. Returns whether the key was used.
    pub(super) fn activity_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(activity) = self.activity.as_mut() else { return false };
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" if !activity.filter.is_empty() => activity.filter.clear(),
            "escape" => self.activity = None,
            "backspace" => {
                activity.filter.pop();
            },
            _ => match keystroke.key_char.as_deref().filter(|c| !c.chars().any(char::is_control)) {
                Some(text) if !keystroke.modifiers.control => activity.filter.push_str(text),
                _ => return false,
            },
        }
        cx.notify();
        true
    }

    /// Leave Activity and show a pane, wherever it is.
    fn jump_to(&mut self, workspace: u64, pane: PaneId, cx: &mut Context<Self>) {
        let here = self.layout.workspaces.iter().any(|w| w.id == workspace);
        if let (false, Some(window)) = (here, self.window_on(ids::server_of(workspace))) {
            self.send(ClientMessage::OpenWorkspace { workspace, window }, cx);
        }
        self.activity = None;
        self.send(ClientMessage::FocusPane { pane }, cx);
        cx.notify();
    }

    /// Stop a process: SIGTERM, or SIGKILL with ⌥. Shells and agents need a
    /// second click.
    fn stop_process(
        &mut self,
        server: ServerId,
        pid: i32,
        confirm: bool,
        kill: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(activity) = self.activity.as_mut() else { return };
        if confirm && activity.armed != Some(pid) {
            activity.armed = Some(pid);
            return cx.notify();
        }
        activity.armed = None;
        let signal = if kill { libc::SIGKILL } else { libc::SIGTERM };
        let answer =
            app_state::control_on(server, Request::Signal { pid, signal: Some(signal) }, cx);
        cx.spawn(async move |this, cx| {
            let response = answer.await;
            let _ = this.update(cx, |this, cx| {
                if let (Some(activity), Ok(Response::Error { message })) =
                    (this.activity.as_mut(), response)
                {
                    activity.error = Some(message);
                }
                this.refresh_activity(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn toggle_group(&mut self, group: Group, cx: &mut Context<Self>) {
        if let Some(activity) = self.activity.as_mut()
            && !activity.collapsed.remove(&group)
        {
            activity.collapsed.insert(group);
        }
        cx.notify();
    }

    fn set_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        if let Some(activity) = self.activity.as_mut() {
            activity.sort = sort;
        }
        cx.notify();
    }

    pub(super) fn activity_view(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(activity) = self.activity.as_ref() else { return div() };
        let filter = activity.filter.to_lowercase();
        let matches = |text: &str| filter.is_empty() || text.to_lowercase().contains(&filter);
        let process_matches = |p: &ProcessRow| matches(&p.name) || p.pid.to_string() == filter;

        // Workspaces in sidebar order, each with its panes, then sorted.
        let mut groups: Vec<(&WorkspaceInfo, Vec<&PaneProcesses>)> = activity
            .workspaces
            .iter()
            .map(|w| (w, activity.panes.iter().filter(|p| p.workspace as u64 == w.id).collect()))
            .collect();
        let cpu =
            |panes: &[&PaneProcesses]| panes.iter().map(|p| p.cpu_percent).fold(0., |a, b| a + b);
        let memory = |panes: &[&PaneProcesses]| panes.iter().map(|p| p.memory_bytes).sum::<u64>();
        for (_, panes) in &mut groups {
            match activity.sort {
                Sort::Cpu => panes.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent)),
                Sort::Memory => panes.sort_by_key(|p| std::cmp::Reverse(p.memory_bytes)),
                Sort::Name => panes.sort_by_key(|p| p.title.to_lowercase()),
            }
        }
        match activity.sort {
            Sort::Cpu => groups.sort_by(|a, b| cpu(&b.1).total_cmp(&cpu(&a.1))),
            Sort::Memory => groups.sort_by_key(|g| std::cmp::Reverse(memory(&g.1))),
            Sort::Name => groups.sort_by_key(|g| g.0.title.to_lowercase()),
        }

        let all: Vec<&PaneProcesses> = activity.panes.iter().collect();
        let process_count: usize = activity.panes.iter().map(|p| p.processes.len()).sum();
        let history = total_history(&all);
        let cpu_history: Vec<f32> = history.iter().map(|h| h.0).collect();
        let memory_history: Vec<f32> = history.iter().map(|h| h.1 as f32).collect();
        let peak_memory = memory_history.iter().copied().fold(0., f32::max) * 1.25;
        let gauge = |label: String, graph: gpui::Div| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_color(rgba(0xffffffd0)).child(label))
                .child(div().p(px(4.)).rounded(px(5.)).bg(rgba(0x0000002e)).child(graph))
        };
        let summary =
            div()
                .flex()
                .items_end()
                .gap(px(20.))
                .child(gauge(
                    format!("CPU {:.1}%", cpu(&all)),
                    sparkline(&cpu_history, 100., 180., 30., 0x8fb4ffcc),
                ))
                .child(gauge(
                    format!("Memory {}", crate::cli::bytes(memory(&all))),
                    sparkline(&memory_history, peak_memory, 180., 30., 0xbb9af7cc),
                ))
                .child(
                    div().flex().flex_col().gap(px(2.)).text_color(rgba(0xffffff90)).child(
                        format!("{process_count} processes in {} panes", activity.panes.len()),
                    ),
                );

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (workspace, panes) in &groups {
            let workspace_matches = matches(&workspace.title);
            let shown: Vec<(&PaneProcesses, Vec<Process>)> = panes
                .iter()
                .filter_map(|pane| {
                    let processes = tree(&pane.processes);
                    if workspace_matches || matches(&pane.title) {
                        return Some((*pane, processes));
                    }
                    let found: Vec<Process> =
                        processes.into_iter().filter(|p| process_matches(p.row)).collect();
                    (!found.is_empty()).then_some((*pane, found))
                })
                .collect();
            if shown.is_empty() {
                continue;
            }
            let group = Group::Workspace(workspace.id);
            let folded = activity.collapsed.contains(&group);
            rows.push(self.workspace_activity_row(workspace, panes, folded, cx).into_any_element());
            if folded {
                continue;
            }
            for (pane, processes) in shown {
                let group = Group::Pane(pane.id);
                let folded = activity.collapsed.contains(&group);
                rows.push(
                    self.pane_activity_row(workspace.id, pane, folded, cx).into_any_element(),
                );
                if folded {
                    continue;
                }
                let agent = cx.global::<Shared>().panes.get(&pane.id).and_then(|s| s.agent.clone());
                for process in processes {
                    rows.push(
                        self.process_row(&process, pane, agent.as_deref(), activity.armed, cx)
                            .into_any_element(),
                    );
                }
            }
        }
        let empty = rows.is_empty();

        let heading = |label: &'static str, width: Option<f32>, sort: Option<Sort>| {
            let active = sort.is_some_and(|s| s == activity.sort);
            div()
                .id(label)
                .when_some(width, |d, w| d.w(px(w)).flex_none().flex().justify_end())
                .when(width.is_none(), |d| d.flex_1().min_w_0())
                .text_color(if active { rgba(0xffffffd0) } else { rgba(0xffffff70) })
                .when_some(sort, |d, sort| {
                    d.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.set_sort(sort, cx)))
                })
                .child(if active { format!("{label} ↓") } else { label.to_owned() })
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .px(px(PADDING))
            .pb(px(PADDING))
            .text_size(px(12.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .pb(px(10.))
                    .child(if activity.loaded {
                        summary
                    } else {
                        div().text_color(rgba(0xffffffb0)).child("Loading…")
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(220.))
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .bg(rgba(0x0000004d))
                            .text_color(if activity.filter.is_empty() {
                                rgba(0xffffff50)
                            } else {
                                rgba(0xffffffee)
                            })
                            .child(if activity.filter.is_empty() {
                                "Type to filter".to_owned()
                            } else {
                                format!("{}▏", activity.filter)
                            }),
                    ),
            )
            .when(activity.loaded, |d| {
                d.children(activity.servers.iter().map(|server| self.server_strip(server, cx)))
            })
            .when_some(activity.error.clone(), |d, error| {
                d.child(div().pb(px(8.)).text_color(rgba(0xf7768eff)).child(error))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .pb(px(6.))
                    .border_b_1()
                    .border_color(rgba(0xffffff14))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(heading("NAME", None, Some(Sort::Name)))
                    .child(heading("LAST MINUTE", Some(GRAPH_WIDTH), None))
                    .child(heading("PID", Some(PID_WIDTH), None))
                    .child(heading("CPU", Some(CPU_WIDTH), Some(Sort::Cpu)))
                    .child(heading("MEMORY", Some(MEMORY_WIDTH), Some(Sort::Memory)))
                    .child(heading("TIME", Some(TIME_WIDTH), None))
                    .child(div().w(px(ACTION_WIDTH)).flex_none()),
            )
            .child(
                div()
                    .id("activity-rows")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .pt(px(4.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty && activity.loaded, |d| {
                        d.child(
                            div().p(px(8.)).text_color(rgba(0xffffff60)).child("Nothing matches"),
                        )
                    }),
            )
    }

    /// The server: whether it runs the app's build, what it holds and uses,
    /// and a restart that hands its sessions over (programs keep running).
    fn server_strip(&self, activity: &ServerActivity, cx: &Context<Self>) -> gpui::Div {
        let server = activity.server;
        let local = server == LOCAL;
        let different = local && cx.global::<app_state::Shared>().server_mismatch.is_some();
        let lost = activity.error.is_some();
        let (dot, status) = match (lost, different) {
            (true, _) => (0xf7768eff, "not answering"),
            (false, true) => (0xe0af68ff, "runs a different build"),
            (false, false) => (0x9ece6aff, "running"),
        };
        let status = format!("{} {status}", if local { "Server" } else { activity.name.as_str() });
        let mut facts: Vec<String> = Vec::new();
        if let Some(latency) = app_state::latency(server, cx).filter(|_| !local) {
            facts.push(format!("{} ms away", latency.as_millis()));
        }
        if let Some(info) = &activity.info {
            facts.push(format!("up {}", crate::cli::duration(info.uptime_secs)));
            facts.push(format!("pid {}", info.pid));
        }
        if let Some(usage) = &activity.usage {
            facts.push(format!(
                "CPU {:.1}% · {}",
                usage.cpu_percent,
                crate::cli::bytes(usage.memory_bytes)
            ));
        }
        if let Some(info) = &activity.info {
            let plural = |n: usize, one: &str, many: &str| {
                format!("{n} {}", if n == 1 { one } else { many })
            };
            let detached = info.windows - info.attached_windows.min(info.windows);
            facts.push(if detached > 0 {
                format!("{} ({detached} detached)", plural(info.windows, "window", "windows"))
            } else {
                plural(info.windows, "window", "windows")
            });
            facts.push(plural(info.workspaces, "workspace", "workspaces"));
            facts.push(plural(info.panes, "pane", "panes"));
            facts.push(plural(info.clients, "app connected", "apps connected"));
        }
        let row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().flex_none().size(px(8.)).rounded_full().bg(rgba(dot)))
            .child(div().flex_none().text_color(rgba(0xffffffd0)).child(status))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .line_clamp(1)
                    .text_ellipsis()
                    .text_color(rgba(0xffffff80))
                    .child(facts.join(" · ")),
            )
            // This Mac's server moves to this build; another machine's runs
            // the build installed there.
            .when(!lost && local, |d| {
                d.child(
                    div()
                        .id("restart-server")
                        .flex_none()
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(5.))
                        .bg(if different { rgba(0x3478f6ff) } else { rgba(0xffffff14) })
                        .hover(|d| {
                            d.bg(if different { rgba(0x4a88ffff) } else { rgba(0xffffff26) })
                        })
                        .text_color(rgba(0xffffffe0))
                        .child(if different { "Update Server" } else { "Restart Server" })
                        .on_click(cx.listener(|_, _, _, cx| app_state::restart_server(cx))),
                )
            });
        let network = (!local).then(|| app_state::network(server, cx)).flatten();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mb(px(10.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(rgba(0x0000002e))
            .child(row)
            .when_some(network, |d, network| d.child(Self::network_row(&network, server, cx)))
    }

    /// Another machine's link: round trips and traffic over the last minute,
    /// how long it's been up, and what last went wrong.
    fn network_row(
        network: &app_state::Network,
        server: ServerId,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let trips: Vec<f32> = network.round_trips.iter().copied().collect();
        let down: Vec<f32> = network.rates.iter().map(|r| r.0).collect();
        let (now_down, now_up) = network.rates.back().copied().unwrap_or_default();
        let rate = |bytes: f32| format!("{}/s", crate::cli::bytes(bytes as u64));
        let trip = match (
            trips.last(),
            trips.iter().copied().reduce(f32::min),
            trips.iter().copied().reduce(f32::max),
        ) {
            (Some(last), Some(low), Some(high)) => {
                format!("{last:.0} ms (min {low:.0}, max {high:.0})")
            },
            _ => "measuring…".to_owned(),
        };
        let mut facts = Vec::new();
        if let Some(since) = network.since {
            facts.push(format!("connected {}", crate::cli::duration(since.elapsed().as_secs())));
        }
        if network.reconnects > 0 {
            let n = network.reconnects;
            facts.push(format!("{n} reconnect{}", if n == 1 { "" } else { "s" }));
        }
        if let Some((error, at)) = &network.last_error {
            facts.push(format!(
                "last problem: {error}, {} ago",
                crate::cli::duration(at.elapsed().as_secs())
            ));
        }
        if let Some(how) = app_state::transport(server, cx) {
            facts.push(how);
        }
        let label = |text: &'static str| div().flex_none().text_color(rgba(0xffffff60)).child(text);
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .pl(px(18.))
            .text_size(px(11.5))
            .text_color(rgba(0xffffff90))
            .child(label("Round trip"))
            .child(sparkline(&trips, 50., 80., 14., 0x8fb4ffcc))
            .child(div().flex_none().child(trip))
            .child(label("Down"))
            .child(sparkline(&down, 1024., 80., 14., 0x9ece6acc))
            .child(div().flex_none().child(format!("{} · up {}", rate(now_down), rate(now_up))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .line_clamp(1)
                    .text_ellipsis()
                    .text_color(rgba(0xffffff60))
                    .child(facts.join(" · ")),
            )
    }

    /// A row's CPU, memory and time cells.
    fn figures(
        cpu: Option<f32>,
        memory: Option<u64>,
        time: Option<u64>,
    ) -> impl Iterator<Item = gpui::Div> {
        let cpu_cell =
            div()
                .w(px(CPU_WIDTH))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(6.))
                .when_some(cpu, |d, cpu| {
                    let fill = (cpu.min(100.) / 100. * 40.).max(if cpu > 0.05 { 1. } else { 0. });
                    d.child(
                        div().w(px(40.)).h(px(4.)).rounded(px(2.)).bg(rgba(0xffffff14)).child(
                            div().w(px(fill)).h_full().rounded(px(2.)).bg(rgba(cpu_color(cpu))),
                        ),
                    )
                    .child(div().w(px(48.)).flex().justify_end().child(format!("{cpu:.1}%")))
                });
        let memory_cell = div()
            .w(px(MEMORY_WIDTH))
            .flex_none()
            .flex()
            .justify_end()
            .when_some(memory, |d, m| d.child(crate::cli::bytes(m)));
        let time_cell = div()
            .w(px(TIME_WIDTH))
            .flex_none()
            .flex()
            .justify_end()
            .when_some(time, |d, t| d.child(crate::cli::duration(t)));
        [cpu_cell, memory_cell, time_cell].into_iter()
    }

    fn arrow(folded: bool) -> gpui::Div {
        div()
            .w(px(12.))
            .flex_none()
            .text_size(px(9.))
            .text_color(rgba(0xffffff70))
            .child(if folded { "▶" } else { "▼" })
    }

    fn workspace_activity_row(
        &self,
        workspace: &WorkspaceInfo,
        panes: &[&PaneProcesses],
        folded: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let group = Group::Workspace(workspace.id);
        let cpu = panes.iter().map(|p| p.cpu_percent).fold(0., |a, b| a + b);
        let memory = panes.iter().map(|p| p.memory_bytes).sum();
        let color = workspace.color.as_deref().and_then(workspace_color);
        let place = match (workspace.window, workspace.attached) {
            (None, _) => " · hidden",
            (Some(_), false) => " · closed window",
            _ => "",
        };
        div()
            .id(("activity-workspace", workspace.id as usize))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(28.))
            .mt(px(6.))
            .px(px(8.))
            .rounded(px(6.))
            .bg(rgba(0xffffff0a))
            .hover(|d| d.bg(rgba(0xffffff12)))
            .text_color(rgba(0xffffffe0))
            .font_weight(FontWeight::SEMIBOLD)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(group, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(Self::arrow(folded))
                    .when_some(color, |d, color| {
                        d.child(
                            div()
                                .flex_none()
                                .size(px(7.))
                                .rounded_full()
                                .bg(rgba(color << 8 | 0xff)),
                        )
                    })
                    .child(
                        Self::one_line(workspace.title.clone())
                            .min_w_0()
                            .text_color(rgba(0xffffffe0)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(rgba(0xffffff80))
                            .child(format!(
                                "{} pane{}{place}",
                                panes.len(),
                                if panes.len() == 1 { "" } else { "s" }
                            )),
                    ),
            )
            .child(Self::cpu_graph(&total_history(panes)))
            .child(div().w(px(PID_WIDTH)).flex_none())
            .children(Self::figures(Some(cpu), Some(memory), None))
            .child(div().w(px(ACTION_WIDTH)).flex_none())
    }

    /// The last minute of CPU use, in the row's graph column.
    fn cpu_graph(history: &[(f32, u64)]) -> gpui::Div {
        let values: Vec<f32> = history.iter().map(|h| h.0).collect();
        let peak = values.iter().copied().fold(0., f32::max);
        sparkline(&values, 100., GRAPH_WIDTH, 14., cpu_color(peak) & 0xffffff00 | 0xb0)
    }

    fn pane_activity_row(
        &self,
        workspace: u64,
        pane: &PaneProcesses,
        folded: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let group = Group::Pane(pane.id);
        let id = pane.id;
        div()
            .id(("activity-pane", pane.id as usize))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(26.))
            .px(px(8.))
            .rounded(px(6.))
            .text_color(rgba(0xffffffc0))
            .hover(|d| d.bg(rgba(0xffffff0a)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(INDENT))
                    .child(
                        div()
                            .id(("activity-fold", pane.id as usize))
                            .flex_none()
                            .child(Self::arrow(folded))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(group, cx))),
                    )
                    // The sidebar's icon when there is one (it knows, say, that
                    // mosh isn't something to wait for); else from the status.
                    .child(self.icon(
                        &cx.global::<Shared>()
                            .panes
                            .get(&pane.id)
                            .map_or_else(|| status_icon(&pane.status).to_owned(), |state| state.icon.clone()),
                    ))
                    .child(
                        div()
                            .id(("activity-jump", pane.id as usize))
                            .min_w_0()
                            .flex()
                            .gap(px(6.))
                            .cursor_pointer()
                            .hover(|d| d.text_color(rgba(0x8fb4ffff)))
                            .text_color(rgba(0xffffffd0))
                            .on_click(cx.listener(move |this, _, _, cx| this.jump_to(workspace, id, cx)))
                            .child(Self::one_line(pane.title.clone()).min_w_0())
                            .child(div().flex_none().text_color(rgba(0xffffff80)).child(format!("tab {} · pane {}", pane.tab, pane.id))),
                    ),
            )
            .child(Self::cpu_graph(&pane.history))
            .child(div().w(px(PID_WIDTH)).flex_none())
            .children(Self::figures(Some(pane.cpu_percent), Some(pane.memory_bytes), None))
            .child(div().w(px(ACTION_WIDTH)).flex_none())
    }

    fn process_row(
        &self,
        process: &Process,
        pane: &PaneProcesses,
        agent: Option<&str>,
        armed: Option<i32>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = process.row;
        let pid = row.pid;
        let server = ids::server_of(pane.id);
        // The pane's shell, or its agent: stopping those deserves a second click.
        let shell =
            process.depth == 0 && pane.processes.first().is_some_and(|first| first.pid == pid);
        let confirm = shell || agent.is_some_and(|agent| row.name == agent);
        let is_armed = armed == Some(pid);
        div()
            .id(("activity-process", pid as usize))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(22.))
            .px(px(8.))
            .rounded(px(5.))
            .text_size(px(12.))
            .hover(|d| d.bg(rgba(0xffffff08)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    // Past the pane's title, then one step per level.
                    .pl(px(INDENT + 48. + process.depth as f32 * 14.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .when(process.depth > 0, |d| d.child(div().flex_none().text_color(rgba(0xffffff60)).child("└")))
                    .child(
                        Self::one_line(row.name.clone())
                            .min_w_0()
                            .text_color(if row.foreground { rgba(0xffffffd8) } else { rgba(0xffffff90) })
                            .when(row.foreground, |d| d.font_weight(FontWeight::MEDIUM)),
                    )
                    .children(row.ports.iter().map(|&port| self.port_chip(server, port, cx).flex_none())),
            )
            .child(div().w(px(GRAPH_WIDTH)).flex_none())
            .child(div().w(px(PID_WIDTH)).flex_none().flex().justify_end().text_color(rgba(0xffffff90)).child(pid.to_string()))
            .children(
                Self::figures(Some(row.cpu_percent), Some(row.memory_bytes), Some(row.elapsed_secs))
                    .map(|cell| cell.text_color(rgba(0xffffffa0))),
            )
            .child(
                div().w(px(ACTION_WIDTH)).flex_none().flex().justify_end().child(
                    div()
                        .id(("activity-stop", pid as usize))
                        .px(px(5.))
                        .rounded(px(4.))
                        .text_size(px(11.))
                        .map(|d| {
                            if is_armed {
                                d.bg(rgba(0xf7768eff)).text_color(rgba(0xffffffff)).child("Stop?")
                            } else {
                                d.text_color(rgba(0xffffff90))
                                    .border_1()
                                    .border_color(rgba(0xffffff26))
                                    .hover(|d| d.bg(rgba(0xf7768e60)).text_color(rgba(0xffffffee)))
                                    .child("Stop")
                            }
                        })
                        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                            this.stop_process(server, pid, confirm, event.modifiers().alt, cx);
                        })),
                ),
            )
    }
}
