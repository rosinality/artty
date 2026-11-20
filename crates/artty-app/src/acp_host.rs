//! `artty acp AGENT [--resume ID]`: an agent run without its terminal UI,
//! through its Agent Client Protocol adapter (JSON-RPC over the adapter's
//! stdin and stdout), in a tab of its own. What the agent does comes as
//! data: its messages, its tools' use, its permission requests, a turn's
//! end (or its being cancelled). This tells the artty server what the
//! agent's hooks would (`artty status --hook`), so asks, threads, edits and
//! permission requests work as for any agent; the agent's own hooks are
//! told to keep quiet (`ARTTY_ACP_HOST`), so nothing is said twice.
//!
//! The tab shows the conversation and takes a message typed there (or an
//! ask, typed in as a paste). Esc interrupts; ^T hands the conversation to
//! the agent's terminal UI (`claude --resume`) in the same tab, and back
//! once that's quit: one of the two at a time, never both.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use artty_core::control::{AgentState, Decision, HookEvent, Request, Response};
use serde_json::{Value, json};

/// Set for the adapter (and so the agent): its hooks don't report to artty,
/// this does.
pub const HOST_ENV: &str = "ARTTY_ACP_HOST";

/// The agent as artty names it: not its terminal UI's name (restoring a
/// tab resumes it here, not there).
fn reported_name(agent: &str) -> String {
    format!("{agent}-acp")
}

/// The command for the agent's terminal UI, resuming a conversation.
fn terminal_ui(agent: &str, session: &str) -> Option<Vec<String>> {
    match agent {
        "claude" => Some(vec!["claude".into(), "--resume".into(), session.into()]),
        _ => None,
    }
}

enum Event {
    /// A message from the adapter (of this start: see `Host::started`).
    Rpc(u64, Value),
    /// The adapter exited (or closed its output).
    AdapterGone(u64),
    Keys(Vec<u8>),
    /// A permission request (the adapter's request id) put to artty, decided
    /// there, or not (`None`: dismissed, expired, artty not running).
    Decided(Value, Option<Decision>),
}

/// What a request to the adapter was, for its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    Initialize,
    NewSession,
    LoadSession,
    Prompt,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Starting the adapter, or its session.
    Starting,
    /// Loading a conversation: the adapter replays it, already shown.
    Loading,
    Idle,
    /// A turn: from a message sent (`true`), or the agent's own (a
    /// background task finished).
    Working(bool),
}

struct Adapter {
    child: Child,
    stdin: ChildStdin,
    next_id: u64,
    asked: HashMap<u64, Asked>,
}

impl Adapter {
    fn start(command: &str, events: Sender<Event>, started: u64) -> Result<Self, String> {
        let stderr =
            std::fs::File::create(log_path()).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .env(HOST_ENV, "1")
            // The login shell's PATH (an adapter run by npx, node from a
            // version manager): the app, started from the Dock, has only
            // macOS's.
            .env("PATH", artty_core::tool_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("{command}: {err}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && events.send(Event::Rpc(started, value)).is_err()
                {
                    return;
                }
            }
            let _ = events.send(Event::AdapterGone(started));
        });
        Ok(Self { child, stdin, next_id: 0, asked: HashMap::new() })
    }

    fn write(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let _ = self.stdin.write_all(line.as_bytes());
        let _ = self.stdin.flush();
    }

    fn request(&mut self, method: &str, params: Value, asked: Asked) {
        self.next_id += 1;
        self.asked.insert(self.next_id, asked);
        let id = self.next_id;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn answer(&mut self, id: &Value, result: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn refuse(&mut self, id: &Value, message: &str) {
        let error = json!({ "code": -32601, "message": message });
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The terminal, raw while this runs (restored when dropped).
struct Raw(Option<libc::termios>);

impl Raw {
    fn on() -> Self {
        // SAFETY: tcgetattr/tcsetattr on stdin with a valid termios.
        unsafe {
            let mut old: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut old) != 0 {
                return Self(None);
            }
            let mut raw = old;
            libc::cfmakeraw(&mut raw);
            // (Output as usual: \n is a new line.)
            raw.c_oflag |= libc::OPOST;
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            // Pastes (and asks) come bracketed.
            print!("\x1b[?2004h");
            let _ = std::io::stdout().flush();
            Self(Some(old))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(old) = self.0 {
            print!("\x1b[?2004l");
            let _ = std::io::stdout().flush();
            // SAFETY: restoring the attributes read in `on`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
        }
    }
}

/// Keys typed, read off the main loop (paused while the agent's terminal UI
/// has the tab).
fn read_keys(events: Sender<Event>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut fds, 1, 100) };
            if ready <= 0 || paused.load(Ordering::SeqCst) {
                continue;
            }
            match std::io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if events.send(Event::Keys(buf[..n].to_vec())).is_err() {
                        return;
                    }
                },
            }
        }
    });
}

/// What's told to the artty server, in order, off the main loop.
fn reporter() -> Option<Sender<Request>> {
    let tab = crate::cli::own_tab()?;
    let (tx, rx) = mpsc::channel::<Request>();
    std::thread::spawn(move || {
        for mut request in rx {
            if let Request::Status { tab: t, .. } = &mut request {
                *t = Some(tab);
            }
            let _ = crate::cli::send(&request, Some(Duration::from_secs(10)));
        }
    });
    Some(tx)
}

/// A permission request pending: the adapter's request id, its options,
/// and the connection it's put to artty on (closed when answered here).
struct Permission {
    id: Value,
    title: String,
    options: Vec<Value>,
    asked: Option<UnixStream>,
}

struct Host {
    agent: String,
    command: String,
    events: Sender<Event>,
    adapter: Option<Adapter>,
    /// How many times the adapter's been started (each hand-over back):
    /// what an earlier one said is stale.
    started: u64,
    session: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    /// Its permission mode (`default`, `acceptEdits`, `plan`, ...), else
    /// the agent's own setting.
    mode: Option<String>,
    phase: Phase,
    report: Option<Sender<Request>>,
    /// The message being typed, and a paste coming in.
    input: String,
    paste: Option<Vec<u8>>,
    /// What a read ended in the middle of (an escape, a paste's end).
    partial: Vec<u8>,
    queued: VecDeque<String>,
    /// The turn's last message (its id, its text), and what of it isn't
    /// shown yet (a line at a time).
    reply: (Option<String>, String),
    unshown: String,
    /// Tools in use: name and input, by call id.
    tools: HashMap<String, (String, Value, String)>,
    permission: Option<Permission>,
    /// The conversation's title, as the agent named it (set again after a
    /// hand-over: its terminal UI sets its own).
    title: Option<String>,
    paused: Arc<AtomicBool>,
    quit: bool,
    /// The adapter failed (the tab's kept open to say why).
    failed: bool,
}

pub fn run(args: &[String]) -> Result<i32, String> {
    let mut agent = "claude".to_owned();
    let mut resume = None;
    let mut model = None;
    let mut mode = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--resume" => {
                resume = Some(rest.next().ok_or("--resume: which conversation?")?.clone())
            },
            "--model" => model = Some(rest.next().ok_or("--model: which?")?.clone()),
            "--mode" => mode = Some(rest.next().ok_or("--mode: which?")?.clone()),
            "-h" | "--help" => {
                println!(
                    "artty acp [AGENT] [--resume ID] [--model M] [--mode MODE]\n  runs AGENT (claude) through \
                     its Agent Client Protocol adapter, in this tab:\n  Enter sends, Esc \
                     interrupts, ^T opens the conversation in its terminal UI, ^D quits.\n  The \
                     adapter's command is the `acp` setting (acp.AGENT)."
                );
                return Ok(0);
            },
            name if !name.starts_with('-') => agent = name.to_owned(),
            other => return Err(format!("acp: unknown option {other}")),
        }
    }
    let (config, _) = artty_core::Config::load();
    let command = config
        .acp
        .get(&agent)
        .cloned()
        .ok_or_else(|| format!("acp: no adapter for {agent} (the acp.{agent} setting)"))?;
    let (events, rx) = mpsc::channel();
    let paused = Arc::new(AtomicBool::new(false));
    let _raw = Raw::on();
    read_keys(events.clone(), paused.clone());
    let mut host = Host {
        agent,
        command,
        events,
        adapter: None,
        started: 0,
        session: None,
        resume,
        model,
        mode,
        phase: Phase::Starting,
        report: reporter(),
        input: String::new(),
        paste: None,
        partial: Vec::new(),
        queued: VecDeque::new(),
        reply: (None, String::new()),
        unshown: String::new(),
        tools: HashMap::new(),
        permission: None,
        title: None,
        paused,
        quit: false,
        failed: false,
    };
    host.start()?;
    host.main(&rx);
    if let Some(adapter) = host.adapter.take() {
        adapter.stop();
    }
    let log = log_path();
    if host.failed {
        // Kept open, to be read.
        let tail = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = tail.lines().collect();
        for line in &lines[lines.len().saturating_sub(15)..] {
            host.say(&format!("\x1b[2m{line}\x1b[0m"));
        }
        host.say(&format!(
            "\x1b[2m(the adapter's log: {}) · a key closes this\x1b[0m",
            log.display()
        ));
        host.status(AgentState::Idle, None, Some(hook("SessionEnd")));
        while let Ok(event) = rx.recv() {
            if matches!(event, Event::Keys(_)) {
                break;
            }
        }
    } else {
        let _ = std::fs::remove_file(&log);
    }
    host.status(AgentState::Idle, None, Some(hook("SessionEnd")));
    // (Let the reporter finish.)
    drop(host.report.take());
    std::thread::sleep(Duration::from_millis(200));
    print!("\r\n");
    Ok(0)
}

/// Where the adapter's own messages go (not into the tab).
fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("artty-acp-{}.log", std::process::id()))
}

fn hook(event: &str) -> HookEvent {
    HookEvent { event: event.to_owned(), ..Default::default() }
}

impl Host {
    fn start(&mut self) -> Result<(), String> {
        self.started += 1;
        let mut adapter = Adapter::start(&self.command, self.events.clone(), self.started)?;
        let capabilities =
            json!({ "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false });
        adapter.request(
            "initialize",
            json!({ "protocolVersion": 1, "clientCapabilities": capabilities }),
            Asked::Initialize,
        );
        self.adapter = Some(adapter);
        self.phase = Phase::Starting;
        self.say(&format!("\x1b[2mstarting {} …\x1b[0m", self.agent));
        Ok(())
    }

    fn main(&mut self, rx: &Receiver<Event>) {
        while !self.quit {
            let Ok(event) = rx.recv() else { break };
            match event {
                Event::Rpc(started, _) | Event::AdapterGone(started) if started != self.started => {
                },
                Event::Rpc(_, message) => self.rpc(message),
                Event::AdapterGone(_) => {
                    if self.adapter.is_some() {
                        self.say("\x1b[31mthe agent's adapter exited\x1b[0m");
                        if matches!(self.phase, Phase::Working(_)) {
                            self.interrupted("ended");
                        }
                        if let Some(adapter) = self.adapter.take() {
                            adapter.stop();
                        }
                        self.failed = true;
                        self.quit = true;
                    }
                },
                Event::Keys(keys) => self.keys(&keys),
                Event::Decided(id, decision) => self.decided(id, decision),
            }
        }
    }

    // --- What's shown -----------------------------------------------------

    /// A line above the one being typed.
    fn say(&mut self, line: &str) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\r\x1b[2K{}\r\n", line.replace('\n', "\r\n"));
        self.prompt_line();
    }

    fn prompt_line(&self) {
        let mut out = std::io::stdout();
        let mark = match self.phase {
            Phase::Working(_) if self.permission.is_none() => "\x1b[2m…\x1b[0m ",
            _ if self.permission.is_some() => "\x1b[33m?\x1b[0m ",
            _ => "\x1b[1m›\x1b[0m ",
        };
        let shown = self.input.replace('\n', "⏎");
        let _ = write!(out, "\r\x1b[2K{mark}{shown}");
        let _ = out.flush();
    }

    /// The reply so far, its complete lines shown.
    fn show_reply(&mut self, all: bool) {
        while let Some(at) = self.unshown.find('\n') {
            let line: String = self.unshown.drain(..=at).collect();
            self.say(line.trim_end_matches('\n'));
        }
        if all && !self.unshown.is_empty() {
            let line = std::mem::take(&mut self.unshown);
            self.say(&line);
        }
    }

    // --- Told to artty ----------------------------------------------------

    fn status(&self, state: AgentState, message: Option<String>, hook: Option<HookEvent>) {
        let Some(report) = &self.report else { return };
        let _ = report.send(Request::Status {
            tab: None,
            state,
            message,
            agent_session: self.session.clone(),
            agent: Some(reported_name(&self.agent)),
            hook,
        });
    }

    // --- Turns ------------------------------------------------------------

    fn send_prompt(&mut self, text: String) {
        let Some(session) = self.session.clone() else { return };
        self.say(&format!("\x1b[1m›\x1b[0m {text}"));
        self.reply = (None, String::new());
        self.phase = Phase::Working(true);
        self.status(
            AgentState::Working,
            None,
            Some(HookEvent { prompt: Some(text.clone()), ..hook("UserPromptSubmit") }),
        );
        let prompt = json!([{ "type": "text", "text": text }]);
        if let Some(adapter) = &mut self.adapter {
            adapter.request(
                "session/prompt",
                json!({ "sessionId": session, "prompt": prompt }),
                Asked::Prompt,
            );
        }
        self.prompt_line();
    }

    fn turn_ended(&mut self) {
        self.show_reply(true);
        self.phase = Phase::Idle;
        let reply = std::mem::take(&mut self.reply.1);
        self.status(AgentState::Done, None, Some(HookEvent { reply: Some(reply), ..hook("Stop") }));
        self.next_queued();
    }

    fn interrupted(&mut self, how: &str) {
        self.show_reply(true);
        self.say("\x1b[2m(interrupted)\x1b[0m");
        self.phase = Phase::Idle;
        self.permission = None;
        self.status(
            AgentState::Idle,
            None,
            Some(HookEvent { reply: Some(how.to_owned()), ..hook("Interrupt") }),
        );
        self.next_queued();
    }

    fn next_queued(&mut self) {
        if let Some(text) = self.queued.pop_front() {
            self.send_prompt(text);
        } else {
            self.prompt_line();
        }
    }

    fn cancel(&mut self) {
        let Some(session) = self.session.clone() else { return };
        if let Some(permission) = self.permission.take() {
            self.answer_permission(permission, None);
        }
        if let Some(adapter) = &mut self.adapter {
            adapter.notify("session/cancel", json!({ "sessionId": session }));
        }
        // A turn of the agent's own has no answer to say it's over: over now.
        if self.phase == Phase::Working(false) {
            self.interrupted("interrupted");
        }
    }

    // --- The adapter ------------------------------------------------------

    fn rpc(&mut self, message: Value) {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        match (method, message.get("id")) {
            // An answer to a request of ours.
            (None, Some(id)) => {
                let asked = id
                    .as_u64()
                    .and_then(|id| self.adapter.as_mut()?.asked.remove(&id))
                    .unwrap_or(Asked::Other);
                self.answered(asked, &message);
            },
            (Some(method), Some(id)) => self.requested(&method, id.clone(), &message["params"]),
            (Some(method), None) if method == "session/update" => {
                self.update(&message["params"]["update"])
            },
            // (Others, as the account's sign-in status: not shown, not kept.)
            _ => {},
        }
    }

    fn answered(&mut self, asked: Asked, message: &Value) {
        if let Some(error) = message.get("error") {
            let text = error.get("message").and_then(Value::as_str).unwrap_or("failed");
            self.say(&format!("\x1b[31m{asked:?}: {text}\x1b[0m"));
            match asked {
                Asked::Initialize | Asked::NewSession | Asked::LoadSession => {
                    self.failed = true;
                    self.quit = true;
                },
                Asked::Prompt => self.interrupted("failed"),
                Asked::Other => {},
            }
            return;
        }
        let result = &message["result"];
        let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        match asked {
            Asked::Initialize => {
                let Some(adapter) = &mut self.adapter else { return };
                match self.resume.clone().or(self.session.clone()) {
                    Some(session) => {
                        self.phase = Phase::Loading;
                        adapter.request(
                            "session/load",
                            json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }),
                            Asked::LoadSession,
                        );
                    },
                    None => adapter.request(
                        "session/new",
                        json!({ "cwd": cwd, "mcpServers": [] }),
                        Asked::NewSession,
                    ),
                }
            },
            Asked::NewSession | Asked::LoadSession => {
                let session = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(self.resume.take())
                    .or(self.session.clone());
                self.session = session;
                self.phase = Phase::Idle;
                if let (Some(model), Some(adapter), Some(session)) =
                    (self.model.clone(), &mut self.adapter, &self.session)
                {
                    adapter.request(
                        "session/set_config_option",
                        json!({ "sessionId": session, "configId": "model", "value": model }),
                        Asked::Other,
                    );
                }
                if let (Some(mode), Some(adapter), Some(session)) =
                    (self.mode.clone(), &mut self.adapter, &self.session)
                {
                    adapter.request(
                        "session/set_mode",
                        json!({ "sessionId": session, "modeId": mode }),
                        Asked::Other,
                    );
                }
                let source = if asked == Asked::NewSession { "startup" } else { "resume" };
                self.status(
                    AgentState::Idle,
                    None,
                    Some(HookEvent { source: Some(source.into()), ..hook("SessionStart") }),
                );
                self.say(&format!(
                    "\x1b[2m{} · conversation {} · Enter sends · Esc interrupts · ^T opens it in \
                     its terminal UI · ^D quits\x1b[0m",
                    self.agent,
                    self.session.as_deref().unwrap_or("?")
                ));
                self.next_queued();
            },
            Asked::Prompt => {
                let reason = result.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                if reason == "cancelled" {
                    self.interrupted("interrupted");
                } else {
                    self.turn_ended();
                }
            },
            Asked::Other => {},
        }
    }

    fn requested(&mut self, method: &str, id: Value, params: &Value) {
        if method != "session/request_permission" {
            if let Some(adapter) = &mut self.adapter {
                adapter.refuse(&id, "not supported by artty acp");
            }
            return;
        }
        let call = &params["toolCall"];
        let call_id = call.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
        let known = self.tools.get(call_id).cloned();
        let tool_name = call
            .pointer("/_meta/claudeCode/toolName")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(known.as_ref().map(|k| k.0.clone()))
            .unwrap_or_else(|| "a tool".to_owned());
        let tool_input = call
            .get("rawInput")
            .cloned()
            .or(known.as_ref().map(|k| k.1.clone()))
            .unwrap_or(Value::Null);
        let title = call
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(known.map(|k| k.2))
            .unwrap_or_else(|| tool_name.clone());
        let options = params.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
        self.show_reply(true);
        self.say(&format!("\x1b[33m? {title}: allow? y / n (or in artty's Agents view)\x1b[0m"));
        // Put to artty, on a connection of its own (closed if answered here).
        let asked = self.report.as_ref().and_then(|_| {
            let path = crate::cli::socket().ok()?;
            let stream = UnixStream::connect(path).ok()?;
            let request = Request::Approve {
                tab: crate::cli::own_tab(),
                agent: Some(reported_name(&self.agent)),
                tool_name: tool_name.clone(),
                tool_input,
                suggestions: Vec::new(),
            };
            let mut line = serde_json::to_string(&request).ok()?;
            line.push('\n');
            (&stream).write_all(line.as_bytes()).ok()?;
            let reader = stream.try_clone().ok()?;
            let events = self.events.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                let mut reply = String::new();
                let _ = BufReader::new(reader).read_line(&mut reply);
                let decision = match serde_json::from_str::<Response>(&reply) {
                    Ok(Response::Decision { decision }) => decision,
                    _ => None,
                };
                let _ = events.send(Event::Decided(id, decision));
            });
            Some(stream)
        });
        self.permission = Some(Permission { id, title: title.clone(), options, asked });
        self.status(AgentState::Waiting, Some(format!("Approve {title}?")), None);
        self.prompt_line();
    }

    /// Answer the adapter's permission request: an option of `kind`
    /// (`allow_once`, `reject_once`, ...), or cancelled (`None`).
    fn answer_permission(&mut self, permission: Permission, kind: Option<&str>) {
        if let Some(stream) = &permission.asked {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let option = kind.and_then(|kind| {
            permission
                .options
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|o| o.get("optionId").cloned())
        });
        let outcome = match option {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        if let Some(adapter) = &mut self.adapter {
            adapter.answer(&permission.id, json!({ "outcome": outcome }));
        }
        let shown = match kind {
            Some(k) if k.starts_with("allow") => "allowed",
            Some(_) => "denied",
            None => "cancelled",
        };
        self.say(&format!("\x1b[2m  {} — {shown}\x1b[0m", permission.title));
        if matches!(self.phase, Phase::Working(_)) {
            self.status(AgentState::Working, None, None);
        }
    }

    fn decided(&mut self, id: Value, decision: Option<Decision>) {
        let Some(decision) = decision else {
            // Not decided in artty: it's for this tab (y / n), still.
            return;
        };
        if self.permission.as_ref().is_none_or(|p| p.id != id) {
            return;
        }
        let permission = self.permission.take().unwrap();
        let kind = match (decision.allow, decision.permissions.is_empty()) {
            (true, true) => "allow_once",
            (true, false) => "allow_always",
            (false, _) => "reject_once",
        };
        self.answer_permission(permission, Some(kind));
        self.prompt_line();
    }

    fn update(&mut self, update: &Value) {
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();
        // A conversation loading is replayed: shown already, or before.
        if self.phase == Phase::Loading || self.phase == Phase::Starting {
            return;
        }
        let text = |key: &str| update.get(key).and_then(Value::as_str).map(str::to_owned);
        // The agent at work with no message sent: its own turn.
        if self.phase == Phase::Idle
            && matches!(kind, "agent_message_chunk" | "tool_call" | "agent_thought_chunk")
        {
            self.phase = Phase::Working(false);
            self.reply = (None, String::new());
            let notice = "<task-notification>\n<summary>The agent went on by itself (a \
                          background task finished)</summary>\n</task-notification>";
            self.status(
                AgentState::Working,
                None,
                Some(HookEvent { prompt: Some(notice.to_owned()), ..hook("UserPromptSubmit") }),
            );
            self.say("\x1b[2m(the agent goes on by itself)\x1b[0m");
        }
        match kind {
            "agent_message_chunk" => {
                let chunk = update.pointer("/content/text").and_then(Value::as_str).unwrap_or("");
                let id = text("messageId");
                if id != self.reply.0 {
                    // A new message: the turn's last so far.
                    self.show_reply(true);
                    self.reply = (id, String::new());
                }
                self.reply.1.push_str(chunk);
                self.unshown.push_str(chunk);
                self.show_reply(false);
            },
            "tool_call" | "tool_call_update" => {
                let Some(call) = text("toolCallId") else { return };
                let entry = self
                    .tools
                    .entry(call.clone())
                    .or_insert_with(|| (String::new(), Value::Null, String::new()));
                if let Some(name) =
                    update.pointer("/_meta/claudeCode/toolName").and_then(Value::as_str)
                {
                    entry.0 = name.to_owned();
                }
                if let Some(input) = update.get("rawInput").filter(|v| !v.is_null()) {
                    entry.1 = input.clone();
                }
                if let Some(title) = text("title").filter(|t| !t.is_empty()) {
                    entry.2 = title;
                }
                let status = text("status");
                if matches!(status.as_deref(), Some("completed" | "failed")) {
                    let (name, input, title) = self.tools.remove(&call).unwrap_or_default();
                    let failed = status.as_deref() == Some("failed");
                    self.show_reply(true);
                    let mark = if failed { "\x1b[31m✕\x1b[0m" } else { "\x1b[2m⏺\x1b[0m" };
                    self.say(&format!("{mark} \x1b[2m{title}\x1b[0m"));
                    if !failed && !name.is_empty() {
                        // As Claude Code's PostToolUse: files changed, a
                        // permission request answered.
                        self.status(
                            AgentState::Working,
                            None,
                            Some(HookEvent {
                                tool_name: Some(name),
                                tool_input: Some(input),
                                ..hook("PostToolUse")
                            }),
                        );
                    }
                }
            },
            "session_info_update" => {
                if let Some(title) = text("title") {
                    print!("\x1b]0;{title}\x07");
                    let _ = std::io::stdout().flush();
                    self.title = Some(title);
                }
            },
            // The end of a turn of the agent's own: its cost, once it's done.
            "usage_update"
                if self.phase == Phase::Working(false) && update.get("cost").is_some() =>
            {
                self.turn_ended()
            },
            _ => {},
        }
    }

    // --- Keys -------------------------------------------------------------

    fn keys(&mut self, read: &[u8]) {
        // (A key alone: Esc, as the user pressed it, not an escape's start.)
        let alone = read == b"\x1b" && self.partial.is_empty();
        let mut keys = std::mem::take(&mut self.partial);
        keys.extend_from_slice(read);
        let mut at = 0;
        while at < keys.len() {
            let rest = &keys[at..];
            if let Some(paste) = &mut self.paste {
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        let paste = self.paste.take().unwrap();
                        self.input.push_str(&String::from_utf8_lossy(&paste).replace('\r', "\n"));
                        at += end + 6;
                        self.prompt_line();
                    },
                    None => {
                        // (The end may come split, in the next read.)
                        let keep = (1..PASTE_END.len())
                            .rev()
                            .find(|&n| rest.ends_with(&PASTE_END[..n]))
                            .unwrap_or(0);
                        paste.extend_from_slice(&rest[..rest.len() - keep]);
                        self.partial = rest[rest.len() - keep..].to_vec();
                        at = keys.len();
                    },
                }
                continue;
            }
            // An escape cut short by the read: the rest comes next.
            if rest[0] == 0x1b && !alone && unfinished_escape(rest) {
                self.partial = rest.to_vec();
                break;
            }
            if rest.starts_with(b"\x1b[200~") {
                self.paste = Some(Vec::new());
                at += 6;
                continue;
            }
            match rest[0] {
                // Esc alone: interrupt. (Other escapes, as arrows: ignored.)
                0x1b => {
                    if alone {
                        if matches!(self.phase, Phase::Working(_)) {
                            self.cancel();
                        }
                        at += 1;
                    } else {
                        // Skip the sequence: ESC [ ... final, or ESC x.
                        let len = if rest[1] == b'[' || rest[1] == b'O' {
                            rest[2..]
                                .iter()
                                .position(|b| (0x40..=0x7e).contains(b))
                                .map_or(rest.len(), |p| p + 3)
                        } else {
                            2
                        };
                        at += len;
                    }
                    continue;
                },
                b'\r' | b'\n' => self.submit(),
                // ^C: interrupt, else clear, else quit.
                0x03 => {
                    if matches!(self.phase, Phase::Working(_)) {
                        self.cancel();
                    } else if !self.input.is_empty() {
                        self.input.clear();
                    } else {
                        self.quit = true;
                    }
                },
                0x04 if self.input.is_empty() => self.quit = true,
                0x14 => self.hand_over(),
                0x15 => self.input.clear(),
                0x7f | 0x08 => {
                    self.input.pop();
                },
                b'y' | b'n' if self.permission.is_some() && self.input.is_empty() => {
                    let permission = self.permission.take().unwrap();
                    let kind = if rest[0] == b'y' { "allow_once" } else { "reject_once" };
                    self.answer_permission(permission, Some(kind));
                },
                byte if byte >= 0x20 || byte == b'\t' => {
                    // A character (UTF-8: its whole sequence).
                    let len = match byte {
                        0xf0.. => 4,
                        0xe0.. => 3,
                        0xc0.. => 2,
                        _ => 1,
                    }
                    .min(rest.len());
                    self.input.push_str(&String::from_utf8_lossy(&rest[..len]));
                    at += len;
                    self.prompt_line();
                    continue;
                },
                _ => {},
            }
            at += 1;
            self.prompt_line();
        }
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input).trim().to_owned();
        if text.is_empty() {
            return;
        }
        match self.phase {
            Phase::Idle => self.send_prompt(text),
            _ => {
                self.say(&format!("\x1b[2m(after this turn) {text}\x1b[0m"));
                self.queued.push_back(text);
            },
        }
    }

    /// ^T: the conversation to the agent's terminal UI in this tab, and
    /// back here once that's quit.
    fn hand_over(&mut self) {
        let (Phase::Idle, Some(session)) = (self.phase, self.session.clone()) else {
            self.say("\x1b[2m(once the turn is over)\x1b[0m");
            return;
        };
        let Some(command) = terminal_ui(&self.agent, &session) else {
            self.say(&format!("\x1b[2m(no terminal UI known for {})\x1b[0m", self.agent));
            return;
        };
        self.say(&format!("\x1b[2mopening it in {} (quit it to come back) …\x1b[0m", command[0]));
        // One at a time: the adapter goes first.
        if let Some(adapter) = self.adapter.take() {
            adapter.stop();
        }
        self.paused.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
        let mut ran = {
            // (Its own terminal modes, and its own hooks report.)
            let _cooked = Cooked::now();
            Command::new(&command[0]).args(&command[1..]).env_remove(HOST_ENV).status()
        };
        self.paused.store(false, Ordering::SeqCst);
        if let Err(err) = &mut ran {
            self.say(&format!("\x1b[31m{}: {err}\x1b[0m", command[0]));
        }
        self.say(&format!("\x1b[2mback from {} …\x1b[0m", command[0]));
        if let Some(title) = &self.title {
            print!("\x1b]0;{title}\x07");
        }
        if let Err(err) = self.start() {
            self.say(&format!("\x1b[31m{err}\x1b[0m"));
            self.failed = true;
            self.quit = true;
        }
    }
}

/// The terminal as it was before `artty acp` (for the agent's terminal UI),
/// raw again when dropped.
struct Cooked(Option<libc::termios>);

impl Cooked {
    fn now() -> Self {
        // SAFETY: tcgetattr on stdin with a valid termios.
        unsafe {
            let mut raw: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut raw) != 0 {
                return Self(None);
            }
            let mut cooked = raw;
            cooked.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN;
            cooked.c_iflag |= libc::ICRNL | libc::IXON;
            libc::tcsetattr(0, libc::TCSANOW, &cooked);
            print!("\x1b[?2004l\r\n");
            let _ = std::io::stdout().flush();
            Self(Some(raw))
        }
    }
}

impl Drop for Cooked {
    fn drop(&mut self) {
        if let Some(raw) = self.0 {
            // SAFETY: restoring the attributes read in `now`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
            print!("\x1b[?2004h");
            let _ = std::io::stdout().flush();
        }
    }
}

const PASTE_END: &[u8] = b"\x1b[201~";

/// Whether `rest` (from an ESC) ends before its escape does.
fn unfinished_escape(rest: &[u8]) -> bool {
    match rest {
        [_] => true,
        [_, b'[' | b'O', tail @ ..] => !tail.iter().any(|b| (0x40..=0x7e).contains(b)),
        _ => false,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
