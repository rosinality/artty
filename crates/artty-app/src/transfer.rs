//! Files between this Mac and another machine's panes: files dropped on a
//! pane there are sent through the connection (and their paths there typed),
//! and `file://` links to files there are fetched to open here.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use artty_core::layout::PaneId;
use artty_core::protocol::{Query, Reply, UploadError, UploadTo, Uploaded};
use gpui::AsyncApp;

use crate::app_state;

/// How much is sent at once: keys typed meanwhile wait behind at most this.
const CHUNK: usize = 128 * 1024;

/// The largest file fetched to open.
const MAX_FETCH: u64 = 256 << 20;

/// Send `paths` to the pane's machine, each into a folder of its own in
/// its cache; their paths there.
pub async fn send(
    pane: PaneId,
    paths: &[PathBuf],
    cx: &mut AsyncApp,
) -> Result<Vec<PathBuf>, String> {
    let mut sent = Vec::new();
    for path in paths {
        let uploaded = send_file(pane, path, UploadTo::Cache, cx)
            .await
            .map_err(|err| format!("{}: {err}", path.display()))?;
        sent.push(uploaded.path);
    }
    Ok(sent)
}

/// Send the file at `path` to the pane's machine, `to` there. A refusal
/// ([`UploadError::Exists`], [`UploadError::Changed`]) comes before any of
/// it is sent but the first part.
pub async fn send_file(
    pane: PaneId,
    path: &Path,
    to: UploadTo,
    cx: &mut AsyncApp,
) -> Result<Uploaded, UploadError> {
    let failed = |err: String| UploadError::Failed(err);
    let name = path
        .file_name()
        .ok_or_else(|| failed("no file name".into()))?
        .to_string_lossy()
        .into_owned();
    let mut file = std::fs::File::open(path).map_err(|err| failed(err.to_string()))?;
    if file.metadata().is_ok_and(|m| m.is_dir()) {
        return Err(failed("it's a folder: only files can be sent".into()));
    }
    let upload = cx.update(app_state::next_id).map_err(|_| failed("the app is closing".into()))?;
    loop {
        let mut data = vec![0; CHUNK];
        let read = read_full(&mut file, &mut data).map_err(|err| failed(err.to_string()))?;
        data.truncate(read);
        let last = read < CHUNK;
        let query = Query::Upload { pane, upload, name: name.clone(), data, last, to: to.clone() };
        let answer = cx
            .update(|cx| app_state::query(query, cx))
            .map_err(|_| failed("the app is closing".into()))?;
        match answer.await {
            Ok(Reply::Upload(Ok(Some(uploaded)))) => return Ok(uploaded),
            Ok(Reply::Upload(Ok(None))) if !last => {},
            Ok(Reply::Upload(Err(err))) => return Err(err),
            _ => return Err(failed("the connection was lost".into())),
        }
    }
}

/// Fill `buffer` unless the file ends first; how much was read.
fn read_full(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Fetch the file at `path` on the pane's machine (`server` names it) into
/// a folder here; where it is.
pub async fn fetch(
    pane: PaneId,
    server: &str,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<PathBuf, String> {
    let name = path.file_name().ok_or("not a file")?;
    let server: String = server
        .chars()
        .map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect();
    let dir = std::env::temp_dir().join("artty-remote-files").join(server);
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(fetch_to(pane, path, &dir.join(name), cx).await?.0)
}

/// Fetch the file at `path` on the pane's machine into ~/Downloads (as
/// `name (1)`... beside a file of its name); where it is.
pub async fn download(pane: PaneId, path: &Path, cx: &mut AsyncApp) -> Result<PathBuf, String> {
    let name = path.file_name().ok_or("not a file")?;
    let home = std::env::var_os("HOME").ok_or("no home")?;
    let downloads = PathBuf::from(home).join("Downloads");
    let mut here = downloads.join(name);
    let stem = Path::new(name).file_stem().unwrap_or(name).to_string_lossy().into_owned();
    let extension = Path::new(name).extension().map(|e| format!(".{}", e.to_string_lossy()));
    let mut n = 1;
    while here.exists() {
        here = downloads.join(format!("{stem} ({n}){}", extension.as_deref().unwrap_or("")));
        n += 1;
    }
    Ok(fetch_to(pane, path, &here, cx).await?.0)
}

/// Fetch the file at `path` on the pane's machine to `here`; with its
/// modification time there.
pub async fn fetch_to(
    pane: PaneId,
    path: &Path,
    here: &Path,
    cx: &mut AsyncApp,
) -> Result<(PathBuf, u64), String> {
    let here = here.to_owned();
    let mut file = std::fs::File::create(&here).map_err(|err| err.to_string())?;
    let mut offset = 0;
    let mut modified = 0;
    let result = loop {
        let query = Query::ReadFile { pane, path: path.to_owned(), offset };
        let answer =
            cx.update(|cx| app_state::query(query, cx)).map_err(|_| "the app is closing")?;
        match answer.await {
            Ok(Reply::File(Ok(chunk))) if chunk.length > MAX_FETCH => {
                break Err(format!("it's {} MB: too large to fetch", chunk.length >> 20));
            },
            Ok(Reply::File(Ok(chunk))) => {
                if let Err(err) = file.write_all(&chunk.data) {
                    break Err(err.to_string());
                }
                modified = chunk.modified;
                offset += chunk.data.len() as u64;
                if chunk.data.is_empty() || offset >= chunk.length {
                    break Ok(());
                }
            },
            Ok(Reply::File(Err(err))) => break Err(err),
            _ => break Err("the connection was lost".into()),
        }
    };
    drop(file);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&here);
        return Err(err);
    }
    quarantine(&here);
    Ok((here, modified))
}

/// Mark a fetched file as downloaded, as browsers do: opening a program in
/// it asks first.
fn quarantine(path: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = std::process::Command::new("xattr")
        .args(["-w", "com.apple.quarantine", &format!("0081;{now:x};artty;")])
        .arg(path)
        .status();
}
