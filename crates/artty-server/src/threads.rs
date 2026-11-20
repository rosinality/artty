//! Each agent's thread: what it was given and what it said, its permission
//! requests and how they went, its sessions starting and ending — in order,
//! from its hooks' events and artty's own records (asks, permission
//! requests). Kept per pane, and on disk (see `store`) under the thread's
//! own key, which a pane reopened after a restart takes back.

use std::collections::{HashMap, VecDeque};

use artty_core::control::{ApprovalState, HookEvent, ThreadEntry, ThreadItem};
use artty_core::layout::PaneId;

use crate::store::Store;

/// Entries kept per pane, at most (the oldest go first).
const MOST: usize = 1000;

pub struct Threads {
    panes: HashMap<PaneId, VecDeque<ThreadEntry>>,
    /// Each pane's thread's key, on disk.
    keys: HashMap<PaneId, u64>,
    store: Store,
}

impl Default for Threads {
    fn default() -> Self {
        Threads::new(Store::none())
    }
}

impl Threads {
    pub fn new(store: Store) -> Self {
        crate::queue::seen_id(store.last_seq());
        Threads { panes: HashMap::new(), keys: HashMap::new(), store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// `pane`'s thread's key, given it if it has none yet.
    pub fn key(&mut self, pane: PaneId) -> u64 {
        *self.keys.entry(pane).or_insert_with(crate::queue::next_id)
    }

    /// `pane`'s thread's key, if it has one.
    pub fn key_of(&self, pane: PaneId) -> Option<u64> {
        self.keys.get(&pane).copied()
    }

    /// The pane whose thread is `key`'s.
    pub fn pane_of(&self, key: u64) -> Option<PaneId> {
        self.keys.iter().find(|&(_, &k)| k == key).map(|(&pane, _)| pane)
    }

    /// `pane` (reopened, or handed over) takes back the thread kept as
    /// `key`. Its permission requests still pending when it was kept are
    /// over: their requests went with that server.
    pub fn adopt(&mut self, pane: PaneId, key: u64) {
        if self.pane_of(key).is_some() {
            return;
        }
        let mut entries = self.store.entries(key, MOST);
        for entry in &mut entries {
            if let ThreadItem::Permission { state: state @ ApprovalState::Pending, .. } =
                &mut entry.item
            {
                *state = ApprovalState::Ended;
                self.store.put_entry(key, entry);
            }
        }
        self.keys.insert(pane, key);
        if !entries.is_empty() {
            self.panes.insert(pane, entries.into());
        }
    }

    /// Forget kept threads no pane took back.
    pub fn forget_others(&self) {
        let keep: Vec<u64> = self.keys.values().copied().collect();
        self.store.keep_threads(&keep);
    }

    /// Add `item` to `pane`'s thread; its `seq`.
    pub fn add(&mut self, pane: PaneId, item: ThreadItem) -> u64 {
        let seq = crate::queue::next_id();
        let entry = ThreadEntry { seq, at_ms: crate::queue::now_ms(), item };
        let key = self.key(pane);
        self.store.put_entry(key, &entry);
        let thread = self.panes.entry(pane).or_default();
        thread.push_back(entry);
        if thread.len() > MOST
            && let Some(old) = thread.pop_front()
        {
            self.store.trim(key, old.seq);
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

    /// Permission requests pending in any thread, by id, to bring up to
    /// date.
    pub fn pending_permissions(&self) -> Vec<u64> {
        self.panes
            .values()
            .flatten()
            .filter_map(|e| match e.item {
                ThreadItem::Permission { id, state: ApprovalState::Pending, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    /// A permission request's state, as it is now.
    pub fn permission(&mut self, id: u64, now: ApprovalState) {
        for (pane, thread) in &mut self.panes {
            for entry in thread {
                if let ThreadItem::Permission { id: this, state, .. } = &mut entry.item
                    && *this == id
                    && *state != now
                {
                    *state = now;
                    if let Some(&key) = self.keys.get(pane) {
                        self.store.put_entry(key, entry);
                    }
                }
            }
        }
    }

    /// The pane closed: its thread is over. (It stays on disk until the
    /// next start, which forgets those no saved tab names: tabs saved a
    /// moment before may yet reopen with theirs, as when the shells end
    /// just before a reboot ends the server.)
    pub fn pane_closed(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
        self.keys.remove(&pane);
    }

    /// When `pane`'s thread last had something.
    pub fn last_ms(&self, pane: PaneId) -> Option<u64> {
        self.panes.get(&pane)?.back().map(|e| e.at_ms)
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
