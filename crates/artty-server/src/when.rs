//! `artty when`: a command in a pane finishing (or failing) handed to an
//! agent — one running (asked, as `artty ask` does: once it's between
//! turns) or a new one (started with it as its task) — told enough to act:
//! what ran, how it ended, how long it took, its output's last lines, and
//! the note it was set with.

use std::time::Duration;

use artty_core::control::WhenInfo;
use serde::{Deserialize, Serialize};

use artty_core::layout::PaneId;

/// Lines of a command's output the agent is given.
const LAST_LINES: usize = 40;

/// Who handles it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Handler {
    /// The agent in this pane: asked.
    Ask(PaneId),
    /// A new agent (`claude`, `codex`, ...): started with it.
    New(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct When {
    pub id: u64,
    pub pane: PaneId,
    /// Only when it fails (exit not 0).
    pub failed: bool,
    /// Every time (else once).
    pub every: bool,
    pub handler: Handler,
    pub note: Option<String>,
}

impl When {
    pub fn info(&self) -> WhenInfo {
        let (ask, agent) = match &self.handler {
            Handler::Ask(pane) => (Some(*pane), None),
            Handler::New(agent) => (None, Some(agent.clone())),
        };
        WhenInfo {
            id: self.id,
            tab: self.pane,
            failed: self.failed,
            every: self.every,
            ask,
            agent,
            note: self.note.clone(),
        }
    }

    /// Whether a command finishing with `exit` sets it off.
    pub fn fires(&self, exit: Option<i32>) -> bool {
        !self.failed || exit.is_some_and(|code| code != 0)
    }
}

/// A when kept for a restart: its panes by their threads' keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub pane: u64,
    pub failed: bool,
    pub every: bool,
    pub ask: Option<u64>,
    pub agent: Option<String>,
    pub note: Option<String>,
}

/// What the agent is given: where, what ran and how it ended, its last
/// lines, how to read more, and the note.
pub struct Finished<'a> {
    pub pane: PaneId,
    /// Its workspace · tab title.
    pub place: &'a str,
    pub command: Option<&'a str>,
    pub exit: Option<i32>,
    pub took: Option<Duration>,
    pub output: &'a str,
    pub note: Option<&'a str>,
}

pub fn message(finished: &Finished) -> String {
    let Finished { pane, place, command, exit, took, output, note } = finished;
    let what = match command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(command) => format!("The command `{command}`"),
        None => "The command".to_owned(),
    };
    let how = match exit {
        Some(0) => "finished (exit 0)".to_owned(),
        Some(code) => format!("failed (exit {code})"),
        None => "finished".to_owned(),
    };
    let took = took.map(|d| format!(", after {}", duration(d))).unwrap_or_default();
    let mut out = format!("{what} in tab {pane} ({place}) {how}{took}.");
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let last = &lines[lines.len().saturating_sub(LAST_LINES)..];
    if last.is_empty() {
        out.push_str(" It printed nothing.");
    } else {
        out.push_str(&format!(" Its last lines:\n\n```\n{}\n```", last.join("\n")));
    }
    out.push_str(&format!("\n\n(More of it: `artty read --tab {pane} --last`.)"));
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str(&format!("\n\n{note}"));
    }
    out
}

/// `2h 13m`, `4m 2s`, `12s`.
fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_agent_is_told() {
        let output: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        let finished = Finished {
            pane: 5,
            place: "DevPod · training",
            command: Some("python train.py"),
            exit: Some(1),
            took: Some(Duration::from_secs(2 * 3600 + 13 * 60 + 5)),
            output: &output,
            note: Some("  If it's OOM, halve the batch.  "),
        };
        let said = message(&finished);
        assert!(said.starts_with(
            "The command `python train.py` in tab 5 (DevPod · training) failed (exit 1), after 2h 13m."
        ), "{said}");
        // The last 40 lines: 11 to 50.
        assert!(said.contains("```\nline 11\n") && said.contains("line 50\n```"), "{said}");
        assert!(!said.contains("line 10\n"), "{said}");
        assert!(said.contains("artty read --tab 5 --last"));
        assert!(said.ends_with("\n\nIf it's OOM, halve the batch."));

        let quiet = Finished {
            output: "",
            command: None,
            exit: Some(0),
            took: None,
            note: None,
            ..finished
        };
        assert_eq!(
            message(&quiet),
            "The command in tab 5 (DevPod · training) finished (exit 0). It printed nothing.\n\n\
             (More of it: `artty read --tab 5 --last`.)"
        );
    }

    #[test]
    fn what_sets_it_off() {
        let when = |failed| When {
            id: 1,
            pane: 2,
            failed,
            every: false,
            handler: Handler::Ask(3),
            note: None,
        };
        assert!(
            when(false).fires(Some(0)) && when(false).fires(Some(2)) && when(false).fires(None)
        );
        assert!(!when(true).fires(Some(0)) && when(true).fires(Some(2)) && !when(true).fires(None));
    }
}
