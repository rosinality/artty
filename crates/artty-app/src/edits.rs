//! Another machine's files, edited here: opened from the files drawer, a
//! file is fetched into a folder of this Mac's and opened in its app; each
//! time it's saved there, it goes back, replacing the file on that machine
//! if that hasn't changed since (else asking first).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use artty_core::layout::PaneId;
use artty_core::protocol::{UploadError, UploadTo};
use gpui::{App, Global, PromptLevel};

use crate::app_state;
use crate::ids;

/// A file being edited here.
struct Edit {
    pane: PaneId,
    remote: PathBuf,
    server: String,
    /// The file there's modification time, as last fetched or saved.
    modified: u64,
    /// The copy here's, as last fetched or sent.
    sent: Option<SystemTime>,
    /// Being sent now.
    busy: bool,
}

#[derive(Default)]
pub struct Edits {
    /// By the copy here.
    edits: HashMap<PathBuf, Edit>,
}

impl Global for Edits {}

/// Where the copy of `remote` (on `server`) is edited here: a folder per
/// file, so files of a name from different folders don't meet.
fn local_copy(server: &str, remote: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (server, remote).hash(&mut hasher);
    let home = std::env::var_os("HOME")?;
    let server: String = server
        .chars()
        .map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect();
    Some(
        PathBuf::from(home)
            .join("Library/Caches/artty/edits")
            .join(server)
            .join(format!("{:016x}", hasher.finish()))
            .join(remote.file_name()?),
    )
}

fn modified_here(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Open `remote` (on the pane's machine) to edit here.
pub fn open(pane: PaneId, remote: PathBuf, cx: &mut App) {
    let server = app_state::server_name(ids::server_of(pane), cx);
    let Some(here) = local_copy(&server, &remote) else { return };
    // Being edited already: open it again as it is (it may hold changes not
    // yet sent).
    if cx.try_global::<Edits>().is_some_and(|e| e.edits.contains_key(&here)) && here.exists() {
        let _ = std::process::Command::new("open").arg(&here).spawn();
        return;
    }
    let file = remote.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    toast(format!("Fetching {file} from {server}…"), cx);
    cx.spawn(async move |cx| {
        if let Some(dir) = here.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let fetched = crate::transfer::fetch_to(pane, &remote, &here, cx).await;
        let _ = cx.update(|cx| match fetched {
            Ok((here, modified)) => {
                let edit = Edit {
                    pane,
                    remote,
                    server: server.clone(),
                    modified,
                    sent: modified_here(&here),
                    busy: false,
                };
                cx.default_global::<Edits>().edits.insert(here.clone(), edit);
                toast(format!("Editing {file}: saving it sends it back to {server}"), cx);
                let _ = std::process::Command::new("open").arg(&here).spawn();
            },
            Err(err) => toast(format!("Couldn't fetch {file}: {err}"), cx),
        });
    })
    .detach();
}

/// Each second: copies saved here since go back.
pub fn tick(cx: &mut App) {
    let Some(edits) = cx.try_global::<Edits>() else { return };
    let saved: Vec<PathBuf> = edits
        .edits
        .iter()
        .filter(|(_, edit)| !edit.busy)
        .filter(|(here, edit)| {
            let now = modified_here(here);
            now.is_some() && now != edit.sent
        })
        .map(|(here, _)| here.clone())
        .collect();
    // Copies gone here: forget them.
    let gone: Vec<PathBuf> = edits.edits.keys().filter(|here| !here.exists()).cloned().collect();
    let edits = cx.default_global::<Edits>();
    for here in gone {
        edits.edits.remove(&here);
    }
    for here in saved {
        send_back(here, true, cx);
    }
}

/// Send the copy at `here` back, in place of the file there; only if that's
/// unchanged since it was fetched or last saved (`checked`), else asking.
fn send_back(here: PathBuf, checked: bool, cx: &mut App) {
    let Some(edit) = cx.default_global::<Edits>().edits.get_mut(&here) else { return };
    edit.busy = true;
    // As it is now: saves during the send go next time.
    edit.sent = modified_here(&here);
    let (pane, remote, server) = (edit.pane, edit.remote.clone(), edit.server.clone());
    let to = UploadTo::File { path: remote.clone(), modified: checked.then_some(edit.modified) };
    let file = remote.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    cx.spawn(async move |cx| {
        let sent = crate::transfer::send_file(pane, &here, to, cx).await;
        let _ = cx.update(|cx| {
            let Some(edit) = cx.default_global::<Edits>().edits.get_mut(&here) else { return };
            edit.busy = false;
            match sent {
                Ok(uploaded) => {
                    edit.modified = uploaded.modified;
                    toast(format!("Saved {file} to {server}"), cx);
                },
                Err(UploadError::Changed) => ask_to_replace(here, file, server, cx),
                Err(err) => toast(format!("Couldn't save {file} to {server}: {err}"), cx),
            }
        });
    })
    .detach();
}

/// The file there changed since it was fetched: replace it anyway, or stop
/// sending this copy back.
fn ask_to_replace(here: PathBuf, file: String, server: String, cx: &mut App) {
    let Some(view) = front_view(cx) else { return };
    // Saves meanwhile wait for the answer.
    if let Some(edit) = cx.default_global::<Edits>().edits.get_mut(&here) {
        edit.busy = true;
    }
    let handle = view.read(cx).window_handle();
    let detail = format!(
        "It changed on {server} since it was opened here. Replace it with this Mac's copy, or \
         stop sending this copy back (it stays at {}).",
        here.display()
    );
    let answer = handle.update(cx, |_, window, cx| {
        window.prompt(
            PromptLevel::Warning,
            &format!("“{file}” changed on {server}"),
            Some(&detail),
            &["Replace", "Stop Sending"],
            cx,
        )
    });
    let Ok(answer) = answer else { return };
    cx.spawn(async move |cx| {
        let replace = answer.await == Ok(0);
        let _ = cx.update(|cx| {
            if replace {
                send_back(here, false, cx);
            } else {
                cx.default_global::<Edits>().edits.remove(&here);
                toast(format!("{file} is no longer sent to {server}"), cx);
            }
        });
    })
    .detach();
}

fn front_view(cx: &App) -> Option<gpui::Entity<crate::window_view::WindowView>> {
    let views = app_state::app_views(cx);
    let front = cx.active_window();
    views.iter().find(|v| Some(v.read(cx).window_handle()) == front).or(views.first()).cloned()
}

fn toast(message: String, cx: &mut App) {
    if let Some(view) = front_view(cx) {
        view.update(cx, |view, cx| view.toast(message, cx));
    }
}
