//! Files agents change, from their edit tools as their hooks report them
//! (Claude Code's `PostToolUse` for Edit, MultiEdit, Write, NotebookEdit,
//! with the tool's input): each turn's, for its thread, and who changed a
//! file lately, to tell when two agents change the same one.
//!
//! Only what an edit tool does is seen: files a shell command changes
//! aren't.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use artty_core::control::FileEdit;
use artty_core::layout::PaneId;
use serde_json::Value;

/// Two agents changing a file this far apart overlap.
pub const OVERLAP: Duration = Duration::from_secs(15 * 60);
/// The text of a file's edits kept per turn, at most.
const HUNK_TEXT: usize = 8 << 10;

#[derive(Default)]
pub struct Edits {
    /// Each pane's turn so far.
    turn: HashMap<PaneId, Vec<FileEdit>>,
    /// Who changed each file, and when (last time each).
    recent: HashMap<String, Vec<(PaneId, Instant)>>,
    /// Overlaps told, so each is told once a while.
    told: HashMap<(PaneId, PaneId, String), Instant>,
}

/// What an edit tool's use did to a file, if it's one.
pub fn edit_of(tool: &str, input: &Value) -> Option<FileEdit> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let lines = |s: &str| if s.is_empty() { 0 } else { s.lines().count() as u32 };
    let path = text(input, "file_path").or_else(|| text(input, "notebook_path"))?;
    let mut edit = FileEdit { path, ..FileEdit::default() };
    let mut hunk = |old: String, new: String| {
        edit.removed += lines(&old);
        edit.added += lines(&new);
        edit.hunks.push((old, new));
    };
    match tool {
        "Edit" => hunk(text(input, "old_string")?, text(input, "new_string")?),
        "MultiEdit" => {
            for one in input.get("edits")?.as_array()? {
                hunk(text(one, "old_string")?, text(one, "new_string")?);
            }
        },
        "Write" => {
            hunk(String::new(), text(input, "content")?);
            edit.written = true;
        },
        "NotebookEdit" => hunk(String::new(), text(input, "new_source").unwrap_or_default()),
        _ => return None,
    }
    Some(edit)
}

/// `edit` folded into `into` (the same file): counts added, text kept to a
/// budget.
fn merge(into: &mut FileEdit, edit: FileEdit) {
    into.added += edit.added;
    into.removed += edit.removed;
    into.written |= edit.written;
    let mut used: usize = into.hunks.iter().map(|(a, b)| a.len() + b.len()).sum();
    for (old, new) in edit.hunks {
        if used >= HUNK_TEXT {
            break;
        }
        let clip = |s: String, room: usize| {
            if s.len() <= room {
                return s;
            }
            let mut end = room;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n…", &s[..end])
        };
        // Half the room each; what one doesn't need, the other may use.
        let room = HUNK_TEXT - used;
        let old = clip(old, room - new.len().min(room / 2));
        let new = clip(new, room.saturating_sub(old.len()));
        used += old.len() + new.len();
        into.hunks.push((old, new));
    }
}

impl Edits {
    /// `pane`'s agent used `tool`: if it changed a file, kept for its turn.
    /// The other panes that changed that file lately, newly so (each told
    /// once a while), with the file.
    pub fn tool_used(
        &mut self,
        pane: PaneId,
        tool: &str,
        input: &Value,
        now: Instant,
    ) -> Vec<(PaneId, String)> {
        let Some(edit) = edit_of(tool, input) else { return Vec::new() };
        let path = edit.path.clone();
        let turn = self.turn.entry(pane).or_default();
        match turn.iter_mut().find(|e| e.path == path) {
            Some(known) => merge(known, edit),
            None => {
                let mut first = FileEdit { hunks: Vec::new(), ..edit.clone() };
                (first.added, first.removed) = (0, 0);
                merge(&mut first, edit);
                turn.push(first);
            },
        }
        let who = self.recent.entry(path.clone()).or_default();
        who.retain(|&(p, at)| p != pane && now.duration_since(at) < OVERLAP);
        let others: Vec<PaneId> = who.iter().map(|&(p, _)| p).collect();
        who.push((pane, now));
        self.told.retain(|_, at| now.duration_since(*at) < OVERLAP);
        others
            .into_iter()
            .filter(|&other| {
                let key = (pane.min(other), pane.max(other), path.clone());
                self.told.insert(key, now).is_none()
            })
            .map(|other| (other, path.clone()))
            .collect()
    }

    /// The files `pane`'s turn changed (and it starts afresh).
    pub fn take_turn(&mut self, pane: PaneId) -> Vec<FileEdit> {
        self.turn.remove(&pane).unwrap_or_default()
    }

    pub fn pane_closed(&mut self, pane: PaneId) {
        self.turn.remove(&pane);
        for who in self.recent.values_mut() {
            who.retain(|&(p, _)| p != pane);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn edits_of_a_turn_by_file() {
        let mut edits = Edits::default();
        let now = Instant::now();
        let edit = json!({"file_path": "/d/plan.md", "old_string": "a\nb", "new_string": "c"});
        assert!(edits.tool_used(1, "Edit", &edit, now).is_empty());
        let more = json!({"file_path": "/d/plan.md", "edits": [
            {"old_string": "x", "new_string": "y\nz"}]});
        edits.tool_used(1, "MultiEdit", &more, now);
        edits.tool_used(1, "Write", &json!({"file_path": "/d/new.md", "content": "1\n2\n3"}), now);
        edits.tool_used(1, "Bash", &json!({"command": "rm -rf /d"}), now);
        let turn = edits.take_turn(1);
        let counts: Vec<_> =
            turn.iter().map(|e| (e.path.as_str(), e.added, e.removed, e.written)).collect();
        assert_eq!(counts, [("/d/plan.md", 3, 3, false), ("/d/new.md", 3, 0, true)]);
        assert_eq!(turn[0].hunks.len(), 2);
        assert!(edits.take_turn(1).is_empty());
    }

    #[test]
    fn two_agents_on_one_file_told_once() {
        let mut edits = Edits::default();
        let now = Instant::now();
        let edit = json!({"file_path": "/d/plan.md", "old_string": "a", "new_string": "b"});
        assert!(edits.tool_used(1, "Edit", &edit, now).is_empty());
        assert!(edits.tool_used(1, "Edit", &edit, now).is_empty(), "the same agent again");
        assert_eq!(edits.tool_used(2, "Edit", &edit, now), [(1, "/d/plan.md".to_owned())]);
        assert!(edits.tool_used(1, "Edit", &edit, now).is_empty(), "told already");
        let later = now + OVERLAP + Duration::from_secs(1);
        assert!(edits.tool_used(3, "Edit", &edit, later).is_empty(), "long after");
    }

    #[test]
    fn long_edits_shortened() {
        let mut edits = Edits::default();
        let big = "한".repeat(5000);
        let edit = json!({"file_path": "/d/a", "old_string": big, "new_string": big});
        edits.tool_used(1, "Edit", &edit, Instant::now());
        let turn = edits.take_turn(1);
        let kept: usize = turn[0].hunks.iter().map(|(a, b)| a.len() + b.len()).sum();
        assert!(kept <= HUNK_TEXT + 8, "{kept}");
        assert_eq!(turn[0].removed, 1);
    }
}
