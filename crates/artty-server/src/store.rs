//! What's kept on disk for agents, so a restart (or a hand-over to a new
//! build) doesn't lose it: each thread's entries, and records the server
//! keeps whole (its asks). An SQLite file beside the saved sessions,
//! owner-only.
//!
//! A thread is stored under its own key, not its pane's id: a pane reopened
//! after a restart gets a new id, and takes its thread back by the key its
//! saved layout names (see `Threads::adopt`).
//!
//! Writes go straight through, on the main loop (WAL; a commit doesn't wait
//! for the disk). When the file can't be opened, nothing is kept: the server
//! carries on as if it had no disk.

use std::path::{Path, PathBuf};

use artty_core::SavedState;
use artty_core::control::ThreadEntry;
use rusqlite::{Connection, OptionalExtension, params};

/// Bumped when the tables change incompatibly (an older file is started
/// afresh).
const VERSION: i64 = 2;

pub struct Store {
    db: Option<Connection>,
}

/// Where the file goes: beside the saved sessions.
fn path() -> Option<PathBuf> {
    Some(SavedState::path()?.with_file_name("agents.db"))
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    }
    // Owner-only from the start: what agents were told may be private.
    let _ = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path);
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    // 1 → 2: entries undone (rewound) in their agent.
    if version == 1 {
        db.execute_batch("ALTER TABLE entries ADD COLUMN rewound INTEGER NOT NULL DEFAULT 0;")?;
        db.pragma_update(None, "user_version", 2)?;
        version = 2;
    }
    if version != VERSION {
        if version != 0 {
            log::warn!(
                "{}: version {version}, expected {VERSION}; starting afresh",
                path.display()
            );
        }
        db.execute_batch(
            "DROP TABLE IF EXISTS entries;
             DROP TABLE IF EXISTS records;",
        )?;
    }
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
             thread INTEGER NOT NULL,
             seq INTEGER NOT NULL,
             at_ms INTEGER NOT NULL,
             item TEXT NOT NULL,
             rewound INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (thread, seq)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS records (
             name TEXT PRIMARY KEY,
             json TEXT NOT NULL
         );",
    )?;
    db.pragma_update(None, "user_version", VERSION)?;
    Ok(db)
}

impl Store {
    /// The server's file; if it can't be opened, a store that keeps nothing.
    pub fn open() -> Store {
        let Some(path) = path() else { return Store::none() };
        match open(&path) {
            Ok(db) => Store { db: Some(db) },
            Err(err) => {
                log::error!("{}: {err}; agents' threads won't outlive this server", path.display());
                Store::none()
            },
        }
    }

    /// One that keeps nothing.
    pub fn none() -> Store {
        Store { db: None }
    }

    #[cfg(test)]
    pub fn at(path: &Path) -> Store {
        Store { db: Some(open(path).unwrap()) }
    }

    /// Run `f` on the file, if there is one; errors are logged.
    fn with<T>(&self, what: &str, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        let db = self.db.as_ref()?;
        f(db).inspect_err(|err| log::error!("agents' store, {what}: {err}")).ok()
    }

    /// Add `entry` to `thread`, or (the same `seq`) bring it up to date.
    pub fn put_entry(&self, thread: u64, entry: &ThreadEntry) {
        let Ok(item) = serde_json::to_string(&entry.item) else { return };
        self.with("saving an entry", |db| {
            db.prepare_cached(
                "INSERT OR REPLACE INTO entries (thread, seq, at_ms, item, rewound) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                thread as i64,
                entry.seq as i64,
                entry.at_ms as i64,
                item,
                entry.rewound
            ])
        });
    }

    /// Drop `thread`'s entries up to `seq`, inclusive.
    pub fn trim(&self, thread: u64, seq: u64) {
        self.with("trimming a thread", |db| {
            db.prepare_cached("DELETE FROM entries WHERE thread = ?1 AND seq <= ?2")?
                .execute(params![thread as i64, seq as i64])
        });
    }

    /// `thread`'s last `most` entries, oldest first (any that can't be read,
    /// left out).
    pub fn entries(&self, thread: u64, most: usize) -> Vec<ThreadEntry> {
        let entries = self.with("reading a thread", |db| {
            let mut query = db.prepare(
                "SELECT seq, at_ms, item, rewound FROM entries WHERE thread = ?1 \
                 ORDER BY seq DESC LIMIT ?2",
            )?;
            let rows = query.query_map(params![thread as i64, most as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        });
        let mut entries: Vec<ThreadEntry> = entries
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(seq, at_ms, item, rewound)| {
                let item = serde_json::from_str(&item)
                    .inspect_err(|err| log::warn!("thread {thread}, entry {seq}: {err}"))
                    .ok()?;
                Some(ThreadEntry { seq: seq as u64, at_ms: at_ms as u64, item, rewound })
            })
            .collect();
        entries.reverse();
        entries
    }

    /// Forget every thread but `keep`'s (those whose panes are gone).
    pub fn keep_threads(&self, keep: &[u64]) {
        self.with("forgetting old threads", |db| {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS kept (thread INTEGER PRIMARY KEY)")?;
            tx.execute("DELETE FROM kept", [])?;
            for thread in keep {
                tx.execute("INSERT OR IGNORE INTO kept VALUES (?1)", params![*thread as i64])?;
            }
            let gone = tx
                .execute("DELETE FROM entries WHERE thread NOT IN (SELECT thread FROM kept)", [])?;
            tx.commit()?;
            if gone > 0 {
                log::info!("forgot {gone} entries of threads whose panes are gone");
            }
            Ok(())
        });
    }

    /// The largest id stored (entries' `seq`s), so new ones go past it.
    pub fn last_seq(&self) -> u64 {
        self.with("reading", |db| {
            db.query_row("SELECT MAX(seq) FROM entries", [], |row| row.get::<_, Option<i64>>(0))
        })
        .flatten()
        .map_or(0, |seq| seq as u64)
    }

    pub fn record(&self, name: &str) -> Option<String> {
        self.with("reading a record", |db| {
            db.query_row("SELECT json FROM records WHERE name = ?1", params![name], |row| {
                row.get(0)
            })
            .optional()
        })
        .flatten()
    }

    pub fn set_record(&self, name: &str, json: &str) {
        self.with("saving a record", |db| {
            db.prepare_cached("INSERT OR REPLACE INTO records (name, json) VALUES (?1, ?2)")?
                .execute(params![name, json])
        });
    }
}

#[cfg(test)]
mod tests {
    use artty_core::control::ThreadItem;

    use super::*;

    fn entry(seq: u64, text: &str) -> ThreadEntry {
        ThreadEntry {
            seq,
            at_ms: seq * 10,
            item: ThreadItem::Reply { text: text.into() },
            rewound: false,
        }
    }

    #[test]
    fn threads_and_records_kept() {
        let dir = std::env::temp_dir().join(format!("artty-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("agents.db");
        {
            let store = Store::at(&path);
            for seq in 1..=4 {
                store.put_entry(7, &entry(seq, &seq.to_string()));
            }
            store.put_entry(7, &entry(2, "two"));
            store.put_entry(8, &entry(5, "other"));
            store.trim(7, 1);
            store.set_record("asks", "[1]");
        }
        let store = Store::at(&path);
        let texts = |entries: Vec<ThreadEntry>| -> Vec<(u64, String)> {
            entries
                .into_iter()
                .map(|e| match e.item {
                    ThreadItem::Reply { text } => (e.seq, text),
                    _ => unreachable!(),
                })
                .collect()
        };
        assert_eq!(texts(store.entries(7, 2)), [(3, "3".into()), (4, "4".into())]);
        assert_eq!(texts(store.entries(7, 10)).len(), 3);
        assert_eq!(store.last_seq(), 5);
        assert_eq!(store.record("asks").as_deref(), Some("[1]"));
        store.keep_threads(&[8]);
        assert!(store.entries(7, 10).is_empty());
        assert_eq!(store.entries(8, 10).len(), 1);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_file() {
        let store = Store::none();
        store.put_entry(1, &entry(1, "x"));
        assert!(store.entries(1, 10).is_empty());
        assert_eq!(store.record("asks"), None);
    }
}
