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

pub const COMMANDS: &[&str] = &["list", "read", "send", "new", "run", "notify", "status", "focus", "close", "hooks"];

const HELP: &str = "\
artty — a terminal for working with AI agents

usage:
  artty [-e command [args...]]           open the app (optionally running a command)

control a running artty. Ids name panes (a tab has one or more); inside
artty, your own pane is found automatically:
  artty list [--json]                    list panes: id, tab, status, title
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
  artty notify [--tab N] [--title T] BODY
  artty status [--tab N] working|waiting|done|idle [--message M | --hook]
                                         report an agent's state (for hooks);
                                         --hook reads a Claude Code hook event
                                         from stdin (message and session id)
  artty hooks [install]                  print (or add to ~/.claude/settings.json)
                                         Claude Code hooks that report status and
                                         let artty resume conversations
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
            Request::Status { tab: tab.or_else(own_tab), state, message, agent_session }
        }
        "hooks" => return hooks(rest.first().map(String::as_str)),
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
            println!("    ID  TAB  STATUS   TITLE");
            for pane in tabs {
                let marker = if pane.active { "*" } else if pane.needs_attention { "!" } else { " " };
                let detail = pane.detail.map(|d| format!("  ({d})")).unwrap_or_default();
                println!("{marker} {:>4}  {:>3}  {:<8} {}{detail}", pane.id, pane.tab, pane.status, pane.title);
            }
            Ok(0)
        }
    }
}

/// The `message` and `session_id` of a Claude Code hook event on stdin.
fn hook_event() -> (Option<String>, Option<String>) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return (None, None) };
    let field = |name: &str| event.get(name).and_then(Value::as_str).map(str::to_owned);
    (field("message"), field("session_id"))
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

/// Claude Code hooks that report the agent's state (and conversation id) to
/// artty. They do nothing outside artty.
fn claude_hooks() -> Value {
    let hook = |state: &str| {
        json!([{ "hooks": [{
            "type": "command",
            "command": format!("[ -n \"$ARTTY_SOCKET\" ] && artty status {state} --hook || true"),
        }]}])
    };
    json!({
        "SessionStart": hook("idle"),
        "UserPromptSubmit": hook("working"),
        "PostToolUse": hook("working"),
        "Notification": hook("waiting"),
        "Stop": hook("done"),
        "SessionEnd": hook("idle"),
    })
}

fn hooks(action: Option<&str>) -> Result<i32, String> {
    match action {
        None => {
            let snippet = json!({ "hooks": claude_hooks() });
            println!("{}", serde_json::to_string_pretty(&snippet).unwrap());
            eprintln!("Add these to ~/.claude/settings.json, or run `artty hooks install`.");
            Ok(0)
        }
        Some("install") => install_hooks(),
        Some(other) => Err(format!("hooks: unknown action {other}")),
    }
}

fn install_hooks() -> Result<i32, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let path = PathBuf::from(home).join(".claude").join("settings.json");
    let mut settings: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let root = settings.as_object_mut().ok_or("settings.json is not an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or("settings.json: \"hooks\" is not an object")?;

    // Replace hooks from earlier versions of artty, keep everything else.
    let mut changed = 0;
    for (event, entries) in claude_hooks().as_object().unwrap() {
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        let list = list.as_array_mut().ok_or(format!("settings.json: hooks.{event} is not a list"))?;
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
        println!("artty hooks are up to date in {}", path.display());
        return Ok(0);
    }

    if path.exists() {
        let backup = path.with_extension("json.artty-backup");
        std::fs::copy(&path, &backup).map_err(|err| format!("backup failed: {err}"))?;
        println!("Backed up {} to {}", path.display(), backup.display());
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|err| err.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|err| format!("{}: {err}", path.display()))?;
    println!("Updated artty hooks for {changed} events in {}", path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_guarded() {
        let hooks = claude_hooks();
        let command = hooks["Notification"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, "[ -n \"$ARTTY_SOCKET\" ] && artty status waiting --hook || true");
        assert!(hooks["SessionStart"].is_array());
    }
}
