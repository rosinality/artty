//! `artty ask`: messages for agents, typed into their panes when they're
//! between turns, and the replies their hooks report when the turn ends.
//!
//! An ask is a record of its own, kept after whoever asked has gone (an
//! agent's shell tool gives up long before a long turn ends): it can be
//! asked about again by id. Its life:
//!
//! - **queued** until its pane's agent is between turns, nobody has typed
//!   there for a moment, and it takes pastes whole;
//! - **delivering**: pasted in, then Enter a moment later (an agent may
//!   take a paste and an Enter in one read as a newline in the text), until
//!   the agent's `UserPromptSubmit` says it took it;
//! - **running** until the turn's `Stop`, whose last message is the reply.
//!
//! Turns are told from the hooks' event names, not the state shown in the
//! sidebar: Claude Code's `Notification` (shown as waiting) comes both for a
//! permission prompt, mid-turn, and for an agent idle between turns. After
//! `SessionEnd`, nothing is typed until a `SessionStart`: the agent may be
//! on its way out, with a shell next.
//!
//! Whether an agent (not a shell) is in the foreground is asked again just
//! before typing anything, the paste and the Enter both: the sidebar's
//! status is up to a second old.
//!
//! Nothing is typed twice: an agent that doesn't take a message fails the
//! ask (a second try could send it twice). The prompt its hook reports must
//! hold the message: more means it went in after text someone left in the
//! input box (it goes on, with a warning).

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use artty_core::control::{AgentState, AskInfo, AskState, HookEvent};
use artty_core::layout::PaneId;

/// Between the paste and the Enter.
const ENTER_DELAY: Duration = Duration::from_millis(150);
/// How long an agent has to take a message once Enter is typed.
const TAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long since someone typed in a pane before typing into it.
pub const QUIET: Duration = Duration::from_secs(3);
/// How long an agent must have been between turns before typing into it:
/// one quitting right after a turn (its `SessionEnd` comes later, if at
/// all) would leave the paste to the shell after it.
const SETTLE: Duration = Duration::from_millis(500);
/// How long a new ask waits for its pane to show an agent whose hooks
/// report (one just started, or showing a dialog first: Claude Code runs
/// no hooks before a folder is trusted). Nothing's typed meanwhile.
const START_GRACE: Duration = Duration::from_secs(30);
/// How long finished asks are kept for `artty ask --result`.
const KEEP: Duration = Duration::from_secs(3600);

/// What [`Asks::pump`] needs to know about a pane.
#[derive(Clone, Copy, Debug)]
pub struct PaneView {
    /// An agent is in the foreground (not a shell).
    pub agent: bool,
    /// Its hooks have reported (else its turns can't be told).
    pub hooks: bool,
    /// The state shown, for agents whose turns this server hasn't seen begin
    /// or end (e.g. they started under an older build).
    pub state: Option<AgentState>,
    /// It takes pastes whole (bracketed paste).
    pub bracketed: bool,
    /// A shell is in the foreground.
    pub shell: bool,
}

/// Bytes to type into a pane.
#[derive(Debug, PartialEq, Eq)]
pub struct Write {
    pub pane: PaneId,
    pub bytes: Vec<u8>,
}

/// Where a pane's agent is, as its hooks said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// At work on a prompt.
    Open,
    /// Between turns, taking prompts.
    Between,
    /// Its session ended (it may be exiting).
    Ended,
}

fn turn_of(event: &HookEvent) -> Option<Turn> {
    match event.event.as_str() {
        "SessionEnd" => Some(Turn::Ended),
        _ => event.turn().map(|open| if open { Turn::Open } else { Turn::Between }),
    }
}

struct Ask {
    pane: PaneId,
    text: String,
    added: Instant,
    state: AskState,
    warning: Option<String>,
    /// When the next step is due: Enter (delivering, not yet typed), or
    /// giving up on the agent taking it.
    due: Option<Instant>,
    entered: bool,
    prompt_id: Option<String>,
    reply: Option<String>,
    error: Option<String>,
    finished: Option<Instant>,
}

#[derive(Default)]
pub struct Asks {
    /// The last id given. Ids start from when the server started, in
    /// milliseconds: asks don't outlive a server (nor a hand-over), and an
    /// id from an earlier one mustn't name another's ask.
    next: u64,
    asks: BTreeMap<u64, Ask>,
    /// Where each pane's agent is, as its hooks said, and since when.
    turns: HashMap<PaneId, (Turn, Instant)>,
    /// When someone last typed in each pane.
    typed: HashMap<PaneId, Instant>,
}

/// A prompt as Claude Code reports a long paste in it: wrapped in
/// `<pasted_content id="…">` and `</pasted_content id="…">`, which go.
fn unwrap_pastes(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(at) =
        ["<pasted_content", "</pasted_content"].iter().filter_map(|tag| rest.find(tag)).min()
    {
        out.push_str(&rest[..at]);
        let Some(end) = rest[at..].find('>') else {
            rest = &rest[at..];
            break;
        };
        rest = &rest[at + end + 1..];
        // The newline the tag sits on its own line with.
        rest = rest.strip_prefix('\n').unwrap_or(rest);
    }
    out.push_str(rest);
    out
}

/// Text an agent would take as a command rather than a message.
pub fn commandlike(text: &str) -> bool {
    text.starts_with(['/', '!', '#'])
}

impl Asks {
    pub fn new() -> Self {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        Asks { next: start, ..Asks::default() }
    }

    pub fn add(&mut self, pane: PaneId, text: String, now: Instant) -> u64 {
        self.next += 1;
        let id = self.next;
        let ask = Ask {
            pane,
            text,
            added: now,
            state: AskState::Queued,
            warning: None,
            due: None,
            entered: false,
            prompt_id: None,
            reply: None,
            error: None,
            finished: None,
        };
        self.asks.insert(id, ask);
        id
    }

    pub fn info(&self, id: u64) -> Option<AskInfo> {
        let ask = self.asks.get(&id)?;
        let mut text: String = ask.text.chars().take(80).collect();
        if text.len() < ask.text.len() {
            text.push('…');
        }
        Some(AskInfo {
            id,
            pane: ask.pane,
            state: ask.state,
            reply: ask.reply.clone(),
            error: ask.error.clone(),
            warning: ask.warning.clone(),
            text,
        })
    }

    /// Asks not yet over: all, or `pane`'s; oldest first.
    pub fn pending_ids(&self, pane: Option<PaneId>) -> Vec<u64> {
        let mine = |ask: &Ask| pane.is_none_or(|pane| ask.pane == pane);
        self.asks
            .iter()
            .filter(|(_, a)| !a.state.finished() && mine(a))
            .map(|(&id, _)| id)
            .collect()
    }

    pub fn finished(&self, id: u64) -> bool {
        self.asks.get(&id).is_none_or(|ask| ask.state.finished())
    }

    pub fn cancel(&mut self, id: u64, abandon: bool, now: Instant) -> Result<(), String> {
        let ask = self.asks.get_mut(&id).ok_or(format!("no ask {id}"))?;
        match ask.state {
            AskState::Queued => {
                ask.state = AskState::Cancelled;
                ask.finished = Some(now);
                Ok(())
            },
            state if state.finished() => Ok(()),
            // With its agent: stop waiting for its reply (what the agent
            // does with it goes on).
            AskState::Running | AskState::Delivering if abandon => {
                ask.state = AskState::Cancelled;
                ask.error = Some("abandoned: the agent may still be at work on it".to_owned());
                ask.finished = Some(now);
                ask.due = None;
                Ok(())
            },
            _ => {
                Err(format!("ask {id} is already with its agent (--abandon stops waiting for it)"))
            },
        }
    }

    /// The pane's agent stopped its turn at a permission prompt: its ask
    /// gets no answer. (Whether the agent goes on, as with feedback given
    /// at the prompt, or is idle can't be told: the turn stays open until
    /// a hook says otherwise.)
    pub fn interrupted(&mut self, pane: PaneId, now: Instant) {
        for ask in self.asks.values_mut().filter(|a| a.pane == pane && a.state.in_agent()) {
            ask.fail(
                "the agent's turn was interrupted at a permission prompt (check the pane)",
                now,
            );
        }
    }

    /// Someone typed in `pane`.
    pub fn typed(&mut self, pane: PaneId, now: Instant) {
        self.typed.insert(pane, now);
    }

    /// The pane closed, or its asks can't go on for another reason.
    pub fn fail_pane(&mut self, pane: PaneId, message: &str, now: Instant) {
        for ask in self.asks.values_mut().filter(|a| a.pane == pane && !a.state.finished()) {
            ask.fail(message, now);
        }
        self.turns.remove(&pane);
        self.typed.remove(&pane);
    }

    /// An agent's hook reported `event`.
    pub fn hook(&mut self, pane: PaneId, event: &HookEvent, now: Instant) {
        if let Some(turn) = turn_of(event) {
            self.turns.insert(pane, (turn, now));
        }
        let Some(ask) = self.asks.values_mut().find(|a| a.pane == pane && a.state.in_agent())
        else {
            return;
        };
        if event.session_boundary() {
            ask.fail(
                "the agent's session ended before it answered (check the pane before asking \
                 again)",
                now,
            );
            return;
        }
        match (event.turn(), ask.state) {
            // It took a message right after our Enter: ours. (Another would
            // need someone to send one in those milliseconds; failing ours
            // while the agent works on it would invite sending it twice.)
            // The prompt as reported should be ours; if not, say so.
            (Some(true), AskState::Delivering) if ask.entered => {
                let prompt = event.prompt.as_deref().map(unwrap_pastes);
                let prompt = prompt.as_deref().map(str::trim);
                let ours = ask.text.trim();
                ask.warning = match prompt {
                    Some(prompt) if prompt == ours => None,
                    // Typed after a draft: the draft first.
                    Some(prompt) if prompt.ends_with(ours) => {
                        Some("it went in after text left in the agent's input box".to_owned())
                    },
                    Some(_) => Some(
                        "the prompt the agent reported isn't the message as sent (check the \
                         pane)"
                            .to_owned(),
                    ),
                    None => None,
                };
                ask.state = AskState::Running;
                ask.due = None;
                ask.prompt_id = event.prompt_id.clone();
            },
            // Another prompt got in first (someone typed it).
            (Some(true), AskState::Delivering) => {
                ask.fail("another message got to the agent first", now);
            },
            // Another prompt: its turn ended without a Stop (interrupted).
            (Some(true), AskState::Running) => {
                ask.fail("the agent's turn ended without an answer (interrupted?)", now);
            },
            (Some(false), AskState::Running) => {
                let same = match (&ask.prompt_id, &event.prompt_id) {
                    (Some(ours), Some(theirs)) => ours == theirs,
                    _ => true,
                };
                if !same {
                    return;
                }
                match event.reply.clone() {
                    Some(reply) => {
                        ask.state = AskState::Answered;
                        ask.reply = Some(reply);
                        ask.finished = Some(now);
                        ask.due = None;
                    },
                    None => ask.fail("the agent's turn ended but its hook gave no reply", now),
                }
            },
            _ => {},
        }
    }

    /// Move asks along: type into panes that are ready for one, Enter after
    /// a paste, give up on agents that didn't take theirs.
    pub fn pump(&mut self, now: Instant, view: impl Fn(PaneId) -> Option<PaneView>) -> Vec<Write> {
        let mut writes = Vec::new();
        let busy: Vec<PaneId> =
            self.asks.values().filter(|a| a.state.in_agent()).map(|a| a.pane).collect();
        for ask in self.asks.values_mut() {
            match ask.state {
                AskState::Delivering if !ask.entered && ask.due.is_some_and(|d| d <= now) => {
                    // Still the agent? Else the paste stays unsent, and is
                    // taken out of a shell's line (Ctrl-U).
                    let view = view(ask.pane);
                    if !view.is_some_and(|v| v.agent) {
                        if view.is_some_and(|v| v.shell) {
                            writes.push(Write { pane: ask.pane, bytes: b"\x15".to_vec() });
                        }
                        ask.fail("the agent went away before Enter (its message wasn't sent)", now);
                        continue;
                    }
                    writes.push(Write { pane: ask.pane, bytes: b"\r".to_vec() });
                    ask.entered = true;
                    ask.due = Some(now + TAKE_TIMEOUT);
                },
                AskState::Delivering if ask.due.is_some_and(|d| d <= now) => {
                    ask.fail(
                        "the agent's hook didn't confirm it took the message (not sent \
                         again; check the pane before asking again)",
                        now,
                    );
                },
                _ => {},
            }
        }
        // The oldest queued ask of each pane with none under way.
        let mut started: Vec<PaneId> = Vec::new();
        for ask in self.asks.values_mut().filter(|a| a.state == AskState::Queued) {
            let pane = ask.pane;
            if busy.contains(&pane) || started.contains(&pane) {
                continue;
            }
            // At work: nothing to look at yet (and this is the common case,
            // checked on every batch of output).
            let (turn, since) = self.turns.get(&pane).copied().unzip();
            let settled = since.is_none_or(|at| now.duration_since(at) >= SETTLE);
            if turn == Some(Turn::Open) || !settled {
                started.push(pane);
                continue;
            }
            let Some(view) = view(pane) else {
                ask.fail("the pane closed", now);
                continue;
            };
            // A new ask may wait for its agent to start, but not for one
            // to start again in a shell its last one left.
            let left = view.shell && turn == Some(Turn::Ended);
            let starting = now.duration_since(ask.added) < START_GRACE && !left;
            if !view.agent || !view.hooks {
                if starting {
                    started.push(pane);
                } else if !view.agent {
                    ask.fail("no agent is running in the pane", now);
                } else {
                    ask.fail(
                        "the agent's hooks haven't reported: is it showing a dialog? (else: \
                         artty hooks install)",
                        now,
                    );
                }
                continue;
            }
            // Its turn: from the hooks' events, else the state shown.
            let between_turns = match turn {
                Some(turn) => turn == Turn::Between,
                None => matches!(view.state, Some(AgentState::Done | AgentState::Idle)),
            };
            let quiet = self.typed.get(&pane).is_none_or(|&at| now.duration_since(at) >= QUIET);
            if !between_turns || !quiet || !view.bracketed {
                started.push(pane);
                continue;
            }
            writes.push(Write { pane, bytes: artty_core::keys::paste(&ask.text, true) });
            ask.state = AskState::Delivering;
            ask.due = Some(now + ENTER_DELAY);
            started.push(pane);
        }
        let old = |ask: &Ask| ask.finished.is_some_and(|at| now.duration_since(at) >= KEEP);
        self.asks.retain(|_, ask| !old(ask));
        writes
    }

    /// When [`Asks::pump`] next has something to do on its own (not counting
    /// panes becoming ready, which other events bring).
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let due = self.asks.values().filter_map(|a| a.due).min();
        // A pane someone typed in becomes quiet; an agent's turn settles.
        let queued = self.asks.values().filter(|a| a.state == AskState::Queued);
        let quiet = queued.clone().filter_map(|a| self.typed.get(&a.pane)).map(|&at| at + QUIET);
        let settled = queued
            .filter_map(|a| self.turns.get(&a.pane))
            .filter(|(turn, _)| *turn != Turn::Open)
            .map(|&(_, at)| at + SETTLE);
        due.into_iter().chain(quiet.chain(settled).filter(|&at| at > now)).min()
    }

    /// Whether any ask isn't over (the pump has work to watch for).
    pub fn pending(&self) -> bool {
        self.asks.values().any(|a| !a.state.finished())
    }
}

impl Ask {
    fn fail(&mut self, message: &str, now: Instant) {
        self.state = AskState::Failed;
        self.error = Some(message.to_owned());
        self.finished = Some(now);
        self.due = None;
    }
}

trait InAgent {
    /// Given to the agent and not over.
    fn in_agent(self) -> bool;
}

impl InAgent for AskState {
    fn in_agent(self) -> bool {
        matches!(self, AskState::Delivering | AskState::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: PaneId = 4;

    fn ready(_: PaneId) -> Option<PaneView> {
        Some(PaneView {
            agent: true,
            hooks: true,
            state: Some(AgentState::Done),
            bracketed: true,
            shell: false,
        })
    }

    fn event(name: &str, prompt_id: Option<&str>, reply: Option<&str>) -> HookEvent {
        HookEvent {
            event: name.to_owned(),
            prompt: None,
            prompt_id: prompt_id.map(str::to_owned),
            reply: reply.map(str::to_owned),
            ..HookEvent::default()
        }
    }

    fn state(asks: &Asks, id: u64) -> AskState {
        asks.info(id).unwrap().state
    }

    /// Through paste and Enter, the agent taking it.
    fn deliver(asks: &mut Asks, id: u64, now: Instant) -> Instant {
        let writes = asks.pump(now, ready);
        assert_eq!(writes, vec![Write { pane: PANE, bytes: b"\x1b[200~hi\x1b[201~".to_vec() }]);
        let later = now + ENTER_DELAY;
        assert_eq!(asks.pump(later, ready), vec![Write { pane: PANE, bytes: b"\r".to_vec() }]);
        asks.hook(PANE, &event("UserPromptSubmit", Some("p1"), None), later);
        assert_eq!(state(asks, id), AskState::Running);
        later
    }

    #[test]
    fn answered_with_the_stop_hooks_reply() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        let now = deliver(&mut asks, id, now);
        // Mid-turn events change nothing.
        asks.hook(PANE, &event("Notification", None, None), now);
        asks.hook(PANE, &event("Stop", Some("p1"), Some("hello")), now);
        let info = asks.info(id).unwrap();
        assert_eq!((info.state, info.reply.as_deref()), (AskState::Answered, Some("hello")));
    }

    #[test]
    fn waits_for_the_turn_to_end() {
        let mut asks = Asks::default();
        let now = Instant::now();
        asks.hook(PANE, &event("UserPromptSubmit", None, None), now);
        let id = asks.add(PANE, "hi".into(), now);
        assert!(asks.pump(now, ready).is_empty());
        assert_eq!(state(&asks, id), AskState::Queued);
        // Idle between turns, though shown as waiting: Claude Code's idle
        // notification.
        asks.hook(PANE, &event("Stop", None, Some("done")), now);
        asks.hook(PANE, &event("Notification", None, None), now);
        let waiting = |_| {
            Some(PaneView {
                agent: true,
                hooks: true,
                state: Some(AgentState::Waiting),
                bracketed: true,
                shell: false,
            })
        };
        assert!(asks.pump(now, waiting).is_empty());
        assert_eq!(asks.next_due(now), Some(now + SETTLE));
        assert_eq!(asks.pump(now + SETTLE, waiting).len(), 1);
        assert_eq!(state(&asks, id), AskState::Delivering);
    }

    #[test]
    fn waits_while_someone_types() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        asks.typed(PANE, now);
        assert!(asks.pump(now + Duration::from_secs(1), ready).is_empty());
        assert_eq!(asks.next_due(now), Some(now + QUIET));
        assert_eq!(asks.pump(now + QUIET, ready).len(), 1);
        assert_eq!(state(&asks, id), AskState::Delivering);
    }

    #[test]
    fn one_at_a_time_per_pane() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let first = asks.add(PANE, "hi".into(), now);
        let second = asks.add(PANE, "again".into(), now);
        let now = deliver(&mut asks, first, now);
        assert!(asks.pump(now, ready).is_empty());
        asks.hook(PANE, &event("Stop", Some("p1"), Some("ok")), now);
        assert_eq!(asks.pump(now + SETTLE, ready).len(), 1);
        assert_eq!(state(&asks, second), AskState::Delivering);
    }

    #[test]
    fn never_typed_twice() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        asks.pump(now, ready);
        asks.pump(now + ENTER_DELAY, ready);
        // No UserPromptSubmit.
        let late = now + ENTER_DELAY + TAKE_TIMEOUT;
        assert!(asks.pump(late, ready).is_empty());
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn someone_elses_prompt_first() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        asks.pump(now, ready);
        // Before our Enter.
        asks.hook(PANE, &event("UserPromptSubmit", None, None), now);
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn interrupted_or_ended() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        let now = deliver(&mut asks, id, now);
        asks.hook(PANE, &event("UserPromptSubmit", Some("p2"), None), now);
        assert_eq!(state(&asks, id), AskState::Failed);
        // That turn ends, isn't ours: the next waits for it.
        let id = asks.add(PANE, "hi".into(), now);
        assert!(asks.pump(now, ready).is_empty());
        asks.hook(PANE, &event("Stop", Some("p2"), Some("theirs")), now);
        assert_eq!(state(&asks, id), AskState::Queued);
        let now = deliver(&mut asks, id, now + SETTLE);
        asks.hook(PANE, &event("SessionEnd", None, None), now);
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn a_stop_of_another_turn_is_not_the_answer() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        let now = deliver(&mut asks, id, now);
        asks.hook(PANE, &event("Stop", Some("p0"), Some("old")), now);
        assert_eq!(state(&asks, id), AskState::Running);
    }

    #[test]
    fn not_into_a_shell_or_an_agent_without_hooks() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        let shell = |_| {
            Some(PaneView {
                agent: false,
                hooks: false,
                state: None,
                bracketed: true,
                shell: false,
            })
        };
        // Maybe one about to start: held, untyped, for a while.
        assert!(asks.pump(now, shell).is_empty());
        assert_eq!(state(&asks, id), AskState::Queued);
        assert!(asks.pump(now + START_GRACE, shell).is_empty());
        assert_eq!(state(&asks, id), AskState::Failed);
        let id = asks.add(PANE, "hi".into(), now);
        let no_hooks = |_| {
            Some(PaneView { agent: true, hooks: false, state: None, bracketed: true, shell: false })
        };
        assert!(asks.pump(now, no_hooks).is_empty());
        assert_eq!(state(&asks, id), AskState::Queued);
        assert!(asks.pump(now + START_GRACE, no_hooks).is_empty());
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn nothing_typed_after_the_session_ends() {
        let mut asks = Asks::default();
        let now = Instant::now();
        asks.hook(PANE, &event("SessionEnd", None, None), now);
        let id = asks.add(PANE, "hi".into(), now);
        // Shown as done, still the agent's process for a moment: held.
        assert!(asks.pump(now, ready).is_empty());
        assert_eq!(state(&asks, id), AskState::Queued);
        // A new session (`/clear`, a resume) takes it.
        asks.hook(PANE, &event("SessionStart", None, None), now);
        assert_eq!(asks.pump(now + SETTLE, ready).len(), 1);
        // A shell back instead: failed, untyped.
        let id = asks.add(PANE + 1, "hi".into(), now);
        asks.hook(PANE + 1, &event("SessionEnd", None, None), now);
        let shell = |_| {
            Some(PaneView { agent: false, hooks: true, state: None, bracketed: true, shell: true })
        };
        let writes = asks.pump(now + SETTLE, shell);
        assert!(writes.iter().all(|w| w.pane != PANE + 1), "{writes:?}");
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn compacting_mid_turn_is_no_boundary() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        let now = deliver(&mut asks, id, now);
        let compact = HookEvent {
            event: "SessionStart".into(),
            source: Some("compact".into()),
            ..HookEvent::default()
        };
        asks.hook(PANE, &compact, now);
        assert_eq!(state(&asks, id), AskState::Running);
        // Still in the turn: the next waits.
        let next = asks.add(PANE, "next".into(), now);
        assert!(asks.pump(now + SETTLE, ready).is_empty());
        assert_eq!(state(&asks, next), AskState::Queued);
        asks.hook(PANE, &event("Stop", Some("p1"), Some("ok")), now);
        assert_eq!(state(&asks, id), AskState::Answered);
    }

    #[test]
    fn no_enter_once_the_agent_has_gone() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        assert_eq!(asks.pump(now, ready).len(), 1);
        let shell = |_| {
            Some(PaneView { agent: false, hooks: true, state: None, bracketed: true, shell: true })
        };
        // The paste taken out of the shell's line, no Enter.
        let writes = asks.pump(now + ENTER_DELAY, shell);
        assert_eq!(writes, vec![Write { pane: PANE, bytes: b"\x15".to_vec() }]);
        assert_eq!(state(&asks, id), AskState::Failed);
    }

    #[test]
    fn the_prompt_must_hold_the_message() {
        let prompt = |text: &str| HookEvent {
            event: "UserPromptSubmit".into(),
            prompt: Some(text.into()),
            ..HookEvent::default()
        };
        let mut asks = Asks::default();
        let now = Instant::now();
        // After a draft left in the box: on, with a warning.
        let id = asks.add(PANE, "hi".into(), now);
        asks.pump(now, ready);
        asks.pump(now + ENTER_DELAY, ready);
        asks.hook(PANE, &prompt("draft hi"), now);
        let info = asks.info(id).unwrap();
        assert_eq!(info.state, AskState::Running);
        assert!(info.warning.is_some());
        asks.hook(PANE, &event("Stop", None, Some("ok")), now);
        // Not ours at all: failed.
        let id = asks.add(PANE, "hi".into(), now);
        let now = now + SETTLE;
        asks.pump(now, ready);
        asks.pump(now + ENTER_DELAY, ready);
        asks.hook(PANE, &prompt("hi, and something else"), now);
        let info = asks.info(id).unwrap();
        assert_eq!(info.state, AskState::Running);
        assert!(info.warning.is_some());
        asks.hook(PANE, &event("Stop", None, Some("ok")), now);
        // A long paste, as Claude Code reports it: as sent.
        let id = asks.add(PANE, "hi".into(), now);
        let now = now + SETTLE;
        asks.pump(now, ready);
        asks.pump(now + ENTER_DELAY, ready);
        let wrapped = "\n\n<pasted_content id=\"9c36\">\nhi\n</pasted_content id=\"9c36\">\n";
        asks.hook(PANE, &prompt(wrapped), now);
        let info = asks.info(id).unwrap();
        assert_eq!((info.state, info.warning), (AskState::Running, None));
    }

    #[test]
    fn cancel_only_before_delivery() {
        let mut asks = Asks::default();
        let now = Instant::now();
        let id = asks.add(PANE, "hi".into(), now);
        asks.cancel(id, false, now).unwrap();
        assert_eq!(state(&asks, id), AskState::Cancelled);
        let id = asks.add(PANE, "hi".into(), now);
        deliver(&mut asks, id, now);
        assert!(asks.cancel(id, false, now).is_err());
    }
}
