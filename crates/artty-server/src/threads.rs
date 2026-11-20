//! Each agent's thread: what it was given and what it said, its permission
//! requests and how they went, its sessions starting and ending — in order,
//! from its hooks' events and artty's own records (asks, permission
//! requests). Kept per pane, in memory, for as long as the server runs.

use std::collections::{HashMap, VecDeque};

use artty_core::control::{ApprovalState, HookEvent, ThreadEntry, ThreadItem};
use artty_core::layout::PaneId;

/// Entries kept per pane, at most (the oldest go first).
const MOST: usize = 1000;

#[derive(Default)]
pub struct Threads {
    panes: HashMap<PaneId, VecDeque<ThreadEntry>>,
}

impl Threads {
    /// Add `item` to `pane`'s thread; its `seq`.
    pub fn add(&mut self, pane: PaneId, item: ThreadItem) -> u64 {
        let seq = crate::queue::next_id();
        let entry = ThreadEntry { seq, at_ms: crate::queue::now_ms(), item };
        let thread = self.panes.entry(pane).or_default();
        thread.push_back(entry);
        if thread.len() > MOST {
            thread.pop_front();
        }
        seq
    }

    /// An agent's hook reported `event` (`message`: what it said with it;
    /// `ask`: the ask its turn took, if a turn began).
    pub fn hook(
        &mut self,
        pane: PaneId,
        event: &HookEvent,
        message: Option<&str>,
        ask: Option<u64>,
    ) {
        if let Some(item) = item_of(event, message, ask) {
            self.add(pane, item);
        }
    }

    /// The thread's entries after `after`, the last `limit` of them.
    pub fn entries(
        &self,
        pane: PaneId,
        after: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<ThreadEntry> {
        let Some(thread) = self.panes.get(&pane) else { return Vec::new() };
        let newer: Vec<&ThreadEntry> =
            thread.iter().filter(|e| after.is_none_or(|after| e.seq > after)).collect();
        let skip = limit.map_or(0, |limit| newer.len().saturating_sub(limit));
        newer.into_iter().skip(skip).cloned().collect()
    }

    /// Permission requests in `pane`'s thread, by id, to bring up to date.
    pub fn permission_ids(&self, pane: PaneId) -> Vec<u64> {
        let thread = self.panes.get(&pane).into_iter().flatten();
        thread
            .filter_map(|e| match e.item {
                ThreadItem::Permission { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    /// A permission request's state, as it is now.
    pub fn permission(&mut self, id: u64, now: ApprovalState) {
        for entry in self.panes.values_mut().flatten() {
            if let ThreadItem::Permission { id: this, state, .. } = &mut entry.item
                && *this == id
            {
                *state = now;
            }
        }
    }

    pub fn pane_closed(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
    }
}

/// What a hook event adds to a thread, if anything. Permission prompts come
/// as permission requests instead (their own entries); an idle agent's
/// notification is no news.
fn item_of(event: &HookEvent, message: Option<&str>, ask: Option<u64>) -> Option<ThreadItem> {
    let text = |t: Option<&str>| t.unwrap_or_default().to_owned();
    match event.event.as_str() {
        "UserPromptSubmit" | "BeforeAgent" => {
            Some(ThreadItem::Prompt { text: text(event.prompt.as_deref()), ask })
        },
        "Stop" | "AfterAgent" => event
            .reply
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .map(|r| ThreadItem::Reply { text: r.to_owned() }),
        "SessionStart" => {
            let event = match event.source.as_deref() {
                Some("resume") => "resumed",
                Some("clear") => "cleared",
                Some("compact") => "compacted",
                _ => "started",
            };
            Some(ThreadItem::Session { event: event.to_owned() })
        },
        "SessionEnd" => Some(ThreadItem::Session { event: "ended".to_owned() }),
        "Notification" => match event.notification_type.as_deref() {
            None | Some("elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input") => {
                Some(ThreadItem::Waiting { message: text(message) })
            },
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str) -> HookEvent {
        HookEvent { event: name.into(), ..HookEvent::default() }
    }

    #[test]
    fn a_turn_in_order() {
        let mut threads = Threads::default();
        let prompt = HookEvent { prompt: Some("fix it".into()), ..event("UserPromptSubmit") };
        threads.hook(1, &prompt, None, Some(42));
        let permission = ThreadItem::Permission {
            id: 7,
            tool_name: "Bash".into(),
            summary: "cargo test".into(),
            state: ApprovalState::Pending,
        };
        threads.add(1, permission);
        // A permission prompt's notification adds nothing: the request is there.
        let prompted = HookEvent {
            notification_type: Some("permission_prompt".into()),
            ..event("Notification")
        };
        threads.hook(1, &prompted, Some("Claude needs your permission"), None);
        threads.permission(7, ApprovalState::Allowed);
        let stop = HookEvent { reply: Some("Fixed.".into()), ..event("Stop") };
        threads.hook(1, &stop, None, None);
        let items: Vec<ThreadItem> =
            threads.entries(1, None, None).into_iter().map(|e| e.item).collect();
        assert_eq!(
            items,
            [
                ThreadItem::Prompt { text: "fix it".into(), ask: Some(42) },
                ThreadItem::Permission {
                    id: 7,
                    tool_name: "Bash".into(),
                    summary: "cargo test".into(),
                    state: ApprovalState::Allowed,
                },
                ThreadItem::Reply { text: "Fixed.".into() },
            ]
        );
    }

    #[test]
    fn after_and_limit() {
        let mut threads = Threads::default();
        let seqs: Vec<u64> =
            (0..5).map(|i| threads.add(2, ThreadItem::Reply { text: i.to_string() })).collect();
        let texts = |entries: Vec<ThreadEntry>| -> Vec<String> {
            entries
                .into_iter()
                .map(|e| match e.item {
                    ThreadItem::Reply { text } => text,
                    _ => unreachable!(),
                })
                .collect()
        };
        assert_eq!(texts(threads.entries(2, Some(seqs[2]), None)), ["3", "4"]);
        assert_eq!(texts(threads.entries(2, None, Some(2))), ["3", "4"]);
        assert!(threads.entries(3, None, None).is_empty());
    }

    #[test]
    fn sessions() {
        let mut threads = Threads::default();
        let compact = HookEvent { source: Some("compact".into()), ..event("SessionStart") };
        threads.hook(1, &compact, None, None);
        threads.hook(1, &event("SessionEnd"), None, None);
        threads.hook(1, &event("Notification"), Some("Which branch?"), None);
        let items: Vec<ThreadItem> =
            threads.entries(1, None, None).into_iter().map(|e| e.item).collect();
        assert_eq!(
            items,
            [
                ThreadItem::Session { event: "compacted".into() },
                ThreadItem::Session { event: "ended".into() },
                ThreadItem::Waiting { message: "Which branch?".into() },
            ]
        );
    }
}
