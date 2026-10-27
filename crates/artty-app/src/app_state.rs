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
    AppCommand, ClientMessage, Hello, PaneSpec, PaneState, Query, Reply, ServerMessage, WindowId,
    decode_response, encode_request, read_message, write_message,
};
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
        }
    }

    /// Let go of the connection and what carries it.
    fn close(&mut self) {
        self.outgoing = None;
        self.attaching.clear();
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
    /// Visual bells: when each pane last rang.
    pub flashes: HashMap<PaneId, Instant>,
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
        flashes: HashMap::new(),
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
    });
    connect_and_open(spec, cx);

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = cx.update(|cx| {
                // Windows that closed since: detach them in the server.
                window_view::flush_detached(cx);
                sync_servers(cx);
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
    mut reader: impl std::io::Read + Send + 'static,
    mut writer: impl Write + Send + 'static,
    cx: &mut App,
) -> std::io::Result<()> {
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
    }

    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(message) = received.next().await {
            // Apply everything already queued before redrawing.
            let mut batch = vec![message];
            while let Ok(message) = received.try_recv() {
                batch.push(message);
            }
            let lost = batch.iter().any(Option::is_none);
            let handled = cx.update(|cx| {
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
fn app_views(cx: &App) -> Vec<gpui::Entity<WindowView>> {
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
        ServerMessage::WindowClosed { window } => {
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
        ServerMessage::Notify { title, body } => {
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

fn next_id(cx: &mut App) -> u64 {
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
                attach(server, &entity, window, spec, bounds, cx);
            }
        },
        Err(err) => log::error!("opening a window: {err}"),
    }
}

/// Ask `server` to show its `window` (or a new one) in `view`.
fn attach(
    server: ServerId,
    view: &gpui::Entity<WindowView>,
    window: Option<WindowId>,
    spec: PaneSpec,
    bounds: Option<SavedWindow>,
    cx: &mut App,
) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    if connection.outgoing.is_none() {
        return;
    }
    connection.attaching.push_back(view.downgrade());
    send_to(server, ClientMessage::AttachWindow { window, spec, bounds }, cx);
}

/// Show a new window of `server` in `view`: a workspace there, for the +
/// beside a server with nothing in this window yet.
pub fn attach_new(server: ServerId, view: &gpui::Entity<WindowView>, cx: &mut App) {
    attach(server, view, None, PaneSpec::default(), None, cx);
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

/// Connect to a configured server, in the background (ssh can take a while).
pub fn connect_remote(server: ServerId, cx: &mut App) {
    let Some(connection) = cx.global_mut::<Shared>().connections.get_mut(server as usize) else {
        return;
    };
    let Some(config) = connection.remote.clone() else { return };
    connection.close();
    connection.status = Status::Connecting;
    connection.attempt += 1;
    let attempt = connection.attempt;
    cx.refresh_windows();
    let opening = cx.background_spawn(async move { artty_core::remote::open(&config) });
    cx.spawn(async move |cx| {
        let opened = opening.await;
        let _ = cx.update(|cx| remote_opened(server, attempt, opened, cx));
    })
    .detach();
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
    let opened = match opened {
        Ok(mut opened) if !current => {
            let _ = opened.child.kill();
            return;
        },
        Ok(opened) => opened,
        Err(_) if !current => return,
        Err(error) => return retry_later(server, error, cx),
    };
    let artty_core::remote::Opened { mut child, reader, writer, welcome } = opened;
    let version = artty_core::protocol::protocol_version();
    match welcome {
        ServerMessage::Welcome { version: theirs, .. } if theirs == version => {},
        ServerMessage::Welcome { server_version, .. } => {
            let _ = child.kill();
            std::thread::spawn(move || child.wait());
            let connection = &mut cx.global_mut::<Shared>().connections[server as usize];
            connection.status = Status::Off(format!(
                "runs another artty build ({server_version}); install this one there"
            ));
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
    let detached = query_on(server, Query::DetachedWindows, cx);
    cx.spawn(async move |cx| {
        let windows = match detached.await {
            Ok(Reply::Windows(windows)) => windows,
            _ => return,
        };
        let _ = cx.update(|cx| {
            let views = app_views(cx);
            let mut free: VecDeque<WindowId> = windows.iter().map(|w| w.window).collect();
            let mut waiting = Vec::new();
            let mut attached = false;
            for view in views.iter() {
                match view.read(cx).remembered(server).filter(|w| free.contains(w)) {
                    Some(window) => {
                        free.retain(|&w| w != window);
                        attach(server, view, Some(window), PaneSpec::default(), None, cx);
                        attached = true;
                    },
                    None => waiting.push(view.clone()),
                }
            }
            for view in &waiting {
                let Some(window) = free.pop_front() else { break };
                attach(server, view, Some(window), PaneSpec::default(), None, cx);
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

    /// Through a command, as through ssh: a server of its own (in a
    /// directory of its own), greeting, a window, then stopped.
    #[test]
    fn reaches_a_server_through_a_command() {
        // The app binary, built beside the tests.
        let exe = std::env::current_exe().unwrap();
        let artty = exe.parent().and_then(Path::parent).map(|dir| dir.join("artty"));
        let Some(artty) = artty.filter(|path| path.exists()) else { return };
        let dir = std::env::temp_dir().join(format!("artty-rt-{}", std::process::id()));
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
        let mut opened = artty_core::remote::open(&config).unwrap();
        assert!(matches!(
            opened.welcome,
            ServerMessage::Welcome { version, .. } if version == artty_core::protocol::protocol_version()
        ));

        let attach =
            ClientMessage::AttachWindow { window: None, spec: PaneSpec::default(), bounds: None };
        write_message(&mut opened.writer, &attach).unwrap();
        let (mut attached, mut laid_out) = (None, false);
        for _ in 0..200 {
            match read_message::<ServerMessage>(&mut opened.reader).unwrap() {
                ServerMessage::Attached { window, .. } => attached = Some(window),
                ServerMessage::Layout { window, layout } if Some(window) == attached => {
                    laid_out = layout.active_tab().is_some();
                },
                _ => {},
            }
            if laid_out {
                break;
            }
        }
        assert!(laid_out, "a window with a tab");

        let stop = ClientMessage::Control { id: 1, request: encode_request(&Request::StopServer) };
        write_message(&mut opened.writer, &stop).unwrap();
        while read_message::<ServerMessage>(&mut opened.reader).is_ok() {}
        let _ = opened.child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
