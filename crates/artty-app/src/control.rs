//! The control socket: lets `artty` commands running anywhere (including
//! agents inside a tab) list, read, drive and annotate tabs.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use artty_core::control::{Request, Response, socket_path};
use futures::channel::{mpsc, oneshot};

/// A request from a client and where to send the answer.
pub type Pending = (Request, oneshot::Sender<Response>);

/// Removes the socket file when the app exits.
pub struct ControlServer {
    path: PathBuf,
}

impl ControlServer {
    pub fn start(requests: mpsc::UnboundedSender<Pending>) -> std::io::Result<Self> {
        let path = socket_path(std::process::id());
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // Only this user may drive the terminal.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        std::thread::Builder::new().name("artty-control".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let requests = requests.clone();
                let _ = std::thread::Builder::new()
                    .name("artty-control-client".into())
                    .spawn(move || serve(stream, requests));
            }
        })?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve(stream: UnixStream, requests: mpsc::UnboundedSender<Pending>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = oneshot::channel();
                if requests.unbounded_send((request, reply)).is_err() {
                    return;
                }
                futures::executor::block_on(answer).unwrap_or_else(|_| Response::error("artty is closing"))
            }
            Err(err) => Response::error(format!("bad request: {err}")),
        };
        let Ok(mut json) = serde_json::to_string(&response) else { return };
        json.push('\n');
        if writer.write_all(json.as_bytes()).is_err() {
            return;
        }
    }
}

/// Put an `artty` command on the PATH of shells we start: a directory holding
/// only a link to this executable.
pub fn cli_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = std::env::temp_dir().join("artty-bin");
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join("artty");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(exe, &link).ok()?;
    Some(dir)
}
