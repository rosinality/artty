//! App-wide state: the connections to artty servers, which own every
//! session (this Mac's, and other machines' over ssh), and what they have
//! told us about panes. Windows attach to the servers' windows and draw
//! from here. Ids from each server are tagged with its number (see `ids`).

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{Request, Response};
use artty_core::layout::PaneId;
use artty_core::protocol::{
    AppCommand, ClientMessage, Hello, PaneSpec, PaneState, Query, Reply, ServerExtension,
    ServerMessage, StatusUpdate, WindowId, decode_response, encode_request, read_message,
    write_message,
};
use artty_core::remote::{Login, Prompts};
use artty_core::vt::term::TermMode;
use artty_core::{Config, Frame, SavedWindow, ServerConfig};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, AppContext, AsyncApp, Bounds, Global, WeakEntity, WindowBounds, WindowOptions, point, px,
    size,
};

use crate::ids::{self, LOCAL, ServerId};
use crate::window_view::{self, WindowView};

/// A connection to a server: this Mac's (number 0), or a configured one.
pub struct Connection {
    pub name: String,
    /// How to reach it; `None` for this Mac's.
    remote: Option<ServerConfig>,
    /// Messages to it (a writer thread sends them).
    outgoing: Option<std::sync::mpsc::Sender<ClientMessage>>,
    /// Windows asked to attach, in order; it answers in order.
    attaching: VecDeque<WeakEntity<WindowView>>,
    pub status: Status,
    /// What carries the connection (ssh), ended when we let go.
    child: Option<std::process::Child>,
    /// Failed attempts in a row, for backing off.
    failures: u32,
    /// The current attempt, so an outdated one's result is dropped.
    attempt: u64,
    /// It said it's ending every session: don't reconnect by ourselves.
    stopped: bool,
    /// When it last sent anything, and the ping awaiting its answer.
    last_heard: Instant,
    ping: Option<(u64, Instant)>,
    /// How long its last ping took to come back.
    pub latency: Option<Duration>,
    /// Checking it's still there (after a wake or a network change): lost
    /// if nothing comes by then.
    probe_until: Option<Instant>,
    /// Bytes received and sent, counted by the reader and writer threads.
    traffic: Arc<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>,
    /// How the link has been, for Activity.
    pub network: Network,
    /// What ssh said in the latest attempt.
    pub output: Vec<String>,
}

/// A connection's last minute and history, for Activity.
#[derive(Clone, Debug, Default)]
pub struct Network {
    /// Round trips (ms), one per ping.
    pub round_trips: VecDeque<f32>,
    /// Bytes per second received and sent, one sample a second.
    pub rates: VecDeque<(f32, f32)>,
    /// When it connected this time.
    pub since: Option<Instant>,
    pub reconnects: u32,
    /// The last thing that went wrong, and when.
    pub last_error: Option<(String, Instant)>,
    /// The traffic counters as of the last sample.
    counted: (u64, u64),
}

/// Samples kept of round trips and rates.
const NETWORK_HISTORY: usize = 60;

fn keep<T>(history: &mut VecDeque<T>, value: T) {
    if history.len() == NETWORK_HISTORY {
        history.pop_front();
    }
    history.push_back(value);
}

/// A reader or writer that counts the bytes through it.
struct Counted<T> {
    inner: T,
    count: Arc<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>,
    sent: bool,
}

impl<T: std::io::Read> std::io::Read for Counted<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.count.0.fetch_add(read as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(read)
    }
}

impl<T: Write> Write for Counted<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        let counter = if self.sent { &self.count.1 } else { &self.count.0 };
        counter.fetch_add(written as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    /// Lost or couldn't connect; trying again at `retry_at`.
    Waiting {
        error: String,
        retry_at: Instant,
    },
    /// Not trying (it runs another build, ended its sessions, or was
    /// removed from the settings).
    Off(String),
    /// Reached by ssh, but artty there isn't this build (or isn't there):
    /// this app can install it ([`install_remote`]).
    NeedsInstall(String),
    Installing,
    /// Logging in failed, or was cancelled: trying again asks again.
    NeedsLogin(String),
    /// Logging in waits for someone (ssh said something, then went quiet):
    /// a sign-in in the browser (Tailscale SSH's check), a push to approve,
    /// a key to touch. What it said last, and the attempt's process.
    WaitsForYou {
        said: String,
        pid: i32,
    },
    /// Logging in in a tab (see [`login_in_tab`]): it connects once that's
    /// done.
    LoggingInTab,
}

impl Connection {
    fn new(name: String, remote: Option<ServerConfig>) -> Self {
        Self {
            name,
            remote,
            outgoing: None,
            attaching: VecDeque::new(),
            status: Status::Connecting,
            child: None,
            failures: 0,
            attempt: 0,
            stopped: false,
            last_heard: Instant::now(),
            output: Vec::new(),
            ping: None,
            latency: None,
            probe_until: None,
            traffic: Arc::default(),
            network: Network::default(),
        }
    }

    /// Let go of the connection and what carries it.
    fn close(&mut self) {
        self.outgoing = None;
        self.attaching.clear();
        self.ping = None;
        self.latency = None;
        self.probe_until = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            std::thread::spawn(move || child.wait());
        }
    }
}

/// A pane's screen as last sent by the server, and its terminal mode.
pub struct Screen {
    pub frame: Frame,
    pub mode: TermMode,
}

pub struct Shared {
    /// Servers, by number: this Mac's first.
    pub connections: Vec<Connection>,
    next_request: u64,
    replies: HashMap<u64, oneshot::Sender<Reply>>,
    controls: HashMap<u64, oneshot::Sender<Response>>,
    pub screens: HashMap<PaneId, Screen>,
    /// Images screens show, by id (decoded off the main thread).
    pub images: HashMap<u64, ShownImage>,
    pub panes: HashMap<PaneId, PaneState>,
    /// Windows' status lines (`[status]`), those not empty.
    pub statuses: HashMap<WindowId, artty_core::protocol::StatusLine>,
    /// What needs the user, each server's as it last sent it (pane ids
    /// tagged; see [`queue`]).
    queues: HashMap<ServerId, Vec<artty_core::protocol::QueueItem>>,
    /// The extension kinds each server sends and reads (its `Extensions`),
    /// and how many times it's said so (once a connection).
    kinds: HashMap<ServerId, (Vec<String>, u64)>,
    /// Visual bells: when each pane last rang.
    pub flashes: HashMap<PaneId, Instant>,
    /// Typing shown before its echo comes back, per pane (see `echo`).
    pub echoes: HashMap<PaneId, crate::echo::Echo>,
    /// Views by the server windows they show (a view shows one window of
    /// each server it's attached to).
    windows: HashMap<WindowId, WeakEntity<WindowView>>,
    settings: Option<gpui::WindowHandle<crate::settings::Settings>>,
    /// The running executable and when it was built, to notice rebuilds.
    executable: Option<(PathBuf, SystemTime)>,
    update_ready: bool,
    /// The server runs a different build; restarting it picks up this one.
    pub server_mismatch: Option<String>,
    /// This app's build, as of when it started.
    build: String,
    /// This Mac's server said it's ending every session: quit instead of
    /// reconnecting when it goes.
    stopped: bool,
    /// Between losing the server and reopening windows from a new one, when
    /// having no windows doesn't mean quitting.
    reconnecting: bool,
    /// When the connection was last lost, to stop reconnecting in a loop.
    lost_at: Option<Instant>,
    /// For noticing wakes and network changes: the last second's tick and
    /// the interface addresses then.
    last_tick: Option<Instant>,
    addresses: Option<Vec<String>>,
    /// The colors and cursor sent to servers, to send again when they change.
    appearance: Option<(artty_core::Theme, artty_core::CursorStyle)>,
}

impl Global for Shared {}

/// An image a screen shows: how big, in cells, and its pixels once decoded.
pub struct ShownImage {
    pub columns: f32,
    pub rows: f32,
    pub render: Option<Arc<gpui::RenderImage>>,
}

/// Keep an image the server sent, decoding it in the background.
fn receive_image(image: artty_core::protocol::ImageData, cx: &mut App) {
    let id = image.id;
    cx.global_mut::<Shared>()
        .images
        .insert(id, ShownImage { columns: image.columns, rows: image.rows, render: None });
    let decoding = cx.background_spawn(async move { decode_image(image) });
    cx.spawn(async move |cx| {
        let render = decoding.await;
        let _ = cx.update(|cx| {
            if let (Some(render), Some(shown)) =
                (render, cx.global_mut::<Shared>().images.get_mut(&id))
            {
                shown.render = Some(Arc::new(render));
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// Pixels GPUI can draw: BGRA, the first frame of an animation.
fn decode_image(image: artty_core::protocol::ImageData) -> Option<gpui::RenderImage> {
    use artty_core::vt::term::cell::ImageFormat;
    let mut rgba = match image.format {
        ImageFormat::Encoded => ::image::load_from_memory(&image.data)
            .inspect_err(|err| log::warn!("image {}: {err}", image.id))
            .ok()?
            .into_rgba8(),
        ImageFormat::Rgba => ::image::RgbaImage::from_raw(image.width, image.height, image.data)?,
    };
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Some(gpui::RenderImage::new(vec![::image::Frame::new(rgba)]))
}

/// Let go of images no screen shows anymore, and tell the server, which
/// sends them again if they come back.
fn forget_unshown_images(cx: &mut App) {
    let shared = cx.global_mut::<Shared>();
    let shown: std::collections::HashSet<u64> = shared
        .screens
        .values()
        .flat_map(|screen| {
            screen.frame.lines.iter().flat_map(|line| line.images.iter().map(|span| span.image))
        })
        .collect();
    let gone: Vec<u64> = shared.images.keys().copied().filter(|id| !shown.contains(id)).collect();
    if gone.is_empty() {
        return;
    }
    for id in &gone {
        shared.images.remove(id);
    }
    send(ClientMessage::ForgetImages { ids: gone }, cx);
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Connect to the server (starting it if needed), then open windows: the
/// server's detached ones, or one new window with `spec`.
pub fn init(spec: PaneSpec, cx: &mut App) {
    let mut connections = vec![Connection::new("This Mac".to_owned(), None)];
    connections.extend(
        Config::load().0.servers.into_iter().map(|s| Connection::new(s.name.clone(), Some(s))),
    );
    cx.set_global(Shared {
        connections,
        next_request: 1,
        replies: HashMap::new(),
        controls: HashMap::new(),
        screens: HashMap::new(),
        panes: HashMap::new(),
        statuses: HashMap::new(),
        queues: HashMap::new(),
        kinds: HashMap::new(),
        flashes: HashMap::new(),
        echoes: HashMap::new(),
        windows: HashMap::new(),
        settings: None,
        executable: std::env::current_exe().ok().and_then(|path| {
            let built = modified(&path)?;
            Some((path, built))
        }),
        update_ready: false,
        images: HashMap::new(),
        server_mismatch: None,
        build: std::env::current_exe()
            .map(|exe| artty_core::protocol::build_id(&exe))
            .unwrap_or_default(),
        stopped: false,
        reconnecting: false,
        lost_at: None,
        last_tick: None,
        addresses: None,
        appearance: None,
    });
    connect_and_open(spec, cx);

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = cx.update(|cx| {
                // Windows that closed since: detach them in the server.
                window_view::flush_detached(cx);
                sync_servers(cx);
                sync_appearance(cx);
                heartbeat(cx);
                check_for_update(cx);
                forget_unshown_images(cx);
                crate::keymap::refresh(cx);
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
    cx.on_window_closed(|cx| {
        // The Settings window alone doesn't keep the app open. Sessions keep
        // running in the server either way.
        let shared = cx.global::<Shared>();
        let settings = shared.settings.map(|handle| handle.window_id());
        if !shared.reconnecting
            && cx.windows().iter().all(|window| Some(window.window_id()) == settings)
        {
            cx.quit();
        }
    })
    .detach();
}

fn connect_and_open(spec: PaneSpec, cx: &mut App) {
    if let Err(err) = connect(cx) {
        log::error!("connecting to the artty server: {err}");
        crate::notify::post("artty", &format!("Couldn't start the artty server: {err}"));
        return cx.quit();
    }
    let detached = query(Query::DetachedWindows, cx);
    cx.spawn(async move |cx| {
        let windows = match detached.await {
            Ok(Reply::Windows(windows)) => windows,
            _ => Vec::new(),
        };
        let _ = cx.update(|cx| {
            cx.global_mut::<Shared>().reconnecting = false;
            if windows.is_empty() {
                open_window(None, None, spec, cx);
            }
            for window in windows {
                open_window(Some(window.window), window.bounds, PaneSpec::default(), cx);
            }
            // Other servers: connect, or (after this Mac's reconnected)
            // show them again.
            let count = cx.global::<Shared>().connections.len();
            for server in 1..count as ServerId {
                let connection = &cx.global::<Shared>().connections[server as usize];
                if connection.remote.is_none() {
                    continue;
                }
                if connection.outgoing.is_some() {
                    attach_remote(server, cx);
                } else if connection.status == Status::Connecting && connection.attempt == 0 {
                    connect_remote(server, cx);
                }
            }
        });
    })
    .detach();
}

/// Start the server if needed and attach to it.
fn connect(cx: &mut App) -> std::io::Result<()> {
    connect_once(cx, true, 25)
}

fn connect_once(cx: &mut App, may_restart: bool, retries: u32) -> std::io::Result<()> {
    if !artty_server::running() {
        let exe = std::env::current_exe()?;
        // Apps launched from Finder have no locale; shells need one.
        let mut env = Vec::new();
        if std::env::var_os("LANG").is_none()
            && let Some(lang) = crate::notify::default_lang()
        {
            env.push(("LANG".to_owned(), lang));
        }
        artty_server::spawn(&exe, &env)?;
    }
    let stream = UnixStream::connect(artty_core::control::server_socket_path())?;
    let mut hello = serde_json::to_string(&Hello::Attach {
        version: artty_core::protocol::protocol_version(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        extensions: ServerExtension::KINDS.iter().map(|&k| k.to_owned()).collect(),
    })?;
    hello.push('\n');
    let mut writer = stream.try_clone()?;
    writer.write_all(hello.as_bytes())?;

    // Every version greets first with its protocol version. A server from
    // another build can't talk to us: move it to ours (handing over, so
    // programs keep running, where it knows how).
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let welcome = read_message::<ServerMessage>(&mut reader);
    stream.set_read_timeout(None)?;
    // Cut off without a greeting: likely a server mid hand-over. Try again.
    if let Err(err) = &welcome
        && err.kind() != std::io::ErrorKind::InvalidData
    {
        drop((writer, reader, stream));
        if retries == 0 {
            return Err(std::io::Error::other(format!("the artty server didn't greet: {err}")));
        }
        std::thread::sleep(Duration::from_millis(200));
        return connect_once(cx, may_restart, retries - 1);
    }
    let compatible = matches!(welcome, Ok(ServerMessage::Welcome { version, .. }) if version == artty_core::protocol::protocol_version());
    if !compatible {
        drop((writer, reader, stream));
        if !may_restart {
            return Err(std::io::Error::other("the artty server speaks another protocol"));
        }
        if restart_other_server()? == artty_server::Upgrade::Restarted {
            crate::notify::post(
                "artty",
                "Restarted the artty server to update it: tabs reopen and agents resume, but programs that were running in them restart.",
            );
        }
        return connect_once(cx, false, retries);
    }
    if let Ok(welcome) = welcome {
        handle(LOCAL, welcome, cx);
    }

    start_io(LOCAL, reader, writer, cx)
}

/// Carry a connection: a writer thread sends what goes to `server`, a
/// reader thread receives what comes, and the main thread handles it.
fn start_io(
    server: ServerId,
    reader: impl std::io::Read + Send + 'static,
    writer: impl Write + Send + 'static,
    cx: &mut App,
) -> std::io::Result<()> {
    let traffic = cx
        .global::<Shared>()
        .connections
        .get(server as usize)
        .map(|c| c.traffic.clone())
        .unwrap_or_default();
    let mut reader = Counted { inner: reader, count: traffic.clone(), sent: false };
    let mut writer = Counted { inner: writer, count: traffic, sent: true };
    let (outgoing, to_send) = std::sync::mpsc::channel::<ClientMessage>();
    std::thread::Builder::new().name(format!("artty-server-{server}-writer")).spawn(move || {
        for message in to_send {
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
    })?;
    // `None` means the server went away.
    let (incoming, mut received) = mpsc::unbounded::<Option<ServerMessage>>();
    std::thread::Builder::new().name(format!("artty-server-{server}-reader")).spawn(move || {
        loop {
            match read_message::<ServerMessage>(&mut reader) {
                Ok(message) => {
                    if incoming.unbounded_send(Some(message)).is_err() {
                        return;
                    }
                },
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::InvalidData {
                        // A message this build can't read: the server is
                        // from another build, though it said otherwise.
                        log::error!("reading from server {server}: {err}");
                        if server == LOCAL {
                            UNREADABLE.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else if err.kind() != std::io::ErrorKind::UnexpectedEof {
                        log::warn!("reading from server {server}: {err}");
                    }
                    break;
                },
            }
        }
        let _ = incoming.unbounded_send(None);
    })?;
    if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) {
        connection.outgoing = Some(outgoing);
        connection.status = Status::Connected;
        connection.failures = 0;
        connection.stopped = false;
        if connection.network.since.is_some() {
            connection.network.reconnects += 1;
        }
        connection.network.since = Some(Instant::now());
    }
    // Panes there look as set here.
    let config = Config::load().0;
    send_to(
        server,
        ClientMessage::Appearance { theme: config.theme(), cursor: config.cursor_style },
        cx,
    );

    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(message) = received.next().await {
            // Apply everything already queued before redrawing.
            let mut batch = vec![message];
            while let Ok(message) = received.try_recv() {
                batch.push(message);
            }
            let lost = batch.iter().any(Option::is_none);
            let handled = cx.update(|cx| {
                if let Some(connection) =
                    cx.global_mut::<Shared>().connections.get_mut(server as usize)
                {
                    connection.last_heard = Instant::now();
                    connection.probe_until = None;
                }
                for mut message in batch.into_iter().flatten() {
                    ids::incoming(server, &mut message);
                    handle(server, message, cx);
                }
            });
            if handled.is_err() {
                break;
            }
            if lost {
                let _ = cx.update(|cx| match server {
                    LOCAL => server_lost(cx),
                    _ => remote_lost(server, cx),
                });
                break;
            }
        }
    })
    .detach();
    Ok(())
}

/// Ask the running server (from another build) to save its sessions and
/// exit, over the JSON control channel every version has, and wait for it.
fn restart_other_server() -> std::io::Result<artty_server::Upgrade> {
    artty_server::upgrade(&std::env::current_exe()?)
}

/// The server went away (it crashed, or restarted). Close our windows and
/// reconnect: a new server reopens what the old one saved. If it ended every
/// session (`artty server stop`), quit.
/// The server sent something this build couldn't read.
static UNREADABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn server_lost(cx: &mut App) {
    // Move a server we can't read to our own build first (handing over,
    // so programs keep running), then reconnect as usual.
    if UNREADABLE.swap(false, std::sync::atomic::Ordering::Relaxed) {
        match restart_other_server() {
            Ok(_) => cx.global_mut::<Shared>().lost_at = None,
            Err(err) => log::error!("moving the server to this build: {err}"),
        }
    }
    let shared = cx.global_mut::<Shared>();
    if shared.stopped {
        return cx.quit();
    }
    // Losing it again right away means reconnecting won't help (say, the
    // two can't understand each other): give up rather than loop.
    let now = Instant::now();
    if shared
        .lost_at
        .replace(now)
        .is_some_and(|at| now.duration_since(at) < Duration::from_secs(10))
    {
        crate::notify::post(
            "artty",
            "Lost the connection to the artty server again; quitting. Sessions may still be running.",
        );
        return cx.quit();
    }
    shared.connections[LOCAL as usize].close();
    shared.screens.retain(|&pane, _| ids::server_of(pane) != LOCAL);
    shared.panes.retain(|&pane, _| ids::server_of(pane) != LOCAL);
    let views = app_views(cx);
    if views.is_empty() && !cx.global::<Shared>().reconnecting {
        return cx.quit();
    }
    cx.global_mut::<Shared>().reconnecting = true;
    // Windows close and reopen from the new server. Other servers' windows
    // in them are let go (detached), to come back once they've reopened.
    for view in views {
        let others: Vec<WindowId> = view.read(cx).server_windows();
        for window in others {
            if ids::server_of(window) != LOCAL {
                send(ClientMessage::DetachWindow { window }, cx);
            }
            cx.global_mut::<Shared>().windows.remove(&window);
        }
        view.update(cx, |view, _| view.forget_servers());
        let handle = view.read(cx).window_handle();
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    cx.global_mut::<Shared>().windows.clear();
    connect_and_open(PaneSpec::default(), cx);
}

/// Every live window view, each once.
pub fn app_views(cx: &App) -> Vec<gpui::Entity<WindowView>> {
    let mut views: Vec<(WindowId, gpui::Entity<WindowView>)> = Vec::new();
    for (&window, view) in &cx.global::<Shared>().windows {
        if let Some(view) = view.upgrade() {
            match views.iter_mut().find(|(_, v)| v.entity_id() == view.entity_id()) {
                Some(entry) => entry.0 = entry.0.min(window),
                None => views.push((window, view)),
            }
        }
    }
    views.sort_by_key(|(window, _)| *window);
    views.into_iter().map(|(_, view)| view).collect()
}

fn handle(server: ServerId, message: ServerMessage, cx: &mut App) {
    match message {
        ServerMessage::Welcome { version, server_version } => {
            let shared = cx.global_mut::<Shared>();
            if server == LOCAL {
                shared.server_mismatch = (version != artty_core::protocol::protocol_version()
                    || server_version != shared.build)
                    .then_some(server_version);
            }
            cx.refresh_windows();
        },
        ServerMessage::Attached { window, bounds } => {
            let shared = cx.global_mut::<Shared>();
            let view =
                shared.connections.get_mut(server as usize).and_then(|c| c.attaching.pop_front());
            if let Some(view) = view {
                shared.windows.insert(window, view.clone());
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.attached(window, bounds, cx));
                }
            }
        },
        ServerMessage::Layout { window, layout } => {
            if let Some(view) = view_for(window, cx) {
                view.update(cx, |view, cx| view.set_layout(window, layout, cx));
            }
        },
        // Kinds this build doesn't know are skipped.
        ServerMessage::Extension { kind, data } => match ServerExtension::decode(&kind, &data) {
            Some(ServerExtension::Status(StatusUpdate { window, line })) => {
                let statuses = &mut cx.global_mut::<Shared>().statuses;
                if line.is_empty() {
                    statuses.remove(&window);
                } else {
                    statuses.insert(window, line);
                }
                if let Some(view) = view_for(window, cx) {
                    view.update(cx, |_, cx| cx.notify());
                }
            },
            Some(ServerExtension::Queue(update)) => {
                cx.global_mut::<Shared>().queues.insert(server, update.items);
                cx.refresh_windows();
            },
            None => {},
        },
        // What the server sends and reads: the app sends it only those.
        ServerMessage::Extensions { kinds } => {
            let known = &mut cx.global_mut::<Shared>().kinds;
            let connection = known.get(&server).map_or(0, |(_, n)| n + 1);
            known.insert(server, (kinds, connection));
            // What windows asked of the last connection is asked again.
            cx.refresh_windows();
        },
        ServerMessage::WindowClosed { window } => {
            cx.global_mut::<Shared>().statuses.remove(&window);
            if let Some(view) =
                cx.global_mut::<Shared>().windows.remove(&window).and_then(|w| w.upgrade())
            {
                // The view closes with the last server window it shows.
                let empty = view.update(cx, |view, cx| view.server_window_gone(window, cx));
                if empty {
                    let handle = view.read(cx).window_handle();
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                }
            }
        },
        ServerMessage::Frame { pane, update } => {
            let shared = cx.global_mut::<Shared>();
            let screen = shared
                .screens
                .entry(pane)
                .or_insert_with(|| Screen { frame: Frame::default(), mode: TermMode::empty() });
            screen.mode = TermMode::from_bits_truncate(update.mode);
            update.apply(&mut screen.frame);
            if let Some(echo) = shared.echoes.get_mut(&pane) {
                let round_trip = shared
                    .connections
                    .get(ids::server_of(pane) as usize)
                    .and_then(|c| c.latency)
                    .unwrap_or_default();
                echo.confirm(&screen.frame, round_trip, Instant::now());
            }
            refresh_pane(pane, cx);
        },
        ServerMessage::Panes { panes } => {
            let shared = cx.global_mut::<Shared>();
            for pane in panes {
                shared.panes.insert(pane.info.id, pane);
            }
            cx.refresh_windows();
        },
        ServerMessage::Bell { pane } => {
            let config = Config::load().0;
            if config.visual_bell {
                cx.global_mut::<Shared>().flashes.insert(pane, Instant::now());
                refresh_pane(pane, cx);
            }
            if config.bell_sound {
                let _ = std::process::Command::new("osascript").args(["-e", "beep"]).spawn();
            }
        },
        ServerMessage::Notify { title, body, .. } => {
            // Another machine's: say which.
            let title = match cx.global::<Shared>().connections.get(server as usize) {
                Some(connection) if server != LOCAL => format!("{} · {title}", connection.name),
                _ => title,
            };
            crate::notify::post(&title, &body)
        },
        ServerMessage::App(command) => match command {
            AppCommand::Activate { window } => activate(window, cx),
            AppCommand::OpenWindow { window } => {
                open_window(Some(window), None, PaneSpec::default(), cx)
            },
            AppCommand::OpenSettings => open_settings(cx),
            AppCommand::ShowActivity { window } => {
                if let Some(view) = view_for(window, cx) {
                    view.update(cx, |view, cx| view.show_activity(cx));
                }
                activate(window, cx);
            },
            AppCommand::Restart => restart(cx),
            // Web pages only: a program on another machine mustn't open
            // this Mac's files or apps.
            AppCommand::OpenUrl { url } => {
                if url.starts_with("https://") || url.starts_with("http://") {
                    let _ = std::process::Command::new("open").arg(&url).spawn();
                } else {
                    log::warn!("not opening {url} for server {server}: not a web page");
                }
            },
        },
        ServerMessage::DirChanged { pane } => {
            for view in app_views(cx) {
                view.update(cx, |view, cx| view.dir_changed(pane, cx));
            }
        },
        ServerMessage::Clipboard { text } => {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text))
        },
        ServerMessage::Reply { id, reply } => {
            if let Some(waiter) = cx.global_mut::<Shared>().replies.remove(&id) {
                let _ = waiter.send(reply);
            }
        },
        ServerMessage::Control { id, response } => {
            if let Some(waiter) = cx.global_mut::<Shared>().controls.remove(&id) {
                let _ = waiter.send(decode_response(&response));
            }
        },
        ServerMessage::Stopping { restart } => {
            let shared = cx.global_mut::<Shared>();
            if server == LOCAL {
                shared.stopped = !restart;
            } else if let Some(connection) = shared.connections.get_mut(server as usize) {
                connection.stopped = !restart;
            }
        },
        ServerMessage::Image(image) => receive_image(image, cx),
        // The screens before it are applied: ready for more.
        ServerMessage::Sync { seq } => send_to(server, ClientMessage::Ack { seq }, cx),
        ServerMessage::Pong { id } => {
            if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize)
                && let Some((sent_id, sent)) = connection.ping
                && sent_id == id
            {
                connection.ping = None;
                connection.latency = Some(sent.elapsed());
                keep(&mut connection.network.round_trips, sent.elapsed().as_secs_f32() * 1000.);
            }
        },
    }
}

fn view_for(window: WindowId, cx: &App) -> Option<gpui::Entity<WindowView>> {
    cx.global::<Shared>().windows.get(&window)?.upgrade()
}

/// Redraw the window showing `pane`.
fn refresh_pane(pane: PaneId, cx: &mut App) {
    let windows: Vec<_> =
        cx.global::<Shared>().windows.values().filter_map(WeakEntity::upgrade).collect();
    for view in windows {
        if view.read(cx).shows(pane) {
            view.update(cx, |_, cx| cx.notify());
        }
    }
}

// Talking to the server.

/// Send to the server the message's ids came from (this Mac's for one
/// naming none).
pub fn send(mut message: ClientMessage, cx: &App) {
    if let ClientMessage::ForgetImages { ids } = &message {
        // Images of several servers: each its own.
        let mut by_server: HashMap<ServerId, Vec<u64>> = HashMap::new();
        for &id in ids {
            by_server.entry(ids::server_of(id)).or_default().push(id);
        }
        if by_server.len() > 1 {
            for (_, ids) in by_server {
                send(ClientMessage::ForgetImages { ids }, cx);
            }
            return;
        }
    }
    let server = ids::outgoing(&mut message).unwrap_or(LOCAL);
    deliver(server, message, cx);
}

/// Send to `server` a message that names nothing of it (or names it).
/// Have `server` also send `pane`'s screens (its local id), or none's, if
/// it reads `peek` (an older build doesn't): the connection it went to (a
/// new one forgets it), if it went.
pub fn peek(server: ServerId, pane: Option<PaneId>, cx: &App) -> Option<u64> {
    use artty_core::protocol::{Peek, extension};
    let (kinds, connection) = cx.global::<Shared>().kinds.get(&server)?;
    if !kinds.iter().any(|k| k == extension::PEEK) {
        return None;
    }
    deliver(server, Peek { pane }.message(), cx);
    Some(*connection)
}

/// `server`'s connection, as `peek` counts them.
pub fn connection(server: ServerId, cx: &App) -> Option<u64> {
    cx.global::<Shared>().kinds.get(&server).map(|(_, n)| *n)
}

/// A pane's screen as last received, if it's been sent.
pub fn screen_of(pane: PaneId, cx: &App) -> Option<&Screen> {
    cx.global::<Shared>().screens.get(&pane)
}

pub fn send_to(server: ServerId, mut message: ClientMessage, cx: &App) {
    ids::outgoing(&mut message);
    deliver(server, message, cx);
}

fn deliver(server: ServerId, message: ClientMessage, cx: &App) {
    let outgoing =
        cx.global::<Shared>().connections.get(server as usize).and_then(|c| c.outgoing.as_ref());
    if let Some(outgoing) = outgoing {
        let _ = outgoing.send(message);
    }
}

/// How another machine's server is reached.
pub fn remote_config(server: ServerId, cx: &App) -> Option<ServerConfig> {
    cx.global::<Shared>().connections.get(server as usize)?.remote.clone()
}

/// What a server is called (this Mac's, for local panes).
pub fn server_name(server: ServerId, cx: &App) -> String {
    cx.global::<Shared>()
        .connections
        .get(server as usize)
        .map_or_else(|| "this Mac".to_owned(), |c| c.name.clone())
}

pub fn next_id(cx: &mut App) -> u64 {
    let shared = cx.global_mut::<Shared>();
    let id = shared.next_request;
    shared.next_request += 1;
    id
}

pub fn query(query: Query, cx: &mut App) -> oneshot::Receiver<Reply> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().replies.insert(id, waiter);
    send(ClientMessage::Query { id, query }, cx);
    answer
}

/// Search a pane; the answer comes back like a query's.
pub fn search(
    pane: PaneId,
    query: String,
    older: bool,
    restart: bool,
    cx: &mut App,
) -> oneshot::Receiver<Reply> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().replies.insert(id, waiter);
    send(ClientMessage::Search { id, pane, query, older, restart }, cx);
    answer
}

/// A query to `server` (one naming no pane, which picks its own).
pub fn query_on(server: ServerId, query: Query, cx: &mut App) -> oneshot::Receiver<Reply> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().replies.insert(id, waiter);
    send_to(server, ClientMessage::Query { id, query }, cx);
    answer
}

/// A control request to this Mac's server.
pub fn control(request: Request, cx: &mut App) -> oneshot::Receiver<Response> {
    control_on(LOCAL, request, cx)
}

pub fn control_on(server: ServerId, request: Request, cx: &mut App) -> oneshot::Receiver<Response> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().controls.insert(id, waiter);
    send_to(server, ClientMessage::Control { id, request: encode_request(&request) }, cx);
    answer
}

/// What needs the user, from every server connected now: permission
/// requests (oldest first), agents waiting, then replies (newest first).
pub fn queue(cx: &App) -> Vec<artty_core::protocol::QueueItem> {
    use artty_core::protocol::QueueKind;
    let shared = cx.global::<Shared>();
    let connected = |server: &ServerId| {
        shared.connections.get(*server as usize).is_some_and(|c| c.status == Status::Connected)
    };
    let mut items: Vec<_> = shared
        .queues
        .iter()
        .filter(|(server, _)| connected(server))
        .flat_map(|(_, items)| items.iter().cloned())
        .collect();
    let rank = |kind: QueueKind| match kind {
        QueueKind::Approval => 0,
        QueueKind::Waiting => 1,
        QueueKind::Reply => 2,
    };
    items.sort_by_key(|item| {
        let since = item.since_ms as i64;
        (rank(item.kind), if item.kind == QueueKind::Reply { -since } else { since })
    });
    items
}

/// Bring the view of the server's `window` forward.
pub fn activate(window: WindowId, cx: &mut App) {
    if let Some(view) = view_for(window, cx) {
        let handle = view.read(cx).window_handle();
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

/// The app's windows other than `except`, with what each shows, for "Move
/// to" menus.
pub fn other_windows(except: Option<WindowId>, cx: &App) -> Vec<(WindowId, String)> {
    // Workspaces move only between one server's windows.
    let server = except.map_or(LOCAL, ids::server_of);
    let mut windows: Vec<(WindowId, String)> = cx
        .global::<Shared>()
        .windows
        .iter()
        .filter(|(id, _)| Some(**id) != except && ids::server_of(**id) == server)
        .filter_map(|(&id, view)| Some((id, view.upgrade()?.read(cx).title(cx))))
        .collect();
    windows.sort_by_key(|(id, _)| *id);
    windows
}

/// Open a window attached to the server's `window` (or a new one).
pub fn open_window(
    window: Option<WindowId>,
    bounds: Option<SavedWindow>,
    spec: PaneSpec,
    cx: &mut App,
) {
    let (config, config_error) = Config::load();
    let open_windows = app_views(cx).len();
    let window_bounds = bounds.as_ref().map(window_view::window_bounds).unwrap_or_else(|| {
        // New windows cascade.
        let offset = px(24.) * open_windows as f32;
        let base = Bounds::centered(None, size(px(1100.), px(680.)), cx);
        WindowBounds::Windowed(Bounds::new(
            point(base.origin.x + offset, base.origin.y + offset),
            base.size,
        ))
    });
    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("artty".into()),
            appears_transparent: true,
            // Where macOS itself puts them in a 32 pt titlebar.
            traffic_light_position: Some(point(px(9.), px(9.))),
        }),
        window_background: window_view::background_appearance(&config),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        cx.new(|cx| WindowView::new(config, config_error, window, cx))
    });
    match opened {
        Ok(handle) => {
            if let Ok(entity) = handle.entity(cx) {
                let server = window.map_or(LOCAL, ids::server_of);
                attach(server, &entity, window, spec, bounds, false, cx);
            }
        },
        Err(err) => log::error!("opening a window: {err}"),
    }
}

/// Ask `server` to show its `window` (or a new one) in `view`; with
/// `share`, even while another app shows it.
fn attach(
    server: ServerId,
    view: &gpui::Entity<WindowView>,
    window: Option<WindowId>,
    spec: PaneSpec,
    bounds: Option<SavedWindow>,
    share: bool,
    cx: &mut App,
) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    if connection.outgoing.is_none() {
        return;
    }
    connection.attaching.push_back(view.downgrade());
    send_to(server, ClientMessage::AttachWindow { window, spec, bounds, share }, cx);
}

/// Show a new window of `server` in `view`: a workspace there, for the +
/// beside a server with nothing in this window yet.
pub fn attach_new(server: ServerId, view: &gpui::Entity<WindowView>, cx: &mut App) {
    attach(server, view, None, PaneSpec::default(), None, false, cx);
}

// Other machines' servers.

/// The servers, for the sidebar: number, name and status (not ones
/// removed from the settings).
pub fn servers(cx: &App) -> Vec<(ServerId, String, Status)> {
    cx.global::<Shared>()
        .connections
        .iter()
        .enumerate()
        .filter(|(i, c)| *i == 0 || c.remote.is_some())
        .map(|(i, c)| (i as ServerId, c.name.clone(), c.status.clone()))
        .collect()
}

/// Whether there's more than this Mac's server.
pub fn several_servers(cx: &App) -> bool {
    cx.global::<Shared>().connections.iter().skip(1).any(|c| c.remote.is_some())
}

/// Follow the settings' servers: connect new ones, reconnect changed ones,
/// let go of removed ones. Numbers stay put, so tagged ids stay valid.
fn sync_servers(cx: &mut App) {
    let configured = Config::load().0.servers;
    let count = cx.global::<Shared>().connections.len();
    for (index, config) in configured.iter().enumerate() {
        let server = (index + 1) as ServerId;
        if index + 1 >= count {
            cx.global_mut::<Shared>()
                .connections
                .push(Connection::new(config.name.clone(), Some(config.clone())));
            connect_remote(server, cx);
        } else if cx.global::<Shared>().connections[index + 1].remote.as_ref() != Some(config) {
            drop_remote(server, cx);
            let connection = &mut cx.global_mut::<Shared>().connections[index + 1];
            connection.name = config.name.clone();
            connection.remote = Some(config.clone());
            connection.failures = 0;
            connect_remote(server, cx);
        }
    }
    for index in configured.len() + 1..count {
        if cx.global::<Shared>().connections[index].remote.is_some() {
            drop_remote(index as ServerId, cx);
            let connection = &mut cx.global_mut::<Shared>().connections[index];
            connection.remote = None;
            connection.status = Status::Off("removed from the settings".to_owned());
        }
    }
}

/// Send servers the colors and cursor when the settings change them.
fn sync_appearance(cx: &mut App) {
    let config = Config::load().0;
    let current = (config.theme(), config.cursor_style);
    let shared = cx.global_mut::<Shared>();
    if shared.appearance.as_ref() == Some(&current) {
        return;
    }
    // (Connections send it as they start.)
    let first = shared.appearance.replace(current.clone()).is_none();
    if first {
        return;
    }
    let count = cx.global::<Shared>().connections.len();
    for server in 0..count as ServerId {
        let (theme, cursor) = current.clone();
        send_to(server, ClientMessage::Appearance { theme, cursor }, cx);
    }
}

/// Pings every few seconds; a remote silent for long is taken for lost
/// (ssh can take minutes to notice a dead network). After a wake or a
/// network change, remotes are checked at once: one waiting reconnects now,
/// one connected must answer within seconds.
fn heartbeat(cx: &mut App) {
    crate::forward::tick(cx);
    crate::edits::tick(cx);
    const PING_EVERY: Duration = Duration::from_secs(5);
    const SILENCE: Duration = Duration::from_secs(15);
    const PROBE: Duration = Duration::from_secs(4);
    let now = Instant::now();
    let changed = network_changed(cx);
    let next_ping = cx.global::<Shared>().next_request;
    let mut lost = Vec::new();
    let mut retry = Vec::new();
    let count = cx.global::<Shared>().connections.len();
    for server in 0..count {
        let connection = &mut cx.global_mut::<Shared>().connections[server];
        let remote = connection.remote.is_some();
        let counted = (
            connection.traffic.0.load(std::sync::atomic::Ordering::Relaxed),
            connection.traffic.1.load(std::sync::atomic::Ordering::Relaxed),
        );
        let before = std::mem::replace(&mut connection.network.counted, counted);
        if connection.outgoing.is_some() {
            let rate = ((counted.0 - before.0) as f32, (counted.1 - before.1) as f32);
            keep(&mut connection.network.rates, rate);
        }
        if connection.outgoing.is_none() {
            if changed && remote && matches!(connection.status, Status::Waiting { .. }) {
                retry.push(server as ServerId);
            }
            continue;
        }
        if changed && remote {
            connection.probe_until.get_or_insert(now + PROBE);
            connection.ping = None;
        }
        let silent = now.duration_since(connection.last_heard);
        if remote && (silent >= SILENCE || connection.probe_until.is_some_and(|until| now >= until))
        {
            lost.push(server as ServerId);
            continue;
        }
        let due = connection.ping.is_none_or(|(_, sent)| now.duration_since(sent) >= PING_EVERY);
        if due {
            connection.ping = Some((next_ping + server as u64, now));
            send_to(server as ServerId, ClientMessage::Ping { id: next_ping + server as u64 }, cx);
        }
    }
    cx.global_mut::<Shared>().next_request += count as u64;
    for server in lost {
        log::warn!("server {server}: no answer; reconnecting");
        // Its shared ssh connection is likely dead too, and new sessions
        // would wait on it: end it.
        if let Some(exit) = remote_config(server, cx).and_then(|r| r.shared("exit", None)) {
            std::thread::spawn(move || artty_core::remote::ssh_process(&exit).output());
        }
        // Ending what carries it ends the connection: `remote_lost` follows.
        if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize)
            && let Some(child) = connection.child.as_mut()
        {
            let _ = child.kill();
        }
    }
    for server in retry {
        connect_remote(server, cx);
    }
}

/// Whether the Mac woke or its network changed since the last call: a gap
/// in this once-a-second loop, or different interface addresses.
fn network_changed(cx: &mut App) -> bool {
    let now = Instant::now();
    let addresses = interface_addresses();
    let shared = cx.global_mut::<Shared>();
    let woke = shared.last_tick.is_some_and(|at| now.duration_since(at) > Duration::from_secs(10));
    let moved = shared.addresses.as_ref().is_some_and(|before| *before != addresses);
    shared.last_tick = Some(now);
    shared.addresses = Some(addresses);
    if woke || moved {
        log::info!("network: {}", if woke { "woke" } else { "addresses changed" });
    }
    woke || moved
}

/// The addresses of the network interfaces that are up (not loopback).
fn interface_addresses() -> Vec<String> {
    let mut list = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we free below; we only read it.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return list;
        }
        let mut entry = head;
        while !entry.is_null() {
            let ifa = &*entry;
            entry = ifa.ifa_next;
            let flags = ifa.ifa_flags as i32;
            if ifa.ifa_addr.is_null()
                || flags & libc::IFF_UP == 0
                || flags & libc::IFF_LOOPBACK != 0
            {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            let address = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).to_string()
                },
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr).to_string()
                },
                _ => continue,
            };
            list.push(format!("{name} {address}"));
        }
        libc::freeifaddrs(head);
    }
    list.sort();
    list
}

/// How `server` is reached: "ssh me@host (compressed)", or the command.
pub fn transport(server: ServerId, cx: &App) -> Option<String> {
    let config = cx.global::<Shared>().connections.get(server as usize)?.remote.as_ref()?;
    Some(match (&config.ssh, config.command.is_empty()) {
        (_, false) => config.command.join(" "),
        (Some(ssh), true) => format!("ssh {ssh} (compressed)"),
        (None, true) => return None,
    })
}

/// How `server`'s link has been, for Activity.
pub fn network(server: ServerId, cx: &App) -> Option<Network> {
    Some(cx.global::<Shared>().connections.get(server as usize)?.network.clone())
}

/// Typing `bytes` into `pane`: guess its echo, where that helps (another
/// machine's pane with a noticeable round trip).
pub fn typed(pane: PaneId, bytes: &[u8], cx: &mut App) {
    const NOTICEABLE: Duration = Duration::from_millis(20);
    let shared = cx.global_mut::<Shared>();
    let slow = ids::server_of(pane) != LOCAL
        && shared
            .connections
            .get(ids::server_of(pane) as usize)
            .and_then(|c| c.latency)
            .is_some_and(|latency| latency >= NOTICEABLE);
    let Some(screen) = shared.screens.get(&pane).filter(|_| slow) else {
        shared.echoes.remove(&pane);
        return;
    };
    shared.echoes.entry(pane).or_default().typed(&screen.frame, screen.mode, bytes, Instant::now());
}

/// Let go of guesses no echo came for. Returns whether any went.
pub fn expire_echoes(cx: &mut App) -> bool {
    let now = Instant::now();
    let shared = cx.global_mut::<Shared>();
    let latency = |pane: PaneId| {
        shared
            .connections
            .get(ids::server_of(pane) as usize)
            .and_then(|c| c.latency)
            .unwrap_or_default()
    };
    let trips: Vec<(PaneId, Duration)> = shared.echoes.keys().map(|&p| (p, latency(p))).collect();
    let mut changed = false;
    for (pane, trip) in trips {
        if let Some(echo) = shared.echoes.get_mut(&pane) {
            changed |= echo.expire(trip, now);
        }
    }
    changed
}

/// How long `server`'s last ping took, for the sidebar.
pub fn latency(server: ServerId, cx: &App) -> Option<Duration> {
    cx.global::<Shared>().connections.get(server as usize)?.latency
}

/// Connect to a configured server, in the background (ssh can take a while).
pub fn connect_remote(server: ServerId, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    let Some(config) = connection.remote.clone() else { return };
    connection.close();
    connection.status = Status::Connecting;
    connection.attempt += 1;
    connection.output.clear();
    let attempt = connection.attempt;
    cx.refresh_windows();
    let mut askpass = crate::askpass::askpass(&server_name(server, cx), cx);
    // What logging in says, kept; and when it waits for you, a sheet.
    let (watch, mut said) = futures::channel::mpsc::unbounded::<Login>();
    if let Some(askpass) = askpass.as_mut() {
        askpass.watch = Some(std::sync::Arc::new(move |login| {
            let _ = watch.unbounded_send(login);
        }));
    }
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(login) = said.next().await {
            let _ = cx.update(|cx| login_said(server, attempt, login, cx));
        }
    })
    .detach();
    let opening = cx.background_spawn(async move {
        let prompts = askpass.as_ref().map_or(Prompts::Never, Prompts::Helper);
        artty_core::remote::open(&config, prompts)
    });
    cx.spawn(async move |cx| {
        let opened = opening.await;
        let _ = cx.update(|cx| remote_opened(server, attempt, opened, cx));
    })
    .detach();
}

/// What the login of `server`'s `attempt` said: kept, and when it waits
/// for you, asked in the window in front.
fn login_said(server: ServerId, attempt: u64, login: Login, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    if connection.attempt != attempt {
        return;
    }
    match login {
        Login::Said(line) => {
            if connection.output.len() == 200 {
                connection.output.remove(0);
            }
            connection.output.push(line);
        },
        Login::Stalled { said, url, pid } => {
            if !matches!(connection.status, Status::Connecting | Status::WaitsForYou { .. }) {
                return;
            }
            log::info!("server {}: login waits: {}", connection.name, said.join(" / "));
            connection.status =
                Status::WaitsForYou { said: said.last().cloned().unwrap_or_default(), pid };
            let name = connection.name.clone();
            let sheet = crate::login_sheet::Sheet::Waits { server, name, said, url, pid };
            crate::login_sheet::show(sheet, cx);
            cx.refresh_windows();
        },
    }
}

/// Stop `server`'s login waiting for you (its process `pid`): not tried
/// again until asked.
pub fn cancel_login(server: ServerId, pid: i32, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    // Its outcome is no one's now.
    connection.attempt += 1;
    connection.status = Status::NeedsLogin("login cancelled".to_owned());
    // SAFETY: plain syscall; the attempt's own ssh.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    crate::login_sheet::dismiss(server, cx);
    cx.refresh_windows();
}

/// Log in to `server` in a tab of `view` (on this Mac), where ssh can ask
/// for anything a login needs; it becomes the shared connection, and once
/// it's up the app connects through it. The attempt waiting (`pid`) ends.
pub fn login_in_tab(
    server: ServerId,
    pid: Option<i32>,
    view: &gpui::Entity<WindowView>,
    cx: &mut App,
) {
    let Some(config) = remote_config(server, cx) else { return };
    let (Some(login), Some(check)) = (config.login_command(), config.shared("check", None)) else {
        return;
    };
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    connection.attempt += 1;
    let attempt = connection.attempt;
    connection.status = Status::LoggingInTab;
    if let Some(pid) = pid {
        // SAFETY: plain syscall; the attempt's own ssh.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    crate::login_sheet::dismiss(server, cx);
    let title = format!("Log in to {}", config.name);
    view.update(cx, |view, cx| view.open_command_tab(login, title, cx));
    cx.refresh_windows();
    // Up once ssh there is in: then connected through it.
    cx.spawn(async move |cx| {
        const EVERY: Duration = Duration::from_secs(2);
        const AT_MOST: u32 = 600 / 2;
        for _ in 0..AT_MOST {
            cx.background_executor().timer(EVERY).await;
            let still = cx
                .update(|cx| {
                    cx.global::<Shared>()
                        .connections
                        .get(server as usize)
                        .is_some_and(|c| c.attempt == attempt && c.status == Status::LoggingInTab)
                })
                .unwrap_or(false);
            if !still {
                return;
            }
            let check = check.clone();
            let up = cx
                .background_spawn(async move {
                    artty_core::remote::ssh_process(&check)
                        .output()
                        .is_ok_and(|output| output.status.success())
                })
                .await;
            if up {
                let _ = cx.update(|cx| connect_remote(server, cx));
                return;
            }
        }
        let _ = cx.update(|cx| {
            if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize)
                && connection.attempt == attempt
            {
                connection.status = Status::NeedsLogin("not logged in".to_owned());
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// Show what `server`'s login said: waiting for you (its process `pid`), or
/// failing.
pub fn show_login_sheet(server: ServerId, waiting: Option<i32>, cx: &mut App) {
    let name = server_name(server, cx);
    let (output, error) = last_attempt(server, cx);
    let said = crate::login_sheet::worth_showing(&output);
    let sheet = match waiting {
        Some(pid) => {
            let url = said
                .iter()
                .rev()
                .find_map(|line| artty_core::remote::sign_in_url(line))
                .map(str::to_owned);
            crate::login_sheet::Sheet::Waits { server, name, said, url, pid }
        },
        None => crate::login_sheet::Sheet::Failed { server, name, error, said: output },
    };
    crate::login_sheet::show(sheet, cx);
}

/// What `server`'s latest attempt said, and what went wrong last.
pub fn last_attempt(server: ServerId, cx: &App) -> (Vec<String>, Option<String>) {
    cx.global::<Shared>().connections.get(server as usize).map_or_else(Default::default, |c| {
        (c.output.clone(), c.network.last_error.as_ref().map(|(error, _)| error.clone()))
    })
}

/// While `server` tries again after failing: what went wrong.
pub fn retrying_after(server: ServerId, cx: &App) -> Option<String> {
    let connection = cx.global::<Shared>().connections.get(server as usize)?;
    (connection.failures > 0)
        .then(|| connection.network.last_error.as_ref().map(|(error, _)| error.clone()))?
}

fn remote_opened(
    server: ServerId,
    attempt: u64,
    opened: Result<artty_core::remote::Opened, String>,
    cx: &mut App,
) {
    let current = cx
        .global::<Shared>()
        .connections
        .get(server as usize)
        .is_some_and(|c| c.attempt == attempt && c.remote.is_some());
    // Past waiting for anyone, whichever way it went.
    if current {
        crate::login_sheet::dismiss(server, cx);
    }
    let opened = match opened {
        Ok(mut opened) if !current => {
            let _ = opened.child.kill();
            return;
        },
        Ok(opened) => opened,
        Err(_) if !current => return,
        Err(error)
            if error.starts_with(artty_core::remote::NOT_INSTALLED) && by_ssh(server, cx) =>
        {
            let connection = &mut cx.global_mut::<Shared>().connections[server as usize];
            connection.close();
            connection.status = Status::NeedsInstall(artty_core::remote::NOT_INSTALLED.to_owned());
            cx.refresh_windows();
            return;
        },
        Err(error) if needs_login(&error) && by_ssh(server, cx) => {
            let connection = &mut cx.global_mut::<Shared>().connections[server as usize];
            connection.close();
            connection.status = Status::NeedsLogin(error);
            cx.refresh_windows();
            return;
        },
        Err(error) => {
            // No answer in time: its shared ssh connection may be what
            // hangs (a network gone under it); the next attempt makes a new
            // one.
            if error.starts_with(artty_core::remote::TIMED_OUT)
                && let Some(exit) = remote_config(server, cx).and_then(|r| r.shared("exit", None))
            {
                std::thread::spawn(move || artty_core::remote::ssh_process(&exit).output());
            }
            return retry_later(server, error, cx);
        },
    };
    let artty_core::remote::Opened { mut child, reader, writer, welcome } = opened;
    let version = artty_core::protocol::protocol_version();
    match welcome {
        ServerMessage::Welcome { version: theirs, .. } if theirs == version => {},
        ServerMessage::Welcome { server_version, .. } => {
            let _ = child.kill();
            std::thread::spawn(move || child.wait());
            let connection = &mut cx.global_mut::<Shared>().connections[server as usize];
            let reason = format!("runs another artty build ({server_version})");
            connection.status = match connection.remote.as_ref().is_some_and(|r| r.ssh.is_some()) {
                true => Status::NeedsInstall(reason),
                false => Status::Off(format!("{reason}; install this one there")),
            };
            cx.refresh_windows();
            return;
        },
        _ => return retry_later(server, "unexpected greeting".to_owned(), cx),
    }
    cx.global_mut::<Shared>().connections[server as usize].child = Some(child);
    if let Err(err) = start_io(server, reader, writer, cx) {
        return retry_later(server, err.to_string(), cx);
    }
    attach_remote(server, cx);
    cx.refresh_windows();
}

/// Whether ssh failed for want of a login (wrong, or not given).
fn needs_login(error: &str) -> bool {
    ["Permission denied", "Host key verification failed", "authentication", "password"]
        .iter()
        .any(|sign| error.contains(sign))
}

fn by_ssh(server: ServerId, cx: &App) -> bool {
    cx.global::<Shared>()
        .connections
        .get(server as usize)
        .and_then(|c| c.remote.as_ref())
        .is_some_and(|r| r.ssh.is_some())
}

/// Disconnect from another machine's server, until connected again (a
/// click on its header): its sessions keep running there.
pub fn disconnect_remote(server: ServerId, cx: &mut App) {
    if server == LOCAL {
        return;
    }
    // A login waiting for you ends too.
    if let Some(Status::WaitsForYou { pid, .. }) =
        cx.global::<Shared>().connections.get(server as usize).map(|c| &c.status)
    {
        // SAFETY: plain syscall; the attempt's own ssh.
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    crate::login_sheet::dismiss(server, cx);
    drop_remote(server, cx);
    if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) {
        connection.status = Status::Off("disconnected · click to connect".to_owned());
    }
    cx.refresh_windows();
}

/// Connect to `server` from scratch: end its shared ssh connection, so ssh
/// makes a new one — checking the host's key again, reading its config and
/// keys again. Its connection (and forwards) end with it, and it reconnects
/// as after any lost connection; one not connected now connects at once.
pub fn reconnect_from_scratch(server: ServerId, cx: &mut App) {
    let Some(exit) = remote_config(server, cx).and_then(|r| r.shared("exit", None)) else {
        return;
    };
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    connection.failures = 0;
    let connected = connection.outgoing.is_some();
    let exiting = cx.background_spawn(async move {
        let _ = artty_core::remote::ssh_process(&exit).output();
    });
    cx.spawn(async move |cx| {
        exiting.await;
        if !connected {
            let _ = cx.update(|cx| connect_remote(server, cx));
        }
    })
    .detach();
}

/// Install this build of artty on `server`'s machine, then connect again.
pub fn install_remote(server: ServerId, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    let Some(config) = connection.remote.clone().filter(|r| r.ssh.is_some()) else { return };
    if connection.status == Status::Installing {
        return;
    }
    connection.close();
    connection.status = Status::Installing;
    cx.refresh_windows();
    let installing = cx.background_spawn(async move {
        artty_core::remote::install(&config, Prompts::Never, crate::cli::linux_build)
    });
    cx.spawn(async move |cx| {
        let installed = installing.await;
        let _ = cx.update(|cx| {
            let connection = &mut cx.global_mut::<Shared>().connections[server as usize];
            match installed {
                Ok(installed) => {
                    log::info!("installed artty on {}", connection.name);
                    connection.failures = 0;
                    connection.status = Status::Connecting;
                    // Have the config run what was installed.
                    let config = connection.remote.clone();
                    let setting =
                        config.as_ref().and_then(|c| installed.artty_setting(c.artty.as_deref()));
                    if let (Some(mut config), Some(artty)) = (config, setting) {
                        config.artty = artty;
                        let saved =
                            artty_core::config_file::ConfigFile::open().and_then(|mut file| {
                                file.add_server(&config);
                                file.save()
                            });
                        if let Err(err) = saved {
                            log::error!("couldn't name artty's path in the config: {err}");
                        }
                        connection.remote = Some(config);
                    }
                    connect_remote(server, cx);
                },
                Err(err) => {
                    connection.status = Status::NeedsInstall(format!("couldn't install: {err}"))
                },
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Try `server` again after a while, longer after each failure in a row.
fn retry_later(server: ServerId, error: String, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    connection.close();
    connection.failures += 1;
    let delay = Duration::from_secs((1u64 << connection.failures.min(5)).min(30));
    let retry_at = Instant::now() + delay;
    log::warn!("server {}: {error}; trying again in {}s", connection.name, delay.as_secs());
    connection.network.last_error = Some((error.clone(), Instant::now()));
    connection.status = Status::Waiting { error, retry_at };
    let attempt = connection.attempt;
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        let _ = cx.update(|cx| {
            let waiting = cx.global::<Shared>().connections.get(server as usize).is_some_and(|c| {
                c.attempt == attempt && matches!(c.status, Status::Waiting { .. })
            });
            if waiting {
                connect_remote(server, cx);
            }
        });
    })
    .detach();
}

/// Take a configured server's windows out of every view and forget what
/// it showed.
fn drop_remote(server: ServerId, cx: &mut App) {
    crate::forward::stop_all(server, cx);
    if let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) {
        connection.close();
        connection.attempt += 1;
    }
    for view in app_views(cx) {
        let empty = view.update(cx, |view, cx| view.server_gone(server, cx));
        if empty {
            let handle = view.read(cx).window_handle();
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }
    let shared = cx.global_mut::<Shared>();
    shared.windows.retain(|&window, _| ids::server_of(window) != server);
    shared.screens.retain(|&pane, _| ids::server_of(pane) != server);
    shared.panes.retain(|&pane, _| ids::server_of(pane) != server);
    shared.kinds.remove(&server);
    shared.images.retain(|&image, _| ids::server_of(image) != server);
    cx.refresh_windows();
}

/// A configured server's connection ended: reconnect (unless it ended its
/// sessions); views keep their place for it meanwhile.
fn remote_lost(server: ServerId, cx: &mut App) {
    let stopped = cx.global::<Shared>().connections.get(server as usize).is_some_and(|c| c.stopped);
    drop_remote(server, cx);
    if stopped {
        cx.global_mut::<Shared>().connections[server as usize].status =
            Status::Off("its sessions ended".to_owned());
        cx.refresh_windows();
    } else {
        retry_later(server, "connection lost".to_owned(), cx);
    }
}

/// Show a server's windows in the app's windows: each view gets back the
/// one it showed before (after a reconnect), or a detached one, in order.
/// If it has none for any view, the first view gets a new one.
fn attach_remote(server: ServerId, cx: &mut App) {
    let all = query_on(server, Query::AllWindows, cx);
    cx.spawn(async move |cx| {
        let windows = match all.await {
            Ok(Reply::Windows(windows)) => windows,
            _ => return,
        };
        let _ = cx.update(|cx| {
            let views = app_views(cx);
            let mut free: VecDeque<WindowId> =
                windows.iter().filter(|w| w.apps == 0).map(|w| w.window).collect();
            let mut waiting = Vec::new();
            let mut attached = false;
            for view in views.iter() {
                let remembered = view.read(cx).remembered(server);
                match remembered.filter(|&r| windows.iter().any(|w| w.window == r)) {
                    // Its own window back, even if the server still counts
                    // it shown: by this app's lost connection, which the
                    // server only lets go after a minute's silence (it
                    // would otherwise get a new window, a fresh shell).
                    Some(window) => {
                        free.retain(|&w| w != window);
                        attach(server, view, Some(window), PaneSpec::default(), None, true, cx);
                        attached = true;
                    },
                    None => waiting.push(view.clone()),
                }
            }
            for view in &waiting {
                let Some(window) = free.pop_front() else { break };
                attach(server, view, Some(window), PaneSpec::default(), None, false, cx);
                attached = true;
            }
            if !attached && let Some(view) = views.first() {
                attach_new(server, view, cx);
            }
        });
    })
    .detach();
}

/// Show windows that were closed while the app kept running.
pub fn reopen_closed_windows(cx: &mut App) {
    let detached = query(Query::DetachedWindows, cx);
    cx.spawn(async move |cx| {
        if let Ok(Reply::Windows(windows)) = detached.await {
            let _ = cx.update(|cx| {
                for window in windows {
                    open_window(Some(window.window), window.bounds, PaneSpec::default(), cx);
                }
            });
        }
    })
    .detach();
}

/// A new window, starting in the current directory.
pub fn new_window(cx: &mut App) {
    open_window(None, None, PaneSpec::default(), cx);
}

/// Quit the app and end every session (⌘Q leaves them running).
pub fn quit_and_end_sessions(cx: &mut App) {
    let stop = control(Request::StopServer, cx);
    cx.spawn(async move |cx| {
        let _ = stop.await;
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

/// Restart the server so it runs this build. Running programs end; the new
/// server reopens the tabs (and resumes agents) from what the old one saved.
pub fn restart_server(cx: &mut App) {
    // Hand over where the server knows how (programs keep running); the
    // connection drops either way and `server_lost` reconnects.
    let Ok(executable) = std::env::current_exe() else { return };
    let handover = control(Request::HandOver { executable }, cx);
    cx.spawn(async move |cx| {
        if let Ok(Response::Error { message }) = handover.await
            && message.starts_with("bad request")
        {
            let _ = cx.update(|cx| drop(control(Request::RestartServer, cx)));
        }
    })
    .detach();
}

/// Open the Settings window, or bring it forward.
pub fn open_settings(cx: &mut App) {
    let existing = cx.global::<Shared>().settings;
    let handle = crate::settings::open(existing, cx);
    cx.global_mut::<Shared>().settings = handle;
}

pub fn update_ready(cx: &App) -> bool {
    cx.global::<Shared>().update_ready
}

/// Notice when a build replaces the running executable.
fn check_for_update(cx: &mut App) {
    let shared = cx.global::<Shared>();
    let Some((path, built)) = &shared.executable else { return };
    if shared.update_ready {
        return;
    }
    if modified(path).is_some_and(|now| now != *built) {
        cx.global_mut::<Shared>().update_ready = true;
        cx.refresh_windows();
    }
}

/// Quit and start the new build. Sessions keep running in the server and the
/// new app reattaches to them.
pub fn restart(cx: &mut App) {
    let Some((path, _)) = cx.global::<Shared>().executable.clone() else { return };
    let bundle =
        path.ancestors().nth(3).filter(|dir| dir.extension().is_some_and(|ext| ext == "app"));
    let target = bundle.unwrap_or(&path);
    let relaunch =
        if bundle.is_some() { "exec open \"$2\"" } else { "exec \"$2\" >/dev/null 2>&1" };
    // Wait for this process to exit first.
    let script = format!("while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; {relaunch}");
    let spawned = std::process::Command::new("/bin/sh")
        .args(["-c", &script, "sh", &std::process::id().to_string()])
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => cx.quit(),
        Err(err) => log::error!("restarting: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artty_core::protocol::{Existing, Listing, UploadError, UploadTo};

    /// A server of its own (in a directory of its own), reached through a
    /// command as through ssh. `None` without the app binary built.
    fn scratch_server(tag: &str) -> Option<(artty_core::remote::Opened, PathBuf)> {
        // The app binary, built beside the tests.
        let exe = std::env::current_exe().unwrap();
        let artty = exe.parent().and_then(Path::parent).map(|dir| dir.join("artty"));
        let artty = artty.filter(|path| path.exists())?;
        let dir = std::env::temp_dir().join(format!("artty-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("t")).unwrap();
        let d = dir.display();
        let config = ServerConfig {
            name: "test".into(),
            command: vec![
                "env".into(),
                format!("HOME={d}"),
                format!("XDG_STATE_HOME={d}/state"),
                format!("TMPDIR={d}/t/"),
                artty.display().to_string(),
                "server".into(),
                "--stdio".into(),
            ],
            ..Default::default()
        };
        Some((artty_core::remote::open(&config, Prompts::Never).unwrap(), dir))
    }

    /// Attach a new window; returns its first pane once laid out.
    fn attach_window(opened: &mut artty_core::remote::Opened) -> PaneId {
        let attach = ClientMessage::AttachWindow {
            window: None,
            spec: PaneSpec::default(),
            bounds: None,
            share: false,
        };
        write_message(&mut opened.writer, &attach).unwrap();
        let mut attached = None;
        for _ in 0..200 {
            match read_message::<ServerMessage>(&mut opened.reader).unwrap() {
                ServerMessage::Attached { window, .. } => attached = Some(window),
                ServerMessage::Layout { window, layout } if Some(window) == attached => {
                    if let Some(tab) = layout.active_tab() {
                        return tab.focused;
                    }
                },
                _ => {},
            }
        }
        panic!("no window");
    }

    fn stop(mut opened: artty_core::remote::Opened, dir: PathBuf) {
        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        write_message(&mut opened.writer, &stop).unwrap();
        while read_message::<ServerMessage>(&mut opened.reader).is_ok() {}
        let _ = opened.child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app on the scratch server in `dir`, its messages read on a thread.
    struct TestApp {
        writer: std::process::ChildStdin,
        rx: std::sync::mpsc::Receiver<ServerMessage>,
        child: std::process::Child,
    }

    impl TestApp {
        fn on(opened: artty_core::remote::Opened) -> Self {
            let artty_core::remote::Opened { child, mut reader, writer, .. } = opened;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                    if tx.send(message).is_err() {
                        break;
                    }
                }
            });
            Self { writer, rx, child }
        }

        fn send(&mut self, message: &ClientMessage) {
            write_message(&mut self.writer, message).unwrap();
        }

        /// Messages until `want` takes one (acknowledging screens, as the app
        /// does); panics after a while.
        fn until<T>(&mut self, mut want: impl FnMut(&ServerMessage) -> Option<T>) -> T {
            let deadline = Instant::now() + Duration::from_secs(10);
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                let Ok(message) = self.rx.recv_timeout(left) else { break };
                if let ServerMessage::Sync { seq } = message {
                    self.send(&ClientMessage::Ack { seq });
                }
                if let Some(found) = want(&message) {
                    return found;
                }
            }
            panic!("not in time");
        }

        /// Until `pane`'s screen is `columns` wide.
        fn sees_width(&mut self, pane: PaneId, columns: usize) {
            self.until(|message| match message {
                ServerMessage::Frame { pane: p, update }
                    if *p == pane && update.columns == columns =>
                {
                    Some(())
                },
                _ => None,
            });
        }
    }

    /// Two apps show one window: its pane takes the size of the one used
    /// last — typed in, or focused — and both see its screen.
    #[test]
    fn a_window_shown_by_two_apps_follows_the_one_in_use() {
        let Some((opened, dir)) = scratch_server("share") else { return };
        let config = |dir: &Path| {
            let d = dir.display();
            let exe = std::env::current_exe().unwrap();
            let artty = exe.parent().and_then(Path::parent).unwrap().join("artty");
            ServerConfig {
                name: "test".into(),
                command: vec![
                    "env".into(),
                    format!("HOME={d}"),
                    format!("XDG_STATE_HOME={d}/state"),
                    format!("TMPDIR={d}/t/"),
                    artty.display().to_string(),
                    "server".into(),
                    "--stdio".into(),
                ],
                ..Default::default()
            }
        };
        let size = |columns| artty_core::TerminalSize {
            columns,
            lines: 20,
            cell_width: 8,
            cell_height: 16,
        };
        let mut mac = TestApp::on(opened);
        mac.send(&ClientMessage::AttachWindow {
            window: None,
            spec: PaneSpec::default(),
            bounds: None,
            share: false,
        });
        let window = mac.until(|m| match m {
            ServerMessage::Attached { window, .. } => Some(*window),
            _ => None,
        });
        let pane = mac.until(|m| match m {
            ServerMessage::Layout { window: w, layout } if *w == window => {
                layout.active_tab().map(|t| t.focused)
            },
            _ => None,
        });
        mac.send(&ClientMessage::Resize { pane, size: size(100) });
        mac.send(&ClientMessage::WindowFocused { window, focused: true });
        mac.sees_width(pane, 100);

        // The phone shows it too, in the Mac's size until it's used there.
        let mut phone =
            TestApp::on(artty_core::remote::open(&config(&dir), Prompts::Never).unwrap());
        phone.send(&ClientMessage::AttachWindow {
            window: Some(window),
            spec: PaneSpec::default(),
            bounds: None,
            share: true,
        });
        let shared = phone.until(|m| match m {
            ServerMessage::Attached { window, .. } => Some(*window),
            _ => None,
        });
        assert_eq!(shared, window);
        phone.send(&ClientMessage::Resize { pane, size: size(40) });
        phone.sees_width(pane, 100);

        // Typed on the phone: its size, on both.
        phone.send(&ClientMessage::Input { pane, bytes: b" ".to_vec() });
        phone.sees_width(pane, 40);
        mac.sees_width(pane, 40);

        // Back at the Mac.
        mac.send(&ClientMessage::WindowFocused { window, focused: true });
        mac.sees_width(pane, 100);
        phone.sees_width(pane, 100);

        // The phone lets it go; the Mac still shows it.
        phone.send(&ClientMessage::DetachWindow { window });
        drop(phone.writer);
        let _ = phone.child.wait();
        mac.send(&ClientMessage::Query { id: 9, query: Query::AllWindows });
        let apps = mac.until(|m| match m {
            ServerMessage::Reply { id: 9, reply: Reply::Windows(windows) } => {
                windows.iter().find(|w| w.window == window).map(|w| w.apps)
            },
            _ => None,
        });
        assert_eq!(apps, 1);

        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        mac.send(&stop);
        drop(mac.writer);
        let _ = mac.child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Through a command, as through ssh: greeting, a window, then stopped.
    #[test]
    fn reaches_a_server_through_a_command() {
        let Some((mut opened, dir)) = scratch_server("rt") else { return };
        assert!(matches!(
            opened.welcome,
            ServerMessage::Welcome { version, .. } if version == artty_core::protocol::protocol_version()
        ));
        attach_window(&mut opened);
        stop(opened, dir);
    }

    /// Over a real link: `ARTTY_TEST_SSH=user@host cargo test -p artty
    /// over_ssh -- --ignored --nocapture`. Floods a pane with `yes` while
    /// acknowledging as the app does, and reports round trips and screens.
    #[test]
    #[ignore]
    fn over_ssh() {
        let Ok(destination) = std::env::var("ARTTY_TEST_SSH") else { return };
        let config =
            ServerConfig { name: "test".into(), ssh: Some(destination), ..Default::default() };
        let opened = artty_core::remote::open(&config, Prompts::Never).unwrap();
        let artty_core::remote::Opened { mut child, mut reader, writer, .. } = opened;
        let writer = Arc::new(std::sync::Mutex::new(writer));
        let send =
            |message: &ClientMessage| write_message(&mut *writer.lock().unwrap(), message).unwrap();
        send(&ClientMessage::AttachWindow {
            window: None,
            spec: PaneSpec::default(),
            bounds: None,
            share: false,
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                let size = postcard::to_stdvec(&message).map_or(0, |b| b.len());
                if tx.send((message, size, Instant::now())).is_err() {
                    break;
                }
            }
        });
        let (mut window, mut pane) = (None, None);
        while pane.is_none() {
            match rx.recv_timeout(Duration::from_secs(20)).unwrap().0 {
                ServerMessage::Attached { window: w, .. } => window = Some(w),
                ServerMessage::Layout { window: w, layout } if Some(w) == window => {
                    pane = layout.active_tab().map(|t| t.focused)
                },
                _ => {},
            }
        }
        let pane = pane.unwrap();
        // This Mac's cursor setting, not that machine's.
        let theme = artty_core::Theme::default();
        send(&ClientMessage::Appearance { theme, cursor: artty_core::CursorStyle::Bar });
        let shape = loop {
            let (message, _, _) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match message {
                ServerMessage::Sync { seq } => send(&ClientMessage::Ack { seq }),
                ServerMessage::Frame { update, .. } if update.cursor.is_some() => {
                    let shape = update.cursor.unwrap().shape;
                    if shape == artty_core::vt::ansi::CursorShape::Beam {
                        break shape;
                    }
                },
                _ => {},
            }
        };
        println!("cursor there: {shape:?}");
        let mut rtts = Vec::new();
        for id in 0..5 {
            let sent = Instant::now();
            send(&ClientMessage::Ping { id });
            loop {
                let (message, _, at) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if let ServerMessage::Sync { seq } = message {
                    send(&ClientMessage::Ack { seq });
                }
                if matches!(message, ServerMessage::Pong { id: got } if got == id) {
                    rtts.push(at.duration_since(sent).as_millis());
                    break;
                }
            }
        }
        send(&ClientMessage::Input { pane, bytes: b"yes\r".to_vec() });
        let start = Instant::now();
        let (mut frames, mut bytes, mut syncs) = (0, 0, 0);
        while start.elapsed() < Duration::from_secs(5) {
            let Ok((message, size, _)) = rx.recv_timeout(Duration::from_millis(500)) else {
                continue;
            };
            bytes += size;
            match message {
                ServerMessage::Frame { .. } => frames += 1,
                ServerMessage::Sync { seq } => {
                    syncs += 1;
                    send(&ClientMessage::Ack { seq });
                },
                _ => {},
            }
        }
        // Typing still gets through quickly while it floods.
        let sent = Instant::now();
        send(&ClientMessage::Ping { id: 99 });
        let pong = loop {
            let (message, _, at) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let ServerMessage::Sync { seq } = message {
                send(&ClientMessage::Ack { seq });
            }
            if matches!(message, ServerMessage::Pong { id: 99 }) {
                break at.duration_since(sent).as_millis();
            }
        };
        // Close the pane, and wait for that: the server must not be left
        // printing `yes` after the connection goes.
        send(&ClientMessage::ClosePane { pane });
        let closed = Instant::now() + Duration::from_secs(10);
        loop {
            let wait = closed.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait).map(|(message, _, _)| message) {
                Ok(ServerMessage::Sync { seq }) => send(&ClientMessage::Ack { seq }),
                Ok(ServerMessage::WindowClosed { window: w }) if Some(w) == window => break,
                Ok(_) => {},
                Err(_) => panic!("the pane wasn't closed"),
            }
        }
        println!(
            "round trips {rtts:?} ms; flooding 5 s: {syncs} batches, {frames} screens, {} KB/s; \
             a ping during it: {pong} ms",
            bytes / 5 / 1024
        );
        let _ = child.kill();
    }

    /// Files go to a pane's machine in parts and come back; a program's
    /// copy (OSC 52) and `artty open` come to the app.
    #[test]
    fn files_clipboard_and_links_reach_the_app() {
        let Some((mut opened, dir)) = scratch_server("files") else { return };
        let pane = attach_window(&mut opened);
        let artty_core::remote::Opened { mut child, mut reader, mut writer, .. } = opened;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        // Send, then wait for the message `wanted` picks (acknowledging
        // screens meanwhile).
        let mut exchange =
            |message: ClientMessage, wanted: &dyn Fn(ServerMessage) -> Option<ServerMessage>| {
                write_message(&mut writer, &message).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait).expect("an answer") {
                        ServerMessage::Sync { seq } => {
                            write_message(&mut writer, &ClientMessage::Ack { seq }).unwrap()
                        },
                        message => {
                            if let Some(message) = wanted(message) {
                                return message;
                            }
                        },
                    }
                }
            };
        let reply = |m: ServerMessage| matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m);
        let mut ask = |query: Query| match exchange(ClientMessage::Query { id: 9, query }, &reply) {
            ServerMessage::Reply { reply, .. } => reply,
            _ => unreachable!(),
        };
        let upload = |data: &[u8], last| Query::Upload {
            pane,
            upload: 1,
            name: "../notes.txt".into(),
            data: data.to_vec(),
            last,
            to: UploadTo::Cache,
        };
        assert!(matches!(ask(upload(b"hello ", false)), Reply::Upload(Ok(None))));
        let Reply::Upload(Ok(Some(there))) = ask(upload(b"there", true)) else { panic!() };
        // Its own name, in a folder of its own in the cache.
        let there = there.path;
        assert_eq!(there.file_name().unwrap(), "notes.txt");
        assert!(there.starts_with(&dir), "{there:?}");
        let read = ask(Query::ReadFile { pane, path: there.clone(), offset: 6 });
        assert!(matches!(read, Reply::File(Ok(c)) if c.data == b"there" && c.length == 11));
        let folder = ask(Query::ReadFile { pane, path: dir.clone(), offset: 0 });
        assert!(matches!(folder, Reply::File(Err(_))), "a folder isn't a file");

        // Into a folder: asked about a name that's there (at once), then
        // replacing it or beside it as "name (1)".
        let into = dir.join("into");
        std::fs::create_dir_all(&into).unwrap();
        let mut send = |upload, data: &[u8], to: UploadTo| {
            let query = Query::Upload {
                pane,
                upload,
                name: "a.txt".into(),
                data: data.to_vec(),
                last: true,
                to,
            };
            match ask(query) {
                Reply::Upload(result) => result.map(|done| done.unwrap()),
                other => panic!("{other:?}"),
            }
        };
        let folder = |existing| UploadTo::Folder { dir: into.clone(), existing };
        assert_eq!(send(2, b"one", folder(Existing::Refuse)).unwrap().path, into.join("a.txt"));
        assert_eq!(send(3, b"two", folder(Existing::Refuse)), Err(UploadError::Exists));
        assert_eq!(send(4, b"two", folder(Existing::Rename)).unwrap().path, into.join("a (1).txt"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(into.join("a.txt"), mode).unwrap();
        }
        let replaced = send(5, b"three", folder(Existing::Replace)).unwrap();
        assert_eq!(std::fs::read(&replaced.path).unwrap(), b"three");
        // As the file it replaced: still runnable.
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&replaced.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // Edited here: back in place if unchanged there since; else refused,
        // unless sent regardless.
        let file = |modified| UploadTo::File { path: into.join("a.txt"), modified };
        let saved = send(6, b"edited", file(Some(replaced.modified))).unwrap();
        assert_eq!(std::fs::read(&saved.path).unwrap(), b"edited");
        assert_eq!(send(7, b"stale", file(Some(saved.modified - 1))), Err(UploadError::Changed));
        assert_eq!(std::fs::read(into.join("a.txt")).unwrap(), b"edited");
        send(8, b"forced", file(None)).unwrap();
        assert_eq!(std::fs::read(into.join("a.txt")).unwrap(), b"forced");

        // Listed: folders first; no part files left. Asked again with what
        // it knows: unchanged, no entries sent.
        std::fs::create_dir_all(into.join("sub")).unwrap();
        let list = |known| Query::ListDir { pane, path: Some(into.clone()), known, watch: false };
        let Reply::Dir(Ok(listing)) = ask(list(None)) else { panic!() };
        assert_eq!(listing.path, into.canonicalize().unwrap());
        let entries = listing.entries.unwrap();
        let names: Vec<(&str, bool)> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("sub", true), ("a (1).txt", false), ("a.txt", false)]);
        let Reply::Dir(Ok(again)) = ask(list(Some(listing.fingerprint))) else { panic!() };
        assert_eq!((again.fingerprint, again.entries), (listing.fingerprint, None));
        // Without a path: the pane's directory.
        let pane_dir = Query::ListDir { pane, path: None, known: None, watch: false };
        assert!(matches!(ask(pane_dir), Reply::Dir(Ok(_))));

        // A program copies; another asks for a page to open.
        let copy = "printf '\\033]52;c;%s\\a' $(printf copied | base64)\r";
        let copied = exchange(ClientMessage::Input { pane, bytes: copy.into() }, &|m| {
            matches!(m, ServerMessage::Clipboard { .. }).then_some(m)
        });
        assert!(matches!(copied, ServerMessage::Clipboard { text } if text == "copied"));
        let open = Request::OpenUrl { url: "https://example.com".into() };
        let control = ClientMessage::Control { id: 3, request: encode_request(&open) };
        let opens = exchange(control, &|m| matches!(m, ServerMessage::App(_)).then_some(m));
        assert!(matches!(
            opens,
            ServerMessage::App(AppCommand::OpenUrl { url }) if url == "https://example.com"
        ));

        // A folder watched: a file made in it is told of (no asking again).
        let watched = dir.join("watched");
        std::fs::create_dir_all(&watched).unwrap();
        let list = |path| ClientMessage::Query {
            id: 9,
            query: Query::ListDir { pane, path, known: None, watch: true },
        };
        let reply = exchange(list(Some(watched.clone())), &|m| {
            matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m)
        });
        assert!(matches!(
            reply,
            ServerMessage::Reply { reply: Reply::Dir(Ok(Listing { watched: true, .. })), .. }
        ));
        std::fs::write(watched.join("new.txt"), b"x").unwrap();
        let changed = |m: ServerMessage| matches!(m, ServerMessage::DirChanged { .. }).then_some(m);
        let told = exchange(ClientMessage::Ping { id: 50 }, &changed);
        assert!(matches!(told, ServerMessage::DirChanged { pane: p } if p == pane));
        // Following the pane's directory: told when the shell moves.
        exchange(list(None), &|m| matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m));
        let cd = format!("cd {}\r", watched.display());
        let told = exchange(ClientMessage::Input { pane, bytes: cd.into_bytes() }, &changed);
        assert!(matches!(told, ServerMessage::DirChanged { pane: p } if p == pane));

        // Everything searched: a pane's output (then shown, selected), and
        // an agent's conversation in this machine's transcripts.
        // (Typed as it isn't printed: the output line alone is "zq-needle-42".)
        let echo = b"printf 'zq-needle-%s\\n' 42\r".to_vec();
        exchange(ClientMessage::Input { pane, bytes: echo }, &|m| {
            matches!(m, ServerMessage::Frame { .. }).then_some(m)
        });
        let search = |text: &str| ClientMessage::Query {
            id: 9,
            query: Query::SearchOutput { text: text.to_owned() },
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let hit = loop {
            let reply = exchange(search("ZQ-NEEDLE-42".to_lowercase().as_str()), &|m| {
                matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m)
            });
            let ServerMessage::Reply { reply: Reply::Found(hits), .. } = reply else { panic!() };
            // (The output line, not the command typed.)
            if let Some(hit) = hits.into_iter().find(|h| h.text.trim() == "zq-needle-42") {
                break hit;
            }
            assert!(Instant::now() < deadline, "not found");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!((hit.pane, hit.column, hit.length), (pane, 0, 12));
        // Shown, selected: the screen changes.
        let reveal = ClientMessage::Reveal { pane, line: hit.line, column: 0, length: 12 };
        exchange(reveal, &|m| matches!(m, ServerMessage::Frame { .. }).then_some(m));
        // ⌘F counts: which match of how many (newest first, going up).
        let print = b"printf 'zq-c%s\\n' ount ount ount\r".to_vec();
        exchange(ClientMessage::Input { pane, bytes: print }, &|m| {
            matches!(m, ServerMessage::Frame { .. }).then_some(m)
        });
        let find = |restart| ClientMessage::Search {
            id: 9,
            pane,
            query: "zq-count".into(),
            older: true,
            restart,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            let reply = exchange(find(true), &|m| {
                matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m)
            });
            let ServerMessage::Reply { reply: Reply::Search(Ok(found)), .. } = reply else {
                panic!()
            };
            if found.total == 3 {
                break found;
            }
            assert!(Instant::now() < deadline, "{found:?}");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!((first.found, first.index, first.more), (true, 3, false));
        let reply = exchange(find(false), &|m| {
            matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m)
        });
        let ServerMessage::Reply { reply: Reply::Search(Ok(next)), .. } = reply else { panic!() };
        assert_eq!((next.index, next.total), (2, 3));

        let transcripts = dir.join(".claude/projects/-tmp-x");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(
            transcripts.join("11111111-2222-3333-4444-555555555555.jsonl"),
            r#"{"type":"user","cwd":"/tmp/x","message":{"role":"user","content":"find the zq-giraffe"}}"#,
        )
        .unwrap();
        let sessions = ClientMessage::Query {
            id: 9,
            query: Query::SearchSessions { text: "zq-giraffe".into() },
        };
        let reply =
            exchange(sessions, &|m| matches!(m, ServerMessage::Reply { id: 9, .. }).then_some(m));
        let ServerMessage::Reply { reply: Reply::Sessions(hits), .. } = reply else { panic!() };
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].session, "11111111-2222-3333-4444-555555555555");
        assert_eq!(hits[0].cwd.as_deref(), Some(Path::new("/tmp/x")));

        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        exchange(stop, &|m| matches!(m, ServerMessage::Stopping { .. }).then_some(m));
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent known by its status (not its process's name: one run
    /// through `artty acp`, or saying what it does) reaches the app as one:
    /// Send To, ⇧⌘K and the palette go by that.
    #[test]
    fn an_agent_by_its_status() {
        let Some((mut opened, dir)) = scratch_server("status") else { return };
        let pane = attach_window(&mut opened);
        let mut app = TestApp::on(opened);
        let program = r#"python3 -c 'import sys,time; sys.stdout.write("\033]7501;state=working:app=claude-code\007"); sys.stdout.flush(); time.sleep(30)'"#;
        app.send(&ClientMessage::Input { pane, bytes: format!("{program}\r").into_bytes() });
        let agent = app.until(|message| match message {
            ServerMessage::Panes { panes } => panes
                .iter()
                .find(|p| p.info.id == pane && p.agent.is_some())
                .map(|p| p.agent.clone()),
            _ => None,
        });
        assert_eq!(agent.as_deref(), Some("claude"));
        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        app.send(&stop);
        let _ = app.child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Panes that never stop printing, in windows no app shows, don't hold
    /// back another window's screens, or answers.
    #[test]
    fn floods_elsewhere_dont_hold_back_screens() {
        let Some((mut busy, dir)) = scratch_server("flood") else { return };
        for _ in 0..2 {
            let pane = attach_window(&mut busy);
            let input = ClientMessage::Input { pane, bytes: b"yes\r".to_vec() };
            write_message(&mut busy.writer, &input).unwrap();
        }
        // That app goes away, leaving them printing for a while.
        let _ = busy.child.kill();
        let _ = busy.child.wait();
        std::thread::sleep(Duration::from_secs(1));

        let (mut opened, _) = scratch_server("flood").unwrap();
        let pane = attach_window(&mut opened);
        let artty_core::remote::Opened { mut child, mut reader, mut writer, .. } = opened;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let input = ClientMessage::Input { pane, bytes: b"echo hi\r".to_vec() };
        write_message(&mut writer, &input).unwrap();
        write_message(&mut writer, &ClientMessage::Ping { id: 1 }).unwrap();
        let (mut screen, mut pong) = (false, false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !(screen && pong) {
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(ServerMessage::Sync { seq }) => {
                    write_message(&mut writer, &ClientMessage::Ack { seq }).unwrap()
                },
                Ok(ServerMessage::Frame { pane: p, .. }) => screen |= p == pane,
                Ok(ServerMessage::Pong { id: 1 }) => pong = true,
                Ok(_) => {},
                Err(_) => panic!("screen {screen}, answer {pong} after 2 s"),
            }
        }

        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        write_message(&mut writer, &stop).unwrap();
        while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Screens wait for acknowledgment: two batches unacknowledged and the
    /// server sends no more, however much output there is, until an Ack.
    /// And pings are answered.
    #[test]
    fn screens_wait_for_acknowledgment() {
        let Some((mut opened, dir)) = scratch_server("fc") else { return };
        let pane = attach_window(&mut opened);
        let artty_core::remote::Opened { mut child, mut reader, mut writer, .. } = opened;
        let input = ClientMessage::Input { pane, bytes: b"yes\r".to_vec() };
        write_message(&mut writer, &input).unwrap();
        // Read on a thread, to wait with a timeout.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let collect = |time: Duration| {
            let deadline = Instant::now() + time;
            let mut got = Vec::new();
            while let Ok(message) =
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                got.push(message);
            }
            got
        };
        let got = collect(Duration::from_millis(1500));
        let syncs: Vec<u64> = got
            .iter()
            .filter_map(|m| if let ServerMessage::Sync { seq } = m { Some(*seq) } else { None })
            .collect();
        assert_eq!(syncs, [1, 2], "two batches, then it waits");
        let after_last = got.iter().rposition(|m| matches!(m, ServerMessage::Sync { .. })).unwrap();
        assert!(!got[after_last..].iter().any(|m| matches!(m, ServerMessage::Frame { .. })));

        write_message(&mut writer, &ClientMessage::Ack { seq: 1 }).unwrap();
        write_message(&mut writer, &ClientMessage::Ping { id: 7 }).unwrap();
        let got = collect(Duration::from_millis(500));
        assert!(got.iter().any(|m| matches!(m, ServerMessage::Frame { .. })), "more after the Ack");
        assert!(got.iter().any(|m| matches!(m, ServerMessage::Sync { seq: 3 })));
        assert!(got.iter().any(|m| matches!(m, ServerMessage::Pong { id: 7 })));

        write_message(&mut writer, &ClientMessage::Input { pane, bytes: vec![3] }).unwrap();
        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        write_message(&mut writer, &stop).unwrap();
        while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
