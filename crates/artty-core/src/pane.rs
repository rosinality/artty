//! A pane: one session plus what the sidebar shows about it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use artty_vt::program_status::{ProgramState, ProgramStatus, StatusRecords, StatusReport};

use crate::control::{AgentState, TabInfo};
use crate::{ProcessInfo, SavedAgent, SavedPane, Session, ShellPhase};
use artty_vt::ansi::Progress;

/// What a pane knows beyond its terminal, carried across a server hand-over:
/// what its agent reported, and what the user hasn't seen yet.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PaneMeta {
    reported: Option<AgentState>,
    agent_session: Option<String>,
    reported_agent: Option<String>,
    message: Option<String>,
    attention: bool,
    done_seen: bool,
    /// The command running (from shell integration) and for how long, in
    /// seconds; and the last one's exit code.
    running: Option<(Option<String>, u64)>,
    last_exit: Option<i32>,
    /// The agent has set the terminal's title (see `Pane::agent_titled`).
    #[serde(default)]
    agent_titled: bool,
    /// The terminal's title (a program's), till it sets another.
    #[serde(default)]
    title: Option<String>,
}

/// What a pane is doing, as shown in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// At the shell prompt.
    Idle,
    /// A command (not an agent) is running. `interactive`: one you use
    /// rather than wait for (an editor, ssh, a full-screen program).
    Running { command: Option<String>, since: Instant, interactive: bool },
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

    /// How much this status matters when summarizing a tab's panes.
    pub fn urgency(&self) -> u8 {
        match self {
            Status::Agent { state: AgentState::Waiting, .. } => 5,
            status if status.busy() => 4,
            Status::Failed { .. } => 3,
            Status::Agent { state: AgentState::Done, .. } => 2,
            Status::Agent { .. } => 1,
            _ => 0,
        }
    }

    /// Whether the sidebar should animate a spinner: something is at work
    /// that you'd wait for.
    pub fn busy(&self) -> bool {
        matches!(
            self,
            Status::Running { interactive: false, .. }
                | Status::Agent { state: AgentState::Working, .. }
        )
    }
}

pub struct Pane {
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
    /// The user has seen the pane since its agent finished, so the sidebar
    /// stops marking it done.
    pub done_seen: bool,
    /// Last notification or agent message.
    pub message: Option<String>,
    pub progress: Option<Progress>,
    /// When the visual bell last rang.
    pub flash: Option<Instant>,
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
    /// Program the tab was opened with, if not the shell.
    pub command: Option<Vec<String>>,
    /// The agent's conversation id, from its hooks.
    agent_session: Option<String>,
    /// That conversation's title, as its agent keeps it (Claude Code's
    /// transcript): shown until the agent sets the terminal's title itself
    /// (resumed, Claude Code does only once given a message). By session.
    session_title: Option<(String, String)>,
    /// The agent in the foreground has set the terminal's title.
    agent_titled: bool,
    /// The agent's name, from its hooks (`claude`, `codex`, ...).
    reported_agent: Option<String>,
    /// Typed into the shell once it shows a prompt (resuming an agent).
    pending_input: Option<(String, Instant)>,
    /// The foreground program, seen waiting for keys.
    reading: Option<Reading>,
    /// What programs here said they're doing (OSC 7501), and since when
    /// the root record's state has been what it is.
    program: StatusRecords,
    program_since: Option<Instant>,
    /// Since when its hooks have said what they last said.
    reported_since: Option<Instant>,
}

/// How long to wait for a prompt before typing pending input anyway (for
/// shells without shell integration).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(3);

const TITLE_ANIMATION_WINDOW: Duration = Duration::from_secs(2);

/// How long a program that was waiting for keys must be at work before it
/// counts as busy: a REPL running a quick line shouldn't flash a spinner.
const AT_WORK_AFTER: Duration = Duration::from_secs(1);

/// A program seen waiting for keys (its process group), and since when it
/// stopped, if it did.
#[derive(Clone, Copy, Debug)]
struct Reading {
    group: i32,
    stopped: Option<Instant>,
}

/// Whether a process name is an interactive shell (login shells start with `-`).
pub fn is_shell(name: &str) -> bool {
    const SHELLS: &[&str] = &[
        "fish", "zsh", "bash", "sh", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh", "login",
    ];
    SHELLS.contains(&name.trim_start_matches('-'))
}

impl Pane {
    pub fn new(
        session: Session,
        fixed_title: Option<String>,
        command: Option<Vec<String>>,
        agents: Arc<[String]>,
    ) -> Self {
        Self {
            session,
            title: None,
            fixed_title,
            process: ProcessInfo::default(),
            status: Status::Idle,
            attention: false,
            done_seen: false,
            message: None,
            progress: None,
            flash: None,
            reported: None,
            title_changes: Vec::new(),
            running: None,
            last_exit: None,
            agents,
            command,
            agent_session: None,
            session_title: None,
            agent_titled: false,
            reported_agent: None,
            pending_input: None,
            reading: None,
            program: StatusRecords::default(),
            program_since: None,
            reported_since: None,
        }
    }

    /// A program's status report (OSC 7501).
    pub fn program_status(&mut self, report: StatusReport) {
        let before = self.program.root().map(|s| s.state);
        self.program.apply(report);
        let root = self.program.root().cloned();
        if root.as_ref().map(|s| s.state) != before {
            self.program_since = Some(Instant::now());
        }
        // What it says it's doing, or waits for, is its message (as hooks'
        // are).
        if let Some(status) = root.filter(|s| self.agent_app(s).is_some()) {
            if status.state == ProgramState::Blocked {
                self.attention = true;
            }
            // What it's doing, or waits for, is its message; at work with
            // nothing said, it's doing something else now.
            match status.state {
                ProgramState::Working | ProgramState::Blocked if status.msg.is_some() => {
                    self.message = status.msg
                },
                ProgramState::Working => self.message = None,
                _ => {},
            }
        }
        self.refresh();
    }

    /// The root record (what the program in the foreground said it's
    /// doing), and since when its state is what it is.
    pub fn program(&self) -> Option<(&ProgramStatus, Instant)> {
        Some((self.program.root()?, self.program_since?))
    }

    /// The agent's turn ended in its own words (`state`: idle, cut short;
    /// done) after its hooks said it was at work, this long ago: in this
    /// turn, not one before it.
    pub fn said_after_hooks(&self, state: ProgramState) -> Option<Duration> {
        let (status, since) = self.program()?;
        let hooks = self.reported_since?;
        (status.state == state && self.agent_app(status).is_some() && since > hooks)
            .then(|| since.elapsed())
    }

    /// The agent a status names, if it names one artty knows (its name as
    /// hooks give it).
    fn agent_app(&self, status: &ProgramStatus) -> Option<&'static str> {
        match status.app.as_deref()? {
            "claude-code" | "claude" => Some("claude"),
            "codex" => Some("codex"),
            "gemini" | "gemini-cli" => Some("gemini"),
            _ => None,
        }
    }

    pub fn title(&self) -> String {
        // An agent that hasn't titled the terminal yet: its conversation's
        // title (not the shell's, which is the command's name).
        let agent = matches!(self.status, Status::Agent { .. });
        let session_title = self
            .session_title
            .as_ref()
            .filter(|(session, title)| {
                agent
                    && !title.is_empty()
                    && !self.agent_titled
                    && self.agent_session.as_ref() == Some(session)
            })
            .map(|(_, title)| title);
        let title = self.fixed_title.as_ref().or(session_title).or(self.title.as_ref());
        title
            .cloned()
            .or_else(|| self.process.foreground.clone())
            .unwrap_or_else(|| "shell".to_owned())
    }

    /// The conversation the agent is in, if its title isn't known yet.
    pub fn untitled_session(&self) -> Option<&str> {
        let session = self.agent_session.as_deref()?;
        let known = self.session_title.as_ref().is_some_and(|(s, _)| s == session);
        (!known && !self.agent_titled).then_some(session)
    }

    /// `session`'s title, as its agent keeps it (none: looked, none there).
    /// (Empty: looked, none there; not looked for again.)
    pub fn set_session_title(&mut self, session: &str, title: Option<String>) {
        let title = title.unwrap_or_default().trim().to_owned();
        self.session_title = Some((session.to_owned(), title));
    }

    /// The agent's conversation id, from its hooks.
    pub fn agent_session(&self) -> Option<&str> {
        self.agent_session.as_deref()
    }

    /// When the title last changed (an agent at work animates it).
    pub fn title_since(&self) -> Option<Instant> {
        self.title_changes.last().copied()
    }

    pub fn set_title(&mut self, title: Option<String>) {
        // While an agent is in the foreground, what titles the terminal is it
        // (a title that's only its name isn't one).
        let named = |t: &str| {
            let t = t.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
            self.agent_name().is_some_and(|n| n.eq_ignore_ascii_case(t))
        };
        if matches!(self.status, Status::Agent { .. })
            && title.as_deref().is_some_and(|t| !t.trim().is_empty() && !named(t))
            && title != self.title
        {
            self.agent_titled = true;
        }
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
            self.status =
                Status::Agent { name: self.agent_name().unwrap(), state: AgentState::Waiting };
        }
    }

    pub fn report(
        &mut self,
        state: AgentState,
        message: Option<String>,
        agent_session: Option<String>,
        agent: Option<String>,
    ) {
        if agent_session.is_some() {
            self.agent_session = agent_session;
        }
        if agent.is_some() {
            self.reported_agent = agent;
        }
        if state == AgentState::Waiting || state == AgentState::Done {
            self.attention = true;
        }
        if message.is_some() {
            self.message = message;
        } else if state == AgentState::Working {
            self.message = None;
        }
        if self.reported != Some(state) {
            self.reported_since = Some(Instant::now());
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
        // Back at the prompt: what was at work or waiting is over.
        self.program.program_ended();
    }

    /// The last command's exit code (none while one runs).
    pub fn last_exit(&self) -> Option<i32> {
        self.last_exit
    }

    pub fn set_agents(&mut self, agents: Arc<[String]>) {
        self.agents = agents;
    }

    /// Its agent's hooks report its state (`artty status`).
    pub fn hooks_reported(&self) -> bool {
        self.reported.is_some()
    }

    pub fn agent_name(&self) -> Option<String> {
        let name = self.process.foreground.as_deref()?;
        self.agents.iter().any(|agent| agent == name).then(|| name.to_owned())
    }

    /// Recompute the status from the latest process info and reports.
    pub fn refresh(&mut self) {
        self.note_reading();
        // A program that reports agent status counts as an agent even if its
        // name isn't on the list, until a shell is back in the foreground.
        let reporter = || {
            let name = self.process.foreground.clone().filter(|name| !is_shell(name))?;
            let says = self.program.root().and_then(|s| self.agent_app(s));
            if self.reported.is_none() && says.is_none() {
                return None;
            }
            Some(self.reported_agent.clone().or(says.map(str::to_owned)).unwrap_or(name))
        };
        let Some(agent) = self.agent_name().or_else(reporter) else {
            // What runs isn't known yet (a pane just taken over from an
            // earlier build, not yet looked at): nothing's exited.
            // (Nor one that's said what it's doing before it's looked at.)
            if self.process.foreground.is_none()
                && (self.reported.is_some() || self.program.root().is_some())
            {
                return;
            }
            // The agent exited: forget what it reported.
            self.reported = None;
            self.program.program_ended();
            self.agent_session = None;
            self.reported_agent = None;
            self.session_title = None;
            self.agent_titled = false;
            self.status = self.shell_status();
            return;
        };

        let state = self.agent_state();
        if state != AgentState::Done {
            self.done_seen = false;
        }
        self.status = Status::Agent { name: agent, state };
    }

    pub fn meta(&self) -> PaneMeta {
        PaneMeta {
            reported: self.reported,
            agent_session: self.agent_session.clone(),
            reported_agent: self.reported_agent.clone(),
            message: self.message.clone(),
            attention: self.attention,
            done_seen: self.done_seen,
            running: self
                .running
                .as_ref()
                .map(|(command, since)| (command.clone(), since.elapsed().as_secs())),
            last_exit: self.last_exit,
            agent_titled: self.agent_titled,
            title: self.title.clone(),
        }
    }

    /// Take back what a previous server build knew about this pane.
    pub fn restore_meta(&mut self, meta: PaneMeta) {
        self.reported = meta.reported;
        self.agent_session = meta.agent_session;
        self.reported_agent = meta.reported_agent;
        self.message = meta.message;
        self.attention = meta.attention;
        self.done_seen = meta.done_seen;
        let now = Instant::now();
        self.running = meta.running.map(|(command, secs)| {
            (command, now.checked_sub(Duration::from_secs(secs)).unwrap_or(now))
        });
        self.last_exit = meta.last_exit;
        self.agent_titled = meta.agent_titled;
        if self.title.is_none() {
            self.title = meta.title;
        }
        self.refresh();
    }

    /// The user is looking at the pane.
    pub fn seen(&mut self) {
        self.attention = false;
        if matches!(self.status, Status::Agent { state: AgentState::Done, .. }) {
            self.done_seen = true;
        }
    }

    fn agent_state(&self) -> AgentState {
        let said = self.program.root().filter(|s| self.agent_app(s).is_some()).map(|s| s.state);
        // Waiting for the user, as it says: over whatever hooks said last
        // (they may say nothing of a prompt, as of a login).
        if said == Some(ProgramState::Blocked) {
            return AgentState::Waiting;
        }
        // Its turn over by its own words, though no hook said so (a Stop
        // lost): done after a while (hooks come well within it).
        const NO_STOP: Duration = Duration::from_secs(10);
        if self.reported == Some(AgentState::Working)
            && self.said_after_hooks(ProgramState::Done).is_some_and(|d| d >= NO_STOP)
        {
            return AgentState::Done;
        }
        if let Some(reported) = self.reported {
            reported
        } else if let Some(said) = said {
            match said {
                ProgramState::Working => AgentState::Working,
                ProgramState::Blocked => AgentState::Waiting,
                ProgramState::Done | ProgramState::Error => AgentState::Done,
                ProgramState::Idle => AgentState::Idle,
            }
        } else {
            let now = Instant::now();
            let animating = self
                .title_changes
                .iter()
                .filter(|t| now.duration_since(**t) < TITLE_ANIMATION_WINDOW)
                .count()
                >= 2;
            match &self.status {
                _ if animating => AgentState::Working,
                Status::Agent { state: AgentState::Waiting, .. } if self.attention => {
                    AgentState::Waiting
                },
                Status::Agent { state: AgentState::Working, .. } => AgentState::Done,
                Status::Agent { state, .. } => *state,
                _ => AgentState::Idle,
            }
        }
    }

    fn shell_status(&self) -> Status {
        if let Some((command, since)) = &self.running {
            let command = command.clone().or_else(|| self.foreground_command());
            return Status::Running { command, since: *since, interactive: self.interactive() };
        }
        // Without shell integration, a foreground process other than the
        // shell means something is running.
        if self.session.shell_phase() == ShellPhase::Unknown && self.foreground_command().is_some()
        {
            let since = match &self.status {
                Status::Running { since, .. } => *since,
                _ => Instant::now(),
            };
            return Status::Running {
                command: self.foreground_command(),
                since,
                interactive: self.interactive(),
            };
        }
        match self.last_exit {
            Some(exit) if exit != 0 => Status::Failed { exit },
            _ => Status::Idle,
        }
    }

    /// Follow whether the foreground program waits for keys.
    fn note_reading(&mut self) {
        let Some(group) = self.process.foreground_group else {
            self.reading = None;
            return;
        };
        if self.process.reading_keys {
            self.reading = Some(Reading { group, stopped: None });
        } else if let Some(reading) = &mut self.reading {
            if reading.group != group {
                self.reading = None;
            } else if reading.stopped.is_none() {
                reading.stopped = Some(Instant::now());
            }
        }
    }

    /// Whether what's running is used rather than waited for: a program
    /// waiting for keys (a REPL at its prompt, a full-screen program) or
    /// only just done waiting, or one of the usual interactive ones.
    fn interactive(&self) -> bool {
        let waiting = self.reading.is_some_and(|r| {
            Some(r.group) == self.process.foreground_group
                && r.stopped.is_none_or(|t| t.elapsed() < AT_WORK_AFTER)
        });
        // REPLs aren't listed: at their prompt they wait for keys, and
        // running your code they're at work.
        const INTERACTIVE: &[&str] = &[
            "ssh",
            "mosh",
            "mosh-client",
            "et",
            "tmux",
            "screen",
            "zellij",
            "vim",
            "nvim",
            "vi",
            "emacs",
            "nano",
            "hx",
            "helix",
            "kak",
            "micro",
            "less",
            "more",
            "man",
            "htop",
            "btop",
            "top",
        ];
        waiting
            || self.session.mode().contains(artty_vt::term::TermMode::ALT_SCREEN)
            || self.process.foreground.as_deref().is_some_and(|name| INTERACTIVE.contains(&name))
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
                // (At work: what it says it's doing, if it says.)
                Some(match &self.message {
                    Some(message) => format!("{name} · {message}"),
                    None => format!("{name} · {what}"),
                })
            },
            Status::Running { command, since, .. } => {
                let elapsed = since.elapsed().as_secs();
                let time = if elapsed >= 60 {
                    format!("{}m{}s", elapsed / 60, elapsed % 60)
                } else {
                    format!("{elapsed}s")
                };
                Some(format!("{} · {time}", command.as_deref().unwrap_or("running")))
            },
            Status::Failed { exit } => Some(format!("exit {exit}")),
            Status::Idle => self.message.clone(),
        }
    }

    /// Type `input` (and Enter) once the shell is ready.
    pub fn type_when_ready(&mut self, input: String) {
        self.pending_input = Some((input, Instant::now()));
    }

    /// Send pending input if the shell is at its prompt, or has had time to
    /// get there.
    pub fn flush_pending_input(&mut self) {
        let Some((input, since)) = &self.pending_input else { return };
        if self.session.shell_phase() == ShellPhase::Prompt || since.elapsed() >= PROMPT_TIMEOUT {
            self.session.write(format!("{input}\r").into_bytes());
            self.pending_input = None;
        }
    }

    /// What to save so the pane can be reopened.
    pub fn saved(&self, scrollback_lines: usize) -> SavedPane {
        let agent = match &self.status {
            Status::Agent { name, .. } => {
                Some(SavedAgent { name: name.clone(), session: self.agent_session.clone() })
            },
            _ => None,
        };
        // An agent tab reopens as a shell that resumes the agent, not as a
        // fresh copy of whatever command started it.
        let command = if agent.is_some() { None } else { self.command.clone() };
        SavedPane {
            title: self.fixed_title.clone(),
            cwd: self.session.cwd(),
            command,
            agent,
            scrollback: self.session.styled_scrollback(scrollback_lines),
            pane: None,
            thread: None,
        }
    }

    /// `window` and `tab` are 1-based numbers.
    pub fn info(&self, window: usize, tab: usize, active: bool, focused: bool) -> TabInfo {
        TabInfo {
            id: self.session.id(),
            window,
            workspace: 0,
            workspace_name: None,
            tab,
            title: self.title(),
            active,
            focused,
            cwd: self.session.cwd(),
            process: self.process.foreground.clone(),
            status: self.status.label().to_owned(),
            detail: self.detail(),
            needs_attention: self.attention,
            shell_integration: self.session.shell_integration(),
            kitty_keyboard: self
                .session
                .mode()
                .intersects(artty_vt::term::TermMode::KITTY_KEYBOARD_PROTOCOL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_programs_dont_spin() {
        let running = |interactive| Status::Running {
            command: Some("x".into()),
            since: Instant::now(),
            interactive,
        };
        assert!(running(false).busy());
        assert!(!running(true).busy());
        // Still reported as running, just not something to wait for.
        assert_eq!(running(true).label(), "running");
        assert!(Status::Agent { name: "claude".into(), state: AgentState::Working }.busy());
    }
}
