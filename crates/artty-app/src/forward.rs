//! Other machines' ports, here: a program in a pane there listening on a
//! port (a dev server, say) is reached on this Mac's `localhost` through ssh
//! (`-L`), on the same port when it's free. Forwards are added to the
//! machine's shared ssh connection (see `ServerConfig::shared`): no login of
//! their own. They're cancelled when stopped or when the app quits, dropped
//! when the connection is lost, and end with the shared connection at the
//! latest (ten minutes after the app's session ends, should the app crash).

use std::collections::{HashMap, HashSet};
use std::process::Command;

use artty_core::Config;
use gpui::{App, Global};

use crate::app_state::{Shared, Status};
use crate::ids::{LOCAL, ServerId};

#[derive(Default)]
pub struct Forwards {
    /// Ports of other machines' reached here, on which port.
    forwards: HashMap<(ServerId, u16), u16>,
    /// Why forwarding a port failed, to show beside it.
    errors: HashMap<(ServerId, u16), String>,
    /// Ports already seen listening, so `auto-forward-ports` forwards each
    /// once (not again after it was stopped by hand).
    seen: HashSet<(ServerId, u16)>,
}

impl Global for Forwards {}

fn spec(local: u16, remote: u16) -> String {
    format!("127.0.0.1:{local}:localhost:{remote}")
}

/// Where `server`'s `port` is reached here, if it is: itself for this Mac's.
pub fn local_port(server: ServerId, port: u16, cx: &App) -> Option<u16> {
    if server == LOCAL {
        return Some(port);
    }
    cx.try_global::<Forwards>()?.forwards.get(&(server, port)).copied()
}

/// Why forwarding `server`'s `port` last failed.
pub fn error(server: ServerId, port: u16, cx: &App) -> Option<String> {
    cx.try_global::<Forwards>()?.errors.get(&(server, port)).cloned()
}

/// Run a command for the shared connection; why it failed, if it did.
fn shared(command: Option<Vec<String>>) -> Result<(), String> {
    let command = command.ok_or("only machines reached by ssh")?;
    let output =
        artty_core::remote::ssh_process(&command).output().map_err(|err| format!("ssh: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(artty_core::remote::last_said(&String::from_utf8_lossy(&output.stderr))
        .unwrap_or("ssh failed".into()))
}

/// Forward `server`'s `port` here (if it isn't): where it's reached.
pub fn forward(server: ServerId, port: u16, cx: &mut App) -> Result<u16, String> {
    if let Some(local) = local_port(server, port, cx) {
        return Ok(local);
    }
    let config = crate::app_state::remote_config(server, cx).ok_or("not another machine's")?;
    // The same port here, if it's free.
    let free = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).ok();
    let local = free(port)
        .or_else(|| free(0))
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .ok_or("no free port here")?;
    // (Quick: it only asks the shared connection, on this Mac.)
    shared(config.shared("forward", Some(&spec(local, port))))
        .map_err(|err| format!("not connected ({err})"))?;
    log::info!("forwarding {}'s port {port} to localhost:{local}", config.name);
    let forwards = cx.default_global::<Forwards>();
    forwards.errors.remove(&(server, port));
    forwards.forwards.insert((server, port), local);
    cx.refresh_windows();
    Ok(local)
}

/// Stop forwarding `server`'s `port`.
pub fn stop(server: ServerId, port: u16, cx: &mut App) {
    if let Some(local) = cx.default_global::<Forwards>().forwards.remove(&(server, port))
        && let Some(config) = crate::app_state::remote_config(server, cx)
    {
        std::thread::spawn(move || shared(config.shared("cancel", Some(&spec(local, port)))));
    }
    cx.refresh_windows();
}

/// Stop every forward of `server`'s.
pub fn stop_all(server: ServerId, cx: &mut App) {
    let ports: Vec<u16> = cx
        .try_global::<Forwards>()
        .map(|f| f.forwards.keys().filter(|(s, _)| *s == server).map(|(_, p)| *p).collect())
        .unwrap_or_default();
    for port in ports {
        stop(server, port, cx);
    }
}

/// The app is quitting: cancel every forward (waiting for that).
pub fn stop_everything(cx: &mut App) {
    let forwards: Vec<((ServerId, u16), u16)> = cx
        .try_global::<Forwards>()
        .map(|f| f.forwards.iter().map(|(&key, &local)| (key, local)).collect())
        .unwrap_or_default();
    for ((server, port), local) in forwards {
        if let Some(config) = crate::app_state::remote_config(server, cx) {
            let _ = shared(config.shared("cancel", Some(&spec(local, port))));
        }
    }
}

/// Open `server`'s `port` in the browser, forwarding it first.
pub fn open(server: ServerId, port: u16, cx: &mut App) -> Result<(), String> {
    let local = forward(server, port, cx)?;
    let _ = Command::new("open").arg(format!("http://localhost:{local}")).spawn();
    Ok(())
}

/// Each second: forwards of servers no longer connected are gone with
/// their connection; and newly listening ports are forwarded when
/// `auto-forward-ports` is on.
pub fn tick(cx: &mut App) {
    let connected: HashSet<ServerId> = crate::app_state::servers(cx)
        .into_iter()
        .filter(|(_, _, status)| *status == Status::Connected)
        .map(|(server, _, _)| server)
        .collect();
    let lost: Vec<(ServerId, u16)> = cx
        .try_global::<Forwards>()
        .map(|f| f.forwards.keys().filter(|(s, _)| !connected.contains(s)).copied().collect())
        .unwrap_or_default();
    if !lost.is_empty() {
        let forwards = cx.default_global::<Forwards>();
        for key in lost {
            forwards.forwards.remove(&key);
            forwards.errors.insert(key, "the connection was lost".to_owned());
        }
        cx.refresh_windows();
    }
    auto_forward(cx);
}

/// Forward ports that newly listen, if the settings say so.
fn auto_forward(cx: &mut App) {
    let listening: Vec<(ServerId, u16)> = cx
        .global::<Shared>()
        .panes
        .iter()
        .filter(|(pane, _)| crate::ids::server_of(**pane) != LOCAL)
        .flat_map(|(&pane, state)| {
            state.ports.iter().map(move |&port| (crate::ids::server_of(pane), port))
        })
        .collect();
    if listening.is_empty() {
        return;
    }
    let new: Vec<(ServerId, u16)> = {
        let forwards = cx.default_global::<Forwards>();
        listening.into_iter().filter(|key| forwards.seen.insert(*key)).collect()
    };
    if new.is_empty() || !Config::load().0.auto_forward_ports {
        return;
    }
    for (server, port) in new {
        if let Err(err) = forward(server, port, cx) {
            cx.default_global::<Forwards>().errors.insert((server, port), err);
        }
    }
}
