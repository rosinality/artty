//! A plain ssh machine's files, through SFTP (its `sftp` subsystem: nearly
//! every sshd has it), for the files browser ([`crate::plain::Files`]). One
//! session on the connection, opened when first needed (again, if it fails).

use std::sync::Arc;

use artty_core::protocol::{DirEntry, FileChunk};
use russh::client::Handle;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::login::Handler;

pub(super) struct Sftp {
    handle: Arc<Handle<Handler>>,
    runtime: tokio::runtime::Handle,
    session: tokio::sync::Mutex<Option<Arc<SftpSession>>>,
}

impl Sftp {
    pub(super) fn new(handle: Arc<Handle<Handler>>, runtime: tokio::runtime::Handle) -> Sftp {
        Sftp { handle, runtime, session: Default::default() }
    }

    async fn session(&self) -> Result<Arc<SftpSession>, String> {
        let mut session = self.session.lock().await;
        if let Some(open) = &*session {
            return Ok(open.clone());
        }
        let failed = |err: russh::Error| format!("SFTP: {err}");
        let channel = self.handle.channel_open_session().await.map_err(failed)?;
        channel.request_subsystem(true, "sftp").await.map_err(failed)?;
        let sftp = SftpSession::new(channel.into_stream()).await.map_err(|err| {
            log::warn!("SFTP: {err}");
            format!("no SFTP there: {err}")
        })?;
        log::info!("SFTP: open");
        let sftp = Arc::new(sftp);
        *session = Some(sftp.clone());
        Ok(sftp)
    }

    /// `work` with the session, waited for (from a thread of the stand-in's).
    fn on<T>(
        &self,
        work: impl AsyncFnOnce(&SftpSession) -> Result<T, russh_sftp::client::error::Error>,
    ) -> Result<T, String> {
        self.runtime.block_on(async {
            let session = self.session().await?;
            let result = work(&session).await;
            // The session itself gone: opened again next time.
            if let Err(
                russh_sftp::client::error::Error::UnexpectedBehavior(_)
                | russh_sftp::client::error::Error::Timeout,
            ) = &result
            {
                *self.session.lock().await = None;
            }
            result.map_err(|err| err.to_string())
        })
    }
}

/// An SFTP time (seconds) as nanoseconds.
fn nanos(seconds: Option<u32>) -> u64 {
    u64::from(seconds.unwrap_or(0)) * 1_000_000_000
}

impl crate::plain::Files for Sftp {
    fn resolve(&self, path: Option<&str>) -> Result<String, String> {
        let path = match path {
            None | Some("~") | Some("") => ".".to_owned(),
            Some(p) => p.strip_prefix("~/").unwrap_or(p).to_owned(),
        };
        self.on(async |s| s.canonicalize(path.clone()).await)
            .map_err(|err| format!("{path}: {err}"))
    }

    fn list(&self, dir: &str) -> Result<Vec<DirEntry>, String> {
        let dir = dir.to_owned();
        self.on(async |s| {
            let mut entries = Vec::new();
            for entry in s.read_dir(dir.clone()).await? {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let mut meta = entry.metadata();
                let link = meta.is_symlink();
                // A link: what it leads to (a folder to go into, say).
                if link && let Ok(target) = s.metadata(entry.path()).await {
                    meta = target;
                }
                entries.push(DirEntry {
                    name,
                    dir: meta.is_dir(),
                    link,
                    size: meta.size.unwrap_or(0),
                    modified: u64::from(meta.mtime.unwrap_or(0)),
                });
            }
            Ok(entries)
        })
    }

    fn read(&self, path: &str, offset: u64, max: usize) -> Result<FileChunk, String> {
        let path = path.to_owned();
        self.on(async |s| {
            let meta = s.metadata(path.clone()).await?;
            let mut file = s.open(path.clone()).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut data = Vec::with_capacity(max.min(meta.size.unwrap_or(0) as usize));
            (&mut file).take(max as u64).read_to_end(&mut data).await?;
            Ok(FileChunk { data, length: meta.size.unwrap_or(0), modified: nanos(meta.mtime) })
        })
    }

    fn modified(&self, path: &str) -> Option<u64> {
        let path = path.to_owned();
        self.on(async |s| s.metadata(path.clone()).await).ok().map(|meta| nanos(meta.mtime))
    }

    fn is_dir(&self, path: &str) -> bool {
        let path = path.to_owned();
        self.on(async |s| s.metadata(path.clone()).await).is_ok_and(|meta| meta.is_dir())
    }

    fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), String> {
        let path = path.to_owned();
        self.on(async |s| {
            let flags = if offset == 0 {
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE
            } else {
                OpenFlags::WRITE
            };
            let mut file = s.open_with_flags(path.clone(), flags).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.write_all(data).await?;
            file.shutdown().await?;
            Ok(())
        })
    }

    fn replace(&self, from: &str, to: &str) -> Result<(), String> {
        let (from, to) = (from.to_owned(), to.to_owned());
        self.on(async |s| {
            // As the file it replaces was (a script stays runnable).
            if let Ok(old) = s.metadata(to.clone()).await {
                let mut mode = russh_sftp::protocol::FileAttributes::empty();
                mode.permissions = old.permissions;
                let _ = s.set_metadata(from.clone(), mode).await;
                // SFTP's rename won't replace.
                let _ = s.remove_file(to.clone()).await;
            }
            s.rename(from.clone(), to.clone()).await
        })
    }

    fn remove(&self, path: &str) {
        let path = path.to_owned();
        let _ = self.on(async |s| s.remove_file(path.clone()).await);
    }
}
