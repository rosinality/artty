//! Other machines' ports, here: a program in a pane there listening on a
//! port (a dev server, say) is reached on this Mac's `localhost` through ssh
//! (`-L`), on the same port when it's free. Each forward is an ssh of its
//! own whose input the app holds: it ends with the app, however that ends.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use artty_core::Config;
use gpui::{App, Global};

use crate::app_state::Shared;
use crate::ids::{LOCAL, ServerId};

/// A port of `server`'s, reached here on `local`.
struct Forward {
    local: u16,
    started: Instant,
    child: Child,
    /// Held: closing it ends the ssh.
    _input: ChildStdin,
}

#[derive(Default)]
pub struct Forwards {
    forwards: HashMap<(ServerId, u16), Forward>,
    /// Why forwarding a port failed, to show beside it.
    errors: HashMap<(ServerId, u16), String>,
    /// Ports already seen listening, so `auto-forward-ports` forwards each
    /// once (not again after it was stopped by hand).
    seen: HashSet<(ServerId, u16)>,
}

impl Global for Forwards {}

/// Where `server`'s `port` is reached here, if it is: itself for this Mac's.
pub fn local_port(server: ServerId, port: u16, cx: &App) -> Option<u16> {
    if server == LOCAL {
        return Some(port);
    }
    Some(cx.try_global::<Forwards>()?.forwards.get(&(server, port))?.local)
}

/// Why forwarding `server`'s `port` last failed.
pub fn error(server: ServerId, port: u16, cx: &App) -> Option<String> {
    cx.try_global::<Forwards>()?.errors.get(&(server, port)).cloned()
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
    let command = config.ssh_forward_command(local, port).ok_or("only machines reached by ssh")?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("ssh: {err}"))?;
    let input = child.stdin.take().ok_or("no input")?;
    log::info!("forwarding {}'s port {port} to localhost:{local}", config.name);
    let forwards = cx.default_global::<Forwards>();
    forwards.errors.remove(&(server, port));
    forwards
        .forwards
        .insert((server, port), Forward { local, started: Instant::now(), child, _input: input });
    cx.refresh_windows();
    Ok(local)
}

/// Stop forwarding `server`'s `port`.
pub fn stop(server: ServerId, port: u16, cx: &mut App) {
    if let Some(mut forward) = cx.default_global::<Forwards>().forwards.remove(&(server, port)) {
        let _ = forward.child.kill();
        std::thread::spawn(move || forward.child.wait());
    }
    cx.refresh_windows();
}

/// Stop every forward of `server`'s (it's gone from the settings).
pub fn stop_all(server: ServerId, cx: &mut App) {
    let ports: Vec<u16> = cx
        .try_global::<Forwards>()
        .map(|f| f.forwards.keys().filter(|(s, _)| *s == server).map(|(_, p)| *p).collect())
        .unwrap_or_default();
    for port in ports {
        stop(server, port, cx);
    }
}

/// Open `server`'s `port` in the browser, forwarding it first.
pub fn open(server: ServerId, port: u16, cx: &mut App) -> Result<(), String> {
    let fresh = local_port(server, port, cx).is_none();
    let local = forward(server, port, cx)?;
    std::thread::spawn(move || {
        // A new forward takes a moment (ssh connecting) to listen.
        let deadline = Instant::now() + Duration::from_secs(5);
        while fresh
            && std::net::TcpStream::connect(("127.0.0.1", local)).is_err()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("open").arg(format!("http://localhost:{local}")).spawn();
    });
    Ok(())
}

/// Each second: forget forwards whose ssh ended (and why, if it failed),
/// and forward newly listening ports when `auto-forward-ports` is on.
pub fn tick(cx: &mut App) {
    let mut ended = Vec::new();
    {
        let forwards = cx.default_global::<Forwards>();
        for (key, forward) in &mut forwards.forwards {
            if let Ok(Some(status)) = forward.child.try_wait() {
                let mut said = String::new();
                if let Some(mut stderr) = forward.child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                let said = said.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
                let quick = forward.started.elapsed() < Duration::from_secs(5);
                let error = match said.trim() {
                    "" if quick => format!("ssh ended ({status})"),
                    "" => "the connection was lost".to_owned(),
                    said => said.to_owned(),
                };
                ended.push((*key, error));
            }
        }
        for (key, error) in &ended {
            log::warn!("forward of port {} ended: {error}", key.1);
            forwards.forwards.remove(key);
            forwards.errors.insert(*key, error.clone());
        }
    }
    if !ended.is_empty() {
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
