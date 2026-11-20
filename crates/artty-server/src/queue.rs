//! What needs the user, from every agent: permission requests (see
//! `approvals`), agents waiting otherwise, and replies not yet read. This
//! module keeps the replies; the server puts the queue together and sends
//! it to apps (the `queue` extension) whenever it changes.
//!
//! A reply is an agent's last message of a turn (its `Stop` hook), with the
//! prompt it answers: an `artty ask`'s or one typed in the terminal. It's
//! read once its pane is seen while someone is at an app, or dismissed.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use artty_core::layout::PaneId;

/// Replies kept, unread, at most (the oldest go first).
const MOST: usize = 200;

/// Ids for asks, permission requests and replies: one sequence, so an id
/// names one thing, from when the server started (in milliseconds), so none
/// names one an earlier server gave.
/// (Ids kept on disk from an earlier server are passed over too: see
/// [`seen_id`].)
pub fn next_id() -> u64 {
    last_id().fetch_add(1, Ordering::Relaxed) + 1
}

/// `id` was given before (by an earlier server): later ones go past it.
pub fn seen_id(id: u64) {
    last_id().fetch_max(id, Ordering::Relaxed);
}

fn last_id() -> &'static AtomicU64 {
    static LAST: OnceLock<AtomicU64> = OnceLock::new();
    LAST.get_or_init(|| AtomicU64::new(now_ms()))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub id: u64,
    pub pane: PaneId,
    pub agent: Option<String>,
    pub prompt: String,
    pub text: String,
    pub at_ms: u64,
}

#[derive(Default)]
pub struct Replies {
    /// The last id given (from the server's start, in milliseconds, like
    /// asks' and permission requests').
    next: u64,
    /// Unread, oldest first.
    unread: Vec<Reply>,
    /// Each pane's turn's prompt, until its reply.
    prompts: HashMap<PaneId, String>,
}

impl Replies {
    pub fn new() -> Self {
        Replies { next: now_ms(), ..Replies::default() }
    }

    /// A turn began with `prompt`.
    pub fn prompted(&mut self, pane: PaneId, prompt: Option<&str>) {
        self.prompts.insert(pane, prompt.unwrap_or_default().to_owned());
    }

    /// A turn ended with `text`; nothing kept for an empty one.
    pub fn replied(&mut self, pane: PaneId, agent: Option<String>, text: &str) {
        let prompt = self.prompts.remove(&pane).unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }
        self.next = crate::queue::next_id();
        let reply =
            Reply { id: self.next, pane, agent, prompt, text: text.to_owned(), at_ms: now_ms() };
        self.unread.push(reply);
        if self.unread.len() > MOST {
            self.unread.remove(0);
        }
    }

    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.unread.len();
        self.unread.retain(|r| r.id != id);
        self.unread.len() != before
    }

    /// The pane was seen (or closed): its replies are read.
    pub fn read_pane(&mut self, pane: PaneId) {
        self.unread.retain(|r| r.pane != pane);
    }

    pub fn pane_closed(&mut self, pane: PaneId) {
        self.read_pane(pane);
        self.prompts.remove(&pane);
    }

    pub fn unread_in(&self, pane: PaneId) -> usize {
        self.unread.iter().filter(|r| r.pane == pane).count()
    }

    /// Unread, newest first.
    pub fn unread(&self) -> impl Iterator<Item = &Reply> {
        self.unread.iter().rev()
    }
}

/// `text`'s start, at most `chars` characters.
pub fn start_of(text: &str, chars: usize) -> String {
    let mut short: String = text.chars().take(chars).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_with_their_prompts_until_read() {
        let mut replies = Replies::new();
        replies.prompted(1, Some("fix the test"));
        replies.replied(1, Some("claude".into()), "Fixed: an off-by-one.");
        replies.prompted(2, None);
        replies.replied(2, None, "  ");
        replies.replied(2, None, "Done.");
        let unread: Vec<_> = replies.unread().map(|r| (r.pane, r.prompt.clone())).collect();
        assert_eq!(unread, [(2, String::new()), (1, "fix the test".to_owned())]);
        replies.read_pane(1);
        assert!(replies.unread().all(|r| r.pane != 1));
        let id = replies.unread().next().unwrap().id;
        assert!(replies.dismiss(id));
        assert_eq!(replies.unread().count(), 0);
    }

    #[test]
    fn starts() {
        assert_eq!(start_of("abc", 5), "abc");
        assert_eq!(start_of("abcdef", 3), "abc…");
    }
}
