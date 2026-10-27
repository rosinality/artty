//! `artty <command>`: control a running artty from the command line. Agents
//! inside a tab use this to read other tabs, run commands, and report status.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use artty_core::SessionId;
use artty_core::control::{
    AgentState, ReadSource, Request, Response, SESSION_ENV, SOCKET_ENV, SplitDirection,
    live_sockets,
};
use serde_json::{Value, json};

pub const COMMANDS: &[&str] = &[
    "list",
    "read",
    "send",
    "new",
    "run",
    "notify",
    "status",
    "focus",
    "close",
    "rename",
    "settings",
    "open",
    "restart",
    "server",
    "remote",
    "ps",
    "top",
    "kill",
    "workspace",
    "workspaces",
    "activity",
    "hooks",
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
            [--workspace NAME] [--background] [-- COMMAND...]
                                         open a tab, or split pane N (default:
                                         your own, or the focused one); prints
                                         the new pane's id. --workspace opens it
                                         in that workspace, creating it if needed
  artty focus N | artty close N
  artty rename [--tab N] TITLE           name the tab holding pane N (empty
                                         title: back to automatic)
  artty notify [--tab N] [--title T] BODY
  artty status [--tab N] working|waiting|done|idle [--agent NAME]
             [--message M | --hook]      report an agent's state (for hooks);
                                         --hook reads the agent's hook event
                                         from stdin (message and session id)
  artty ps [--tab N] [--json]            panes and their processes: status,
                                         CPU, memory, how long they've run
  artty top                              the same, live (Ctrl-C to quit)
  artty kill N [--signal SIG] [--pane]   stop pane N's foreground program
                                         (default TERM), or close the pane
  artty workspace [list] [--json]        workspaces: id, window, tabs, status
  artty workspace new [NAME] [--cwd DIR] [-- COMMAND...]
                                         a workspace in the frontmost window
  artty workspace open|hide|close W      show W here (or bring its window
                                         forward), take it out of its window
                                         (it keeps running), or end its tabs;
                                         W is an id or a name
  artty workspace rename W NAME          an empty NAME goes back to automatic
                                         (its git repository or directory)
  artty workspace set W [--root DIR | --no-root] [--color C|none]
                    [--env K=V]... [--unset-env K]...
                                         where its new tabs start, its color
                                         (red, orange, yellow, green, teal,
                                         blue, purple, pink), and environment
                                         variables for its new panes
  artty server [status|stop|restart]     the server that keeps sessions running:
                                         show it, end every session and stop it,
                                         or move it to this build (programs keep
                                         running; a server too old to hand over
                                         restarts, and tabs and agents return)
  artty remote [list]                    other machines' servers shown in the app
  artty remote add NAME --ssh DESTINATION [--artty PATH] [--no-test]
                                         add one, reached over ssh (artty must be
                                         installed there; --artty if it isn't on
                                         the login shell's PATH); checks it first
  artty remote add NAME --command COMMAND
                                         or reached by any shell command that
                                         runs `artty server --stdio` somewhere
  artty remote remove NAME               stop showing it (its sessions keep
                                         running there)
  artty remote test NAME                 try connecting to it
  artty remote install NAME              install this build of artty there (by
                                         ssh, into ~/.local/bin; Linux with
                                         glibc 2.28+), moving a running server
                                         onto it
  artty server --stdio                   carry an app's connection to this
                                         machine's server over stdin and stdout
                                         (what a remote artty app runs over ssh)
  artty settings                         open the Settings window
  artty open URL                         open a web page in the browser of the
                                         Mac showing artty (panes on other
                                         machines have it as $BROWSER)
  artty activity                         show the Activity view (processes,
                                         CPU and memory of every pane)
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
        },
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
    // A server build taking over from another (see `artty_server::upgrade`):
    // before the usual option parsing, which doesn't know these.
    match (args.first().map(String::as_str), args.get(1).map(String::as_str), args.get(2)) {
        (Some("server"), Some("--resume"), Some(header)) => {
            return artty_server::resume(std::path::Path::new(header))
                .map(|()| 0)
                .map_err(|err| format!("server: {err}"));
        },
        (Some("server"), Some("--handover-check"), _) => {
            println!("{}", artty_server::handover_check());
            return Ok(0);
        },
        (Some("remote"), _, _) => return remote(&args[1..]),
        (Some("server"), Some("--stdio"), _) => {
            let exe = std::env::current_exe().map_err(|err| err.to_string())?;
            return artty_server::stdio_relay(&exe)
                .map(|()| 0)
                .map_err(|err| format!("server: {err}"));
        },
        _ => {},
    }
    let mut args = Args { args: args.into_iter() };
    let command = args.next().unwrap_or_default();
    let mut tab: Option<SessionId> = None;
    let mut rest = Vec::new();
    let mut flags = std::collections::HashMap::<&str, String>::new();
    let mut switches = std::collections::HashSet::<&str>::new();
    // Where in `rest` the arguments after `--` start.
    let mut command_start = None;
    // Repeatable: `--env K=V`, `--unset-env K`.
    let mut env_vars = Vec::new();
    let mut unset_env = Vec::new();

    // Shared flag parsing; `--` ends flags.
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tab" => tab = Some(args.number("--tab")?),
            "--lines" => _ = flags.insert("lines", args.value("--lines")?),
            "--timeout" => _ = flags.insert("timeout", args.value("--timeout")?),
            "--cwd" => _ = flags.insert("cwd", args.value("--cwd")?),
            "--title" => _ = flags.insert("title", args.value("--title")?),
            "--workspace" => _ = flags.insert("workspace", args.value("--workspace")?),
            "--message" => _ = flags.insert("message", args.value("--message")?),
            "--split" => _ = flags.insert("split", args.value("--split")?),
            "--agent" => _ = flags.insert("agent", args.value("--agent")?),
            "--signal" => _ = flags.insert("signal", args.value("--signal")?),
            "--root" => _ = flags.insert("root", args.value("--root")?),
            "--color" => _ = flags.insert("color", args.value("--color")?),
            "--env" => {
                let pair = args.value("--env")?;
                let (key, value) =
                    pair.split_once('=').ok_or(format!("--env: expected KEY=VALUE, not {pair}"))?;
                env_vars.push((key.to_owned(), value.to_owned()));
            },
            "--unset-env" => unset_env.push(args.value("--unset-env")?),
            "--no-root" => {
                switches.insert("no-root");
            },
            "--pane" => {
                switches.insert("pane");
            },
            "--screen" | "--last" | "--no-enter" | "--background" | "--json" | "--hook" => {
                switches.insert(match arg.as_str() {
                    "--screen" => "screen",
                    "--last" => "last",
                    "--no-enter" => "no-enter",
                    "--background" => "background",
                    "--json" => "json",
                    _ => "hook",
                });
            },
            "--" => {
                command_start = Some(rest.len());
                rest.extend(args.args.by_ref());
                break;
            },
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(0);
            },
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => rest.push(arg),
        }
    }

    let request = match command.as_str() {
        "list" => Request::List,
        "ps" => Request::Processes { tab },
        "top" => return top(),
        "kill" => {
            let id =
                rest.first().and_then(|id| id.parse().ok()).or(tab).ok_or("kill: which pane?")?;
            let signal = flags.get("signal").map(|s| parse_signal(s)).transpose()?;
            Request::Kill { tab: id, signal, pane: switches.contains("pane") }
        },
        "workspace" | "workspaces" => {
            let action = rest.first().map_or("list", String::as_str);
            let which = || {
                rest.get(1)
                    .cloned()
                    .ok_or(format!("workspace {action}: which workspace? (an id or a name)"))
            };
            match action {
                "list" => Request::Workspaces,
                "new" => {
                    let split = command_start.unwrap_or(rest.len()).max(1);
                    let name = rest[1..split].join(" ");
                    Request::NewWorkspace {
                        name: (!name.is_empty()).then_some(name),
                        cwd: flags.get("cwd").map(PathBuf::from),
                        command: rest[split..].to_vec(),
                    }
                },
                "rename" => Request::RenameWorkspace {
                    workspace: which()?,
                    name: rest.get(2..).unwrap_or_default().join(" "),
                },
                "close" => Request::CloseWorkspace { workspace: which()? },
                "open" => Request::OpenWorkspace { workspace: which()? },
                "hide" => Request::HideWorkspace { workspace: which()? },
                "set" => {
                    // Relative to where you are, like any path you type.
                    let root = flags
                        .get("root")
                        .map(|root| {
                            std::fs::canonicalize(root)
                                .map_err(|err| format!("--root {root}: {err}"))
                        })
                        .transpose()?;
                    Request::ConfigureWorkspace {
                        workspace: which()?,
                        root,
                        clear_root: switches.contains("no-root"),
                        color: flags.get("color").cloned(),
                        env: env_vars,
                        unset_env,
                    }
                },
                other => {
                    return Err(format!(
                        "workspace: unknown action {other} (list, new, rename, set, close, open, hide)"
                    ));
                },
            }
        },
        "settings" => Request::Settings,
        "open" => match rest.as_slice() {
            [url] => Request::OpenUrl { url: url.clone() },
            _ => return Err("open: give one URL".into()),
        },
        "activity" => Request::Activity,
        "restart" => Request::Restart,
        "read" => {
            let source = if switches.contains("screen") {
                ReadSource::Screen
            } else if switches.contains("last") {
                ReadSource::LastCommand
            } else {
                let lines = flags
                    .get("lines")
                    .map_or(Ok(100), |l| l.parse().map_err(|_| "--lines: not a number"))?;
                ReadSource::Scrollback { lines }
            };
            Request::Read { tab, source }
        },
        "send" => {
            if rest.is_empty() {
                return Err("send: nothing to send".into());
            }
            Request::Send { tab, text: rest.join(" "), enter: !switches.contains("no-enter") }
        },
        "run" => {
            if rest.is_empty() {
                return Err("run: missing command (artty run [--tab N] -- COMMAND)".into());
            }
            if tab.is_none() && own_tab().is_some() {
                return Err("run: pass --tab N; a tab can't run commands in itself".into());
            }
            let timeout_secs = flags
                .get("timeout")
                .map(|t| t.parse().map_err(|_| "--timeout: not a number"))
                .transpose()?;
            Request::Run { tab, command: rest.join(" "), timeout_secs }
        },
        "new" => {
            let split = match flags.get("split").map(String::as_str) {
                None => None,
                Some("right") => Some(SplitDirection::Right),
                Some("down") => Some(SplitDirection::Down),
                Some(other) => return Err(format!("--split: expected right or down, not {other}")),
            };
            let workspace = flags.get("workspace").cloned();
            if split.is_some() && workspace.is_some() {
                return Err("new: --split and --workspace don't go together".into());
            }
            Request::New {
                workspace,
                cwd: flags.get("cwd").map(PathBuf::from),
                command: rest,
                title: flags.get("title").cloned(),
                focus: !switches.contains("background"),
                split,
                target: split.and(tab.or_else(own_tab)),
            }
        },
        "focus" | "close" => {
            let id = rest
                .first()
                .and_then(|id| id.parse().ok())
                .or(tab)
                .ok_or(format!("{command}: which tab?"))?;
            if command == "focus" { Request::Focus { tab: id } } else { Request::Close { tab: id } }
        },
        "rename" => Request::Rename { tab: tab.or_else(own_tab), title: rest.join(" ") },
        "notify" => {
            if rest.is_empty() {
                return Err("notify: missing message".into());
            }
            Request::Notify {
                tab: tab.or_else(own_tab),
                title: flags.get("title").cloned(),
                body: rest.join(" "),
            }
        },
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
            Request::Status {
                tab: tab.or_else(own_tab),
                state,
                message,
                agent_session,
                agent: flags.get("agent").cloned(),
            }
        },
        "hooks" => return hooks(&rest),
        "server" => match rest.first().map(String::as_str) {
            None | Some("run") => {
                return artty_server::run().map(|()| 0).map_err(|err| format!("server: {err}"));
            },
            Some("status") => Request::ServerStatus,
            Some("stop") => Request::StopServer,
            Some("restart") => {
                let exe = std::env::current_exe().map_err(|err| err.to_string())?;
                return match artty_server::upgrade(&exe)
                    .map_err(|err| format!("server restart: {err}"))?
                {
                    artty_server::Upgrade::HandedOver => {
                        println!("The server now runs this build; every program kept running.");
                        Ok(0)
                    },
                    artty_server::Upgrade::Restarted => {
                        println!(
                            "The server (too old to hand over) restarted on this build: tabs reopen, programs restarted."
                        );
                        Ok(0)
                    },
                };
            },
            Some(other) => {
                return Err(format!("server: unknown action {other} (run, status, stop, restart)"));
            },
        },
        "shell-integration" => {
            let shell = rest.first().map(String::as_str).unwrap_or_default();
            let script = artty_core::integration::script(shell)
                .ok_or("shell-integration: expected zsh or bash (fish has it built in)")?;
            print!("{script}");
            return Ok(0);
        },
        _ => {
            print!("{HELP}");
            return Ok(2);
        },
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
        Response::Workspace { id } => {
            println!("{id}");
            Ok(0)
        },
        Response::Workspaces { workspaces } if json => {
            println!("{}", serde_json::to_string_pretty(&workspaces).unwrap_or_default());
            Ok(0)
        },
        Response::Workspaces { workspaces } => {
            println!("    ID  WIN  TABS  STATUS   NAME");
            for workspace in workspaces {
                let marker = if workspace.needs_attention { "!" } else { " " };
                let window = if workspace.window_number == 0 {
                    "-".to_owned()
                } else {
                    workspace.window_number.to_string()
                };
                let place = match (workspace.window, workspace.attached) {
                    (None, _) => "  (hidden)",
                    (Some(_), false) => "  (closed window)",
                    (Some(_), true) => "",
                };
                // Unnamed ones go by what they show, in brackets.
                let title = match &workspace.name {
                    Some(name) => name.clone(),
                    None => format!("[{}]", workspace.title),
                };
                let root = workspace
                    .root
                    .as_ref()
                    .map(|r| format!("  {}", short_path(r)))
                    .unwrap_or_default();
                println!(
                    "{marker} {:>4}  {window:>3}  {:>4}  {:<8} {title}{root}{place}",
                    workspace.id, workspace.tabs, workspace.status
                );
            }
            Ok(0)
        },
        Response::Error { message } => Err(message),
        Response::Tab { id } => {
            println!("{id}");
            Ok(0)
        },
        Response::Text { text } => {
            println!("{text}");
            Ok(0)
        },
        Response::Server { info } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
            } else {
                println!(
                    "artty server {} (pid {}), up {}\n{} windows ({} shown), {} workspaces, {} panes, {} apps attached",
                    info.version,
                    info.pid,
                    duration(info.uptime_secs),
                    info.windows,
                    info.attached_windows,
                    info.workspaces,
                    info.panes,
                    info.clients
                );
            }
            Ok(0)
        },
        Response::Processes { panes, .. } if json => {
            println!("{}", serde_json::to_string_pretty(&panes).unwrap_or_default());
            Ok(0)
        },
        Response::Processes { panes, .. } => {
            print!("{}", process_table(&panes, false));
            Ok(0)
        },
        Response::Command { output, exit, truncated, .. } => {
            if truncated {
                eprintln!("artty: output truncated");
            }
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(exit.unwrap_or(0))
        },
        Response::Tabs { tabs } if json => {
            println!("{}", serde_json::to_string_pretty(&tabs).unwrap_or_default());
            Ok(0)
        },
        Response::Tabs { tabs } => {
            println!("    ID  WIN  WORKSPACE     TAB  STATUS   TITLE");
            for pane in tabs {
                let marker = if pane.active {
                    "*"
                } else if pane.needs_attention {
                    "!"
                } else {
                    " "
                };
                let detail = pane.detail.map(|d| format!("  ({d})")).unwrap_or_default();
                let workspace = workspace_label(pane.workspace, pane.workspace_name.as_deref());
                // Window 0: the workspace is hidden.
                let window =
                    if pane.window == 0 { "-".to_owned() } else { pane.window.to_string() };
                println!(
                    "{marker} {:>4}  {window:>3}  {workspace:<12}  {:>3}  {:<8} {}{detail}",
                    pane.id, pane.tab, pane.status, pane.title
                );
            }
            Ok(0)
        },
    }
}

/// A path with the home directory as `~`.
fn short_path(path: &std::path::Path) -> String {
    match std::env::var_os("HOME")
        .and_then(|home| path.strip_prefix(home).ok().map(std::path::Path::to_path_buf))
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A workspace's name (shortened to fit a column), or its number.
fn workspace_label(number: usize, name: Option<&str>) -> String {
    match name {
        Some(name) if name.chars().count() > 12 => {
            format!("{}…", name.chars().take(11).collect::<String>())
        },
        Some(name) => name.to_owned(),
        None => format!("#{number}"),
    }
}

fn parse_signal(name: &str) -> Result<i32, String> {
    if let Ok(number) = name.parse() {
        return Ok(number);
    }
    let name = name.trim_start_matches("SIG").to_uppercase();
    Ok(match name.as_str() {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "KILL" => libc::SIGKILL,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => return Err(format!("unknown signal {name}")),
    })
}

/// `1536000` → `1.5M`.
pub fn bytes(n: u64) -> String {
    match n {
        0..1_000_000 => format!("{:.0}K", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}G", n as f64 / 1e9),
    }
}

/// Panes, each followed by its processes (foreground ones marked `*`).
fn process_table(panes: &[artty_core::control::PaneProcesses], by_cpu: bool) -> String {
    use std::fmt::Write as _;
    let bold = |s: &str| if is_tty() { format!("\x1b[1m{s}\x1b[0m") } else { s.to_owned() };
    let dim = |s: &str| if is_tty() { format!("\x1b[2m{s}\x1b[0m") } else { s.to_owned() };
    let mut panes: Vec<_> = panes.iter().collect();
    if by_cpu {
        panes.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    }
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold("PANE WIN WORKSPACE     TAB  STATUS      CPU     MEM  TITLE"));
    for pane in panes {
        let shown = if pane.attached { "" } else { "  (detached)" };
        let _ = writeln!(
            out,
            "{:>4} {:>3} {:<12}  {:>3}  {:<8} {:>5.1}% {:>7}  {}{}",
            pane.id,
            pane.window,
            workspace_label(pane.workspace, pane.workspace_name.as_deref()),
            pane.tab,
            pane.status,
            pane.cpu_percent,
            bytes(pane.memory_bytes),
            pane.title,
            dim(shown)
        );
        let mut processes: Vec<_> = pane.processes.iter().collect();
        if by_cpu {
            processes.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
        }
        for process in processes {
            let marker = if process.foreground { "*" } else { " " };
            let line = format!(
                // Lines CPU and memory up with the pane's.
                "{:>19}{marker}{:<16} {:>5.1}% {:>7}  pid {:<6} {}",
                "",
                process.name,
                process.cpu_percent,
                bytes(process.memory_bytes),
                process.pid,
                duration(process.elapsed_secs)
            );
            let _ = writeln!(out, "{}", if process.foreground { line } else { dim(&line) });
        }
    }
    out
}

fn is_tty() -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::isatty(1) == 1 }
}

/// `artty top`: the process table, refreshed every second until Ctrl-C.
fn top() -> Result<i32, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t) };

    // Alternate screen, hidden cursor; restored on exit.
    print!("\x1b[?1049h\x1b[?25l");
    let result = loop {
        if STOP.load(Ordering::Relaxed) {
            break Ok(0);
        }
        let status = send(&Request::ServerStatus, Some(Duration::from_secs(5)));
        let panes = send(&Request::Processes { tab: None }, Some(Duration::from_secs(5)));
        let (Ok(Response::Server { info }), Ok(Response::Processes { panes, .. })) =
            (status, panes)
        else {
            break Err("lost the artty server".to_owned());
        };
        let total_cpu: f32 = panes.iter().map(|p| p.cpu_percent).fold(0., |a, b| a + b);
        let total_memory: u64 = panes.iter().map(|p| p.memory_bytes).sum();
        let mut screen = String::from("\x1b[H\x1b[2J");
        screen.push_str(&format!(
            "artty server {} · pid {} · up {} · {} windows ({} shown) · {} workspaces · {} panes · CPU {:.1}% · memory {}\n\n",
            info.version,
            info.pid,
            duration(info.uptime_secs),
            info.windows,
            info.attached_windows,
            info.workspaces,
            info.panes,
            total_cpu,
            bytes(total_memory)
        ));
        screen.push_str(&process_table(&panes, true));
        screen.push_str("\n\x1b[2mCtrl-C to quit\x1b[0m");
        print!("{}", screen.replace('\n', "\r\n"));
        let _ = std::io::stdout().flush();
        for _ in 0..10 {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    print!("\x1b[?25h\x1b[?1049l");
    let _ = std::io::stdout().flush();
    result
}

/// `90s` → `1m30s`, `5400s` → `1h30m`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{}s", secs / 60, secs % 60),
        3600..86400 => format!("{}h{}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d{}h", secs / 86400, secs % 86400 / 3600),
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
    let path = artty_core::control::server_socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Ok(path);
    }
    // An app from before the server existed.
    live_sockets().into_iter().next().ok_or_else(|| "artty is not running".to_owned())
}

fn send(request: &Request, timeout: Option<Duration>) -> Result<Response, String> {
    let path = socket()?;
    let mut stream =
        UnixStream::connect(&path).map_err(|err| format!("{}: {err}", path.display()))?;
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
/// `artty remote`: other machines' servers, in the config file.
fn remote(args: &[String]) -> Result<i32, String> {
    use artty_core::config_file::ConfigFile;
    use artty_core::{Config, ServerConfig};
    let describe = |server: &ServerConfig| match (&server.ssh, server.command.is_empty()) {
        (_, false) => server.command.join(" "),
        (Some(ssh), true) => format!("ssh {ssh}"),
        (None, true) => "(no ssh or command)".to_owned(),
    };
    let find = |name: Option<&String>| -> Result<ServerConfig, String> {
        let name = name.ok_or("expected a server's name")?;
        Config::load()
            .0
            .servers
            .into_iter()
            .find(|s| &s.name == name)
            .ok_or_else(|| format!("no server named {name} (see `artty remote list`)"))
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let servers = Config::load().0.servers;
            if servers.is_empty() {
                println!(
                    "No other machines' servers. Add one: artty remote add NAME --ssh DESTINATION"
                );
            }
            for server in servers {
                println!("{:<16} {}", server.name, describe(&server));
            }
            Ok(0)
        },
        Some("add") => {
            let name = args
                .get(1)
                .filter(|name| !name.starts_with("--") && !name.trim().is_empty())
                .ok_or("remote add: expected a name, then --ssh DESTINATION")?;
            let mut server = ServerConfig { name: name.trim().to_owned(), ..Default::default() };
            let mut test = true;
            let mut rest = args[2..].iter();
            while let Some(arg) = rest.next() {
                let mut value = || rest.next().cloned().ok_or(format!("{arg}: expected a value"));
                match arg.as_str() {
                    "--ssh" => server.ssh = Some(value()?),
                    "--artty" => server.artty = Some(value()?),
                    "--command" => server.command = vec!["sh".into(), "-c".into(), value()?],
                    "--no-test" => test = false,
                    other => return Err(format!("remote add: unknown option {other}")),
                }
            }
            if server.connect_command().is_none() {
                return Err("remote add: expected --ssh DESTINATION (or --command)".into());
            }
            if test {
                remote_test(&server)?;
            }
            let mut file = ConfigFile::open().map_err(|err| err.to_string())?;
            file.add_server(&server);
            file.save().map_err(|err| err.to_string())?;
            println!("Added {}: the app shows it in the sidebar.", server.name);
            Ok(0)
        },
        Some("remove") => {
            let server = find(args.get(1))?;
            let mut file = ConfigFile::open().map_err(|err| err.to_string())?;
            file.remove_server(&server.name);
            file.save().map_err(|err| err.to_string())?;
            println!("Removed {}; its sessions keep running there.", server.name);
            Ok(0)
        },
        Some("test") => {
            remote_test(&find(args.get(1))?)?;
            Ok(0)
        },
        Some("install") => {
            let mut server = find(args.get(1))?;
            eprintln!("Installing this build of artty on {}…", server.name);
            let installed = artty_core::remote::install(&server, terminal_prompts(), linux_build)
                .map_err(|err| format!("couldn't install on {}: {err}", server.name))?;
            if let Some(artty) = installed.artty_setting(server.artty.as_deref()) {
                server.artty = artty;
                let mut file = ConfigFile::open().map_err(|err| err.to_string())?;
                file.add_server(&server);
                file.save().map_err(|err| err.to_string())?;
                eprintln!("(the config now runs the installed artty there)");
            }
            remote_test(&server)?;
            Ok(0)
        },
        Some(other) => {
            Err(format!("remote: unknown action {other} (list, add, remove, test, install)"))
        },
    }
}

/// ssh may ask for a login on the terminal, if there is one.
fn terminal_prompts() -> artty_core::remote::Prompts<'static> {
    // SAFETY: plain syscall.
    match unsafe { libc::isatty(0) } == 1 {
        true => artty_core::remote::Prompts::Terminal,
        false => artty_core::remote::Prompts::Never,
    }
}

/// Connect to a server and say how it went.
fn remote_test(server: &artty_core::ServerConfig) -> Result<(), String> {
    use artty_core::protocol::ServerMessage;
    eprintln!(
        "Connecting to {} ({})…",
        server.name,
        server.connect_command().unwrap_or_default().join(" ")
    );
    let mut opened = artty_core::remote::open(server, terminal_prompts())
        .map_err(|err| format!("couldn't reach {}: {err}", server.name))?;
    let _ = opened.child.kill();
    let _ = opened.child.wait();
    match opened.welcome {
        ServerMessage::Welcome { version, server_version }
            if version == artty_core::protocol::protocol_version() =>
        {
            println!("{}: artty {server_version}, ready.", server.name);
            Ok(())
        },
        ServerMessage::Welcome { server_version, .. } => Err(format!(
            "{} runs another artty build ({server_version}); install this one there: \
             artty remote install {}",
            server.name, server.name
        )),
        _ => Err(format!("{}: unexpected answer", server.name)),
    }
}

#[cfg(target_os = "macos")]
/// Run as ssh's askpass helper: ask the app (which shows the question in a
/// window) and print the answer for ssh. Fails (status 1) when cancelled.
pub fn askpass(socket: &std::path::Path, request: &artty_core::remote::AskpassRequest) -> i32 {
    use std::io::{BufRead, Write};
    let asked = (|| -> std::io::Result<Option<String>> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        let mut answer = String::new();
        std::io::BufReader::new(stream).read_line(&mut answer)?;
        let answer: artty_core::remote::AskpassAnswer = serde_json::from_str(&answer)?;
        Ok(answer.answer)
    })();
    match asked {
        Ok(Some(answer)) => {
            println!("{answer}");
            0
        },
        Ok(None) => 1,
        Err(err) => {
            eprintln!("artty: couldn't ask: {err}");
            1
        },
    }
}

/// This app's build of artty for Linux on `cpu`: in its bundle, or (run
/// from the source tree) cargo-zigbuild's output beside it.
pub fn linux_build(cpu: &str) -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bundled = dir.parent()?.join("Resources/linux").join(format!("artty-{cpu}"));
    if bundled.is_file() {
        return Some(bundled);
    }
    let built = dir.parent()?.join(format!("{cpu}-unknown-linux-gnu/release/artty"));
    built.is_file().then_some(built)
}

fn hooks(args: &[String]) -> Result<i32, String> {
    let names: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str) {
        Some("status") => {
            for agent in crate::hooks::AGENTS {
                println!("{:<12} {}", agent.name, agent.status().describe());
            }
            Ok(0)
        },
        Some("install") => {
            println!("{}", crate::hooks::install(&names)?);
            Ok(0)
        },
        Some("uninstall") => {
            println!("{}", crate::hooks::uninstall(&names)?);
            Ok(0)
        },
        name => {
            let agent = crate::hooks::agent(name.unwrap_or("claude"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "hooks": agent.entries() })).unwrap()
            );
            eprintln!("Add these to ~/{}, or run `artty hooks install {}`.", agent.file, agent.id);
            Ok(0)
        },
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
