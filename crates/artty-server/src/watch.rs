//! Folders apps' files drawers show, watched: an app is told when the folder
//! it lists for a pane changes (or, following the pane's directory, when
//! the pane moves to another), instead of asking again and again. Only those
//! folders, not what's below them; each once however many look at it. Changes
//! come in bursts (a build writing files): they're told once the burst has
//! gone on for a moment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use artty_core::layout::PaneId;
use notify::{RecursiveMode, Watcher};

use crate::inbox::Inbox;
use crate::{ClientId, Input};

/// How long changes gather before they're told.
const GATHER: Duration = Duration::from_millis(200);

struct Watch {
    path: PathBuf,
    /// Lists the pane's directory: told when that's another.
    follows: bool,
    /// The pane's directory, as last seen.
    cwd: Option<PathBuf>,
}

pub(crate) struct Watches {
    /// `None` if watching isn't to be had here (the app looks again now and
    /// then instead).
    watcher: Option<notify::RecommendedWatcher>,
    watching: HashMap<(ClientId, PaneId), Watch>,
}

impl Watches {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        let (changes, changed) = mpsc::channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                for path in event.paths {
                    // A change in a folder names what's in it: both count.
                    if let Some(parent) = path.parent() {
                        let _ = changes.send(parent.to_owned());
                    }
                    let _ = changes.send(path);
                }
            }
        });
        let watcher = match watcher {
            Ok(watcher) => watcher,
            Err(err) => {
                log::warn!("no watching folders here: {err}");
                return Self { watcher: None, watching: HashMap::new() };
            },
        };
        let _ = std::thread::Builder::new().name("artty-watch".into()).spawn(move || {
            // The first change of a burst, then all within GATHER of it.
            while let Ok(first) = changed.recv() {
                let mut dirs = vec![first];
                let until = Instant::now() + GATHER;
                while let Ok(path) =
                    changed.recv_timeout(until.saturating_duration_since(Instant::now()))
                {
                    if !dirs.contains(&path) {
                        dirs.push(path);
                    }
                }
                if inbox.send(Input::DirsChanged(dirs)).is_err() {
                    break;
                }
            }
        });
        Self { watcher: Some(watcher), watching: HashMap::new() }
    }

    fn watched(&self, path: &Path) -> bool {
        self.watching.values().any(|w| w.path == path)
    }

    /// `client` lists `path` for `pane` (following its directory, `cwd`,
    /// if `follows`): watch it, instead of what it watched for the pane.
    /// Whether changes will be told.
    pub fn watch(
        &mut self,
        client: ClientId,
        pane: PaneId,
        path: &Path,
        follows: bool,
        cwd: Option<PathBuf>,
    ) -> bool {
        let before =
            self.watching.insert((client, pane), Watch { path: path.to_owned(), follows, cwd });
        if let Some(before) = before.filter(|b| b.path != path) {
            self.release(&before.path);
        }
        let Some(watcher) = self.watcher.as_mut() else { return false };
        // Already watched for another: no second watch.
        let others = self.watching.iter().any(|(&key, w)| key != (client, pane) && w.path == path);
        if others {
            return true;
        }
        match watcher.watch(path, RecursiveMode::NonRecursive) {
            Ok(()) => true,
            Err(err) => {
                log::info!("not watching {}: {err}", path.display());
                false
            },
        }
    }

    /// No one watches `path` any more: stop watching it.
    fn release(&mut self, path: &Path) {
        if !self.watched(path)
            && let Some(watcher) = self.watcher.as_mut()
        {
            let _ = watcher.unwatch(path);
        }
    }

    pub fn unwatch(&mut self, client: ClientId, pane: PaneId) {
        if let Some(watch) = self.watching.remove(&(client, pane)) {
            self.release(&watch.path);
        }
    }

    /// The app went: everything it watched.
    pub fn forget(&mut self, client: ClientId) {
        let panes: Vec<PaneId> =
            self.watching.keys().filter(|(c, _)| *c == client).map(|(_, p)| *p).collect();
        for pane in panes {
            self.unwatch(client, pane);
        }
    }

    /// Who to tell that `dirs` changed.
    pub fn changed(&self, dirs: &[PathBuf]) -> Vec<(ClientId, PaneId)> {
        self.watching
            .iter()
            .filter(|(_, watch)| dirs.contains(&watch.path))
            .map(|(&key, _)| key)
            .collect()
    }

    /// Who to tell that the pane they follow moved (`cwd`: a pane's
    /// directory now).
    pub fn moved(&mut self, cwd: impl Fn(PaneId) -> Option<PathBuf>) -> Vec<(ClientId, PaneId)> {
        let mut moved = Vec::new();
        for (&(client, pane), watch) in self.watching.iter_mut().filter(|(_, w)| w.follows) {
            let now = cwd(pane);
            if now.is_some() && now != watch.cwd {
                watch.cwd = now;
                moved.push((client, pane));
            }
        }
        moved
    }
}
