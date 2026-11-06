//! ssh built in (russh): log in to a machine — as its OpenSSH config says
//! ([`crate::ssh_config`]: its real name, user, keys, jump hosts, forwards…)
//! — and run `artty server --stdio` there, its stdin and stdout the
//! [`Link`]; or serve its shells ([`shells`]). For an app with no `ssh` to
//! run (a phone's). What logging in needs from the user — trusting the
//! machine's key, a password, a code or a key's passphrase, what the server
//! says meanwhile (a banner, Tailscale's sign-in address) — goes through
//! [`Login`].

mod agent;
mod cloudflare;
mod files;
mod forward;
mod held;
mod login;
mod proxy;
mod shells;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use russh::keys::{PublicKey, ssh_key};

pub use cloudflare::Tunnel;
pub use forward::{ForwardState, Forwards, Kind as ForwardKind};
pub use held::held_public_key;
pub use login::{Plan, plan};
pub use proxy::{Proxy, proxy};

use crate::link::{ChunkReader, ChunkWriter, Link};
use login::Connection;

/// A machine logged in to, as shown to the user: `user@host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub user: String,
    pub host: String,
    pub port: u16,
}

/// What's known of the machine's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKey {
    /// As recorded before.
    Known,
    /// Never seen: trust it?
    New,
    /// Not the one recorded: someone may be in between.
    Changed,
}

/// What logging in asks of the user (or of the app, for them).
pub trait Login: Send + Sync {
    /// The machine's key (`fingerprint`, SHA-256): log in? A new one is
    /// remembered when this says yes (see [`KnownHosts`]).
    fn host_key(&self, target: &Target, fingerprint: &str, known: HostKey) -> bool;
    /// What the server says while logging in: a banner, or what to do
    /// (Tailscale SSH's check: an address to sign in at, then it waits).
    fn said(&self, text: &str);
    /// Answers to the server's questions (a password, a code): `echo` says
    /// whether what's typed may show. `None` gives up.
    fn answer(&self, instructions: &str, prompts: &[(String, bool)]) -> Option<Vec<String>>;
    /// May the machine (`host`, `user@alias`) sign with key `name`, through
    /// agent forwarding? Only what's agreed to is signed.
    fn agent_sign(&self, name: &str, host: &str) -> bool {
        let _ = (name, host);
        false
    }
    /// The passphrase of key `name`, if the user gives it.
    fn passphrase(&self, name: &str) -> Option<String> {
        let prompt = format!("Passphrase for the key {name}:");
        self.answer("", &[(prompt, false)])?.into_iter().next()
    }
}

/// A key whose private half is kept elsewhere — a phone's secure hardware
/// (Android Keystore) — and never leaves it: it signs here, on request.
pub trait HeldKey: Send + Sync {
    /// Sign `data` with ECDSA P-256 and SHA-256: the signature in DER, as
    /// Keystore (and most APIs) give it. `None` if it couldn't.
    fn sign(&self, data: &[u8]) -> Option<Vec<u8>>;
}

/// A [`HeldKey`] with its public key, as [`held_public_key`] reads it.
#[derive(Clone)]
pub struct Held {
    /// What `IdentityFile` names it by.
    pub name: String,
    pub public: PublicKey,
    pub key: Arc<dyn HeldKey>,
}

/// A private key (OpenSSH's format, or PEM), and what `IdentityFile` names
/// it by: its file name (`id_ed25519`, `work`).
#[derive(Clone)]
pub struct Key {
    pub name: String,
    pub text: String,
    /// Asked for (see [`Login::passphrase`]) if it needs one and this is `None`.
    pub passphrase: Option<String>,
}

/// Ways to prove who the user is. Keys a host's `IdentityFile`s name are
/// tried first, then (unless `IdentitiesOnly`) the rest, held keys first.
#[derive(Clone, Default)]
pub struct Credentials {
    /// The ssh agent at `SSH_AUTH_SOCK` (on a desktop).
    pub agent: bool,
    pub held: Vec<Held>,
    pub keys: Vec<Key>,
}

impl Credentials {
    /// The agent and `~/.ssh`'s usual keys, as `ssh` would try them.
    pub fn usual() -> Credentials {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let keys = ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .filter_map(|name| {
                let text = std::fs::read_to_string(home.join(".ssh").join(name)).ok()?;
                Some(Key { name: (*name).to_owned(), text, passphrase: None })
            })
            .collect();
        Credentials { agent: std::env::var_os("SSH_AUTH_SOCK").is_some(), held: Vec::new(), keys }
    }
}

/// Machines' keys as OpenSSH records them (`known_hosts`).
pub struct KnownHosts(pub PathBuf);

impl KnownHosts {
    /// `~/.ssh/known_hosts`.
    pub fn usual() -> KnownHosts {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        KnownHosts(home.join(".ssh/known_hosts"))
    }

    /// What's recorded for `host` (a name, or `HostKeyAlias`) on `port`.
    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKey {
        match russh::keys::check_known_hosts_path(host, port, key, &self.0) {
            Ok(true) => HostKey::Known,
            Ok(false) => HostKey::New,
            Err(russh::keys::Error::KeyChanged { .. }) => HostKey::Changed,
            Err(_) => HostKey::New,
        }
    }

    pub fn learn(&self, host: &str, port: u16, key: &PublicKey) {
        if let Err(err) = russh::keys::known_hosts::learn_known_hosts_path(host, port, key, &self.0)
        {
            log::warn!("couldn't remember {host}'s key: {err}");
        }
    }
}

/// Log in as `plan` says and run `command` there (`artty server --stdio`,
/// through the login shell); its stdin and stdout are the link. Its
/// forwards (the config's, at first) live as long as the link.
pub fn connect(
    plan: &Plan,
    command: &str,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
) -> Result<(Link, Forwards), String> {
    let connection = Connection::open(plan, credentials, known, login.clone())?;
    let forwards = connection.forwards.clone();
    let channel = connection.block_on(async {
        let channel =
            connection.handle.channel_open_session().await.map_err(|err| err.to_string())?;
        if plan.target.forward_agent {
            channel.agent_forward(false).await.map_err(|err| err.to_string())?;
        }
        for (name, value) in &plan.target.set_env {
            channel
                .set_env(false, name.as_str(), value.as_str())
                .await
                .map_err(|e| e.to_string())?;
        }
        channel.exec(true, command).await.map_err(|err| err.to_string())?;
        Ok::<_, String>(channel)
    })?;

    // Its output, read as a stream; what's written, sent.
    let (to_reader, chunks) = std::sync::mpsc::channel::<Vec<u8>>();
    let (writer, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let said = login;
    connection.spawn(async move {
        let mut channel = channel;
        let mut stderr = Vec::new();
        loop {
            tokio::select! {
                message = channel.wait() => match message {
                    Some(ChannelMsg::Data { data }) => {
                        if to_reader.send(data.to_vec()).is_err() {
                            break;
                        }
                    },
                    // Its stderr: what the server (or its shell) says.
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        stderr.extend_from_slice(&data);
                        while let Some(end) = stderr.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = stderr.drain(..=end).collect();
                            said.said(String::from_utf8_lossy(&line).trim_end());
                        }
                    },
                    Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                    Some(_) => {},
                },
                bytes = outgoing.recv() => match bytes {
                    Some(bytes) => {
                        if channel.data_bytes(bytes).await.is_err() {
                            break;
                        }
                    },
                    None => break,
                },
            }
        }
        let _ = channel.eof().await;
    });
    let link = Link {
        reader: Box::new(ChunkReader::new(chunks)),
        writer: Box::new(ChunkWriter(writer)),
        keep: Box::new(connection),
    };
    Ok((link, forwards))
}

/// Log in as `plan` says, to a machine without artty, for its shells: the
/// link to a stand-in for an artty server, here ([`crate::plain`]), whose
/// tabs are shells there (keeping `scrollback` lines of history), over this
/// connection; and its forwards. With `keep`, each shell is in a tmux
/// session there, outliving the connection (taken up by the next).
pub fn shells(
    plan: &Plan,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
    scrollback: usize,
    keep: bool,
) -> Result<(Link, Forwards), String> {
    let connection = Connection::open(plan, credentials, known, login)?;
    let forwards = connection.forwards.clone();
    let name = format!("{}@{}", connection.target.user, plan.target.alias);
    let shells = shells::Shells::new(connection, plan.target.clone(), keep);
    Ok((crate::plain::serve(Box::new(shells), name, scrollback)?, forwards))
}

/// Log in as `plan` says and run `command` there with `stdin`: what it
/// printed, or why it failed.
pub fn run(
    plan: &Plan,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
    command: &str,
    stdin: Option<Vec<u8>>,
) -> Result<String, String> {
    Connection::open(plan, credentials, known, login)?.run(command, stdin)
}

/// Install artty as `plan` says, as the Mac app does
/// ([`artty_core::remote::installing`]): `build` gives the build for Linux on
/// a CPU (`x86_64` or `aarch64`), if there's one.
pub fn install(
    plan: &Plan,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
    build: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<artty_core::remote::Installed, String> {
    use artty_core::remote::installing;
    let connection = Connection::open(plan, credentials, known, login)?;
    let run = |command: &str, stdin: Option<Vec<u8>>| connection.run(command, stdin);
    let cpu = installing::linux_cpu(&run(installing::PROBE, None)?)?;
    let build = build(cpu).ok_or_else(|| format!("this app has no build for Linux on {cpu}"))?;
    run(installing::UPLOAD, Some(build))?;
    run(installing::MOVE_IN, None)?;
    Ok(installing::installed(&run(installing::FIND, None)?))
}

/// An ed25519 key from 32 random bytes: its private half in OpenSSH's
/// format, and its public one as `authorized_keys` takes it.
pub fn new_ed25519(seed: [u8; 32], comment: &str) -> Result<(String, String), String> {
    let pair = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let mut key = russh::keys::PrivateKey::from(pair);
    key.set_comment(comment);
    let private = key.to_openssh(ssh_key::LineEnding::LF).map_err(|err| err.to_string())?;
    let public = key.public_key().to_openssh().map_err(|err| err.to_string())?;
    Ok((private.to_string(), public))
}

/// What a private key is: its public half (`authorized_keys`' line) and
/// whether it needs a passphrase. For checking a key the user adds.
pub fn key_info(text: &str) -> Result<(String, bool), String> {
    let key = ssh_key::PrivateKey::from_openssh(text.trim())
        .or_else(|_| russh::keys::decode_secret_key(text, None))
        .map_err(|err| format!("not a private key: {err}"))?;
    let public = key.public_key().to_openssh().map_err(|err| err.to_string())?;
    Ok((public, key.is_encrypted()))
}

/// `artty server --stdio` through the login shell (for the PATH its
/// profile sets up), as the Mac app runs it.
pub fn server_command(artty: Option<&str>) -> String {
    let artty = artty.unwrap_or("artty");
    format!("exec \"$SHELL\" -lc 'exec {artty} server --stdio'")
}

/// A private key file's text, for [`Credentials::keys`].
pub fn read_key(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
}

/// How long connecting may take, unless the config says.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_reads_back() {
        let (private, public) = new_ed25519([7; 32], "phone").unwrap();
        assert!(public.starts_with("ssh-ed25519 ") && public.ends_with(" phone"));
        let key = russh::keys::decode_secret_key(&private, None).unwrap();
        assert_eq!(key.public_key().to_openssh().unwrap(), public);
        assert_eq!(key_info(&private).unwrap(), (public, false));
        assert!(key_info("nonsense").is_err());
    }
}
