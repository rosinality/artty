//! Port forwarding over a connection, as `ssh -L`, `-R` and `-D` do: the
//! config's (`LocalForward`, `RemoteForward`, `DynamicForward`), started
//! when logged in, and any added while connected. A forward that can't
//! start (its port taken, the server refusing) says why and the rest go on,
//! as OpenSSH does without `ExitOnForwardFailure`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use russh::client::{ChannelOpenHandle, Handle};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::login::Handler;
use crate::ssh_config::Forward;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Listening here, to there (`-L`).
    Local,
    /// Listening there, to here (`-R`).
    Remote,
    /// Listening here, SOCKS saying where to there (`-D`).
    Dynamic,
}

/// A forward, as it's doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardState {
    pub id: u64,
    pub kind: Kind,
    pub forward: Forward,
    /// From the ssh config (else added while connected).
    pub configured: bool,
    /// The port listened on (a remote one asked as 0 is the server's choice).
    pub listening: Option<u16>,
    /// Why it isn't listening.
    pub error: Option<String>,
    /// Connections through it now, and in all.
    pub open: usize,
    pub total: usize,
}

impl ForwardState {
    /// As `ssh` writes it: `8080 → localhost:80`, `R 9000 → localhost:3000`, `D 1080`.
    pub fn describe(&self) -> String {
        let bind = self.forward.bind.as_ref().map(|b| format!("{b}:")).unwrap_or_default();
        let to = self.forward.to.as_ref().map(|(h, p)| format!(" → {h}:{p}")).unwrap_or_default();
        let kind = match self.kind {
            Kind::Local => "L",
            Kind::Remote => "R",
            Kind::Dynamic => "D",
        };
        format!("{kind} {bind}{}{to}", self.listening.unwrap_or(self.forward.port))
    }
}

/// Where the server's connections to a remote forward's port go: by the
/// port it listens on.
pub(super) type RemoteTargets = Arc<Mutex<HashMap<u32, Remote>>>;

/// A remote forward's: its forward, where to, and the forwards (for its counts).
#[derive(Clone)]
pub(super) struct Remote {
    id: u64,
    host: String,
    port: u16,
    forwards: Weak<Inner>,
}

/// A connection's forwards: what they're doing, and starting or stopping
/// them. Lives as long as the connection, which it doesn't keep.
#[derive(Clone)]
pub struct Forwards(Arc<Inner>);

struct Inner {
    handle: Weak<Handle<Handler>>,
    runtime: tokio::runtime::Handle,
    states: Mutex<Vec<ForwardState>>,
    /// The tasks listening, by forward.
    tasks: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    remote: RemoteTargets,
    next: AtomicU64,
    /// Told when a state changes.
    changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Forwards {
    pub(super) fn new(
        handle: &Arc<Handle<Handler>>,
        runtime: tokio::runtime::Handle,
        remote: RemoteTargets,
    ) -> Forwards {
        Forwards(Arc::new(Inner {
            handle: Arc::downgrade(handle),
            runtime,
            states: Mutex::new(Vec::new()),
            tasks: Mutex::new(HashMap::new()),
            remote,
            next: AtomicU64::new(1),
            changed: Mutex::new(None),
        }))
    }

    /// What each forward is doing.
    pub fn list(&self) -> Vec<ForwardState> {
        self.0.states.lock().unwrap().clone()
    }

    /// Call `changed` whenever a forward's state changes.
    pub fn on_change(&self, changed: impl Fn() + Send + Sync + 'static) {
        *self.0.changed.lock().unwrap() = Some(Arc::new(changed));
    }

    /// Start a forward: its id.
    pub fn add(&self, kind: Kind, forward: Forward, configured: bool) -> u64 {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let state = ForwardState {
            id,
            kind,
            forward: forward.clone(),
            configured,
            listening: None,
            error: None,
            open: 0,
            total: 0,
        };
        self.0.states.lock().unwrap().push(state);
        let inner = self.0.clone();
        let task = self.0.runtime.spawn(async move {
            let result = match kind {
                Kind::Local | Kind::Dynamic => listen_here(&inner, id, kind, &forward).await,
                Kind::Remote => listen_there(&inner, id, &forward).await,
            };
            if let Err(error) = result {
                inner.update(id, |s| s.error = Some(error));
            }
        });
        self.0.tasks.lock().unwrap().insert(id, task.abort_handle());
        self.0.notify();
        id
    }

    /// Stop forward `id`: no more connections through it (those open stay).
    pub fn stop(&self, id: u64) {
        if let Some(task) = self.0.tasks.lock().unwrap().remove(&id) {
            task.abort();
        }
        let state = {
            let mut states = self.0.states.lock().unwrap();
            let at = states.iter().position(|s| s.id == id);
            at.map(|at| states.remove(at))
        };
        if let Some(state) = state
            && state.kind == Kind::Remote
            && let Some(port) = state.listening
            && let Some(handle) = self.0.handle.upgrade()
        {
            self.0.remote.lock().unwrap().remove(&u32::from(port));
            let address = remote_bind(&state.forward);
            self.0.runtime.spawn(async move {
                let _ = handle.cancel_tcpip_forward(address, u32::from(port)).await;
            });
        }
        self.0.notify();
    }

    /// A connection the server opened to a remote forward's port: to where
    /// that forward goes, from here.
    pub(super) fn arrived(
        remote: &RemoteTargets,
        channel: Channel<russh::client::Msg>,
        port: u32,
        reply: ChannelOpenHandle,
    ) -> bool {
        let Some(Remote { id, host, port: to, forwards }) =
            remote.lock().unwrap().get(&port).cloned()
        else {
            return false;
        };
        tokio::spawn(async move {
            match TcpStream::connect((host.as_str(), to)).await {
                Ok(mut tcp) => {
                    reply.accept().await;
                    let count = |change: fn(&mut ForwardState)| {
                        if let Some(inner) = forwards.upgrade() {
                            inner.update(id, change);
                        }
                    };
                    count(|s| {
                        s.open += 1;
                        s.total += 1;
                    });
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    count(|s| s.open = s.open.saturating_sub(1));
                },
                Err(err) => {
                    log::warn!("forwarding to {host}:{to}: {err}");
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                },
            }
        });
        true
    }
}

impl Inner {
    fn update(&self, id: u64, change: impl FnOnce(&mut ForwardState)) {
        if let Some(state) = self.states.lock().unwrap().iter_mut().find(|s| s.id == id) {
            change(state);
        }
        self.notify();
    }

    fn notify(&self) {
        let changed = self.changed.lock().unwrap().clone();
        if let Some(changed) = changed {
            changed();
        }
    }
}

/// Where a forward listens here: its bind address (`*`: every one), else
/// the loopback, as OpenSSH without `GatewayPorts`.
fn local_bind(forward: &Forward) -> String {
    match forward.bind.as_deref() {
        None | Some("localhost") => "127.0.0.1".to_owned(),
        Some("*") => "0.0.0.0".to_owned(),
        Some(bind) => bind.to_owned(),
    }
}

/// What a remote forward asks the server to listen on: its address, else
/// the server's loopback.
fn remote_bind(forward: &Forward) -> String {
    match forward.bind.as_deref() {
        None => "localhost".to_owned(),
        Some("*") => String::new(),
        Some(bind) => bind.to_owned(),
    }
}

/// `-L` and `-D`: listen here; each connection through a `direct-tcpip`
/// channel, to the forward's target, or (SOCKS) the one it asks for.
async fn listen_here(
    inner: &Arc<Inner>,
    id: u64,
    kind: Kind,
    forward: &Forward,
) -> Result<(), String> {
    let bind = local_bind(forward);
    let listener = TcpListener::bind((bind.as_str(), forward.port))
        .await
        .map_err(|err| format!("can't listen on {bind}:{}: {err}", forward.port))?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(forward.port);
    inner.update(id, |s| s.listening = Some(port));
    loop {
        let (mut tcp, from) = listener.accept().await.map_err(|err| err.to_string())?;
        let inner = inner.clone();
        let to = forward.to.clone();
        tokio::spawn(async move {
            let Some(handle) = inner.handle.upgrade() else { return };
            let target = match to {
                Some((host, port)) => Some((host, port, 0)),
                None => socks_request(&mut tcp).await,
            };
            let Some((host, port, socks)) = target else { return };
            let channel = handle
                .channel_open_direct_tcpip(
                    host.as_str(),
                    u32::from(port),
                    from.ip().to_string(),
                    u32::from(from.port()),
                )
                .await;
            drop(handle);
            let dynamic = kind == Kind::Dynamic;
            let channel = match channel {
                Ok(channel) => channel,
                Err(err) => {
                    log::info!("forwarding to {host}:{port}: {err}");
                    if dynamic {
                        let _ = tcp.write_all(&socks_reply(socks, false)).await;
                    }
                    return;
                },
            };
            if dynamic && tcp.write_all(&socks_reply(socks, true)).await.is_err() {
                return;
            }
            inner.update(id, |s| {
                s.open += 1;
                s.total += 1;
            });
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
            inner.update(id, |s| s.open = s.open.saturating_sub(1));
        });
    }
}

/// `-R`: the server listens; its connections arrive as `forwarded-tcpip`
/// channels (see [`Forwards::arrived`]).
async fn listen_there(inner: &Arc<Inner>, id: u64, forward: &Forward) -> Result<(), String> {
    let (host, to) = forward.to.clone().ok_or("a remote forward needs where to go")?;
    let handle = inner.handle.upgrade().ok_or("not connected")?;
    let address = remote_bind(forward);
    let port = handle
        .tcpip_forward(address.as_str(), u32::from(forward.port))
        .await
        .map_err(|err| format!("the server won't listen on {}: {err}", forward.port))?;
    // Port 0: the server chose (and says); else it's the one asked for.
    let port = if forward.port == 0 { port } else { u32::from(forward.port) };
    let remote = Remote { id, host, port: to, forwards: Arc::downgrade(inner) };
    inner.remote.lock().unwrap().insert(port, remote);
    inner.update(id, |s| s.listening = u16::try_from(port).ok());
    Ok(())
}

/// A SOCKS 4, 4a or 5 client's CONNECT (no authentication): where to, and
/// the version (for the answer).
async fn socks_request(tcp: &mut TcpStream) -> Option<(String, u16, u8)> {
    let version = tcp.read_u8().await.ok()?;
    match version {
        4 => {
            // command, port, address, user id \0 (4a: 0.0.0.x, then a host \0).
            let command = tcp.read_u8().await.ok()?;
            let port = tcp.read_u16().await.ok()?;
            let mut ip = [0; 4];
            tcp.read_exact(&mut ip).await.ok()?;
            read_until_nul(tcp).await?;
            let host = if ip[..3] == [0, 0, 0] && ip[3] != 0 {
                read_until_nul(tcp).await?
            } else {
                std::net::Ipv4Addr::from(ip).to_string()
            };
            if command != 1 {
                let _ = tcp.write_all(&[0, 0x5b, 0, 0, 0, 0, 0, 0]).await;
                return None;
            }
            Some((host, port, 4))
        },
        5 => {
            let methods = tcp.read_u8().await.ok()?;
            let mut offered = vec![0; methods as usize];
            tcp.read_exact(&mut offered).await.ok()?;
            if !offered.contains(&0) {
                let _ = tcp.write_all(&[5, 0xff]).await;
                return None;
            }
            tcp.write_all(&[5, 0]).await.ok()?;
            let mut head = [0; 4];
            tcp.read_exact(&mut head).await.ok()?;
            let [5, command, _, kind] = head else { return None };
            let host = match kind {
                1 => {
                    let mut ip = [0; 4];
                    tcp.read_exact(&mut ip).await.ok()?;
                    std::net::Ipv4Addr::from(ip).to_string()
                },
                3 => {
                    let len = tcp.read_u8().await.ok()?;
                    let mut name = vec![0; len as usize];
                    tcp.read_exact(&mut name).await.ok()?;
                    String::from_utf8(name).ok()?
                },
                4 => {
                    let mut ip = [0; 16];
                    tcp.read_exact(&mut ip).await.ok()?;
                    std::net::Ipv6Addr::from(ip).to_string()
                },
                _ => return None,
            };
            let port = tcp.read_u16().await.ok()?;
            if command != 1 {
                // Only CONNECT.
                let _ = tcp.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                return None;
            }
            Some((host, port, 5))
        },
        _ => None,
    }
}

async fn read_until_nul(tcp: &mut TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    loop {
        match tcp.read_u8().await.ok()? {
            0 => return String::from_utf8(bytes).ok(),
            b if bytes.len() < 255 => bytes.push(b),
            _ => return None,
        }
    }
}

/// SOCKS's answer, in its version: connected or not.
fn socks_reply(version: u8, ok: bool) -> Vec<u8> {
    match (version, ok) {
        (4, true) => vec![0, 0x5a, 0, 0, 0, 0, 0, 0],
        (4, false) => vec![0, 0x5b, 0, 0, 0, 0, 0, 0],
        (_, true) => vec![5, 0, 0, 1, 0, 0, 0, 0, 0, 0],
        (_, false) => vec![5, 5, 0, 1, 0, 0, 0, 0, 0, 0],
    }
}
