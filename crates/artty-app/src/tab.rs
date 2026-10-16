//! A tab: one session plus what the sidebar shows about it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use artty_core::control::{AgentState, TabInfo};
use artty_core::vt::ansi::Progress;
use artty_core::{ProcessInfo, Session, ShellPhase};
use gpui::SharedString;

/// What a tab is doing, as shown in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// At the shell prompt.
    Idle,
    /// A command (not an agent) is running.
    Running { command: Option<String>, since: Instant },
    /// A coding agent is running.
    Agent { name: String, state: AgentState },
    /// The last command failed.
    Failed { exit: i32 },
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Running { .. } => "running",
            Status::Agent { state: AgentState::Working, .. } => "working",
            Status::Agent { state: AgentState::Waiting, .. } => "waiting",
            Status::Agent { state: AgentState::Done, .. } => "done",
            Status::Agent { state: AgentState::Idle, .. } => "idle",
            Status::Failed { .. } => "failed",
        }
    }

    /// Whether the sidebar should animate a spinner.
    pub fn busy(&self) -> bool {
        matches!(self, Status::Running { .. } | Status::Agent { state: AgentState::Working, .. })
    }
}

pub struct Tab {
    pub session: Session,
    /// Title set by the program.
    pub title: Option<String>,
    /// Title set through `artty new --title`, which wins.
    pub fixed_title: Option<String>,
    pub process: ProcessInfo,
    pub status: Status,
    /// Something happened that the user hasn't seen: the bell, a notification,
    /// an agent waiting, a background command finishing.
    pub attention: bool,
    /// Last notification or agent message.
    pub message: Option<String>,
    pub progress: Option<Progress>,
    /// Agent state reported through `artty status` (agent hooks); trusted
    /// over heuristics while the agent runs.
    reported: Option<AgentState>,
    /// When the title last changed; agents animate their title while working.
    title_changes: Vec<Instant>,
    /// The command running, from shell integration, and when it started.
    running: Option<(Option<String>, Instant)>,
    last_exit: Option<i32>,
    /// Process names treated as coding agents.
    agents: Arc<[String]>,
}

const TITLE_ANIMATION_WINDOW: Duration = Duration::from_secs(2);

impl Tab {
    pub fn new(session: Session, fixed_title: Option<String>, agents: Arc<[String]>) -> Self {
        Self {
            session,
            title: None,
            fixed_title,
            process: ProcessInfo::default(),
            status: Status::Idle,
            attention: false,
            message: None,
            progress: None,
            reported: None,
            title_changes: Vec::new(),
            running: None,
            last_exit: None,
            agents,
        }
    }

    pub fn title(&self) -> SharedString {
        let title = self.fixed_title.as_ref().or(self.title.as_ref());
        title.cloned().or_else(|| self.process.foreground.clone()).unwrap_or_else(|| "shell".to_owned()).into()
    }

    pub fn set_title(&mut self, title: Option<String>) {
        if title != self.title {
            self.title = title;
            let now = Instant::now();
            self.title_changes.retain(|t| now.duration_since(*t) < TITLE_ANIMATION_WINDOW);
            self.title_changes.push(now);
        }
    }

    /// The program needs the user (bell or notification).
    pub fn alert(&mut self, message: Option<String>) {
        self.attention = true;
        if message.is_some() {
            self.message = message;
        }
        if self.agent_name().is_some() && self.reported.is_none() {
            self.status = Status::Agent { name: self.agent_name().unwrap(), state: AgentState::Waiting };
        }
    }

    pub fn report(&mut self, state: AgentState, message: Option<String>) {
        if state == AgentState::Waiting || state == AgentState::Done {
            self.attention = true;
        }
        if message.is_some() {
            self.message = message;
        } else if state == AgentState::Working {
            self.message = None;
        }
        self.reported = Some(state);
        self.refresh();
    }

    pub fn command_started(&mut self, command: Option<String>) {
        self.running = Some((command, Instant::now()));
        self.last_exit = None;
    }

    pub fn command_finished(&mut self, exit: Option<i32>) {
        self.running = None;
        self.last_exit = exit;
    }

    pub fn set_agents(&mut self, agents: Arc<[String]>) {
        self.agents = agents;
    }

    pub fn agent_name(&self) -> Option<String> {
        let name = self.process.foreground.as_deref()?;
        self.agents.iter().any(|agent| agent == name).then(|| name.to_owned())
    }

    /// Recompute the status from the latest process info and reports.
    pub fn refresh(&mut self) {
        // A program that reports agent status counts as an agent even if its
        // name isn't on the list, until the shell is back in the foreground.
        let reporter = || {
            let name = self.process.foreground.clone().filter(|_| !self.process.at_shell)?;
            self.reported.map(|_| name)
        };
        let Some(agent) = self.agent_name().or_else(reporter) else {
            // The agent exited: forget what it reported.
            self.reported = None;
            self.status = self.shell_status();
            return;
        };

        let state = if let Some(reported) = self.reported {
            reported
        } else {
            let now = Instant::now();
            let animating = self.title_changes.iter().filter(|t| now.duration_since(**t) < TITLE_ANIMATION_WINDOW).count() >= 2;
            match &self.status {
                _ if animating => AgentState::Working,
                Status::Agent { state: AgentState::Waiting, .. } if self.attention => AgentState::Waiting,
                Status::Agent { state: AgentState::Working, .. } => AgentState::Done,
                Status::Agent { state, .. } => *state,
                _ => AgentState::Idle,
            }
        };
        self.status = Status::Agent { name: agent, state };
    }

    fn shell_status(&self) -> Status {
        if let Some((command, since)) = &self.running {
            let command = command.clone().or_else(|| self.foreground_command());
            return Status::Running { command, since: *since };
        }
        // Without shell integration, a foreground process other than the
        // shell means something is running.
        if self.session.shell_phase() == ShellPhase::Unknown && self.foreground_command().is_some() {
            let since = match &self.status {
                Status::Running { since, .. } => *since,
                _ => Instant::now(),
            };
            return Status::Running { command: self.foreground_command(), since };
        }
        match self.last_exit {
            Some(exit) if exit != 0 => Status::Failed { exit },
            _ => Status::Idle,
        }
    }

    fn foreground_command(&self) -> Option<String> {
        (!self.process.at_shell).then(|| self.process.foreground.clone()).flatten()
    }

    /// Second line in the sidebar.
    pub fn detail(&self) -> Option<String> {
        match &self.status {
            Status::Agent { name, state } => {
                let what = match state {
                    AgentState::Working => "working",
                    AgentState::Waiting => "needs input",
                    AgentState::Done => "done",
                    AgentState::Idle => "ready",
                };
                Some(match &self.message {
                    Some(message) if *state != AgentState::Working => format!("{name} · {message}"),
                    _ => format!("{name} · {what}"),
                })
            }
            Status::Running { command, since } => {
                let elapsed = since.elapsed().as_secs();
                let time = if elapsed >= 60 { format!("{}m{}s", elapsed / 60, elapsed % 60) } else { format!("{elapsed}s") };
                Some(format!("{} · {time}", command.as_deref().unwrap_or("running")))
            }
            Status::Failed { exit } => Some(format!("exit {exit}")),
            Status::Idle => self.message.clone(),
        }
    }

    pub fn info(&self, active: bool) -> TabInfo {
        TabInfo {
            id: self.session.id(),
            title: self.title().to_string(),
            active,
            cwd: self.session.cwd(),
            process: self.process.foreground.clone(),
            status: self.status.label().to_owned(),
            detail: self.detail(),
            needs_attention: self.attention,
            shell_integration: self.session.shell_integration(),
        }
    }
}
