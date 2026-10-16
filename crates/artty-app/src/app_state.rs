//! State shared by all windows: pane ids, the control socket, and saving.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use artty_core::control::{Request, Response};
use artty_core::{Config, SavedState, SavedWorkspace, SessionId};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, AppContext, Bounds, Entity, Global, WeakEntity, WindowBounds, WindowOptions, point, px,
    size,
};

use crate::control::{ControlServer, Pending, cli_dir};
use crate::workspace::{self, TabSpec, Workspace};

/// How often all windows' tabs are saved for the next launch.
const SAVE_INTERVAL: Duration = Duration::from_secs(5);

pub struct Shared {
    next_id: SessionId,
    pub control_path: Option<PathBuf>,
    pub cli_dir: Option<PathBuf>,
    pub integration_dir: Option<PathBuf>,
    windows: Vec<WeakEntity<Workspace>>,
    frontmost: Option<WeakEntity<Workspace>>,
    /// Save tabs for the next launch (off when another artty owns the state).
    save_enabled: bool,
    last_saved: Option<String>,
    settings: Option<gpui::WindowHandle<crate::settings::Settings>>,
    /// The running executable and when it was built, to notice rebuilds.
    executable: Option<(PathBuf, SystemTime)>,
    /// A newer build has replaced the executable; restarting picks it up.
    update_ready: bool,
    _control: Option<ControlServer>,
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

impl Global for Shared {}

/// Set up shared state, the control socket, saving, and quitting when the
/// last window closes.
pub fn init(save_enabled: bool, cx: &mut App) {
    let (requests, mut incoming) = mpsc::unbounded::<Pending>();
    let control = ControlServer::start(requests).inspect_err(|err| log::error!("control socket: {err}")).ok();
    cx.set_global(Shared {
        next_id: 1,
        control_path: control.as_ref().map(|c| c.path().to_owned()),
        cli_dir: cli_dir(),
        integration_dir: artty_core::integration::install()
            .inspect_err(|err| log::error!("shell integration: {err}"))
            .ok(),
        windows: Vec::new(),
        frontmost: None,
        save_enabled,
        last_saved: None,
        settings: None,
        executable: std::env::current_exe().ok().and_then(|path| {
            let built = modified(&path)?;
            Some((path, built))
        }),
        update_ready: false,
        _control: control,
    });

    cx.spawn(async move |cx| {
        while let Some((request, reply)) = incoming.next().await {
            if cx.update(|cx| route(request, reply, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(SAVE_INTERVAL).await;
            if cx.update(|cx| {
                save(cx);
                check_for_update(cx);
            })
            .is_err()
            {
                break;
            }
        }
    })
    .detach();
    cx.on_app_quit(|cx| {
        save(cx);
        async {}
    })
    .detach();
    cx.on_window_closed(|cx| {
        // The Settings window alone doesn't keep the app open.
        let settings = cx.global::<Shared>().settings.map(|handle| handle.window_id());
        if cx.windows().iter().all(|window| Some(window.window_id()) == settings) {
            cx.quit();
        }
    })
    .detach();
}

pub fn next_pane_id(cx: &mut App) -> SessionId {
    let shared = cx.global_mut::<Shared>();
    let id = shared.next_id;
    shared.next_id += 1;
    id
}

pub fn set_frontmost(workspace: WeakEntity<Workspace>, cx: &mut App) {
    cx.global_mut::<Shared>().frontmost = Some(workspace);
}

/// Live windows, in the order they were opened.
fn workspaces(cx: &mut App) -> Vec<Entity<Workspace>> {
    let shared = cx.global_mut::<Shared>();
    shared.windows.retain(|w| w.upgrade().is_some());
    shared.windows.iter().filter_map(WeakEntity::upgrade).collect()
}

fn frontmost(cx: &mut App) -> Option<Entity<Workspace>> {
    let front = cx.global::<Shared>().frontmost.as_ref().and_then(WeakEntity::upgrade);
    front.or_else(|| workspaces(cx).into_iter().last())
}

/// Open a window: restored from `restore`, or with one tab from `spec`.
pub fn open_window(restore: Option<SavedWorkspace>, spec: TabSpec, cx: &mut App) {
    let (config, config_error) = Config::load();
    let bounds = restore
        .as_ref()
        .and_then(|saved| saved.window.as_ref())
        .map(workspace::window_bounds)
        .unwrap_or_else(|| {
            // New windows cascade from the frontmost one.
            let offset = px(24.) * workspaces(cx).len() as f32;
            let base = Bounds::centered(None, size(px(1100.), px(680.)), cx);
            WindowBounds::Windowed(Bounds::new(point(base.origin.x + offset, base.origin.y + offset), base.size))
        });
    let options = WindowOptions {
        window_bounds: Some(bounds),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("artty".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(14.), px(14.))),
        }),
        window_background: workspace::background_appearance(&config),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        cx.new(|cx| Workspace::new(config, config_error, spec, restore, window, cx))
    });
    match opened {
        Ok(handle) => {
            if let Ok(entity) = handle.entity(cx) {
                let weak = entity.downgrade();
                let shared = cx.global_mut::<Shared>();
                shared.windows.push(weak.clone());
                shared.frontmost = Some(weak);
            }
        }
        Err(err) => log::error!("opening a window: {err}"),
    }
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
    // Mid-rebuild the file can be missing for a moment; check again later.
    if modified(path).is_some_and(|now| now != *built) {
        cx.global_mut::<Shared>().update_ready = true;
        cx.refresh_windows();
    }
}

/// Quit and start the new build, which restores the tabs saved on the way out.
pub fn restart(cx: &mut App) {
    let Some((path, _)) = cx.global::<Shared>().executable.clone() else { return };
    save(cx);
    // Inside an app bundle, reopen the bundle; otherwise run the binary.
    let bundle = path.ancestors().nth(3).filter(|dir| dir.extension().is_some_and(|ext| ext == "app"));
    let target = bundle.unwrap_or(&path);
    let relaunch = if bundle.is_some() { "exec open \"$2\"" } else { "exec \"$2\" >/dev/null 2>&1" };
    // Wait for this process to exit first, so the new one is the instance
    // that restores and saves tabs.
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

/// Open the Settings window, or bring it forward.
pub fn open_settings(cx: &mut App) {
    let existing = cx.global::<Shared>().settings;
    let handle = crate::settings::open(existing, cx);
    cx.global_mut::<Shared>().settings = handle;
}

/// A new window starting in the frontmost window's directory.
pub fn new_window(cx: &mut App) {
    let cwd = frontmost(cx).and_then(|w| w.read(cx).focused_cwd());
    open_window(None, TabSpec { cwd, ..Default::default() }, cx);
}

/// The pane a request is about, if it names one: `Some(None)` means "the
/// focused pane".
fn requested_pane(request: &Request) -> Option<Option<SessionId>> {
    match request {
        Request::Read { tab, .. }
        | Request::Send { tab, .. }
        | Request::Run { tab, .. }
        | Request::Notify { tab, .. }
        | Request::Status { tab, .. }
        | Request::Rename { tab, .. } => Some(*tab),
        Request::Focus { tab } | Request::Close { tab } => Some(Some(*tab)),
        Request::New { target, .. } => Some(*target),
        Request::List
        | Request::Settings
        | Request::Restart
        | Request::ServerStatus
        | Request::StopServer
        | Request::Processes { .. }
        | Request::Kill { .. } => None,
    }
}

/// Send a request to the window holding its pane (or the frontmost window).
fn route(request: Request, reply: oneshot::Sender<Response>, cx: &mut App) {
    let windows = workspaces(cx);
    if let Request::List = request {
        let front = frontmost(cx);
        let mut tabs = Vec::new();
        for (index, window) in windows.iter().enumerate() {
            let is_front = front.as_ref() == Some(window);
            tabs.extend(window.read(cx).pane_infos(index + 1, is_front));
        }
        let _ = reply.send(Response::Tabs { tabs });
        return;
    }
    if let Request::Settings = request {
        open_settings(cx);
        let _ = reply.send(Response::Ok);
        return;
    }
    if let Request::Restart = request {
        let _ = reply.send(Response::Ok);
        restart(cx);
        return;
    }

    let target = match requested_pane(&request) {
        Some(Some(id)) => windows.into_iter().find(|w| w.read(cx).has_pane(id)),
        _ => frontmost(cx),
    };
    let Some(target) = target else {
        let _ = reply.send(Response::error("no such pane"));
        return;
    };
    let activate = matches!(request, Request::Focus { .. });
    let handle = target.read(cx).window_handle();
    target.update(cx, |workspace, cx| workspace.handle_request(request, reply, cx));
    if activate {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

/// Save every window's tabs for the next launch, if anything changed.
pub fn save(cx: &mut App) {
    if !cx.global::<Shared>().save_enabled {
        return;
    }
    let windows: Vec<SavedWorkspace> =
        workspaces(cx).iter().map(|w| w.read(cx).saved()).filter(|w| !w.tabs.is_empty()).collect();
    let state = SavedState::new(windows);
    let Ok(json) = serde_json::to_string(&state) else { return };
    let shared = cx.global_mut::<Shared>();
    if shared.last_saved.as_ref() == Some(&json) {
        return;
    }
    match state.save() {
        Ok(()) => shared.last_saved = Some(json),
        Err(err) => log::error!("saving tabs: {err}"),
    }
}
