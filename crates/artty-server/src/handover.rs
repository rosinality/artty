//! Handing every running session to a new server build without ending them.
//!
//! The server `exec`s the new build in place: the process stays the same, so
//! the shells stay its children, and the descriptors it clears close-on-exec
//! on survive (each pane's PTY, and the listening socket, whose queue holds
//! anyone connecting meanwhile). Before that, each pane's reader stops with
//! everything parsed (output arriving meanwhile waits in the PTY), and the
//! state goes to files the new build reads at start:
//!
//! - a JSON header, deliberately plain so builds keep reading each other's:
//!   the layout (the saved-session format, with each pane's id) and, per
//!   pane, its PTY descriptor, shell pid and size;
//! - a binary snapshot of each pane's terminal. If a build can't read one,
//!   only that screen is lost: the pane carries on blank and redraws.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use artty_core::pane::PaneMeta;
use artty_core::vt::term::TermSnapshot;
use artty_core::{SavedState, SessionHandover};
use serde::{Deserialize, Serialize};

/// Bumped when the header changes incompatibly.
pub(crate) const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct Header {
    pub version: u32,
    /// The listening socket, inherited.
    pub listener_fd: i32,
    pub next_pane: u64,
    /// Layout; panes name their hand-over by id.
    pub state: SavedState,
    pub panes: Vec<HandedPane>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct HandedPane {
    pub session: SessionHandover,
    pub meta: PaneMeta,
}

/// Where hand-over files go: beside the saved sessions, owner-only.
fn dir() -> io::Result<PathBuf> {
    let state = SavedState::path().ok_or_else(|| io::Error::other("no HOME"))?;
    let dir = state.with_file_name("handover");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    Ok(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn snapshot_path(dir: &Path, pane: u64) -> PathBuf {
    dir.join(format!("pane-{pane}.bin"))
}

/// Write the hand-over; returns the header's path, for the new build.
pub(crate) fn write(mut header: Header, snapshots: &[(u64, TermSnapshot)]) -> io::Result<PathBuf> {
    let dir = dir()?;
    for (pane, snapshot) in snapshots {
        let bytes = postcard::to_stdvec(snapshot).map_err(io::Error::other)?;
        write_private(&snapshot_path(&dir, *pane), &bytes)?;
    }
    header.version = VERSION;
    let path = dir.join("header.json");
    write_private(&path, &serde_json::to_vec(&header)?)?;
    Ok(path)
}

/// What a previous build left: the header, and each pane's terminal if it
/// can be read. The files are removed.
pub(crate) fn read(path: &Path) -> io::Result<(Header, HashMap<u64, TermSnapshot>)> {
    let header: Header = serde_json::from_slice(&std::fs::read(path)?)?;
    if header.version != VERSION {
        log::warn!("hand-over header version {} (expected {VERSION})", header.version);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut snapshots = HashMap::new();
    for pane in &header.panes {
        let file = snapshot_path(dir, pane.session.id);
        match std::fs::read(&file)
            .map_err(io::Error::other)
            .and_then(|b| postcard::from_bytes(&b).map_err(io::Error::other))
        {
            Ok(snapshot) => {
                snapshots.insert(pane.session.id, snapshot);
            },
            Err(err) => log::warn!(
                "pane {}: its screen didn't carry over ({err}); it will redraw",
                pane.session.id
            ),
        }
        let _ = std::fs::remove_file(file);
    }
    let _ = std::fs::remove_file(path);
    Ok((header, snapshots))
}
