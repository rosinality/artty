//! App-wide state: the connection to the artty server, which owns every
//! session, and what it has told us about panes. Windows attach to the
//! server's windows and draw from here.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{Request, Response};
use artty_core::layout::PaneId;
use artty_core::protocol::{
    AppCommand, ClientMessage, Hello, PaneSpec, PaneState, Query, Reply,
    ServerMessage, WindowId, decode_response, encode_request, read_message, write_message,
};
use artty_core::vt::term::TermMode;
use artty_core::{Config, Frame, SavedWindow};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext, AsyncApp, Bounds, Global, WeakEntity, WindowBounds, WindowOptions, point, px, size};

use crate::window_view::{self, WindowView};

/// A pane's screen as last sent by the server, and its terminal mode.
pub struct Screen {
    pub frame: Frame,
    pub mode: TermMode,
}

pub struct Shared {
    /// Messages to the server (a writer thread sends them).
    outgoing: Option<std::sync::mpsc::Sender<ClientMessage>>,
    next_request: u64,
    replies: HashMap<u64, oneshot::Sender<Reply>>,
    controls: HashMap<u64, oneshot::Sender<Response>>,
    pub screens: HashMap<PaneId, Screen>,
    /// Images screens show, by id (decoded off the main thread).
    pub images: HashMap<u64, ShownImage>,
    pub panes: HashMap<PaneId, PaneState>,
    /// Visual bells: when each pane last rang.
    pub flashes: HashMap<PaneId, Instant>,
    /// Windows asked to attach, in order; the server answers in order.
    attaching: VecDeque<WeakEntity<WindowView>>,
    windows: HashMap<WindowId, WeakEntity<WindowView>>,
    settings: Option<gpui::WindowHandle<crate::settings::Settings>>,
    /// The running executable and when it was built, to notice rebuilds.
    executable: Option<(PathBuf, SystemTime)>,
    update_ready: bool,
    /// The server runs a different build; restarting it picks up this one.
    pub server_mismatch: Option<String>,
    /// This app's build, as of when it started.
    build: String,
    /// The server said it's ending every session: quit instead of
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
    cx.global_mut::<Shared>().images.insert(id, ShownImage { columns: image.columns, rows: image.rows, render: None });
    let decoding = cx.background_spawn(async move { decode_image(image) });
    cx.spawn(async move |cx| {
        let render = decoding.await;
        let _ = cx.update(|cx| {
            if let (Some(render), Some(shown)) = (render, cx.global_mut::<Shared>().images.get_mut(&id)) {
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
        .flat_map(|screen| screen.frame.lines.iter().flat_map(|line| line.images.iter().map(|span| span.image)))
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
    cx.set_global(Shared {
        outgoing: None,
        next_request: 1,
        replies: HashMap::new(),
        controls: HashMap::new(),
        screens: HashMap::new(),
        panes: HashMap::new(),
        flashes: HashMap::new(),
        attaching: VecDeque::new(),
        windows: HashMap::new(),
        settings: None,
        executable: std::env::current_exe().ok().and_then(|path| {
            let built = modified(&path)?;
            Some((path, built))
        }),
        update_ready: false,
        images: HashMap::new(),
        server_mismatch: None,
        build: std::env::current_exe().map(|exe| artty_core::protocol::build_id(&exe)).unwrap_or_default(),
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
                check_for_update(cx);
                forget_unshown_images(cx);
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
        if !shared.reconnecting && cx.windows().iter().all(|window| Some(window.window_id()) == settings) {
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
            crate::notify::post("artty", "Restarted the artty server to update it: tabs reopen and agents resume, but programs that were running in them restart.");
        }
        return connect_once(cx, false, retries);
    }
    if let Ok(welcome) = welcome {
        handle(welcome, cx);
    }

    let (outgoing, to_send) = std::sync::mpsc::channel::<ClientMessage>();
    std::thread::Builder::new().name("artty-server-writer".into()).spawn(move || {
        for message in to_send {
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
    })?;
    // `None` means the server went away.
    let (incoming, mut received) = mpsc::unbounded::<Option<ServerMessage>>();
    std::thread::Builder::new().name("artty-server-reader".into()).spawn(move || {
        loop {
            match read_message::<ServerMessage>(&mut reader) {
                Ok(message) => {
                    if incoming.unbounded_send(Some(message)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::InvalidData {
                        // A message this build can't read: the server is
                        // from another build, though it said otherwise.
                        log::error!("reading from the server: {err}");
                        UNREADABLE.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else if err.kind() != std::io::ErrorKind::UnexpectedEof {
                        log::warn!("reading from the server: {err}");
                    }
                    break;
                }
            }
        }
        let _ = incoming.unbounded_send(None);
    })?;
    cx.global_mut::<Shared>().outgoing = Some(outgoing);

    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(message) = received.next().await {
            // Apply everything already queued before redrawing.
            let mut batch = vec![message];
            while let Ok(message) = received.try_recv() {
                batch.push(message);
            }
            let lost = batch.iter().any(Option::is_none);
            if cx.update(|cx| batch.into_iter().flatten().for_each(|m| handle(m, cx))).is_err() {
                break;
            }
            if lost {
                let _ = cx.update(server_lost);
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
    if shared.lost_at.replace(now).is_some_and(|at| now.duration_since(at) < Duration::from_secs(10)) {
        crate::notify::post("artty", "Lost the connection to the artty server again; quitting. Sessions may still be running.");
        return cx.quit();
    }
    shared.outgoing = None;
    shared.screens.clear();
    shared.panes.clear();
    shared.attaching.clear();
    let windows: Vec<WeakEntity<WindowView>> = shared.windows.drain().map(|(_, w)| w).collect();
    if windows.is_empty() && !shared.reconnecting {
        return cx.quit();
    }
    shared.reconnecting = true;
    for window in windows.into_iter().filter_map(|w| w.upgrade()) {
        window.update(cx, |view, _| view.forget_server());
        let handle = window.read(cx).window_handle();
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    connect_and_open(PaneSpec::default(), cx);
}

fn handle(message: ServerMessage, cx: &mut App) {
    match message {
        ServerMessage::Welcome { version, server_version } => {
            let shared = cx.global_mut::<Shared>();
            shared.server_mismatch = (version != artty_core::protocol::protocol_version() || server_version != shared.build).then_some(server_version);
            cx.refresh_windows();
        }
        ServerMessage::Attached { window, bounds } => {
            let shared = cx.global_mut::<Shared>();
            if let Some(view) = shared.attaching.pop_front() {
                shared.windows.insert(window, view.clone());
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.attached(window, bounds, cx));
                }
            }
        }
        ServerMessage::Layout { window, layout } => {
            if let Some(view) = view_for(window, cx) {
                view.update(cx, |view, cx| view.set_layout(layout, cx));
            }
        }
        ServerMessage::WindowClosed { window } => {
            if let Some(view) = cx.global_mut::<Shared>().windows.remove(&window).and_then(|w| w.upgrade()) {
                view.update(cx, |view, _| view.forget_server());
                let handle = view.read(cx).window_handle();
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        }
        ServerMessage::Frame { pane, update } => {
            let shared = cx.global_mut::<Shared>();
            let screen = shared.screens.entry(pane).or_insert_with(|| Screen { frame: Frame::default(), mode: TermMode::empty() });
            screen.mode = TermMode::from_bits_truncate(update.mode);
            update.apply(&mut screen.frame);
            refresh_pane(pane, cx);
        }
        ServerMessage::Panes { panes } => {
            let shared = cx.global_mut::<Shared>();
            for pane in panes {
                shared.panes.insert(pane.info.id, pane);
            }
            cx.refresh_windows();
        }
        ServerMessage::Bell { pane } => {
            let config = Config::load().0;
            if config.visual_bell {
                cx.global_mut::<Shared>().flashes.insert(pane, Instant::now());
                refresh_pane(pane, cx);
            }
            if config.bell_sound {
                let _ = std::process::Command::new("osascript").args(["-e", "beep"]).spawn();
            }
        }
        ServerMessage::Notify { title, body } => crate::notify::post(&title, &body),
        ServerMessage::App(command) => match command {
            AppCommand::Activate { window } => activate(window, cx),
            AppCommand::OpenWindow { window } => open_window(Some(window), None, PaneSpec::default(), cx),
            AppCommand::OpenSettings => open_settings(cx),
            AppCommand::ShowActivity { window } => {
                if let Some(view) = view_for(window, cx) {
                    view.update(cx, |view, cx| view.show_activity(cx));
                }
                activate(window, cx);
            }
            AppCommand::Restart => restart(cx),
        },
        ServerMessage::Reply { id, reply } => {
            if let Some(waiter) = cx.global_mut::<Shared>().replies.remove(&id) {
                let _ = waiter.send(reply);
            }
        }
        ServerMessage::Control { id, response } => {
            if let Some(waiter) = cx.global_mut::<Shared>().controls.remove(&id) {
                let _ = waiter.send(decode_response(&response));
            }
        }
        ServerMessage::Stopping { restart } => cx.global_mut::<Shared>().stopped = !restart,
        ServerMessage::Image(image) => receive_image(image, cx),
    }
}

fn view_for(window: WindowId, cx: &App) -> Option<gpui::Entity<WindowView>> {
    cx.global::<Shared>().windows.get(&window)?.upgrade()
}

/// Redraw the window showing `pane`.
fn refresh_pane(pane: PaneId, cx: &mut App) {
    let windows: Vec<_> = cx.global::<Shared>().windows.values().filter_map(WeakEntity::upgrade).collect();
    for view in windows {
        if view.read(cx).shows(pane) {
            view.update(cx, |_, cx| cx.notify());
        }
    }
}

// Talking to the server.

pub fn send(message: ClientMessage, cx: &App) {
    if let Some(outgoing) = &cx.global::<Shared>().outgoing {
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
pub fn search(pane: PaneId, query: String, older: bool, restart: bool, cx: &mut App) -> oneshot::Receiver<Reply> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().replies.insert(id, waiter);
    send(ClientMessage::Search { id, pane, query, older, restart }, cx);
    answer
}

pub fn control(request: Request, cx: &mut App) -> oneshot::Receiver<Response> {
    let (waiter, answer) = oneshot::channel();
    let id = next_id(cx);
    cx.global_mut::<Shared>().controls.insert(id, waiter);
    send(ClientMessage::Control { id, request: encode_request(&request) }, cx);
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
    let mut windows: Vec<(WindowId, String)> = cx
        .global::<Shared>()
        .windows
        .iter()
        .filter(|(id, _)| Some(**id) != except)
        .filter_map(|(&id, view)| Some((id, view.upgrade()?.read(cx).title(cx))))
        .collect();
    windows.sort_by_key(|(id, _)| *id);
    windows
}

/// Open a window attached to the server's `window` (or a new one).
pub fn open_window(window: Option<WindowId>, bounds: Option<SavedWindow>, spec: PaneSpec, cx: &mut App) {
    let (config, config_error) = Config::load();
    let open_windows = cx.global::<Shared>().windows.len();
    let window_bounds = bounds.as_ref().map(window_view::window_bounds).unwrap_or_else(|| {
        // New windows cascade.
        let offset = px(24.) * open_windows as f32;
        let base = Bounds::centered(None, size(px(1100.), px(680.)), cx);
        WindowBounds::Windowed(Bounds::new(point(base.origin.x + offset, base.origin.y + offset), base.size))
    });
    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("artty".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(14.), px(14.))),
        }),
        window_background: window_view::background_appearance(&config),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| cx.new(|cx| WindowView::new(config, config_error, window, cx)));
    match opened {
        Ok(handle) => {
            if let Ok(entity) = handle.entity(cx) {
                cx.global_mut::<Shared>().attaching.push_back(entity.downgrade());
                send(ClientMessage::AttachWindow { window, spec, bounds }, cx);
            }
        }
        Err(err) => log::error!("opening a window: {err}"),
    }
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
    let bundle = path.ancestors().nth(3).filter(|dir| dir.extension().is_some_and(|ext| ext == "app"));
    let target = bundle.unwrap_or(&path);
    let relaunch = if bundle.is_some() { "exec open \"$2\"" } else { "exec \"$2\" >/dev/null 2>&1" };
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
