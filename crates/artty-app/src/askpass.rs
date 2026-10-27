//! What ssh asks while logging in — a password, a passphrase, a code,
//! whether to trust a machine's key — asked in a window. ssh runs `artty`
//! itself for each question (`SSH_ASKPASS`; see `main`), which brings it
//! here over a socket only this user can reach, with a token only ssh
//! started by this app has. Passwords and passphrases can be remembered in
//! the Keychain; one remembered is tried once, then asked for again.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use artty_core::remote::{Askpass, AskpassAnswer, AskpassRequest};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, Global};

use crate::app_state;

/// Where questions come, and what proves they're from this app's ssh.
pub struct Asking {
    socket: PathBuf,
    token: String,
    program: PathBuf,
    next_key: u64,
    open: HashMap<u64, Open>,
    /// When a remembered answer was last given, by (machine, question): a
    /// second ask soon after means it was wrong.
    tried: HashMap<(String, String), Instant>,
}

impl Global for Asking {}

/// A question waiting for its answer.
struct Open {
    key: u64,
    name: String,
    prompt: String,
    reply: mpsc::Sender<Option<String>>,
}

/// How a question is answered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// Typed, unseen.
    Secret,
    /// Yes or no (trusting a machine's key, a confirmation).
    YesNo,
    /// Only a notice ("touch your security key"): it goes when ssh is done.
    Notice,
}

/// A question for a window to show (`WindowView::show_ask`).
#[derive(Clone, Debug)]
pub struct Question {
    pub id: u64,
    pub name: String,
    pub prompt: String,
    pub kind: Kind,
    /// A password or passphrase: it may be remembered.
    pub rememberable: bool,
}

enum Event {
    Asked(AskpassRequest, mpsc::Sender<Option<String>>, u64),
    /// The helper went (ssh was done with it).
    Gone(u64),
}

/// Listen for questions (once, at start).
pub fn start(cx: &mut App) {
    let socket = artty_core::control::private_dir()
        .join(format!("artty-askpass-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(err) => return log::error!("askpass: {}: {err}", socket.display()),
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }
    let token = random_token();
    let Ok(program) = std::env::current_exe() else { return };
    let (events, mut received) = unbounded::<Event>();
    {
        let token = token.clone();
        std::thread::Builder::new()
            .name("artty-askpass".into())
            .spawn(move || {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                for stream in listener.incoming().flatten() {
                    let (events, token) = (events.clone(), token.clone());
                    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::thread::spawn(move || serve(stream, id, &token, events));
                }
            })
            .ok();
    }
    cx.set_global(Asking {
        socket,
        token,
        program,
        next_key: 1,
        open: HashMap::new(),
        tried: HashMap::new(),
    });
    cx.spawn(async move |cx| {
        while let Some(event) = received.next().await {
            if cx.update(|cx| handle(event, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// The socket is gone with the app.
pub fn stop(cx: &App) {
    if let Some(asking) = cx.try_global::<Asking>() {
        let _ = std::fs::remove_file(&asking.socket);
    }
}

/// How ssh started for `name` asks this app.
pub fn askpass(name: &str, cx: &mut App) -> Option<Askpass> {
    if !cx.has_global::<Asking>() {
        return None;
    }
    let asking = cx.global_mut::<Asking>();
    asking.next_key += 1;
    Some(Askpass {
        program: asking.program.clone(),
        socket: asking.socket.clone(),
        token: asking.token.clone(),
        name: name.to_owned(),
        key: asking.next_key,
    })
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = std::fs::File::open("/dev/urandom") {
        let _ = random.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One helper: its question, then the answer back; or, if it goes first
/// (ssh no longer needs it), word of that.
fn serve(stream: UnixStream, id: u64, token: &str, events: UnboundedSender<Event>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(request) = serde_json::from_str::<AskpassRequest>(&line) else { return };
    if request.token != token {
        log::warn!("askpass: a question without this app's token");
        return;
    }
    let (reply, answer) = mpsc::channel();
    if events.unbounded_send(Event::Asked(request, reply, id)).is_err() {
        return;
    }
    // The helper ending means the question is moot.
    let watch = events.clone();
    std::thread::spawn(move || {
        let mut byte = [0];
        let _ = reader.read(&mut byte);
        let _ = watch.unbounded_send(Event::Gone(id));
    });
    let answer = answer.recv().unwrap_or(None);
    if let Ok(mut line) = serde_json::to_string(&AskpassAnswer { answer }) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
    }
}

fn handle(event: Event, cx: &mut App) {
    match event {
        Event::Asked(request, reply, id) => ask(request, reply, id, cx),
        Event::Gone(id) => finish(id, None, false, cx),
    }
}

fn kind(request: &AskpassRequest) -> Kind {
    match request.kind.as_str() {
        "none" => Kind::Notice,
        "confirm" => Kind::YesNo,
        _ if request.prompt.contains("(yes/no") => Kind::YesNo,
        _ => Kind::Secret,
    }
}

fn ask(request: AskpassRequest, reply: mpsc::Sender<Option<String>>, id: u64, cx: &mut App) {
    let kind = kind(&request);
    let lower = request.prompt.to_lowercase();
    let rememberable =
        kind == Kind::Secret && (lower.contains("password") || lower.contains("passphrase"));
    let key = (request.name.clone(), request.prompt.trim().to_owned());
    // Remembered, and not just tried: answer it.
    if rememberable && let Some(remembered) = keychain::get(&key.0, &key.1) {
        let asking = cx.global_mut::<Asking>();
        let fresh = asking.tried.get(&key).is_none_or(|at| at.elapsed() > Duration::from_secs(60));
        if fresh {
            asking.tried.insert(key, Instant::now());
            let _ = reply.send(Some(remembered));
            return;
        }
    }
    artty_core::remote::set_prompting(request.key, true);
    let question = Question {
        id,
        name: request.name.clone(),
        prompt: request.prompt.trim_end().to_owned(),
        kind,
        rememberable,
    };
    cx.global_mut::<Asking>()
        .open
        .insert(id, Open { key: request.key, name: request.name, prompt: request.prompt, reply });
    // In the window in front (or any).
    let views = app_state::app_views(cx);
    let front = cx.active_window();
    let view = views
        .iter()
        .find(|view| Some(view.read(cx).window_handle()) == front)
        .or(views.first())
        .cloned();
    match view {
        Some(view) => {
            let handle = view.read(cx).window_handle();
            view.update(cx, |view, cx| view.show_ask(question, cx));
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        },
        None => finish(id, None, false, cx),
    }
}

/// The question `id` is answered (`None`: cancelled, or no longer asked);
/// `remember` keeps the answer in the Keychain.
pub fn finish(id: u64, answer: Option<String>, remember: bool, cx: &mut App) {
    if !cx.has_global::<Asking>() {
        return;
    }
    let Some(open) = cx.global_mut::<Asking>().open.remove(&id) else { return };
    let still_open = cx.global::<Asking>().open.values().any(|o| o.key == open.key);
    if !still_open {
        artty_core::remote::set_prompting(open.key, false);
    }
    if let Some(answer) = answer.as_ref().filter(|_| remember)
        && let Err(err) = keychain::set(&open.name, open.prompt.trim(), answer)
    {
        log::error!("couldn't keep it in the Keychain: {err}");
    }
    let _ = open.reply.send(answer);
    for view in app_state::app_views(cx) {
        view.update(cx, |view, cx| view.dismiss_ask(id, cx));
    }
}

/// Answers kept in the login Keychain, by machine and question.
mod keychain {
    use security_framework::passwords::{get_generic_password, set_generic_password};

    const SERVICE: &str = "artty ssh login";

    fn account(name: &str, prompt: &str) -> String {
        format!("{name}: {prompt}")
    }

    pub fn get(name: &str, prompt: &str) -> Option<String> {
        let bytes = get_generic_password(SERVICE, &account(name, prompt)).ok()?;
        String::from_utf8(bytes).ok()
    }

    pub fn set(name: &str, prompt: &str, answer: &str) -> Result<(), String> {
        set_generic_password(SERVICE, &account(name, prompt), answer.as_bytes())
            .map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(prompt: &str, kind: &str) -> AskpassRequest {
        AskpassRequest {
            token: String::new(),
            name: "devbox".into(),
            key: 1,
            prompt: prompt.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn questions_by_what_they_want() {
        let trust = "The authenticity of host 'devbox' can't be established.\n\
                     Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        assert_eq!(kind(&request(trust, "")), Kind::YesNo);
        assert_eq!(kind(&request("me@devbox's password: ", "")), Kind::Secret);
        assert_eq!(kind(&request("Verification code: ", "")), Kind::Secret);
        assert_eq!(kind(&request("Allow use of key?", "confirm")), Kind::YesNo);
        assert_eq!(kind(&request("Confirm user presence for key", "none")), Kind::Notice);
    }
}
