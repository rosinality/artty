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
    "list", "read", "send", "new", "run", "notify", "status", "focus", "close", "rename", "settings", "restart", "hooks",
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
  artty restart                          quit and reopen artty (e.g. to run a new
                                         build); tabs and agents are restored
  artty shell-integration zsh|bash       print the shell integration script, to
                                         source from your startup file where
                                         artty can't set it up (e.g. over ssh)
  artty hooks status                     which agents have artty's hooks
  artty hooks install [AGENT...]         add (or update) status hooks in the
                                         agents' settings (default: every
                                         installed agent: claude, codex, gemini),
                                         so artty shows exact status and resumes
                                         the right conversation
  artty hooks uninstall [AGENT...]       remove them (default: everywhere)
  artty hooks [AGENT]                    print an agent's hooks
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
        "restart" => Request::Restart,
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

/// `artty hooks` subcommands: show, status, install, uninstall.
fn hooks(args: &[String]) -> Result<i32, String> {
    let names: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str) {
        Some("status") => {
            for agent in crate::hooks::AGENTS {
                println!("{:<12} {}", agent.name, agent.status().describe());
            }
            Ok(0)
        }
        Some("install") => {
            println!("{}", crate::hooks::install(&names)?);
            Ok(0)
        }
        Some("uninstall") => {
            println!("{}", crate::hooks::uninstall(&names)?);
            Ok(0)
        }
        name => {
            let agent = crate::hooks::agent(name.unwrap_or("claude"))?;
            println!("{}", serde_json::to_string_pretty(&json!({ "hooks": agent.entries() })).unwrap());
            eprintln!("Add these to ~/{}, or run `artty hooks install {}`.", agent.file, agent.id);
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
