//! The artty server: a background process that owns every terminal session,
//! so shells and agents keep running when windows close or the app quits.
//!
//! One thread runs [`Server`]; sessions, connections and a timer feed it
//! [`Input`] over a channel. The app attaches windows over the same socket the
//! `artty` command and agent hooks use (see `artty_core::protocol`).

mod server;

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use artty_core::SessionId;
use artty_core::control::{Request, Response, server_socket_path};
use artty_core::protocol::{ClientMessage, Hello, ServerMessage, read_message, write_message};
use artty_core::vt::event::Event;

use server::Server;

pub type ClientId = u64;

/// Everything the server reacts to.
pub(crate) enum Input {
    Session(SessionId, Event),
    /// A one-off request from the `artty` command; answer on the sender.
    Control(Request, Sender<Response>),
    Connected(ClientId, Sender<ServerMessage>),
    Client(ClientId, ClientMessage),
    Disconnected(ClientId),
}

/// Run the server until its last session ends. Fails if another server is
/// already running.
pub fn run() -> io::Result<()> {
    let path = server_socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "an artty server is already running"));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Only this user may drive the terminal.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let socket = RemoveOnDrop(path.clone());

    let (tx, rx) = mpsc::channel::<Input>();
    let accept = tx.clone();
    std::thread::Builder::new().name("artty-accept".into()).spawn(move || {
        static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
        for stream in listener.incoming().flatten() {
            let tx = accept.clone();
            let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
            let _ = std::thread::Builder::new().name("artty-connection".into()).spawn(move || serve(stream, id, tx));
        }
    })?;

    let mut server = Server::new(path, tx);
    server.start();
    loop {
        let timeout = server.next_deadline().saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout.max(Duration::from_millis(1))) {
            Ok(input) => server.handle(input),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Handle everything already queued before doing per-batch work, so a
        // burst of output costs one screen update.
        while let Ok(input) = rx.try_recv() {
            server.handle(input);
        }
        server.after_batch();
        if server.should_exit() {
            break;
        }
    }
    // Stop taking connections first: an app reconnecting after a restart
    // must reach the new server, not this one.
    drop(socket);
    server.shutdown();
    drop(server);
    // Let apps hear why the server is going away.
    let deadline = Instant::now() + Duration::from_millis(500);
    while WRITERS.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Connections still writing to an app.
static WRITERS: AtomicUsize = AtomicUsize::new(0);

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One connection: a JSON control request, or an attached app.
fn serve(stream: UnixStream, id: ClientId, tx: Sender<Input>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(Hello::Attach { .. }) = serde_json::from_str::<Hello>(&line) {
            return serve_app(reader, writer, id, tx);
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = mpsc::channel();
                if tx.send(Input::Control(request, reply)).is_err() {
                    return;
                }
                // Long-running requests (`artty run`) wait here.
                answer.recv().unwrap_or_else(|_| Response::error("the server stopped"))
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

/// An attached app: binary messages both ways until it disconnects.
fn serve_app(mut reader: BufReader<UnixStream>, mut writer: UnixStream, id: ClientId, tx: Sender<Input>) {
    let (outgoing, messages) = mpsc::channel::<ServerMessage>();
    if tx.send(Input::Connected(id, outgoing)).is_err() {
        return;
    }
    WRITERS.fetch_add(1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("artty-client-writer".into()).spawn(move || {
        for message in messages {
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
        WRITERS.fetch_sub(1, Ordering::Relaxed);
    });
    if spawned.is_err() {
        WRITERS.fetch_sub(1, Ordering::Relaxed);
    }
    while let Ok(message) = read_message::<ClientMessage>(&mut reader) {
        if tx.send(Input::Client(id, message)).is_err() {
            break;
        }
    }
    let _ = tx.send(Input::Disconnected(id));
}

/// Whether a server is running.
pub fn running() -> bool {
    UnixStream::connect(server_socket_path()).is_ok()
}

/// A private copy of `executable` for the server to run from, so rebuilding
/// (which may rewrite the build output in place) can never touch a running
/// server, and with it every session. Older copies are removed.
fn server_copy(executable: &Path) -> io::Result<PathBuf> {
    let dir = server_bin_dir().ok_or_else(|| io::Error::other("no HOME"))?;
    std::fs::create_dir_all(&dir)?;
    let modified = std::fs::metadata(executable)?.modified()?;
    let stamp = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let copy = dir.join(format!("artty-server-{stamp}"));
    if !copy.exists() {
        let temp = dir.join(format!(".artty-server-{stamp}.{}", std::process::id()));
        std::fs::copy(executable, &temp)?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&temp, &copy)?;
    }
    // Keep only this copy (a running server keeps its file open anyway).
    for entry in std::fs::read_dir(&dir)?.flatten() {
        if entry.path() != copy {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(copy)
}

fn server_bin_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(state) => PathBuf::from(state),
        None if cfg!(target_os = "macos") => home.join("Library/Application Support"),
        None => home.join(".local/state"),
    };
    Some(base.join("artty").join("server-bin"))
}

/// Start the server in the background (detached from the caller's session,
/// so it survives the app quitting), then wait for its socket.
pub fn spawn(executable: &Path, env: &[(String, String)]) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let executable = server_copy(executable).unwrap_or_else(|err| {
        log::warn!("running the server from the build output: {err}");
        executable.to_owned()
    });
    let log = log_path().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let mut command = std::process::Command::new(&executable);
    command
        .arg("server")
        .envs(env.iter().cloned())
        .current_dir(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match log {
        Some(file) => command.stderr(file),
        None => command.stderr(std::process::Stdio::null()),
    };
    // SAFETY: setsid is async-signal-safe and only affects the child.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "the artty server didn't start"))
}

/// The server's log: `~/Library/Logs/artty/server.log` on macOS.
pub fn log_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dir = if cfg!(target_os = "macos") { home.join("Library/Logs/artty") } else { home.join(".local/state/artty") };
    Some(dir.join("server.log"))
}
