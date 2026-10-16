//! App-wide state: the connection to the artty server, which owns every
//! session, and what it has told us about panes. Windows attach to the
//! server's windows and draw from here.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use artty_core::control::{Request, Response};
use artty_core::layout::PaneId;
use artty_core::protocol::{
    AppCommand, ClientMessage, Hello, PROTOCOL_VERSION, PaneSpec, PaneState, Query, Reply,
    ServerMessage, WindowId, decode_response, encode_request, read_message, write_message,
};
use artty_core::vt::term::TermMode;
use artty_core::{Config, Frame, SavedWindow};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext, AsyncApp, Bounds, Global, WeakEntity, WindowBounds, WindowOptions, point, px, size};

use crate::workspace::{self, Workspace};

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
    pub panes: HashMap<PaneId, PaneState>,
    /// Visual bells: when each pane last rang.
    pub flashes: HashMap<PaneId, Instant>,
    /// Windows asked to attach, in order; the server answers in order.
    attaching: VecDeque<WeakEntity<Workspace>>,
    windows: HashMap<WindowId, WeakEntity<Workspace>>,
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
}

impl Global for Shared {}

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
        server_mismatch: None,
        build: std::env::current_exe().map(|exe| artty_core::protocol::build_id(&exe)).unwrap_or_default(),
        stopped: false,
        reconnecting: false,
    });
    connect_and_open(spec, cx);

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = cx.update(|cx| {
                // Windows that closed since: detach them in the server.
                workspace::flush_detached(cx);
                check_for_update(cx);
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
        version: PROTOCOL_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    })?;
    hello.push('\n');
    let mut writer = stream.try_clone()?;
    writer.write_all(hello.as_bytes())?;

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
        let mut reader = std::io::BufReader::new(stream);
        while let Ok(message) = read_message::<ServerMessage>(&mut reader) {
            if incoming.unbounded_send(Some(message)).is_err() {
                return;
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

/// The server went away (it crashed, or restarted). Close our windows and
/// reconnect: a new server reopens what the old one saved. If it ended every
/// session (`artty server stop`), quit.
fn server_lost(cx: &mut App) {
    let shared = cx.global_mut::<Shared>();
    if shared.stopped {
        return cx.quit();
    }
    shared.outgoing = None;
    shared.screens.clear();
    shared.panes.clear();
    shared.attaching.clear();
    let windows: Vec<WeakEntity<Workspace>> = shared.windows.drain().map(|(_, w)| w).collect();
    if windows.is_empty() && !shared.reconnecting {
        return cx.quit();
    }
    shared.reconnecting = true;
    for window in windows.into_iter().filter_map(|w| w.upgrade()) {
        window.update(cx, |workspace, _| workspace.forget_server());
        let handle = window.read(cx).window_handle();
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    connect_and_open(PaneSpec::default(), cx);
}

fn handle(message: ServerMessage, cx: &mut App) {
    match message {
        ServerMessage::Welcome { version, server_version } => {
            let shared = cx.global_mut::<Shared>();
            shared.server_mismatch = (version != PROTOCOL_VERSION || server_version != shared.build).then_some(server_version);
            cx.refresh_windows();
        }
        ServerMessage::Attached { window, bounds } => {
            let shared = cx.global_mut::<Shared>();
            if let Some(workspace) = shared.attaching.pop_front() {
                shared.windows.insert(window, workspace.clone());
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| workspace.attached(window, bounds, cx));
                }
            }
        }
        ServerMessage::Layout { window, layout } => {
            if let Some(workspace) = workspace_for(window, cx) {
                workspace.update(cx, |workspace, cx| workspace.set_layout(layout, cx));
            }
        }
        ServerMessage::WindowClosed { window } => {
            if let Some(workspace) = cx.global_mut::<Shared>().windows.remove(&window).and_then(|w| w.upgrade()) {
                workspace.update(cx, |workspace, _| workspace.forget_server());
                let handle = workspace.read(cx).window_handle();
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
            AppCommand::Activate { window } => {
                if let Some(workspace) = workspace_for(window, cx) {
                    let handle = workspace.read(cx).window_handle();
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
            }
            AppCommand::OpenSettings => open_settings(cx),
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
    }
}

fn workspace_for(window: WindowId, cx: &App) -> Option<gpui::Entity<Workspace>> {
    cx.global::<Shared>().windows.get(&window)?.upgrade()
}

/// Redraw the window showing `pane`.
fn refresh_pane(pane: PaneId, cx: &mut App) {
    let windows: Vec<_> = cx.global::<Shared>().windows.values().filter_map(WeakEntity::upgrade).collect();
    for workspace in windows {
        if workspace.read(cx).shows(pane) {
            workspace.update(cx, |_, cx| cx.notify());
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

/// Open a window attached to the server's `window` (or a new one).
pub fn open_window(window: Option<WindowId>, bounds: Option<SavedWindow>, spec: PaneSpec, cx: &mut App) {
    let (config, config_error) = Config::load();
    let open_windows = cx.global::<Shared>().windows.len();
    let window_bounds = bounds.as_ref().map(workspace::window_bounds).unwrap_or_else(|| {
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
        window_background: workspace::background_appearance(&config),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| cx.new(|cx| Workspace::new(config, config_error, window, cx)));
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
    // No answer to wait for: the connection drops when the server exits, and
    // `server_lost` reconnects.
    drop(control(Request::RestartServer, cx));
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
