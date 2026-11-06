//! Logging in as the config says: through each jump host in turn (each a
//! session of its own, the next one carried in its `direct-tcpip`
//! channel), with each machine's algorithms, keepalives, timeout, keys and
//! ways of logging in, and its key checked as `StrictHostKeyChecking` says.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, ssh_key};
use russh::{ChannelMsg, Disconnect, Preferred};

use super::agent::Agent;
use super::forward::{Forwards, Kind, RemoteTargets};
use super::held::HeldSigner;
use super::{CONNECT_TIMEOUT, Credentials, HostKey, KnownHosts, Login, Target};
use crate::ssh_config::{Config, Local, Resolved, StrictHostKeys};

/// How to reach a destination: the jump hosts on the way, in order, then
/// the machine itself; each as the config resolves it, its user known.
#[derive(Clone, Debug)]
pub struct Plan {
    pub hops: Vec<Resolved>,
    pub target: Resolved,
    /// The config's options not done here (for telling the user).
    pub ignored: Vec<String>,
}

/// Resolve `destination` (an alias, `user@host`, `user@host:port`) and its
/// jump hosts with `config`. A machine without a user (none given, none in
/// the config, none local) can't be reached; nor one through `ProxyCommand`.
pub fn plan(config: &Config, local: &Local, destination: &str) -> Result<Plan, String> {
    let target = with_user(config.resolve(destination, local), local)?;
    let hops = hops(config, local, &target, 0)?;
    let mut ignored = target.ignored.clone();
    for hop in &hops {
        ignored.extend(
            hop.ignored.iter().filter(|i| !ignored.contains(i)).cloned().collect::<Vec<_>>(),
        );
    }
    Ok(Plan { hops, target, ignored })
}

fn with_user(mut resolved: Resolved, local: &Local) -> Result<Resolved, String> {
    if resolved.user.is_none() && !local.user.is_empty() {
        resolved.user = Some(local.user.clone());
    }
    if resolved.user.is_none() {
        return Err(format!(
            "no user for {}: give it as user@{}, or a User in the ssh config",
            resolved.alias, resolved.alias
        ));
    }
    Ok(resolved)
}

/// The jump hosts before `resolved`. As `ssh -J a,b`: the first is reached
/// as its own config says (through its own jumps, too); the rest through
/// the ones before them.
fn hops(
    config: &Config,
    local: &Local,
    resolved: &Resolved,
    depth: usize,
) -> Result<Vec<Resolved>, String> {
    if depth > 8 {
        return Err(format!("{}: jump hosts go round in a loop", resolved.alias));
    }
    if resolved.proxy_jump.is_empty() {
        // Only cloudflared's (a Cloudflare Tunnel), which is done here.
        if let Some(command) = &resolved.proxy_command
            && super::cloudflare::tunnel(resolved).is_none()
        {
            return Err(format!(
                "{}: ProxyCommand ({command}) can't run here; a ProxyJump can do the same",
                resolved.alias
            ));
        }
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (index, hop) in resolved.proxy_jump.iter().enumerate() {
        let user = hop.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
        let port = hop.port.map(|p| format!(":{p}")).unwrap_or_default();
        let destination = format!("{user}{}{port}", hop.host);
        let hop = with_user(config.resolve(&destination, local), local)?;
        if index == 0 {
            out.extend(hops(config, local, &hop, depth + 1)?);
        }
        out.push(hop);
    }
    Ok(out)
}

/// A session, the ones it's carried through, and the runtime they run on:
/// ended (the last first) when dropped.
pub(super) struct Connection {
    pub(super) handle: Arc<Handle<Handler>>,
    jumps: Vec<Handle<Handler>>,
    runtime: Option<tokio::runtime::Runtime>,
    /// Who it is, logged in.
    pub(super) target: Target,
    pub(super) forwards: Forwards,
}

impl Connection {
    pub(super) fn open(
        plan: &Plan,
        credentials: &Credentials,
        known: KnownHosts,
        login: Arc<dyn Login>,
    ) -> Result<Connection, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("artty-ssh")
            .build()
            .map_err(|err| err.to_string())?;
        let known = Arc::new(known);
        let passphrases = Arc::new(Mutex::new(HashMap::new()));
        let remote = RemoteTargets::default();
        let (handle, jumps) = runtime.block_on(async {
            let mut jumps: Vec<Handle<Handler>> = Vec::new();
            let stages = plan.hops.iter().chain([&plan.target]);
            let mut last: Option<Handle<Handler>> = None;
            for resolved in stages {
                let mut handler =
                    Handler::new(resolved, login.clone(), known.clone(), remote.clone());
                // The machine's programs may use the keys here, if the config says.
                if std::ptr::eq(resolved, &plan.target) && resolved.forward_agent {
                    let user = resolved.user.clone().unwrap_or_default();
                    let host = format!("{user}@{}", resolved.alias);
                    let agent =
                        Agent::new(credentials.clone(), login.clone(), host, passphrases.clone());
                    handler.agent = Some(Arc::new(agent));
                }
                let config = Arc::new(client_config(resolved)?);
                let timeout = resolved
                    .connect_timeout
                    .map_or(CONNECT_TIMEOUT, |s| Duration::from_secs(s.into()));
                let name = &resolved.alias;
                let connecting = async {
                    // Through the one before (the last logged in to), if any.
                    match last.as_ref() {
                        // Through a Cloudflare Tunnel (ProxyCommand cloudflared…), or direct.
                        None => match super::cloudflare::tunnel(resolved) {
                            Some(tunnel) => {
                                let stream = super::cloudflare::open(&tunnel)
                                    .await
                                    .map_err(|err| russh::Error::IO(std::io::Error::other(err)))?;
                                client::connect_stream(config, stream, handler).await
                            },
                            None => {
                                let address = (resolved.host_name.as_str(), resolved.port);
                                client::connect(config, address, handler).await
                            },
                        },
                        Some(through) => {
                            let channel = through
                                .channel_open_direct_tcpip(
                                    resolved.host_name.as_str(),
                                    resolved.port.into(),
                                    "127.0.0.1",
                                    0,
                                )
                                .await?;
                            client::connect_stream(config, channel.into_stream(), handler).await
                        },
                    }
                };
                let mut handle = tokio::time::timeout(timeout, connecting)
                    .await
                    .map_err(|_| format!("{name}: no answer in time"))?
                    .map_err(|err| match err {
                        russh::Error::UnknownKey => format!("{name}: its key isn't trusted"),
                        russh::Error::ChannelOpenFailure(_) => {
                            format!("{name}: the jump host couldn't reach it")
                        },
                        err => format!("{name}: {err}"),
                    })?;
                authenticate(&mut handle, resolved, credentials, &*login, &passphrases)
                    .await
                    .map_err(
                        |err| if plan.hops.is_empty() { err } else { format!("{name}: {err}") },
                    )?;
                if let Some(previous) = last.replace(handle) {
                    jumps.push(previous);
                }
            }
            Ok::<_, String>((last.expect("the target is a stage"), jumps))
        })?;
        let target = Target {
            user: plan.target.user.clone().unwrap_or_default(),
            host: plan.target.host_name.clone(),
            port: plan.target.port,
        };
        let handle = Arc::new(handle);
        let forwards = Forwards::new(&handle, runtime.handle().clone(), remote);
        let configured = [
            (Kind::Local, &plan.target.local_forwards),
            (Kind::Remote, &plan.target.remote_forwards),
            (Kind::Dynamic, &plan.target.dynamic_forwards),
        ];
        for (kind, list) in configured {
            for forward in list {
                forwards.add(kind, forward.clone(), true);
            }
        }
        Ok(Connection { handle, jumps, runtime: Some(runtime), target, forwards })
    }

    pub(super) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.as_ref().expect("running until dropped").block_on(future)
    }

    pub(super) fn spawn<F: Future<Output = ()> + Send + 'static>(&self, future: F) {
        if let Some(runtime) = &self.runtime {
            runtime.spawn(future);
        }
    }

    /// Run `command` there, with `stdin`: what it printed, or why it failed
    /// (the last thing it said on stderr).
    pub(super) fn run(&self, command: &str, stdin: Option<Vec<u8>>) -> Result<String, String> {
        self.block_on(async {
            let failed = |err: russh::Error| err.to_string();
            let mut channel = self.handle.channel_open_session().await.map_err(failed)?;
            channel.exec(true, command).await.map_err(failed)?;
            if let Some(stdin) = stdin {
                channel.data(&stdin[..]).await.map_err(failed)?;
            }
            channel.eof().await.map_err(failed)?;
            let (mut stdout, mut stderr, mut status) = (Vec::new(), Vec::new(), None);
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                    _ => {},
                }
            }
            if status != Some(0) {
                let said = artty_core::remote::last_said(&String::from_utf8_lossy(&stderr));
                return Err(said.unwrap_or("failed".into()));
            }
            Ok(String::from_utf8_lossy(&stdout).into_owned())
        })
    }

    /// The machine's session, and every jump host's, still there.
    pub(super) fn closed(&self) -> bool {
        self.handle.is_closed() || self.jumps.iter().any(Handle::is_closed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else { return };
        let (handle, jumps) = (self.handle.clone(), std::mem::take(&mut self.jumps));
        runtime.block_on(async {
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            for jump in jumps.iter().rev() {
                let _ = jump.disconnect(Disconnect::ByApplication, "", "en").await;
            }
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
}

/// russh's settings for a machine, as its config says.
fn client_config(resolved: &Resolved) -> Result<client::Config, String> {
    // Silence is noticed: keepalives (15 s, unless the config says) find a
    // link gone dead.
    let interval = match resolved.server_alive_interval {
        0 => Duration::from_secs(15),
        seconds => Duration::from_secs(seconds.into()),
    };
    Ok(client::Config {
        keepalive_interval: Some(interval),
        keepalive_max: resolved.server_alive_count_max.max(1) as usize,
        nodelay: true,
        preferred: preferred(resolved)?,
        ..Default::default()
    })
}

/// The algorithms to offer: russh's, as `Ciphers`, `KexAlgorithms`,
/// `HostKeyAlgorithms` and `MACs` change them (a list; `+` to add, `-` to
/// take away, `^` to put first), and compression if asked for.
fn preferred(resolved: &Resolved) -> Result<Preferred, String> {
    use russh::{cipher, compression, kex, mac};
    let defaults = Preferred::DEFAULT;
    let all_kex: Vec<kex::Name> = kex::ALL_KEX_ALGORITHMS.iter().map(|n| **n).collect();
    // The extensions a client always offers, whatever the list.
    let (extensions, plain_kex): (Vec<kex::Name>, Vec<kex::Name>) = defaults
        .kex
        .iter()
        .partition(|n| n.as_ref().starts_with("ext-info") || n.as_ref().starts_with("kex-strict"));
    let mut kex = choose(
        resolved.kex_algorithms.as_deref(),
        &plain_kex,
        &all_kex,
        |n| n.as_ref(),
        "KexAlgorithms",
    )?;
    kex.extend(extensions);
    let all_ciphers: Vec<cipher::Name> = cipher::ALL_CIPHERS.iter().map(|n| **n).collect();
    let cipher = choose(
        resolved.ciphers.as_deref(),
        &defaults.cipher,
        &all_ciphers,
        |n| n.as_ref(),
        "Ciphers",
    )?;
    let all_macs: Vec<mac::Name> = mac::ALL_MAC_ALGORITHMS.iter().map(|n| **n).collect();
    let mac = choose(resolved.macs.as_deref(), &defaults.mac, &all_macs, |n| n.as_ref(), "MACs")?;
    let mut all_keys = defaults.key.to_vec();
    let sha1_rsa = ssh_key::Algorithm::Rsa { hash: None };
    if !all_keys.contains(&sha1_rsa) {
        all_keys.push(sha1_rsa);
    }
    let key = choose(
        resolved.host_key_algorithms.as_deref(),
        &defaults.key,
        &all_keys,
        |a| a.as_str(),
        "HostKeyAlgorithms",
    )?;
    let compression: Vec<compression::Name> = if resolved.compression {
        compression::ALL_COMPRESSION_ALGORITHMS.iter().map(|n| **n).collect()
    } else {
        vec![compression::NONE]
    };
    Ok(Preferred {
        kex: Cow::Owned(kex),
        key: Cow::Owned(key),
        cipher: Cow::Owned(cipher),
        mac: Cow::Owned(mac),
        compression: Cow::Owned(compression),
        ..defaults
    })
}

/// An algorithm list as OpenSSH changes one: `spec` replacing `default`,
/// adding to it (`+`), taking from it (`-`, wildcards too), or putting
/// first (`^`); names from `all` (those russh has).
fn choose<N: Clone + PartialEq>(
    spec: Option<&str>,
    default: &[N],
    all: &[N],
    name: impl Fn(&N) -> &str,
    what: &str,
) -> Result<Vec<N>, String> {
    let Some(spec) = spec else { return Ok(default.to_vec()) };
    let wildcard = crate::ssh_config::wildcard;
    let named = |list: &str| -> Vec<N> {
        let mut out: Vec<N> = Vec::new();
        for pattern in list.split(',') {
            for n in all.iter().filter(|n| wildcard(pattern, name(n))) {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        out
    };
    let chosen = if let Some(list) = spec.strip_prefix('+') {
        let mut out = default.to_vec();
        out.extend(named(list).into_iter().filter(|n| !default.contains(n)));
        out
    } else if let Some(list) = spec.strip_prefix('-') {
        let gone = named(list);
        default.iter().filter(|n| !gone.contains(n)).cloned().collect()
    } else if let Some(list) = spec.strip_prefix('^') {
        let mut out = named(list);
        out.extend(default.iter().filter(|n| !out.contains(n)).cloned().collect::<Vec<_>>());
        out
    } else {
        named(spec)
    };
    if chosen.is_empty() {
        return Err(format!("{what} {spec}: none of them is supported here"));
    }
    Ok(chosen)
}

/// The session's side of logging in: the machine's key, the server's word.
pub(super) struct Handler {
    target: Target,
    /// The name and port its key is recorded under.
    key_name: (String, u16),
    strict: StrictHostKeys,
    login: Arc<dyn Login>,
    known: Arc<KnownHosts>,
    /// Where remote forwards' connections go.
    remote: RemoteTargets,
    /// The keys lent the machine (`ForwardAgent`).
    agent: Option<Arc<Agent>>,
}

impl Handler {
    fn new(
        resolved: &Resolved,
        login: Arc<dyn Login>,
        known: Arc<KnownHosts>,
        remote: RemoteTargets,
    ) -> Handler {
        let target = Target {
            user: resolved.user.clone().unwrap_or_default(),
            host: resolved.host_name.clone(),
            port: resolved.port,
        };
        let name = resolved.host_key_alias.clone().unwrap_or_else(|| resolved.host_name.clone());
        Handler {
            target,
            key_name: (name, resolved.port),
            strict: resolved.strict_host_key_checking,
            login,
            known,
            remote,
            agent: None,
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // A certificate: the key it's for.
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                russh::keys::PublicKey::from(cert.public_key().clone())
            },
        };
        let (name, port) = (&self.key_name.0, self.key_name.1);
        let known = self.known.check(name, port, &key);
        let fingerprint = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        let trusted = match (known, self.strict) {
            (HostKey::Known, _) => true,
            // Only what's recorded.
            (_, StrictHostKeys::Yes) => {
                self.login.said(&format!(
                    "{name}'s key ({fingerprint}) isn't the one recorded, and StrictHostKeyChecking says only that"
                ));
                false
            },
            (HostKey::New, StrictHostKeys::AcceptNew | StrictHostKeys::No) => true,
            (HostKey::Changed, StrictHostKeys::No) => {
                self.login.said(&format!(
                    "{name}'s key changed ({fingerprint}): taken, as StrictHostKeyChecking says"
                ));
                true
            },
            // A changed key is told of, and refused.
            (HostKey::Changed, _) => {
                self.login.host_key(&self.target, &fingerprint, known);
                false
            },
            (HostKey::New, _) => self.login.host_key(&self.target, &fingerprint, known),
        };
        if trusted && known == HostKey::New {
            self.known.learn(name, port, &key);
        }
        Ok(trusted)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Not one of ours: refused (`reply` dropped).
        Forwards::arrived(&self.remote, channel, connected_port, reply);
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Not lent: refused (`reply` dropped).
        if let Some(agent) = self.agent.clone() {
            tokio::spawn(async move {
                reply.accept().await;
                agent.serve(channel.into_stream()).await;
            });
        }
        Ok(())
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.login.said(banner);
        Ok(())
    }
}

/// Try none (some servers let in with nothing: Tailscale SSH, after its
/// check), then the ways the config prefers: keys (the agent's, then those
/// its `IdentityFile`s name, then the rest unless `IdentitiesOnly`), the
/// server's questions (keyboard-interactive), a password.
async fn authenticate(
    handle: &mut Handle<Handler>,
    resolved: &Resolved,
    credentials: &Credentials,
    login: &dyn Login,
    passphrases: &Mutex<HashMap<String, String>>,
) -> Result<(), String> {
    let user = resolved.user.clone().unwrap_or_default();
    let failed = |err: russh::Error| format!("logging in: {err}");
    if handle.authenticate_none(&user).await.map_err(failed)?.success() {
        return Ok(());
    }
    for method in &resolved.preferred_authentications {
        let done = match method.as_str() {
            "publickey" if resolved.pubkey_authentication => {
                by_key(handle, &user, resolved, credentials, login, passphrases).await?
            },
            "keyboard-interactive" if resolved.kbd_interactive_authentication => {
                by_questions(handle, &user, login).await?
            },
            "password" if resolved.password_authentication => {
                match login.answer("", &[(format!("{user}'s password:"), false)]) {
                    Some(answers) => match answers.into_iter().next() {
                        Some(password) => handle
                            .authenticate_password(&user, password)
                            .await
                            .map_err(failed)?
                            .success(),
                        None => false,
                    },
                    None => false,
                }
            },
            _ => false,
        };
        if done {
            return Ok(());
        }
    }
    Err("not let in: no key or answer was accepted".to_owned())
}

/// The keys, in the order to try them (see [`authenticate`]).
async fn by_key(
    handle: &mut Handle<Handler>,
    user: &str,
    resolved: &Resolved,
    credentials: &Credentials,
    login: &dyn Login,
    passphrases: &Mutex<HashMap<String, String>>,
) -> Result<bool, String> {
    let failed = |err: russh::Error| format!("logging in: {err}");
    let hash = handle.best_supported_rsa_hash().await.map_err(failed)?.flatten();
    if credentials.agent
        && let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await
    {
        for identity in agent.request_identities().await.unwrap_or_default() {
            let key = identity.public_key().into_owned();
            let result = handle.authenticate_publickey_with(user, key, hash, &mut agent).await;
            if result.is_ok_and(|r| r.success()) {
                return Ok(true);
            }
        }
    }
    // What the IdentityFiles name: by file name (or as written).
    let local = Local::default();
    let named: Vec<String> = resolved
        .identity_files
        .iter()
        .flat_map(|file| {
            let expanded = resolved.expand(file, &local);
            let base = expanded.rsplit('/').next().unwrap_or(&expanded).to_owned();
            [base, file.clone()]
        })
        .collect();
    let named_first = |name: &str| named.iter().position(|n| n == name);
    let only = resolved.identities_only && !resolved.identity_files.is_empty();
    enum Try<'a> {
        Held(&'a super::Held),
        Key(&'a super::Key),
    }
    let mut order: Vec<(Option<usize>, usize, Try)> = Vec::new();
    for (i, held) in credentials.held.iter().enumerate() {
        order.push((named_first(&held.name), i, Try::Held(held)));
    }
    for (i, key) in credentials.keys.iter().enumerate() {
        order.push((named_first(&key.name), credentials.held.len() + i, Try::Key(key)));
    }
    order.retain(|(named, _, _)| named.is_some() || !only);
    order.sort_by_key(|(named, i, _)| (named.unwrap_or(usize::MAX), *i));
    for (_, _, key) in order {
        let accepted = match key {
            Try::Held(held) => handle
                .authenticate_publickey_with(user, held.public.clone(), None, &mut HeldSigner(held))
                .await
                .is_ok_and(|r| r.success()),
            Try::Key(key) => {
                let Some(private) = decode(key, login, passphrases) else { continue };
                let private = PrivateKeyWithHashAlg::new(Arc::new(private), hash);
                handle.authenticate_publickey(user, private).await.map_err(failed)?.success()
            },
        };
        if accepted {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A key's private half: with its passphrase, asked for once if it needs one.
pub(super) fn decode(
    key: &super::Key,
    login: &dyn Login,
    passphrases: &Mutex<HashMap<String, String>>,
) -> Option<russh::keys::PrivateKey> {
    let known =
        key.passphrase.clone().or_else(|| passphrases.lock().unwrap().get(&key.name).cloned());
    match russh::keys::decode_secret_key(&key.text, known.as_deref()) {
        Ok(private) => Some(private),
        Err(russh::keys::Error::KeyIsEncrypted) if known.is_none() => {
            let passphrase = login.passphrase(&key.name)?;
            let private = russh::keys::decode_secret_key(&key.text, Some(&passphrase));
            match private {
                Ok(private) => {
                    passphrases.lock().unwrap().insert(key.name.clone(), passphrase);
                    Some(private)
                },
                Err(err) => {
                    login.said(&format!("the key {}: {err}", key.name));
                    None
                },
            }
        },
        Err(err) => {
            login.said(&format!("the key {}: {err}", key.name));
            None
        },
    }
}

/// The server's questions (a password, a code), until it lets in or refuses.
async fn by_questions(
    handle: &mut Handle<Handler>,
    user: &str,
    login: &dyn Login,
) -> Result<bool, String> {
    let failed = |err: russh::Error| format!("logging in: {err}");
    let mut reply =
        handle.authenticate_keyboard_interactive_start(user, None).await.map_err(failed)?;
    loop {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { instructions, prompts, .. } => {
                if !instructions.trim().is_empty() {
                    login.said(&instructions);
                }
                let asked: Vec<(String, bool)> =
                    prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect();
                let answers = if asked.is_empty() {
                    Vec::new()
                } else {
                    login.answer(&instructions, &asked).ok_or("login cancelled")?
                };
                reply = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(failed)?;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(text, &|_| Vec::new())
    }

    #[test]
    fn plans() {
        let c = config(
            "Host t\n  ProxyJump j1,j2\n  User me\nHost j1\n  ProxyJump bastion\n  User a\n\
             Host j2\n  ProxyJump nowhere\n  User b\nHost bastion\n  HostName b.example\n  User c\n\
             Host loop\n  ProxyJump loop\n  User d\nHost pc\n  ProxyCommand nc %h %p\n  User e\n",
        );
        let local = Local::default();
        let p = plan(&c, &local, "t").unwrap();
        let hops: Vec<&str> = p.hops.iter().map(|h| h.host_name.as_str()).collect();
        // j1 through its own bastion; j2's own jump not taken (as `ssh -J`).
        assert_eq!(hops, ["b.example", "j1", "j2"]);
        assert!(plan(&c, &local, "loop").unwrap_err().contains("loop"));
        assert!(plan(&c, &local, "pc").unwrap_err().contains("ProxyCommand"));
        assert!(plan(&c, &local, "someone-else").unwrap_err().contains("no user"));
        assert_eq!(plan(&c, &local, "x@someone-else").unwrap().target.user.as_deref(), Some("x"));
    }

    #[test]
    fn algorithm_lists() {
        let default = ["a", "b", "c"];
        let all = ["a", "b", "c", "d", "old-1", "old-2"];
        let choose = |spec| choose(spec, &default, &all, |n: &&str| n, "X");
        assert_eq!(choose(None).unwrap(), ["a", "b", "c"]);
        assert_eq!(choose(Some("+d,nope")).unwrap(), ["a", "b", "c", "d"]);
        assert_eq!(choose(Some("-b")).unwrap(), ["a", "c"]);
        assert_eq!(choose(Some("^c")).unwrap(), ["c", "a", "b"]);
        assert_eq!(choose(Some("old-*,a")).unwrap(), ["old-1", "old-2", "a"]);
        assert!(choose(Some("nope")).is_err());
        // russh's own lists take the config's names.
        let r = config("Host x\n  Ciphers aes256-ctr\n  KexAlgorithms -mlkem*\n  User u\n")
            .resolve("x", &Local::default());
        let p = preferred(&r).unwrap();
        assert_eq!(p.cipher.iter().map(|c| c.as_ref()).collect::<Vec<_>>(), ["aes256-ctr"]);
        assert!(p.kex.iter().all(|k| !k.as_ref().starts_with("mlkem")));
        assert!(p.kex.iter().any(|k| k.as_ref().starts_with("ext-info")));
    }
}
