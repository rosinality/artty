//! The artty server: a background process that owns every terminal session,
//! so shells and agents keep running when windows close or the app quits.
//!
//! One thread runs [`Server`]; sessions, connections and a timer feed it
//! [`Input`] over a channel. The app attaches windows over the same socket the
//! `artty` command and agent hooks use (see `artty_core::protocol`).

mod approvals;
mod asks;
mod handover;
mod inbox;
mod server;
mod sessions;
mod status;
mod watch;

use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use artty_core::control::{Request, Response, server_socket_path};
use artty_core::protocol::{ClientMessage, Hello, ServerMessage, read_message, write_message};

use inbox::{Inbox, Next};
use server::Server;

pub type ClientId = u64;

/// Who sent a control request: its connection, and the process at the
/// other end (if the system says).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Peer {
    pub conn: ClientId,
    pub pid: Option<i32>,
}

/// Everything the server reacts to.
pub(crate) enum Input {
    /// A one-off request from the `artty` command; answer on the sender.
    Control(Peer, Box<Request>, Sender<Response>),
    /// A control connection waiting for its answer closed (the program
    /// that asked ended: an agent ending its permission hook).
    ControlClosed(ClientId),
    /// An app, with its process (if the system says) and the extension
    /// kinds it reads.
    Connected(ClientId, Option<i32>, Sender<ServerMessage>, Vec<String>),
    Client(ClientId, ClientMessage),
    Disconnected(ClientId),
    /// Folders watched for apps changed (see `watch`).
    DirsChanged(Vec<PathBuf>),
    /// Something a status line shows was found out (see `status`).
    Status(status::Job, Result<String, String>),
}

/// Run the server until its last session ends. Fails if another server is
/// already running.
pub fn run() -> io::Result<()> {
    artty_core::logging::init();
    let path = server_socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "an artty server is already running"));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Only this user may drive the terminal.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    serve_on(listener, path, None)
}

/// What `artty server --handover-check` prints: that this build can take
/// over, and which hand-over it reads. A server checks it before `exec`ing a
/// build, since after that there's no going back.
pub fn handover_check() -> String {
    format!("artty-handover {}", handover::VERSION)
}

/// Take over from a previous server build (which `exec`ed this one): its
/// sessions, its socket, and its process.
pub fn resume(header: &Path) -> io::Result<()> {
    artty_core::logging::init();
    let (header, snapshots) = handover::read(header)?;
    // SAFETY: the previous build left the listening socket open at this
    // descriptor for exactly this.
    let listener = unsafe { UnixListener::from_raw_fd(header.listener_fd) };
    // SAFETY: plain syscall; not to be inherited by what we start.
    unsafe { libc::fcntl(header.listener_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    serve_on(listener, server_socket_path(), Some((header, snapshots)))
}

/// The longest the main loop handles queued inputs before its per-batch
/// work (screens, among others).
const BATCH_TIME: Duration = Duration::from_millis(8);

type Handover =
    (handover::Header, std::collections::HashMap<u64, artty_core::vt::term::TermSnapshot>);

fn serve_on(listener: UnixListener, path: PathBuf, handover: Option<Handover>) -> io::Result<()> {
    let socket = RemoveOnDrop(path.clone());
    let listener_fd = listener.as_raw_fd();

    let inbox = Arc::new(Inbox::default());
    let accept = inbox.clone();
    std::thread::Builder::new().name("artty-accept".into()).spawn(move || {
        static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
        for stream in listener.incoming().flatten() {
            let tx = accept.clone();
            let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
            let _ = std::thread::Builder::new()
                .name("artty-connection".into())
                .spawn(move || serve(stream, id, tx));
        }
    })?;

    let mut server = Server::new(path, inbox.clone(), listener_fd);
    match handover {
        Some((header, snapshots)) => server.adopt(header, snapshots),
        None => server.start(),
    }
    loop {
        inbox.wait(server.next_deadline().max(Instant::now() + Duration::from_millis(1)));
        // Handle what's waiting before doing per-batch work, so a burst of
        // output costs one screen update; but only for so long, so screens
        // still go out while there's always more.
        let end = Instant::now() + BATCH_TIME;
        while let Some(next) = inbox.next() {
            match next {
                Next::Input(input) => survive("handling an input", || server.handle(input)),
                Next::Pane(pane, events) => {
                    survive("handling a pane's events", || server.handle_pane(pane, events))
                },
            }
            if Instant::now() >= end {
                break;
            }
        }
        survive("updating", || server.after_batch());
        // Asked to hand over: it doesn't come back if it works.
        if let Some(executable) = server.take_handover() {
            server.hand_over(&executable);
        }
        if server.should_exit() {
            break;
        }
    }
    inbox.close();
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

/// Run `work`, and if it panics, log it and carry on: the server holds every
/// session, so a bug in handling one thing mustn't end them all. (The panic
/// itself is printed to the log by the default hook.)
fn survive(what: &str, work: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
        log::error!("recovered from a panic while {what}");
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One connection: a JSON control request, or an attached app.
fn serve(stream: UnixStream, id: ClientId, tx: Arc<Inbox>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {},
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(Hello::Attach { extensions, .. }) = serde_json::from_str::<Hello>(&line) {
            return serve_app(reader, writer, id, extensions, tx);
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = mpsc::channel();
                let peer = Peer { conn: id, pid: peer_pid(reader.get_ref()) };
                if tx.send(Input::Control(peer, Box::new(request), reply)).is_err() {
                    return;
                }
                // Long-running requests (`artty run`, `ask`, `approve`)
                // wait here, watching for the asker going away.
                let mut told = false;
                loop {
                    match answer.recv_timeout(Duration::from_millis(250)) {
                        Ok(response) => break response,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            break Response::error("the server stopped");
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !told && hung_up(reader.get_ref()) {
                                told = true;
                                let _ = tx.send(Input::ControlClosed(id));
                            }
                        },
                    }
                }
            },
            Err(err) => Response::error(format!("bad request: {err}")),
        };
        let Ok(mut json) = serde_json::to_string(&response) else { return };
        json.push('\n');
        if writer.write_all(json.as_bytes()).is_err() {
            return;
        }
    }
}

/// The process at the other end of a Unix socket.
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` is valid for `len` bytes.
        let ok = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        } == 0;
        ok.then_some(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` is valid for `len` bytes.
        let ok = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        } == 0;
        ok.then_some(cred.pid)
    }
}

/// Whether the other end closed (a waiting request's asker ended). Sends
/// nothing more while waiting, so anything readable is the end.
fn hung_up(stream: &UnixStream) -> bool {
    let mut poll = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd; no waiting.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    if ready <= 0 {
        return false;
    }
    if poll.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
        return true;
    }
    let mut byte = 0u8;
    // SAFETY: peeks at most one byte into `byte`.
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    read == 0
}

/// An attached app: binary messages both ways until it disconnects.
fn serve_app(
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    id: ClientId,
    extensions: Vec<String>,
    tx: Arc<Inbox>,
) {
    let (outgoing, messages) = mpsc::channel::<ServerMessage>();
    let pid = peer_pid(reader.get_ref());
    if tx.send(Input::Connected(id, pid, outgoing, extensions)).is_err() {
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

/// One JSON control request to the running server.
fn control(request: &Request, timeout: Duration) -> io::Result<Response> {
    let mut stream = UnixStream::connect(server_socket_path())?;
    stream.set_read_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    Ok(serde_json::from_str(&answer)?)
}

fn uptime() -> Option<u64> {
    match control(&Request::ServerStatus, Duration::from_secs(2)) {
        Ok(Response::Server { info }) => Some(info.uptime_secs),
        _ => None,
    }
}

/// How the running server moved to a new build.
#[derive(Debug, PartialEq, Eq)]
pub enum Upgrade {
    /// Handed over: every program kept running.
    HandedOver,
    /// Too old to hand over, so restarted: tabs reopen, programs restart.
    Restarted,
}

/// Move the running server onto the build at `executable`, and wait until
/// it answers.
pub fn upgrade(executable: &Path) -> io::Result<Upgrade> {
    let before = uptime();
    let request = Request::HandOver { executable: executable.to_owned() };
    match control(&request, Duration::from_secs(5))? {
        Response::Ok => {
            // Same socket, same process: the new build is in once the
            // server's uptime starts over.
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                if let (Some(now), Some(before)) = (uptime(), before)
                    && now < before.max(1)
                {
                    return Ok(Upgrade::HandedOver);
                }
            }
            Err(io::Error::new(io::ErrorKind::TimedOut, "the new server build didn't answer"))
        },
        Response::Error { message } if message.starts_with("bad request") => {
            // A server from before hand-overs: restart it.
            control(&Request::RestartServer, Duration::from_secs(5))?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while running() {
                if Instant::now() > deadline {
                    return Err(io::Error::other("the old artty server didn't stop"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(Upgrade::Restarted)
        },
        Response::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected answer")),
    }
}

/// Whether a server is running.
/// `artty server --stdio`: carry an app's connection to this machine's
/// server over stdin and stdout (as through ssh), starting the server first
/// if it isn't running. Returns when either side closes; the server and its
/// sessions keep running.
pub fn stdio_relay(executable: &Path) -> io::Result<()> {
    use std::io::{Read, Write};
    if !running() {
        spawn(executable, &[])?;
    }
    let stream = UnixStream::connect(server_socket_path())?;
    let mut to_server = stream.try_clone()?;
    let mut from_server = stream;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_server);
        // The app went away: let the server see it.
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    // Stdout buffers by line; frames must go out as they come.
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match from_server.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        stdout.write_all(&buffer[..read])?;
        stdout.flush()?;
    }
}

pub fn running() -> bool {
    UnixStream::connect(server_socket_path()).is_ok()
}

/// A private copy of `executable` for the server to run from, so rebuilding
/// (which may rewrite the build output in place) can never touch a running
/// server, and with it every session. Older copies are removed.
pub(crate) fn server_copy(executable: &Path) -> io::Result<PathBuf> {
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
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Logs/artty")
    } else {
        home.join(".local/state/artty")
    };
    Some(dir.join("server.log"))
}
