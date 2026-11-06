//! The files browser's questions, for a machine without artty
//! ([`crate::plain`]): answered as the artty server answers them (folders
//! first, a fingerprint for what hasn't changed, files in parts, uploads
//! into place when whole), the machine's files reached through [`Files`]
//! (SFTP). Made off the stand-in's own thread: they wait on the network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use artty_core::protocol::{
    DirEntry, Existing, FileChunk, Listing, Query, Reply, UploadError, UploadTo, Uploaded,
};

/// The machine's files, as the files browser needs them: blocking calls,
/// paths as the machine writes them.
pub trait Files: Send + Sync {
    /// `path` resolved (relative ones, and `~/…`, from home; `None`: home).
    fn resolve(&self, path: Option<&str>) -> Result<String, String>;
    /// What's in folder `dir` (a link as what it leads to, and said to be one).
    fn list(&self, dir: &str) -> Result<Vec<DirEntry>, String>;
    /// At most `max` bytes of the file from `offset`, its length and modification time.
    fn read(&self, path: &str, offset: u64, max: usize) -> Result<FileChunk, String>;
    /// Its modification time (nanoseconds since the epoch), if it's there.
    fn modified(&self, path: &str) -> Option<u64>;
    fn is_dir(&self, path: &str) -> bool;
    /// `data` into the file at `offset` (made, or emptied, at 0).
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), String>;
    /// `from` in place of `to` (taking `to`'s mode, if it was there).
    fn replace(&self, from: &str, to: &str) -> Result<(), String>;
    fn remove(&self, path: &str);
}

/// The most listed of a folder, as the server.
const MAX_LISTED: usize = 5000;
/// A file's part, as the server.
const FILE_CHUNK: usize = 256 * 1024;

/// A file being received, part by part (by the app's upload id).
pub(crate) struct Receiving {
    part: String,
    path: String,
    written: u64,
    /// A file in place of one fetched: unchanged since then? (nanoseconds)
    expected: Option<u64>,
}

pub(crate) type Uploads = Arc<Mutex<HashMap<u64, Receiving>>>;

/// The answer to a files browser's question; `cwd` is the pane's directory
/// (as its shell reported it), if known.
pub(crate) fn answer(
    files: &dyn Files,
    query: Query,
    cwd: Option<String>,
    uploads: &Uploads,
) -> Reply {
    match query {
        Query::ListDir { path, known, .. } => Reply::Dir(list(files, path, cwd, known)),
        Query::ReadFile { path, offset, .. } => {
            let path = path.to_string_lossy().into_owned();
            Reply::File(files.read(&path, offset, FILE_CHUNK))
        },
        Query::Upload { upload, name, data, last, to, .. } => {
            Reply::Upload(receive(files, uploads, upload, &name, &data, last, to))
        },
        _ => unreachable!("only the files browser's questions come here"),
    }
}

fn list(
    files: &dyn Files,
    path: Option<PathBuf>,
    cwd: Option<String>,
    known: Option<u64>,
) -> Result<Listing, String> {
    // Asked for, or the pane's directory (gone, or never said: home).
    let path = match (path, cwd) {
        (Some(path), _) => files.resolve(Some(&path.to_string_lossy()))?,
        (None, Some(cwd)) => files.resolve(Some(&cwd)).or_else(|_| files.resolve(None))?,
        (None, None) => files.resolve(None)?,
    };
    let mut entries = files.list(&path)?;
    entries.truncate(MAX_LISTED);
    entries.sort_by(|a, b| {
        b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let fingerprint = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        for entry in &entries {
            (&entry.name, entry.dir, entry.link, entry.size, entry.modified).hash(&mut hasher);
        }
        hasher.finish()
    };
    let entries = (known != Some(fingerprint)).then_some(entries);
    Ok(Listing { path: PathBuf::from(path), fingerprint, entries, watched: false })
}

/// A part of an upload; the last puts the file in place.
fn receive(
    files: &dyn Files,
    uploads: &Uploads,
    upload: u64,
    name: &str,
    data: &[u8],
    last: bool,
    to: UploadTo,
) -> Result<Option<Uploaded>, UploadError> {
    let failed = UploadError::Failed;
    let fresh = !uploads.lock().unwrap().contains_key(&upload);
    if fresh {
        let (dir, name, expected, beside) = match to {
            UploadTo::Folder { dir, existing } => {
                let dir = dir.to_string_lossy().into_owned();
                if !files.is_dir(&dir) {
                    return Err(failed(format!("{dir} isn't a folder")));
                }
                let name = file_name(name)?;
                let taken = files.modified(&join(&dir, &name)).is_some();
                match existing {
                    Existing::Refuse if taken => return Err(UploadError::Exists),
                    Existing::Rename => (dir, name, None, true),
                    Existing::Refuse | Existing::Replace => (dir, name, None, false),
                }
            },
            UploadTo::File { path, modified } => {
                let path = path.to_string_lossy().into_owned();
                if modified.is_some() && modified != files.modified(&path) {
                    return Err(UploadError::Changed);
                }
                let (dir, name) = match path.rsplit_once('/') {
                    Some((dir, name)) => (
                        if dir.is_empty() { "/".to_owned() } else { dir.to_owned() },
                        name.to_owned(),
                    ),
                    None => (".".to_owned(), path.clone()),
                };
                (dir, name, modified, false)
            },
            UploadTo::Cache => {
                return Err(failed("no cache folder on a machine without artty".into()));
            },
        };
        let path = if beside { free_name(files, &dir, &name) } else { join(&dir, &name) };
        let part = join(&dir, &format!(".{name}.{upload}.part"));
        let receiving = Receiving { part, path, written: 0, expected };
        uploads.lock().unwrap().insert(upload, receiving);
    }
    let (part, offset) = {
        let uploads = uploads.lock().unwrap();
        let receiving = uploads.get(&upload).expect("just made");
        (receiving.part.clone(), receiving.written)
    };
    if let Err(err) = files.write(&part, offset, data) {
        uploads.lock().unwrap().remove(&upload);
        files.remove(&part);
        return Err(failed(err));
    }
    if !last {
        if let Some(receiving) = uploads.lock().unwrap().get_mut(&upload) {
            receiving.written += data.len() as u64;
        }
        return Ok(None);
    }
    let receiving = uploads.lock().unwrap().remove(&upload).expect("there");
    // Changed meanwhile, after all?
    if receiving.expected.is_some() && receiving.expected != files.modified(&receiving.path) {
        files.remove(&receiving.part);
        return Err(UploadError::Changed);
    }
    files.replace(&receiving.part, &receiving.path).map_err(|err| {
        files.remove(&receiving.part);
        failed(err)
    })?;
    let modified = files.modified(&receiving.path).unwrap_or_default();
    Ok(Some(Uploaded { path: PathBuf::from(&receiving.path), modified }))
}

/// Only its last part: a name, not a path.
fn file_name(name: &str) -> Result<String, UploadError> {
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| UploadError::Failed("no file name".into()))
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// `name`, or `stem (1).ext`, `stem (2).ext`… as the first not taken.
fn free_name(files: &dyn Files, dir: &str, name: &str) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    std::iter::once(name.to_owned())
        .chain((1..).map(|n| format!("{stem} ({n}){ext}")))
        .map(|candidate| join(dir, &candidate))
        .find(|path| files.modified(path).is_none())
        .expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files in memory: paths to contents (folders end in `/`).
    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, Vec<u8>>>);

    impl Files for Memory {
        fn resolve(&self, path: Option<&str>) -> Result<String, String> {
            let path = path.unwrap_or("/home/me").trim_end_matches('/').to_owned();
            if self.is_dir(&path) { Ok(path) } else { Err(format!("{path}: no such folder")) }
        }
        fn list(&self, dir: &str) -> Result<Vec<DirEntry>, String> {
            let files = self.0.lock().unwrap();
            let prefix = format!("{dir}/");
            Ok(files
                .iter()
                .filter_map(|(path, data)| {
                    let rest = path.strip_prefix(&prefix)?;
                    let (name, dir) = match rest.strip_suffix('/') {
                        Some(name) => (name, true),
                        None => (rest, false),
                    };
                    (!name.contains('/') && !name.is_empty()).then(|| DirEntry {
                        name: name.to_owned(),
                        dir,
                        link: false,
                        size: data.len() as u64,
                        modified: 0,
                    })
                })
                .collect())
        }
        fn read(&self, path: &str, offset: u64, max: usize) -> Result<FileChunk, String> {
            let files = self.0.lock().unwrap();
            let data = files.get(path).ok_or("no such file")?;
            let from = (offset as usize).min(data.len());
            let to = (from + max).min(data.len());
            Ok(FileChunk { data: data[from..to].to_vec(), length: data.len() as u64, modified: 1 })
        }
        fn modified(&self, path: &str) -> Option<u64> {
            self.0.lock().unwrap().contains_key(path).then_some(1)
        }
        fn is_dir(&self, path: &str) -> bool {
            self.0.lock().unwrap().contains_key(&format!("{}/", path.trim_end_matches('/')))
        }
        fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), String> {
            let mut files = self.0.lock().unwrap();
            let file = files.entry(path.to_owned()).or_default();
            file.truncate(offset as usize);
            file.extend_from_slice(data);
            Ok(())
        }
        fn replace(&self, from: &str, to: &str) -> Result<(), String> {
            let mut files = self.0.lock().unwrap();
            let data = files.remove(from).ok_or("no such file")?;
            files.insert(to.to_owned(), data);
            Ok(())
        }
        fn remove(&self, path: &str) {
            self.0.lock().unwrap().remove(path);
        }
    }

    #[test]
    fn listing_reading_and_uploading() {
        let files = Memory::default();
        for (path, data) in [
            ("/home/me/", ""),
            ("/home/me/b.txt", "bee"),
            ("/home/me/A/", ""),
            ("/home/me/a.txt", "a"),
        ] {
            files.0.lock().unwrap().insert(path.into(), data.as_bytes().to_vec());
        }
        let uploads = Uploads::default();
        let list = |path: Option<&str>, cwd: Option<&str>, known| match answer(
            &files,
            Query::ListDir { pane: 1, path: path.map(PathBuf::from), known, watch: true },
            cwd.map(str::to_owned),
            &uploads,
        ) {
            Reply::Dir(listing) => listing,
            _ => unreachable!(),
        };
        // Home, folders first; a pane's directory gone: home too.
        let home = list(None, None, None).unwrap();
        let names: Vec<_> =
            home.entries.as_ref().unwrap().iter().map(|e| e.name.as_str()).collect();
        assert_eq!((home.path.to_str(), names), (Some("/home/me"), vec!["A", "a.txt", "b.txt"]));
        assert_eq!(list(None, Some("/gone"), None).unwrap().path.to_str(), Some("/home/me"));
        // Unchanged: no entries sent again.
        assert_eq!(list(Some("/home/me"), None, Some(home.fingerprint)).unwrap().entries, None);
        assert!(list(Some("/nowhere"), None, None).is_err());

        let read = |offset| match answer(
            &files,
            Query::ReadFile { pane: 1, path: "/home/me/b.txt".into(), offset },
            None,
            &uploads,
        ) {
            Reply::File(chunk) => chunk.unwrap(),
            _ => unreachable!(),
        };
        assert_eq!((read(1).data, read(1).length), (b"ee".to_vec(), 3));

        let upload = |id, name: &str, data: &[u8], last, existing| match answer(
            &files,
            Query::Upload {
                pane: 1,
                upload: id,
                name: name.into(),
                data: data.to_vec(),
                last,
                to: UploadTo::Folder { dir: "/home/me".into(), existing },
            },
            None,
            &uploads,
        ) {
            Reply::Upload(result) => result,
            _ => unreachable!(),
        };
        // In parts, into place when whole.
        assert_eq!(upload(1, "c.txt", b"he", false, Existing::Refuse), Ok(None));
        assert_eq!(files.modified("/home/me/c.txt"), None);
        let done = upload(1, "c.txt", b"llo", true, Existing::Refuse).unwrap().unwrap();
        assert_eq!(done.path.to_str(), Some("/home/me/c.txt"));
        assert_eq!(files.0.lock().unwrap()["/home/me/c.txt"], b"hello");
        // Taken: refused, or kept beside.
        assert_eq!(upload(2, "c.txt", b"x", true, Existing::Refuse), Err(UploadError::Exists));
        let beside = upload(3, "c.txt", b"x", true, Existing::Rename).unwrap().unwrap();
        assert_eq!(beside.path.to_str(), Some("/home/me/c (1).txt"));
        assert!(files.0.lock().unwrap().keys().all(|k| !k.ends_with(".part")), "no part left");
    }
}
