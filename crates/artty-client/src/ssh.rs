//! ssh built in (russh): log in to a machine and run `artty server --stdio`
//! there, its stdin and stdout the [`Link`]. For an app with no `ssh` to
//! run (a phone's). What logging in needs from the user — trusting the
//! machine's key, a password or a code, what the server says meanwhile (a
//! banner, Tailscale's sign-in address) — goes through [`Login`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, ssh_key};
use russh::{ChannelMsg, Disconnect};

use crate::link::{ChunkReader, ChunkWriter, Link};

/// Where to log in: `user@host` or `user@host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub user: String,
    pub host: String,
    pub port: u16,
}

impl Target {
    pub fn parse(text: &str) -> Result<Target, String> {
        let (user, rest) = text.split_once('@').ok_or("expected user@host")?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') || host.starts_with('[') => {
                (host.trim_matches(['[', ']']), port.parse().map_err(|_| "a bad port")?)
            },
            _ => (rest, 22),
        };
        if user.is_empty() || host.is_empty() {
            return Err("expected user@host".to_owned());
        }
        Ok(Target { user: user.to_owned(), host: host.to_owned(), port })
    }
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
    pub public: PublicKey,
    pub key: Arc<dyn HeldKey>,
}

/// Ways to prove who the user is, tried in turn after none at all.
#[derive(Default)]
pub struct Credentials {
    /// The ssh agent at `SSH_AUTH_SOCK` (on a desktop).
    pub agent: bool,
    /// Keys held elsewhere, tried before `keys`.
    pub held: Vec<Held>,
    /// Private keys (OpenSSH format), with their passphrases.
    pub keys: Vec<(String, Option<String>)>,
}

impl Credentials {
    /// The agent and `~/.ssh`'s usual keys, as `ssh` would try them.
    pub fn usual() -> Credentials {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let keys = ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .filter_map(|name| std::fs::read_to_string(home.join(".ssh").join(name)).ok())
            .map(|key| (key, None))
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

    pub fn check(&self, target: &Target, key: &PublicKey) -> HostKey {
        match russh::keys::check_known_hosts_path(&target.host, target.port, key, &self.0) {
            Ok(true) => HostKey::Known,
            Ok(false) => HostKey::New,
            Err(russh::keys::Error::KeyChanged { .. }) => HostKey::Changed,
            Err(_) => HostKey::New,
        }
    }

    pub fn learn(&self, target: &Target, key: &PublicKey) {
        if let Err(err) = russh::keys::known_hosts::learn_known_hosts_path(
            &target.host,
            target.port,
            key,
            &self.0,
        ) {
            log::warn!("couldn't remember {}'s key: {err}", target.host);
        }
    }
}

/// The session's side of logging in: the machine's key, the server's word.
struct Handler {
    target: Target,
    login: Arc<dyn Login>,
    known: Arc<KnownHosts>,
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
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        let key = &key;
        let known = self.known.check(&self.target, key);
        let fingerprint = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        let trusted = known == HostKey::Known
            || self.login.host_key(&self.target, &fingerprint, known) && known == HostKey::New;
        if trusted && known == HostKey::New {
            self.known.learn(&self.target, key);
        }
        Ok(trusted)
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

/// Log in to `target`: the runtime the session runs on, and its handle.
fn log_in(
    target: &Target,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
) -> Result<(tokio::runtime::Runtime, Handle<Handler>), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("artty-ssh")
        .build()
        .map_err(|err| err.to_string())?;
    let handle = runtime.block_on(async {
        let config = Arc::new(client::Config {
            // Silence is noticed: the app's pings get answers every few
            // seconds, and keepalives find a link gone dead.
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            nodelay: true,
            ..Default::default()
        });
        let handler =
            Handler { target: target.clone(), login: login.clone(), known: Arc::new(known) };
        let address = (target.host.as_str(), target.port);
        let connecting = client::connect(config, address, handler);
        let mut handle = tokio::time::timeout(Duration::from_secs(15), connecting)
            .await
            .map_err(|_| format!("{}: no answer in time", target.host))?
            .map_err(|err| match err {
                russh::Error::UnknownKey => "its key isn't trusted".to_owned(),
                err => format!("{}: {err}", target.host),
            })?;
        authenticate(&mut handle, &target.user, credentials, &*login).await?;
        Ok::<_, String>(handle)
    })?;
    Ok((runtime, handle))
}

/// Log in to `target` and run `command` there (`artty server --stdio`,
/// through the login shell); its stdin and stdout are the link.
pub fn connect(
    target: &Target,
    command: &str,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
) -> Result<Link, String> {
    let (runtime, handle) = log_in(target, credentials, known, login.clone())?;
    let channel = runtime.block_on(async {
        let channel = handle.channel_open_session().await.map_err(|err| err.to_string())?;
        channel.exec(true, command).await.map_err(|err| err.to_string())?;
        Ok::<_, String>(channel)
    })?;

    // Its output, read as a stream; what's written, sent.
    let (to_reader, chunks) = std::sync::mpsc::channel::<Vec<u8>>();
    let (writer, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let said = login;
    runtime.spawn(async move {
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
    Ok(Link {
        reader: Box::new(ChunkReader::new(chunks)),
        writer: Box::new(ChunkWriter(writer)),
        keep: Box::new(Session { handle: Some(handle), runtime: Some(runtime) }),
    })
}

/// Install artty on `target`, as the Mac app does
/// ([`artty_core::remote::installing`]): `build` gives the build for Linux on
/// a CPU (`x86_64` or `aarch64`), if there's one.
pub fn install(
    target: &Target,
    credentials: &Credentials,
    known: KnownHosts,
    login: Arc<dyn Login>,
    build: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<artty_core::remote::Installed, String> {
    use artty_core::remote::installing;
    let (runtime, handle) = log_in(target, credentials, known, login)?;
    let session = Session { handle: Some(handle), runtime: Some(runtime) };
    let run = |command: &str, stdin: Option<Vec<u8>>| session.run(command, stdin);
    let cpu = installing::linux_cpu(&run(installing::PROBE, None)?)?;
    let build = build(cpu).ok_or_else(|| format!("this app has no build for Linux on {cpu}"))?;
    run(installing::UPLOAD, Some(build))?;
    run(installing::MOVE_IN, None)?;
    Ok(installing::installed(&run(installing::FIND, None)?))
}

/// Try none, then the agent's keys, then the keys given, then the server's
/// questions (keyboard-interactive: a password, a code).
async fn authenticate(
    handle: &mut Handle<Handler>,
    user: &str,
    credentials: &Credentials,
    login: &dyn Login,
) -> Result<(), String> {
    let failed = |err: russh::Error| format!("logging in: {err}");
    // Some servers let in with nothing (Tailscale SSH, after its check).
    if handle.authenticate_none(user).await.map_err(failed)?.success() {
        return Ok(());
    }
    let hash = handle.best_supported_rsa_hash().await.map_err(failed)?.flatten();
    if credentials.agent
        && let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await
    {
        for identity in agent.request_identities().await.unwrap_or_default() {
            let key = identity.public_key().into_owned();
            let result = handle.authenticate_publickey_with(user, key, hash, &mut agent).await;
            if result.is_ok_and(|r| r.success()) {
                return Ok(());
            }
        }
    }
    for held in &credentials.held {
        let result = handle
            .authenticate_publickey_with(user, held.public.clone(), None, &mut HeldSigner(held))
            .await;
        if result.is_ok_and(|r| r.success()) {
            return Ok(());
        }
    }
    for (key, passphrase) in &credentials.keys {
        let Ok(key) = russh::keys::decode_secret_key(key, passphrase.as_deref()) else {
            continue;
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        if handle.authenticate_publickey(user, key).await.map_err(failed)?.success() {
            return Ok(());
        }
    }
    // The server's questions, until it lets in or refuses.
    let mut reply =
        handle.authenticate_keyboard_interactive_start(user, None).await.map_err(failed)?;
    loop {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => break,
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
    // Last, a password, if it takes one.
    if let Some(answers) = login.answer("", &[(format!("{user}'s password:"), false)])
        && let Some(password) = answers.into_iter().next()
        && handle.authenticate_password(user, password).await.map_err(failed)?.success()
    {
        return Ok(());
    }
    Err("not let in: no key or answer was accepted".to_owned())
}

/// Signing for russh with a [`HeldKey`].
struct HeldSigner<'a>(&'a Held);

#[derive(Debug)]
struct HeldError;

impl From<russh::SendError> for HeldError {
    fn from(_: russh::SendError) -> Self {
        HeldError
    }
}

impl russh::Signer for HeldSigner<'_> {
    type Error = HeldError;

    #[allow(clippy::manual_async_fn)]
    fn auth_sign(
        &mut self,
        _key: &russh::keys::agent::AgentIdentity,
        _hash_alg: Option<ssh_key::HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, HeldError>> + Send {
        let key = self.0.key.clone();
        async move {
            // It may take a moment (StrongBox), off the runtime's threads.
            let data = to_sign.clone();
            let der = tokio::task::spawn_blocking(move || key.sign(&data))
                .await
                .ok()
                .flatten()
                .ok_or(HeldError)?;
            // Appended, as an agent's would be.
            let blob = ssh_signature(&der).ok_or(HeldError)?;
            to_sign.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            to_sign.extend_from_slice(&blob);
            Ok(to_sign)
        }
    }
}

/// An ECDSA P-256 signature in DER (`SEQUENCE { INTEGER r, INTEGER s }`) as
/// ssh sends it: `string "ecdsa-sha2-nistp256"`, then `string (mpint r, mpint s)`.
fn ssh_signature(der: &[u8]) -> Option<Vec<u8>> {
    use russh::keys::ssh_encoding::Encode;
    let (r, s) = der_integers(der)?;
    let mut data = Vec::new();
    ssh_key::Mpint::from_positive_bytes(r).encode(&mut data).ok()?;
    ssh_key::Mpint::from_positive_bytes(s).encode(&mut data).ok()?;
    let algorithm = ssh_key::Algorithm::Ecdsa { curve: ssh_key::EcdsaCurve::NistP256 };
    ssh_key::Signature::new(algorithm, data).ok()?.encode_vec().ok()
}

/// DER's `SEQUENCE { INTEGER, INTEGER }`: the two integers' bytes.
fn der_integers(der: &[u8]) -> Option<(&[u8], &[u8])> {
    // A length: short form, or one or two bytes of long form.
    fn length(bytes: &[u8]) -> Option<(usize, &[u8])> {
        match *bytes.first()? {
            n if n < 0x80 => Some((n as usize, &bytes[1..])),
            0x81 => Some((*bytes.get(1)? as usize, bytes.get(2..)?)),
            0x82 => Some(((*bytes.get(1)? as usize) << 8 | *bytes.get(2)? as usize, bytes.get(3..)?)),
            _ => None,
        }
    }
    fn integer(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
        let (&0x02, rest) = bytes.split_first()? else { return None };
        let (n, rest) = length(rest)?;
        Some((rest.get(..n)?, rest.get(n..)?))
    }
    let (&0x30, rest) = der.split_first()? else { return None };
    let (n, rest) = length(rest)?;
    let body = rest.get(..n)?;
    let (r, body) = integer(body)?;
    let (s, body) = integer(body)?;
    body.is_empty().then_some((r, s))
}

/// A P-256 public key as Keystore gives it (X.509 `SubjectPublicKeyInfo`,
/// DER), for [`Held`]: its point is the last 65 bytes.
pub fn held_public_key(spki: &[u8], comment: &str) -> Result<PublicKey, String> {
    let point = spki.len().checked_sub(65).map(|at| &spki[at..]).ok_or("not a P-256 key")?;
    let ecdsa = ssh_key::public::EcdsaPublicKey::from_sec1_bytes(point)
        .map_err(|err| format!("not a P-256 key: {err}"))?;
    if !matches!(ecdsa, ssh_key::public::EcdsaPublicKey::NistP256(_)) {
        return Err("not a P-256 key".into());
    }
    Ok(PublicKey::new(ssh_key::public::KeyData::Ecdsa(ecdsa), comment))
}

/// The ssh session and the runtime it runs on, ended with the link.
struct Session {
    handle: Option<Handle<Handler>>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Session {
    /// Run `command` there, with `stdin`: what it printed, or why it failed
    /// (the last thing it said on stderr).
    fn run(&self, command: &str, stdin: Option<Vec<u8>>) -> Result<String, String> {
        let (Some(handle), Some(runtime)) = (&self.handle, &self.runtime) else {
            return Err("not logged in".into());
        };
        runtime.block_on(async {
            let failed = |err: russh::Error| err.to_string();
            let mut channel = handle.channel_open_session().await.map_err(failed)?;
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
}

impl Drop for Session {
    fn drop(&mut self) {
        if let (Some(handle), Some(runtime)) = (self.handle.take(), self.runtime.take()) {
            runtime.block_on(async {
                let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            });
            runtime.shutdown_timeout(Duration::from_secs(1));
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_reads_back() {
        let (private, public) = new_ed25519([7; 32], "phone").unwrap();
        assert!(public.starts_with("ssh-ed25519 ") && public.ends_with(" phone"));
        let key = russh::keys::decode_secret_key(&private, None).unwrap();
        assert_eq!(key.public_key().to_openssh().unwrap(), public);
    }

    /// Signed in DER (as Keystore signs), sent as ssh's: what a server checks passes.
    #[test]
    fn held_keys_sign_as_ssh_wants() {
        use p256::ecdsa::signature::Signer;
        use p256::pkcs8::EncodePublicKey;
        use russh::keys::signature::Verifier;
        use russh::keys::ssh_encoding::Decode;
        let secret = p256::ecdsa::SigningKey::from_slice(&[9; 32]).unwrap();
        let spki = secret.verifying_key().to_public_key_der().unwrap();
        let public = held_public_key(spki.as_bytes(), "phone").unwrap();
        assert!(public.to_openssh().unwrap().starts_with("ecdsa-sha2-nistp256 "));
        for message in [&b"to sign"[..], &[0xff; 300]] {
            let signature: p256::ecdsa::Signature = secret.sign(message);
            let blob = ssh_signature(signature.to_der().as_bytes()).unwrap();
            let signature = ssh_key::Signature::decode(&mut &blob[..]).unwrap();
            Verifier::verify(public.key_data(), message, &signature).unwrap();
            assert!(Verifier::verify(public.key_data(), b"something else", &signature).is_err());
        }
        assert!(held_public_key(&[0; 10], "").is_err());
        assert!(ssh_signature(&[0x30, 0x03, 0x02, 0x01]).is_none());
    }

    #[test]
    fn targets() {
        let t = Target::parse("me@devbox").unwrap();
        assert_eq!((t.user.as_str(), t.host.as_str(), t.port), ("me", "devbox", 22));
        assert_eq!(Target::parse("me@devbox:2222").unwrap().port, 2222);
        assert_eq!(Target::parse("me@[::1]:2222").unwrap().host, "::1");
        assert!(Target::parse("devbox").is_err());
        assert!(Target::parse("@devbox").is_err());
    }
}
