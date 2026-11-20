//! Each agent's thread: what it was given and what it said, its permission
//! requests and how they went, its sessions starting and ending — in order,
//! from its hooks' events and artty's own records (asks, permission
//! requests). Kept per pane, and on disk (see `store`) under the thread's
//! own key, which a pane reopened after a restart takes back.

use std::collections::{HashMap, VecDeque};

use artty_core::control::{ApprovalState, AskFrom, AskState, HookEvent, ThreadEntry, ThreadItem};
use artty_core::layout::PaneId;

use crate::store::Store;

/// Entries kept per pane, at most (the oldest go first).
const MOST: usize = 1000;

pub struct Threads {
    panes: HashMap<PaneId, VecDeque<ThreadEntry>>,
    /// Each pane's thread's key, on disk.
    keys: HashMap<PaneId, u64>,
    store: Store,
    /// The newest entry's `seq` (what waiters for events go by).
    newest: u64,
}

impl Default for Threads {
    fn default() -> Self {
        Threads::new(Store::none())
    }
}

impl Threads {
    pub fn new(store: Store) -> Self {
        crate::queue::seen_id(store.last_seq());
        Threads { panes: HashMap::new(), keys: HashMap::new(), store, newest: 0 }
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
        self.newest = self.newest.max(seq);
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
    /// `ask`: the ask its turn took, if a turn began, and who asked).
    pub fn hook(
        &mut self,
        pane: PaneId,
        event: &HookEvent,
        message: Option<&str>,
        ask: Option<(u64, Option<AskFrom>)>,
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

    /// Entries matching `text` (case ignored unless it has capitals), in
    /// every thread or `pane`'s, newest first, `most` at most: each with
    /// what it is and the text around the match.
    pub fn search(
        &self,
        text: &str,
        pane: Option<PaneId>,
        most: usize,
    ) -> Vec<(PaneId, &ThreadEntry, &'static str, String)> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(PaneId, &ThreadEntry, &'static str, String)> = self
            .panes
            .iter()
            .filter(|(p, _)| pane.is_none_or(|pane| **p == pane))
            .flat_map(|(&p, thread)| thread.iter().map(move |e| (p, e)))
            .filter_map(|(p, entry)| {
                let (kind, said) = searchable(&entry.item)?;
                let snippet = snippet(&said, text)?;
                Some((p, entry, kind, snippet))
            })
            .collect();
        found.sort_by_key(|(_, e, _, _)| std::cmp::Reverse(e.seq));
        found.truncate(most);
        found
    }

    /// The newest entry's `seq` (0: none yet).
    pub fn newest(&self) -> u64 {
        self.newest
    }

    /// Entries after `after` in every thread (or `pane`'s), oldest first.
    pub fn since(&self, after: u64, pane: Option<PaneId>) -> Vec<(PaneId, ThreadEntry)> {
        if self.newest <= after {
            return Vec::new();
        }
        let mut found: Vec<(PaneId, ThreadEntry)> = self
            .panes
            .iter()
            .filter(|(p, _)| pane.is_none_or(|pane| **p == pane))
            .flat_map(|(&p, thread)| {
                // (Newest last: only the end of each needs looking at.)
                let newer = thread.iter().rev().take_while(|e| e.seq > after);
                newer.map(move |e| (p, e.clone())).collect::<Vec<_>>()
            })
            .collect();
        found.sort_by_key(|(_, e)| e.seq);
        found
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

    /// A handoff's card as its ask now is (`forwarded`: the ask that brought
    /// its answer back, once there is one).
    pub fn handoff(
        &mut self,
        id: u64,
        now: AskState,
        answer: Option<String>,
        failure: Option<String>,
        brought: Option<u64>,
    ) {
        for (pane, thread) in &mut self.panes {
            for entry in thread {
                let ThreadItem::Handoff { ask, state, reply, error, forwarded, .. } =
                    &mut entry.item
                else {
                    continue;
                };
                if *ask != id {
                    continue;
                }
                let brought = brought.or(*forwarded);
                if (*state, &*reply, &*error, *forwarded) == (now, &answer, &failure, brought) {
                    continue;
                }
                (*state, *reply, *error, *forwarded) =
                    (now, answer.clone(), failure.clone(), brought);
                if let Some(&key) = self.keys.get(pane) {
                    self.store.put_entry(key, entry);
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

/// What an entry says that a search looks in, and what it is.
fn searchable(item: &ThreadItem) -> Option<(&'static str, String)> {
    Some(match item {
        ThreadItem::Prompt { text, .. } => ("asked", text.clone()),
        ThreadItem::Reply { text } => ("answered", text.clone()),
        ThreadItem::Handoff { text, reply, .. } => {
            ("handed off", format!("{text}\n{}", reply.as_deref().unwrap_or_default()))
        },
        ThreadItem::Permission { summary, .. } => ("permission", summary.clone()),
        ThreadItem::Waiting { message } => ("waiting", message.clone()),
        ThreadItem::Edited { files } => {
            ("edited", files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join("\n"))
        },
        ThreadItem::Overlap { file, .. } => ("overlap", file.clone()),
        ThreadItem::Interrupted { reason } => ("stopped", reason.clone()),
        ThreadItem::Session { .. } => return None,
    })
}

/// The line of `said` where `text` is (case ignored unless it has
/// capitals), spaces run together, shortened around it; none if it isn't.
pub fn snippet(said: &str, text: &str) -> Option<String> {
    const AROUND: usize = 60;
    let exact = text.chars().any(char::is_uppercase);
    let fold = |s: &str| if exact { s.to_owned() } else { s.to_lowercase() };
    let needle = fold(text);
    let line = said.lines().find(|line| fold(line).contains(&needle))?;
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let folded: Vec<char> = fold(&line).chars().collect();
    let chars: Vec<char> = line.chars().collect();
    // (Folding that changes lengths: from the start.)
    let at = (folded.len() == chars.len())
        .then(|| {
            let hay: String = folded.iter().collect();
            hay.find(&needle).map(|byte| hay[..byte].chars().count())
        })
        .flatten()
        .unwrap_or(0);
    let start = at.saturating_sub(AROUND);
    let end = (at + needle.chars().count() + AROUND).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    Some(out)
}

/// What a hook event adds to a thread, if anything. Permission prompts come
/// as permission requests instead (their own entries); an idle agent's
/// notification is no news.
fn item_of(
    event: &HookEvent,
    message: Option<&str>,
    ask: Option<(u64, Option<AskFrom>)>,
) -> Option<ThreadItem> {
    let text = |t: Option<&str>| t.unwrap_or_default().to_owned();
    match event.event.as_str() {
        "UserPromptSubmit" | "BeforeAgent" => {
            let (ask, from) = ask.unzip();
            Some(ThreadItem::Prompt {
                text: text(event.prompt.as_deref()),
                ask,
                from: from.flatten(),
            })
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
        threads.hook(1, &prompt, None, Some((42, None)));
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
                ThreadItem::Prompt { text: "fix it".into(), ask: Some(42), from: None },
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
    fn searched() {
        let mut threads = Threads::default();
        threads.add(
            1,
            ThreadItem::Prompt { text: "Fix the Login\nredirect".into(), ask: None, from: None },
        );
        threads.add(2, ThreadItem::Reply { text: "Done: the login page redirects home.".into() });
        threads.add(2, ThreadItem::Session { event: "login".into() });
        let found: Vec<(PaneId, &str, String)> = threads
            .search("login", None, 10)
            .into_iter()
            .map(|(p, _, kind, snippet)| (p, kind, snippet))
            .collect();
        assert_eq!(
            found,
            [
                (2, "answered", "Done: the login page redirects home.".to_owned()),
                (1, "asked", "Fix the Login".to_owned()),
            ]
        );
        assert_eq!(threads.search("Login", None, 10).len(), 1, "capitals: exact");
        assert_eq!(threads.search("login", Some(1), 10).len(), 1);
        assert!(threads.search("  ", None, 10).is_empty());
        let long = format!("{} needle {}", "a".repeat(100), "b".repeat(100));
        let shown = snippet(&long, "needle").unwrap();
        assert!(shown.starts_with('…') && shown.ends_with('…') && shown.contains("needle"));
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
