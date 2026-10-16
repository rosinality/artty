//! `artty <command>`: control a running artty from the command line. Agents
//! inside a tab use this to read other tabs, run commands, and report status.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use artty_core::SessionId;
use artty_core::control::{
    AgentState, ReadSource, Request, Response, SESSION_ENV, SOCKET_ENV, SplitDirection, live_sockets,
};
use serde_json::{Value, json};

pub const COMMANDS: &[&str] = &[
    "list", "read", "send", "new", "run", "notify", "status", "focus", "close", "rename", "settings", "hooks",
    "shell-integration",
];

const HELP: &str = "\
artty — a terminal for working with AI agents

usage:
  artty [-e command [args...]]           open the app (optionally running a command)

control a running artty. Ids name panes (a tab has one or more); inside
artty, your own pane is found automatically:
  artty list [--json]                    list panes: id, window, tab, status, title
  artty read [--tab N] [--lines N | --screen | --last]
                                         print a tab's text: the last N lines
                                         (default 100), the screen, or the last
                                         command's output (needs shell integration)
  artty send [--tab N] [--no-enter] TEXT type TEXT into a tab, then Enter
  artty run [--tab N] [--timeout S] -- COMMAND
                                         run COMMAND in a tab's shell, print its
                                         output, and exit with its exit code
  artty new [--split right|down] [--tab N] [--cwd DIR] [--title T]
            [--background] [-- COMMAND...]
                                         open a tab, or split pane N (default:
                                         your own, or the focused one); prints
                                         the new pane's id
  artty focus N | artty close N
  artty rename [--tab N] TITLE           name the tab holding pane N (empty
                                         title: back to automatic)
  artty notify [--tab N] [--title T] BODY
  artty status [--tab N] working|waiting|done|idle [--agent NAME]
             [--message M | --hook]      report an agent's state (for hooks);
                                         --hook reads the agent's hook event
                                         from stdin (message and session id)
  artty settings                         open the Settings window
  artty shell-integration zsh|bash       print the shell integration script, to
                                         source from your startup file where
                                         artty can't set it up (e.g. over ssh)
  artty hooks [claude|codex|gemini]      print an agent's status hooks
  artty hooks install [AGENT...]         add them to the agent's settings
                                         (default: every installed agent), so
                                         artty shows exact status and resumes
                                         the right conversation
";

pub fn main(args: Vec<String>) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("artty: {message}");
            1
        }
    }
}

struct Args {
    args: std::vec::IntoIter<String>,
}

impl Args {
    fn next(&mut self) -> Option<String> {
        self.args.next()
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.next().ok_or_else(|| format!("{flag} needs a value"))
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let value = self.value(flag)?;
        value.parse().map_err(|_| format!("{flag}: not a number: {value}"))
    }
}

fn own_tab() -> Option<SessionId> {
    std::env::var(SESSION_ENV).ok()?.parse().ok()
}

fn run(args: Vec<String>) -> Result<i32, String> {
    let mut args = Args { args: args.into_iter() };
    let command = args.next().unwrap_or_default();
    let mut tab: Option<SessionId> = None;
    let mut rest = Vec::new();
    let mut flags = std::collections::HashMap::<&str, String>::new();
    let mut switches = std::collections::HashSet::<&str>::new();

    // Shared flag parsing; `--` ends flags.
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tab" => tab = Some(args.number("--tab")?),
            "--lines" => _ = flags.insert("lines", args.value("--lines")?),
            "--timeout" => _ = flags.insert("timeout", args.value("--timeout")?),
            "--cwd" => _ = flags.insert("cwd", args.value("--cwd")?),
            "--title" => _ = flags.insert("title", args.value("--title")?),
            "--message" => _ = flags.insert("message", args.value("--message")?),
            "--split" => _ = flags.insert("split", args.value("--split")?),
            "--agent" => _ = flags.insert("agent", args.value("--agent")?),
            "--screen" | "--last" | "--no-enter" | "--background" | "--json" | "--hook" => {
                switches.insert(match arg.as_str() {
                    "--screen" => "screen",
                    "--last" => "last",
                    "--no-enter" => "no-enter",
                    "--background" => "background",
                    "--json" => "json",
                    _ => "hook",
                });
            }
            "--" => {
                rest.extend(args.args.by_ref());
                break;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(0);
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => rest.push(arg),
        }
    }

    let request = match command.as_str() {
        "list" => Request::List,
        "settings" => Request::Settings,
        "read" => {
            let source = if switches.contains("screen") {
                ReadSource::Screen
            } else if switches.contains("last") {
                ReadSource::LastCommand
            } else {
                let lines = flags.get("lines").map_or(Ok(100), |l| l.parse().map_err(|_| "--lines: not a number"))?;
                ReadSource::Scrollback { lines }
            };
            Request::Read { tab, source }
        }
        "send" => {
            if rest.is_empty() {
                return Err("send: nothing to send".into());
            }
            Request::Send { tab, text: rest.join(" "), enter: !switches.contains("no-enter") }
        }
        "run" => {
            if rest.is_empty() {
                return Err("run: missing command (artty run [--tab N] -- COMMAND)".into());
            }
            if tab.is_none() && own_tab().is_some() {
                return Err("run: pass --tab N; a tab can't run commands in itself".into());
            }
            let timeout_secs = flags.get("timeout").map(|t| t.parse().map_err(|_| "--timeout: not a number")).transpose()?;
            Request::Run { tab, command: rest.join(" "), timeout_secs }
        }
        "new" => {
            let split = match flags.get("split").map(String::as_str) {
                None => None,
                Some("right") => Some(SplitDirection::Right),
                Some("down") => Some(SplitDirection::Down),
                Some(other) => return Err(format!("--split: expected right or down, not {other}")),
            };
            Request::New {
                cwd: flags.get("cwd").map(PathBuf::from),
                command: rest,
                title: flags.get("title").cloned(),
                focus: !switches.contains("background"),
                split,
                target: split.and(tab.or_else(own_tab)),
            }
        }
        "focus" | "close" => {
            let id = rest.first().and_then(|id| id.parse().ok()).or(tab).ok_or(format!("{command}: which tab?"))?;
            if command == "focus" { Request::Focus { tab: id } } else { Request::Close { tab: id } }
        }
        "rename" => Request::Rename { tab: tab.or_else(own_tab), title: rest.join(" ") },
        "notify" => {
            if rest.is_empty() {
                return Err("notify: missing message".into());
            }
            Request::Notify { tab: tab.or_else(own_tab), title: flags.get("title").cloned(), body: rest.join(" ") }
        }
        "status" => {
            let state = match rest.first().map(String::as_str) {
                Some("working") => AgentState::Working,
                Some("waiting") => AgentState::Waiting,
                Some("done") => AgentState::Done,
                Some("idle") => AgentState::Idle,
                _ => return Err("status: expected working, waiting, done or idle".into()),
            };
            let (message, agent_session) = if switches.contains("hook") {
                hook_event()
            } else {
                (flags.get("message").cloned(), None)
            };
            Request::Status { tab: tab.or_else(own_tab), state, message, agent_session, agent: flags.get("agent").cloned() }
        }
        "hooks" => return hooks(&rest),
        "shell-integration" => {
            let shell = rest.first().map(String::as_str).unwrap_or_default();
            let script = artty_core::integration::script(shell)
                .ok_or("shell-integration: expected zsh or bash (fish has it built in)")?;
            print!("{script}");
            return Ok(0);
        }
        _ => {
            print!("{HELP}");
            return Ok(2);
        }
    };

    let timeout = match &request {
        Request::Run { timeout_secs, .. } => timeout_secs.map(|s| Duration::from_secs(s + 5)),
        _ => Some(Duration::from_secs(10)),
    };
    let response = send(&request, timeout)?;
    print_response(response, switches.contains("json"))
}

fn print_response(response: Response, json: bool) -> Result<i32, String> {
    match response {
        Response::Ok => Ok(0),
        Response::Error { message } => Err(message),
        Response::Tab { id } => {
            println!("{id}");
            Ok(0)
        }
        Response::Text { text } => {
            println!("{text}");
            Ok(0)
        }
        Response::Command { output, exit, truncated, .. } => {
            if truncated {
                eprintln!("artty: output truncated");
            }
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(exit.unwrap_or(0))
        }
        Response::Tabs { tabs } if json => {
            println!("{}", serde_json::to_string_pretty(&tabs).unwrap_or_default());
            Ok(0)
        }
        Response::Tabs { tabs } => {
            println!("    ID  WIN  TAB  STATUS   TITLE");
            for pane in tabs {
                let marker = if pane.active { "*" } else if pane.needs_attention { "!" } else { " " };
                let detail = pane.detail.map(|d| format!("  ({d})")).unwrap_or_default();
                println!(
                    "{marker} {:>4}  {:>3}  {:>3}  {:<8} {}{detail}",
                    pane.id, pane.window, pane.tab, pane.status, pane.title
                );
            }
            Ok(0)
        }
    }
}

/// The message and session id of an agent's hook event on stdin (Claude
/// Code, Codex and Gemini CLI all send `session_id`).
fn hook_event() -> (Option<String>, Option<String>) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return (None, None) };
    (hook_message(&event), event.get("session_id").and_then(Value::as_str).map(str::to_owned))
}

/// What to show for a hook event: its message, or for a permission request
/// (Codex), the tool's description.
fn hook_message(event: &Value) -> Option<String> {
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(message) = event.get("message").and_then(text) {
        return Some(message);
    }
    if event.get("hook_event_name").and_then(Value::as_str) == Some("PermissionRequest") {
        let description = event.pointer("/tool_input/description").and_then(text);
        let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or("a tool");
        return Some(description.unwrap_or_else(|| format!("Approve {tool}?")));
    }
    None
}

fn socket() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(SOCKET_ENV) {
        return Ok(PathBuf::from(path));
    }
    // Outside artty: use the most recently started instance.
    live_sockets().into_iter().next().ok_or_else(|| "artty is not running".to_owned())
}

fn send(request: &Request, timeout: Option<Duration>) -> Result<Response, String> {
    let path = socket()?;
    let mut stream = UnixStream::connect(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    stream.set_read_timeout(timeout).map_err(|err| err.to_string())?;
    let mut line = serde_json::to_string(request).map_err(|err| err.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|err| err.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).map_err(|err| match err.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "timed out".to_owned(),
        _ => err.to_string(),
    })?;
    serde_json::from_str(&reply).map_err(|err| format!("bad reply: {err}"))
}

/// An agent artty can install status hooks for.
struct AgentHooks {
    agent: &'static str,
    /// Hooks file, relative to the home directory; its `hooks` key is ours.
    file: &'static str,
    /// Hook event → the state it reports.
    events: &'static [(&'static str, &'static str)],
    /// The agent parses hook output as JSON (Gemini CLI).
    json_output: bool,
}

const AGENT_HOOKS: &[AgentHooks] = &[
    AgentHooks {
        agent: "claude",
        file: ".claude/settings.json",
        events: &[
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("PostToolUse", "working"),
            ("Notification", "waiting"),
            ("Stop", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: false,
    },
    AgentHooks {
        agent: "codex",
        file: ".codex/hooks.json",
        events: &[
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("PostToolUse", "working"),
            ("PermissionRequest", "waiting"),
            ("Stop", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: false,
    },
    AgentHooks {
        agent: "gemini",
        file: ".gemini/settings.json",
        events: &[
            ("SessionStart", "idle"),
            ("BeforeAgent", "working"),
            ("AfterTool", "working"),
            ("Notification", "waiting"),
            ("AfterAgent", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: true,
    },
];

impl AgentHooks {
    /// The hooks to add, keyed by event. They do nothing outside artty.
    fn entries(&self) -> Value {
        let command = |state: &str| {
            let report = format!("[ -n \"$ARTTY_SOCKET\" ] && artty status {state} --hook --agent {}", self.agent);
            if self.json_output { format!("{report} >/dev/null; echo '{{}}'") } else { format!("{report} || true") }
        };
        let mut hooks = serde_json::Map::new();
        for (event, state) in self.events {
            hooks.insert((*event).to_owned(), json!([{ "hooks": [{ "type": "command", "command": command(state) }] }]));
        }
        Value::Object(hooks)
    }

    fn path(&self) -> Result<PathBuf, String> {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        Ok(PathBuf::from(home).join(self.file))
    }

    /// Add (or upgrade) artty's hooks in the agent's file, backing it up
    /// first. Returns what happened.
    fn install(&self) -> Result<String, String> {
        let path = self.path()?;
        let name = path.display();
        let mut settings: Value = match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => json!({}),
            Ok(text) => serde_json::from_str(&text).map_err(|err| format!("{name}: {err}"))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(err) => return Err(format!("{name}: {err}")),
        };
        let root = settings.as_object_mut().ok_or(format!("{name} is not a JSON object"))?;
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks = hooks.as_object_mut().ok_or(format!("{name}: \"hooks\" is not an object"))?;

        // Replace hooks from earlier versions of artty, keep everything else.
        let mut changed = 0;
        for (event, entries) in self.entries().as_object().unwrap() {
            let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
            let list = list.as_array_mut().ok_or(format!("{name}: hooks.{event} is not a list"))?;
            let ours = |entry: &Value| entry.to_string().contains("artty status");
            let current: Vec<Value> = list.iter().filter(|e| ours(e)).cloned().collect();
            if current == *entries.as_array().unwrap() {
                continue;
            }
            list.retain(|entry| !ours(entry));
            list.extend(entries.as_array().unwrap().iter().cloned());
            changed += 1;
        }
        if changed == 0 {
            return Ok(format!("{}: hooks are up to date in {name}", self.agent));
        }

        let mut report = String::new();
        if path.exists() {
            let backup = path.with_extension("json.artty-backup");
            std::fs::copy(&path, &backup).map_err(|err| format!("backup failed: {err}"))?;
            report.push_str(&format!("{}: backed up {name} to {}\n", self.agent, backup.display()));
        } else if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        }
        let text = serde_json::to_string_pretty(&settings).map_err(|err| err.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|err| format!("{name}: {err}"))?;
        report.push_str(&format!("{}: updated hooks for {changed} events in {name}", self.agent));
        if self.agent == "codex" {
            report.push_str("\ncodex: approve the new hooks once with /hooks in Codex");
        }
        Ok(report)
    }

    /// Whether the agent seems to be installed (its config directory exists).
    fn present(&self) -> bool {
        self.path().ok().and_then(|p| p.parent().map(|dir| dir.is_dir())).unwrap_or(false)
    }
}

fn agent_hooks(name: &str) -> Result<&'static AgentHooks, String> {
    AGENT_HOOKS
        .iter()
        .find(|hooks| hooks.agent == name)
        .ok_or_else(|| format!("hooks: unknown agent {name} (claude, codex or gemini)"))
}

/// `artty hooks [AGENT]` prints hooks; `artty hooks install [AGENT...]`
/// installs them (default: every agent that's installed).
fn hooks(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("install") => {
            let names: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            println!("{}", install_agent_hooks(&names)?);
            Ok(0)
        }
        name => {
            let hooks = agent_hooks(name.unwrap_or("claude"))?;
            let snippet = json!({ "hooks": hooks.entries() });
            println!("{}", serde_json::to_string_pretty(&snippet).unwrap());
            eprintln!("Add these to ~/{}, or run `artty hooks install {}`.", hooks.file, hooks.agent);
            Ok(0)
        }
    }
}

/// Install hooks for the named agents, or for every installed agent.
pub fn install_agent_hooks(names: &[&str]) -> Result<String, String> {
    let selected: Vec<&AgentHooks> = if names.is_empty() {
        AGENT_HOOKS.iter().filter(|hooks| hooks.present()).collect()
    } else {
        names.iter().map(|name| agent_hooks(name)).collect::<Result<_, _>>()?
    };
    if selected.is_empty() {
        return Err("no supported agent found (~/.claude, ~/.codex or ~/.gemini)".into());
    }
    let reports: Vec<String> = selected.iter().map(|hooks| hooks.install().unwrap_or_else(|err| err)).collect();
    Ok(reports.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_guarded_and_named() {
        let claude = agent_hooks("claude").unwrap().entries();
        let command = claude["Notification"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, "[ -n \"$ARTTY_SOCKET\" ] && artty status waiting --hook --agent claude || true");
        let gemini = agent_hooks("gemini").unwrap().entries();
        let command = gemini["AfterAgent"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.ends_with(">/dev/null; echo '{}'"), "{command}");
        assert!(agent_hooks("codex").unwrap().entries()["PermissionRequest"].is_array());
    }

    #[test]
    fn hook_messages() {
        let codex = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                           "tool_input": {"command": "rm -rf x", "description": "Delete the build dir"}});
        assert_eq!(hook_message(&codex).as_deref(), Some("Delete the build dir"));
        let bare = json!({"hook_event_name": "PermissionRequest", "tool_name": "apply_patch"});
        assert_eq!(hook_message(&bare).as_deref(), Some("Approve apply_patch?"));
        let gemini = json!({"hook_event_name": "Notification", "message": "Allow shell?"});
        assert_eq!(hook_message(&gemini).as_deref(), Some("Allow shell?"));
        assert_eq!(hook_message(&json!({"hook_event_name": "Stop"})), None);
    }
}
