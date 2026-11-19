//! Agents' permission requests, answerable from artty while the agent shows
//! its own prompt.
//!
//! An agent's `PermissionRequest` hook runs `artty approve --hook`, which
//! asks the server and waits. Claude Code shows its prompt meanwhile, so the
//! request is answered either here (the hook prints the decision, and the
//! agent's prompt goes) or there, at the prompt. What tells this server
//! which, and when it's over:
//!
//! - **decided** here: answered to the hook;
//! - **allowed there**: the agent reports using that tool, with that input
//!   (`PostToolUse`): the hook is released with no decision;
//! - **dismissed there** (denied, or the turn stopped): the agent ends the
//!   hook, so its connection closes. (Nothing else says so: no hook event
//!   comes after a prompt is turned down.)
//! - **ended**: the turn or the session ended first.
//!
//! Only one request of a pane's is shown by its agent at a time, but a pane
//! may have several pending (parallel tool calls); each is its own record.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use artty_core::control::{ApprovalInfo, ApprovalState, Decision, HookEvent};
use artty_core::layout::PaneId;

/// How long requests are kept, once over, for `artty approvals`.
const KEEP: Duration = Duration::from_secs(3600);
/// How long a request waits for an answer in artty before it's left to the
/// agent's own prompt. The prompt stays up regardless; an agent that can't
/// show one (`claude -p`) waits this long, then denies it as it would have.
pub const WAIT: Duration = Duration::from_secs(600);

/// The connection a hook waits on.
pub type Conn = u64;

struct Approval {
    pane: PaneId,
    conn: Conn,
    agent: Option<String>,
    tool_name: String,
    tool_input: serde_json::Value,
    state: ApprovalState,
    asked: Instant,
    finished: Option<Instant>,
}

/// A request now over, whose hook (on `conn`) is to be answered.
#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    pub id: u64,
    pub conn: Conn,
    pub decision: Option<Decision>,
}

#[derive(Default)]
pub struct Approvals {
    /// The last id given (from the server's start, in milliseconds: an id
    /// from an earlier server mustn't name another request).
    next: u64,
    items: BTreeMap<u64, Approval>,
}

impl Approvals {
    pub fn new() -> Self {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        Approvals { next: start, ..Approvals::default() }
    }

    pub fn open(
        &mut self,
        pane: PaneId,
        conn: Conn,
        agent: Option<String>,
        tool_name: String,
        tool_input: serde_json::Value,
        now: Instant,
    ) -> u64 {
        self.next += 1;
        let approval = Approval {
            pane,
            conn,
            agent,
            tool_name,
            tool_input,
            state: ApprovalState::Pending,
            asked: now,
            finished: None,
        };
        self.items.insert(self.next, approval);
        self.prune(now);
        self.next
    }

    /// Someone decided `id`.
    pub fn decide(&mut self, id: u64, decision: Decision, now: Instant) -> Result<Release, String> {
        let item = self.items.get_mut(&id).ok_or(format!("no permission request {id}"))?;
        if item.state != ApprovalState::Pending {
            return Err(format!("permission request {id} is already over ({})", name(item.state)));
        }
        item.finish(
            if decision.allow { ApprovalState::Allowed } else { ApprovalState::Denied },
            now,
        );
        Ok(Release { id, conn: item.conn, decision: Some(decision) })
    }

    /// An agent's hook reported `event`: requests answered at its prompt, or
    /// outlived by their turn.
    pub fn hook(&mut self, pane: PaneId, event: &HookEvent, now: Instant) -> Vec<Release> {
        let pending = |a: &&mut Approval| a.pane == pane && a.state == ApprovalState::Pending;
        if event.event == "PostToolUse" {
            let used = |a: &&mut Approval| {
                a.pane == pane
                    && event.tool_name.as_deref() == Some(a.tool_name.as_str())
                    && event.tool_input.as_ref() == Some(&a.tool_input)
            };
            // Denied here after it was allowed at the prompt: the tool ran.
            // (The prompt's answer wins; the hook's comes too late.)
            if let Some((_, late)) =
                self.items.iter_mut().find(|(_, a)| a.state == ApprovalState::Denied && used(a))
            {
                late.state = ApprovalState::AllowedThere;
                return Vec::new();
            }
            // The oldest request for this very call: allowed there.
            let found = self.items.iter_mut().find(|(_, a)| pending(a) && used(a));
            return found
                .map(|(&id, a)| {
                    a.finish(ApprovalState::AllowedThere, now);
                    Release { id, conn: a.conn, decision: None }
                })
                .into_iter()
                .collect();
        }
        if event.turn().is_none() && !event.session_boundary() {
            return Vec::new();
        }
        self.items
            .iter_mut()
            .filter(|(_, a)| pending(a))
            .map(|(&id, a)| {
                a.finish(ApprovalState::Ended, now);
                Release { id, conn: a.conn, decision: None }
            })
            .collect()
    }

    /// The hook waiting on `conn` ended (its agent ended it: the prompt was
    /// dismissed). The pane, if that was a pending request.
    pub fn hangup(&mut self, conn: Conn, now: Instant) -> Option<PaneId> {
        let (_, item) = self
            .items
            .iter_mut()
            .find(|(_, a)| a.conn == conn && a.state == ApprovalState::Pending)?;
        item.finish(ApprovalState::Dismissed, now);
        Some(item.pane)
    }

    /// Requests nobody answered in artty for [`WAIT`]: left to the agent.
    pub fn expire(&mut self, now: Instant) -> Vec<Release> {
        self.items
            .iter_mut()
            .filter(|(_, a)| a.state == ApprovalState::Pending)
            .filter(|(_, a)| now.duration_since(a.asked) >= WAIT)
            .map(|(&id, a)| {
                a.finish(ApprovalState::Expired, now);
                Release { id, conn: a.conn, decision: None }
            })
            .collect()
    }

    pub fn pane_closed(&mut self, pane: PaneId, now: Instant) -> Vec<Release> {
        self.items
            .iter_mut()
            .filter(|(_, a)| a.pane == pane && a.state == ApprovalState::Pending)
            .map(|(&id, a)| {
                a.finish(ApprovalState::Ended, now);
                Release { id, conn: a.conn, decision: None }
            })
            .collect()
    }

    pub fn pending_in(&self, pane: PaneId) -> bool {
        self.items.values().any(|a| a.pane == pane && a.state == ApprovalState::Pending)
    }

    pub fn info(&self, id: u64, now: Instant) -> Option<ApprovalInfo> {
        let item = self.items.get(&id)?;
        Some(ApprovalInfo {
            id,
            pane: item.pane,
            agent: item.agent.clone(),
            tool_name: item.tool_name.clone(),
            summary: summary(&item.tool_name, &item.tool_input),
            tool_input: item.tool_input.clone(),
            state: item.state,
            age_secs: now.duration_since(item.asked).as_secs(),
        })
    }

    /// Pending ones first (oldest first), then those over (newest first).
    pub fn list(&self, pane: Option<PaneId>, now: Instant) -> Vec<ApprovalInfo> {
        let mine = |a: &Approval| pane.is_none_or(|p| a.pane == p);
        let (mut pending, mut over): (Vec<_>, Vec<_>) = self
            .items
            .iter()
            .filter(|(_, a)| mine(a))
            .partition(|(_, a)| a.state == ApprovalState::Pending);
        over.reverse();
        pending.append(&mut over);
        pending.into_iter().filter_map(|(&id, _)| self.info(id, now)).collect()
    }

    fn prune(&mut self, now: Instant) {
        self.items.retain(|_, a| a.finished.is_none_or(|at| now.duration_since(at) < KEEP));
    }
}

impl Approval {
    fn finish(&mut self, state: ApprovalState, now: Instant) {
        self.state = state;
        self.finished = Some(now);
    }
}

pub fn name(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "pending",
        ApprovalState::Allowed => "allowed",
        ApprovalState::Denied => "denied",
        ApprovalState::AllowedThere => "allowed at the agent",
        ApprovalState::Dismissed => "dismissed at the agent",
        ApprovalState::Ended => "ended",
        ApprovalState::Expired => "expired",
    }
}

/// What a tool call would do, in a line.
pub fn summary(tool: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
    let text = field("command")
        .or_else(|| field("file_path"))
        .or_else(|| field("notebook_path"))
        .or_else(|| field("url"))
        .or_else(|| field("pattern"))
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string());
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = line.chars().take(160).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    match tool {
        "Bash" => short,
        _ => format!("{tool}: {short}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PANE: PaneId = 3;

    fn event(name: &str) -> HookEvent {
        HookEvent { event: name.into(), ..HookEvent::default() }
    }

    fn used(tool: &str, input: serde_json::Value) -> HookEvent {
        HookEvent {
            event: "PostToolUse".into(),
            tool_name: Some(tool.into()),
            tool_input: Some(input),
            ..HookEvent::default()
        }
    }

    fn state(approvals: &Approvals, id: u64) -> ApprovalState {
        approvals.info(id, Instant::now()).unwrap().state
    }

    fn open(approvals: &mut Approvals, conn: Conn, command: &str) -> u64 {
        let input = json!({ "command": command });
        approvals.open(PANE, conn, Some("claude".into()), "Bash".into(), input, Instant::now())
    }

    #[test]
    fn decided_here() {
        let mut approvals = Approvals::new();
        let id = open(&mut approvals, 7, "touch x");
        let deny = Decision { allow: false, message: Some("no".into()) };
        let release = approvals.decide(id, deny.clone(), Instant::now()).unwrap();
        assert_eq!(release, Release { id, conn: 7, decision: Some(deny) });
        assert_eq!(state(&approvals, id), ApprovalState::Denied);
        // Once only.
        let allow = Decision { allow: true, message: None };
        assert!(approvals.decide(id, allow, Instant::now()).is_err());
        // Its hook ending afterwards changes nothing.
        assert_eq!(approvals.hangup(7, Instant::now()), None);
    }

    #[test]
    fn allowed_at_the_agents_prompt() {
        let mut approvals = Approvals::new();
        let a = open(&mut approvals, 1, "touch a");
        let b = open(&mut approvals, 2, "touch b");
        let now = Instant::now();
        // Another call's use answers neither.
        assert!(approvals.hook(PANE, &used("Bash", json!({"command": "ls"})), now).is_empty());
        let releases = approvals.hook(PANE, &used("Bash", json!({"command": "touch b"})), now);
        assert_eq!(releases, vec![Release { id: b, conn: 2, decision: None }]);
        assert_eq!(state(&approvals, b), ApprovalState::AllowedThere);
        assert_eq!(state(&approvals, a), ApprovalState::Pending);
        // Mid-turn events end nothing.
        assert!(approvals.hook(PANE, &event("Notification"), now).is_empty());
        assert!(approvals.pending_in(PANE));
    }

    #[test]
    fn dismissed_at_the_agents_prompt() {
        let mut approvals = Approvals::new();
        let id = open(&mut approvals, 5, "rm -rf build");
        assert_eq!(approvals.hangup(5, Instant::now()), Some(PANE));
        assert_eq!(state(&approvals, id), ApprovalState::Dismissed);
        assert!(!approvals.pending_in(PANE));
    }

    #[test]
    fn outlived_by_the_turn() {
        let mut approvals = Approvals::new();
        let id = open(&mut approvals, 5, "touch x");
        let now = Instant::now();
        // Compacting isn't an end.
        let compact = HookEvent { source: Some("compact".into()), ..event("SessionStart") };
        assert!(approvals.hook(PANE, &compact, now).is_empty());
        let releases = approvals.hook(PANE, &event("Stop"), now);
        assert_eq!(releases, vec![Release { id, conn: 5, decision: None }]);
        assert_eq!(state(&approvals, id), ApprovalState::Ended);
    }

    #[test]
    fn a_late_denial_is_corrected_by_the_tools_use() {
        let mut approvals = Approvals::new();
        let id = open(&mut approvals, 4, "sleep 20");
        let now = Instant::now();
        // Allowed at the prompt (no event yet), then denied here.
        approvals.decide(id, Decision { allow: false, message: None }, now).unwrap();
        assert!(
            approvals.hook(PANE, &used("Bash", json!({"command": "sleep 20"})), now).is_empty()
        );
        assert_eq!(state(&approvals, id), ApprovalState::AllowedThere);
    }

    #[test]
    fn left_to_the_agent_after_a_while() {
        let mut approvals = Approvals::new();
        let id = open(&mut approvals, 4, "touch x");
        let now = Instant::now();
        assert!(approvals.expire(now).is_empty());
        let releases = approvals.expire(now + WAIT);
        assert_eq!(releases, vec![Release { id, conn: 4, decision: None }]);
        assert_eq!(state(&approvals, id), ApprovalState::Expired);
    }

    #[test]
    fn summaries() {
        assert_eq!(summary("Bash", &json!({"command": "cargo  test\n -p x"})), "cargo test -p x");
        assert_eq!(
            summary("Edit", &json!({"file_path": "/a/b.rs", "old_string": "x"})),
            "Edit: /a/b.rs"
        );
        assert!(summary("Bash", &json!({"command": "x".repeat(500)})).ends_with('…'));
    }

    #[test]
    fn listed_pending_first() {
        let mut approvals = Approvals::new();
        let a = open(&mut approvals, 1, "a");
        let b = open(&mut approvals, 2, "b");
        approvals.hangup(1, Instant::now());
        let ids: Vec<u64> = approvals.list(None, Instant::now()).iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![b, a]);
    }
}
