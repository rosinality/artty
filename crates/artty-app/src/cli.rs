//! `artty <command>`: control a running artty from the command line. Agents
//! inside a tab use this to read other tabs, run commands, and report status.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use artty_core::SessionId;
use artty_core::control::{
    AgentState, ApprovalInfo, AskInfo, AskState, HookEvent, ReadSource, Request, Response,
    SESSION_ENV, SOCKET_ENV, SplitDirection, ThreadEntry, ThreadItem, live_sockets,
};
use artty_core::protocol::QueueKind;
use serde_json::{Value, json};

pub const COMMANDS: &[&str] = &[
    "attach",
    "acp",
    "list",
    "read",
    "send",
    "new",
    "run",
    "ask",
    "approve",
    "approvals",
    "approval",
    "queue",
    "thread",
    "agents",
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
    "guide",
    "wait",
    "events",
    "shell-integration",
];

const HELP: &str = "\
artty — a terminal for working with AI agents

usage:
  artty [-e command [args...]]           open the app (optionally running a command)

a window of the server in this terminal (as tmux attach), here or over ssh:
  artty attach [WINDOW] [--new] [--prefix KEY]
               [--ssh DESTINATION [--artty PATH] | --remote NAME]
                                         show WINDOW, else the newest window no
                                         app shows, else the newest one (shared
                                         with the apps showing it), else a new
                                         one (--new: a new one anyway; --share
                                         is taken and does nothing). ctrl-b (or
                                         --prefix, or $ARTTY_PREFIX) then d
                                         detaches, ? lists the keys; sessions
                                         keep running
  artty attach --list [...]              the server's windows

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
  artty ask --tab N [--timeout S] [--no-wait] [--raw] TEXT | -
                                         give the agent in pane N a message (- reads
                                         it from stdin) once it's between turns, as
                                         if typed, and print its reply. Needs its
                                         hooks (artty hooks install). After
                                         --timeout or with --no-wait, prints the
                                         ask's id (exit 3 if not over); --raw allows
                                         text it takes as a command (/, !, #)
  artty ask --result ID [--timeout S]    wait for that ask's reply (--timeout 0:
                                         say where it is now)
  artty ask --cancel ID                  take back an ask not yet given to its agent
  artty ask --cancel all [--tab N]       ... every one (of pane N's), printing them
  artty ask --list [--tab N] [--json]    asks not yet over: id, pane, state, text
  artty ask --cancel ID --abandon        stop waiting for one with its agent
  artty wait ID... [--all] [--timeout S] [--json]
                                         wait for the first of these asks to be
                                         over (--all: every one), and print each
                                         one's answer or state; exit 3 if not
  artty events --follow [--tab N] [--text]
                                         every agent's thread entries as they
                                         come (asks, replies, permission
                                         requests, files changed), a JSON object
                                         a line with its pane and seq
  artty events --after SEQ [--tab N]     ... those after SEQ, and exit
  artty ask --bring-back ID              give an answered ask's answer to the
                                         agent whose thread it was written from
  artty approvals [--tab N] [--json]     agents' permission requests (pending
                                         first): id, pane, state, what they'd do
  artty approval allow|deny ID [--message M] [--always N]
                                         answer one (not from inside artty's
                                         panes: an agent mustn't approve its own);
                                         the agent's own prompt answers it too.
                                         --always N: allow with its Nth always-
                                         option (as the agent offered them)
  artty queue [--json]                   what needs you: permission requests,
                                         agents waiting, unread replies
  artty queue --dismiss ID               mark a reply read
  artty agents [--json]                  panes running agents: what each is
                                         doing, its pending permission requests
                                         and unread replies
  artty thread --tab N [--lines N] [--after SEQ] [--json]
                                         what pane N's agent was given and said,
                                         its permission requests and sessions,
                                         oldest first (the last N; or after an
                                         entry's SEQ, from --json)
  artty thread --clear --tab N          forget pane N's thread (its terminal
                                         isn't touched)
  artty thread --search TEXT [--tab N] [--json]
                                         every agent's thread (or pane N's)
                                         searched: newest matches first
  artty approve --hook --agent NAME      (an agent's PermissionRequest hook)
  artty acp [AGENT] [--resume ID] [--model M] [--mode MODE]
                                         run AGENT (claude) without its terminal
                                         UI, through its Agent Client Protocol
                                         adapter (the acp.AGENT setting), in
                                         this tab: Esc interrupts, ^T opens the
                                         conversation in its terminal UI
  artty new [--split right|down] [--tab N] [--cwd DIR] [--title T]
            [--workspace NAME] [--background] [-- COMMAND...]
                                         open a tab, or split pane N (default:
                                         your own, or the focused one); prints
                                         the new pane's id. --workspace opens it
                                         in that workspace, creating it if needed
  artty new --agent NAME [--task TEXT | --task -] [--worktree BRANCH] [...]
                                         ... running agent NAME (claude, codex,
                                         gemini), given TEXT as its first
                                         message (an ask: its id on stderr).
                                         From an agent's pane, it's the new
                                         agent's starter (max-agent-depth deep
                                         at most: 2 unless set).
                                         --worktree: in a git worktree of the
                                         repository (here, or --cwd), beside it
                                         in REPO.worktrees/BRANCH on BRANCH;
                                         removed with the tab if unchanged
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
  artty guide                            the guide for agents asking other agents
                                         (also in ~/.config/artty/AGENTS.md)
  artty guide install|uninstall          add (or take out) a Claude Code skill
                                         pointing agents to it
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

pub(crate) fn own_tab() -> Option<SessionId> {
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
        (Some("acp"), _, _) => return crate::acp_host::run(&args[1..]),
        (Some("attach"), _, _) => return attach(&args[1..]),
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
            "--result" => _ = flags.insert("result", args.value("--result")?),
            "--cancel" => _ = flags.insert("cancel", args.value("--cancel")?),
            "--bring-back" => _ = flags.insert("bring-back", args.value("--bring-back")?),
            "--task" => _ = flags.insert("task", args.value("--task")?),
            "--worktree" => _ = flags.insert("worktree", args.value("--worktree")?),
            "--search" => _ = flags.insert("search", args.value("--search")?),
            "--always" => _ = flags.insert("always", args.value("--always")?),
            "--dismiss" => _ = flags.insert("dismiss", args.value("--dismiss")?),
            "--after" => _ = flags.insert("after", args.value("--after")?),
            "--raw" | "--no-wait" | "--list" | "--abandon" | "--all" | "--follow" | "--text" => {
                switches.insert(match arg.as_str() {
                    "--raw" => "raw",
                    "--list" => "list",
                    "--abandon" => "abandon",
                    "--all" => "all",
                    "--follow" => "follow",
                    "--text" => "text",
                    _ => "no-wait",
                });
            },
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
            "--screen" | "--last" | "--no-enter" | "--background" | "--json" | "--hook"
            | "--clear" => {
                switches.insert(match arg.as_str() {
                    "--screen" => "screen",
                    "--last" => "last",
                    "--no-enter" => "no-enter",
                    "--background" => "background",
                    "--json" => "json",
                    "--clear" => "clear",
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
        "ask" => {
            let number = |name: &str| -> Result<Option<u64>, String> {
                flags
                    .get(name)
                    .map(|v| v.parse().map_err(|_| format!("--{name}: not a number")))
                    .transpose()
            };
            let timeout_secs = number("timeout")?;
            if switches.contains("list") {
                Request::AskList { tab }
            } else if flags.get("cancel").map(String::as_str) == Some("all") {
                Request::AskCancelQueued { tab }
            } else if let Some(id) = number("cancel")? {
                Request::AskCancel { id, abandon: switches.contains("abandon") }
            } else if let Some(id) = number("bring-back")? {
                Request::AskForward { id }
            } else if let Some(id) = number("result")? {
                Request::AskResult { id, wait: timeout_secs != Some(0), timeout_secs }
            } else {
                let tab = tab.ok_or("ask: which agent? (--tab N)")?;
                if Some(tab) == own_tab() {
                    return Err(format!(
                        "ask: pane {tab} is your own: an agent can't ask itself (`artty agents` \
                         lists the others)"
                    ));
                }
                let text = if rest == ["-"] {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text).map_err(|err| err.to_string())?;
                    text
                } else {
                    rest.join(" ")
                };
                Request::Ask {
                    tab,
                    text,
                    raw: switches.contains("raw"),
                    wait: !switches.contains("no-wait"),
                    timeout_secs,
                    thread: None,
                    back: false,
                }
            }
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
            // An agent by name (its command), and its first message.
            let command = match flags.get("agent") {
                Some(agent) if rest.is_empty() => vec![agent.clone()],
                Some(_) => return Err("new: --agent or -- COMMAND, not both".into()),
                None => rest,
            };
            let task = match flags.get("task").map(String::as_str) {
                Some("-") => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text).map_err(|err| err.to_string())?;
                    Some(text)
                },
                task => task.map(str::to_owned),
            };
            if task.is_some() && command.is_empty() {
                return Err("new: --task needs an agent to give it to (--agent NAME)".into());
            }
            let worktree = flags.get("worktree").cloned();
            // A worktree of the repository you're in, unless --cwd says.
            let cwd = match flags.get("cwd") {
                Some(cwd) => Some(std::path::absolute(cwd).map_err(|err| format!("--cwd: {err}"))?),
                None if worktree.is_some() => std::env::current_dir().ok(),
                None => None,
            };
            Request::New {
                workspace,
                worktree,
                cwd,
                command,
                title: flags.get("title").cloned(),
                focus: !switches.contains("background"),
                split,
                target: split.and(tab.or_else(own_tab)),
                task,
                thread: None,
                back: false,
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
            // An agent run by `artty acp`: that reports, not its hooks.
            if switches.contains("hook") && std::env::var_os(crate::acp_host::HOST_ENV).is_some() {
                return Ok(0);
            }
            let (message, agent_session, hook) = if switches.contains("hook") {
                hook_event()
            } else {
                (flags.get("message").cloned(), None, None)
            };
            // Claude Code notifies for more than needing you: not "waiting"
            // for those that don't (idle, it has said "done" already).
            if hook.as_ref().is_some_and(|h| h.event == "Notification")
                && !needs_you(hook.as_ref().and_then(|h| h.notification_type.as_deref()))
            {
                return Ok(0);
            }
            Request::Status {
                tab: tab.or_else(own_tab),
                state,
                message,
                agent_session,
                agent: flags.get("agent").cloned(),
                hook,
            }
        },
        "approve" => return approve(flags.get("agent").cloned()),
        "approvals" => Request::Approvals { tab },
        "approval" => {
            let allow = match rest.first().map(String::as_str) {
                Some("allow") => true,
                Some("deny") => false,
                _ => return Err("approval: allow or deny? (artty approval allow ID)".into()),
            };
            let id = rest
                .get(1)
                .and_then(|id| id.parse().ok())
                .ok_or("approval: which request? (its id, from artty approvals)")?;
            let always = flags
                .get("always")
                .map(|n| n.parse().map_err(|_| "--always: not a number (an option's index)"))
                .transpose()?;
            Request::Decide { id, allow, message: flags.get("message").cloned(), always }
        },
        "agents" => Request::Agents,
        "wait" => {
            let ids: Vec<u64> = rest
                .iter()
                .map(|id| id.parse().map_err(|_| format!("wait: {id} isn't an ask's id")))
                .collect::<Result<_, _>>()?;
            if ids.is_empty() {
                return Err("wait: which asks? (their ids)".into());
            }
            let timeout_secs = flags
                .get("timeout")
                .map(|t| t.parse().map_err(|_| "--timeout: not a number"))
                .transpose()?;
            let all = switches.contains("all");
            let response = keep_waiting(timeout_secs, "artty wait", |left| Request::AskWait {
                ids: ids.clone(),
                all,
                timeout_secs: left,
            })?;
            return print_waited(response, all, switches.contains("json"));
        },
        "events" => {
            let after = flags
                .get("after")
                .map(|v| v.parse().map_err(|_| "--after: not a number (an entry's seq)"))
                .transpose()?;
            return events(after, tab, switches.contains("follow"), switches.contains("text"));
        },
        "thread" => {
            let number = |name: &str| -> Result<Option<u64>, String> {
                flags
                    .get(name)
                    .map(|v| v.parse().map_err(|_| format!("--{name}: not a number")))
                    .transpose()
            };
            if switches.contains("clear") {
                let tab = tab.ok_or("thread --clear: which pane? (--tab N)")?;
                return print_response(send(&Request::ThreadClear { tab }, None)?, false);
            }
            if let Some(text) = flags.get("search") {
                // Every agent's thread, unless one's named.
                return print_response(
                    send(&Request::ThreadSearch { text: text.clone(), tab }, None)?,
                    switches.contains("json"),
                );
            }
            Request::Thread {
                tab: tab.or_else(own_tab).ok_or("thread: which pane? (--tab N)")?,
                after: number("after")?,
                limit: number("lines")?.map(|n| n as usize),
            }
        },
        "queue" => match flags.get("dismiss") {
            Some(id) => Request::QueueDismiss {
                id: id.parse().map_err(|_| "--dismiss: not a number (a reply's id)")?,
            },
            None => Request::Queue,
        },
        "hooks" => return hooks(&rest),
        "guide" => return guide(rest.first().map(String::as_str)),
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
        Request::Ask { wait: true, timeout_secs, .. }
        | Request::AskResult { wait: true, timeout_secs, .. } => {
            timeout_secs.map(|s| Duration::from_secs(s + 5))
        },
        _ => Some(Duration::from_secs(10)),
    };
    // A waiting ask: first its id, on stderr, so a caller cut short (a shell
    // tool's timeout) can pick the reply up rather than ask again.
    if let Request::Ask { tab, text, raw, wait: true, timeout_secs, thread, back } = &request {
        let add = Request::Ask {
            tab: *tab,
            text: text.clone(),
            raw: *raw,
            wait: false,
            timeout_secs: None,
            thread: *thread,
            back: *back,
        };
        let response = send(&add, Some(Duration::from_secs(10)))?;
        let Response::Ask { ask } = &response else {
            return print_response(response, switches.contains("json"));
        };
        if ask.state.finished() {
            return print_response(response, switches.contains("json"));
        }
        eprintln!("artty: ask {} (artty ask --result {} if this is cut short)", ask.id, ask.id);
        let response = wait_for_ask(ask.id, *timeout_secs)?;
        return print_response(response, switches.contains("json"));
    }
    if let Request::AskResult { id, wait: true, timeout_secs } = &request {
        let response = wait_for_ask(*id, *timeout_secs)?;
        return print_response(response, switches.contains("json"));
    }
    let response = send(&request, timeout)?;
    if let (Request::AskCancel { .. }, Response::Ask { ask }) = (&request, &response)
        && ask.state == AskState::Cancelled
    {
        println!("cancelled");
        return Ok(0);
    }
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
        Response::ThreadHits { hits } if json => {
            println!("{}", serde_json::to_string_pretty(&hits).unwrap_or_default());
            Ok(0)
        },
        Response::ThreadHits { hits } => {
            for hit in &hits {
                println!(
                    "{}  pane {:>3}  {} · {}: {}",
                    clock(hit.at_ms),
                    hit.pane,
                    hit.title,
                    hit.kind,
                    hit.snippet
                );
            }
            Ok(if hits.is_empty() { 1 } else { 0 })
        },
        Response::Events { events, .. } => {
            for event in &events {
                println!("{}", serde_json::to_string(event).unwrap_or_default());
            }
            Ok(0)
        },
        Response::Started { id, ask } => {
            println!("{id}");
            eprintln!(
                "artty: its task is ask {ask} (artty ask --result {ask} waits for its answer)"
            );
            Ok(0)
        },
        Response::Text { text } => {
            println!("{text}");
            Ok(0)
        },
        Response::Ask { ask } => print_ask(&ask, json),
        Response::Approvals { approvals } if json => {
            println!("{}", serde_json::to_string_pretty(&approvals).unwrap_or_default());
            Ok(0)
        },
        Response::Approvals { approvals } => {
            for approval in &approvals {
                print_approval(approval);
            }
            Ok(0)
        },
        Response::Queue { items } if json => {
            println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
            Ok(0)
        },
        Response::Queue { items } => {
            for item in &items {
                let kind = match item.kind {
                    QueueKind::Approval => "permission",
                    QueueKind::Waiting => "waiting",
                    QueueKind::Reply => "reply",
                };
                let text = item.text.lines().next().unwrap_or_default();
                println!("{:>14}  pane {:<3} {:<10} {}", item.id, item.pane, kind, text);
            }
            Ok(0)
        },
        Response::Agents { agents } if json => {
            println!("{}", serde_json::to_string_pretty(&agents).unwrap_or_default());
            Ok(0)
        },
        Response::Agents { agents } => {
            for agent in &agents {
                let state = serde_json::to_value(agent.state).unwrap_or_default();
                let mut notes = Vec::new();
                if agent.pending > 0 {
                    notes.push(format!("{} asking permission", agent.pending));
                }
                if agent.unread > 0 {
                    notes.push(format!("{} unread", agent.unread));
                }
                println!(
                    "{:>4}  {:<8} {:<8} {} · {}{}",
                    agent.pane,
                    agent.agent,
                    state.as_str().unwrap_or_default(),
                    agent.workspace,
                    agent.title,
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!("  ({})", notes.join(", "))
                    }
                );
            }
            Ok(0)
        },
        Response::Thread { entries, .. } if json => {
            println!("{}", serde_json::to_string_pretty(&entries).unwrap_or_default());
            Ok(0)
        },
        Response::Thread { agent, entries, .. } => {
            let agent = agent.unwrap_or_else(|| "agent".to_owned());
            for entry in &entries {
                print!("{}", thread_entry(entry, &agent));
            }
            Ok(0)
        },
        Response::Decision { decision } => {
            println!("{}", serde_json::to_string(&decision).unwrap_or_default());
            Ok(0)
        },
        Response::Asks { asks } if json => {
            println!("{}", serde_json::to_string_pretty(&asks).unwrap_or_default());
            Ok(0)
        },
        Response::Asks { asks } => {
            for ask in asks {
                let state = serde_json::to_value(ask.state).unwrap_or_default();
                let text = ask.text.replace('\n', " ");
                println!(
                    "{:>14}  pane {:<3} {:<10} {text}",
                    ask.id,
                    ask.pane,
                    state.as_str().unwrap_or("")
                );
            }
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

/// An ask's reply (exit 0), why it failed (exit 1), or its id while it's not
/// over (exit 3: `artty ask --result ID` waits on).
fn print_ask(ask: &AskInfo, json: bool) -> Result<i32, String> {
    if json {
        println!("{}", serde_json::to_string_pretty(ask).unwrap_or_default());
    }
    match ask.state {
        AskState::Answered => {
            if let Some(warning) = &ask.warning {
                eprintln!("artty: ask {}: {warning}", ask.id);
            }
            if !json {
                println!("{}", ask.reply.as_deref().unwrap_or_default());
            }
            Ok(0)
        },
        AskState::Failed => {
            Err(format!("ask {}: {}", ask.id, ask.error.as_deref().unwrap_or("failed")))
        },
        AskState::Cancelled => Err(format!("ask {}: cancelled", ask.id)),
        state => {
            if !json {
                println!("{}", ask.id);
                let state = serde_json::to_value(state).unwrap_or_default();
                eprintln!(
                    "artty: ask {} is {}; artty ask --result {} waits for its reply",
                    ask.id,
                    state.as_str().unwrap_or("under way"),
                    ask.id
                );
            }
            Ok(3)
        },
    }
}

/// An agent that asked, as "api (claude, pane 3, ask 17…)".
fn sender(from: &artty_core::control::AskFrom, ask: &str) -> String {
    match &from.agent {
        Some(agent) => format!("{} ({agent}, pane {}, {ask})", from.title, from.pane),
        None => format!("{} (pane {}, {ask})", from.title, from.pane),
    }
}

/// A thread's entry for reading: its time, a mark, and what it says (later
/// lines indented under the first).
fn thread_entry(entry: &ThreadEntry, agent: &str) -> String {
    let (mark, head, text) = match &entry.item {
        ThreadItem::Prompt { text, ask: Some(id), from: Some(from), .. } => {
            ("›", sender(from, &format!("ask {id}")), text.clone())
        },
        ThreadItem::Prompt { text, ask: Some(id), from: None, .. } => {
            ("›", format!("you (ask {id})"), text.clone())
        },
        ThreadItem::Prompt { text, ask: None, .. } => ("›", "you".to_owned(), text.clone()),
        ThreadItem::Reply { text } => ("‹", agent.to_owned(), text.clone()),
        ThreadItem::Edited { files } => {
            let list: Vec<String> = files
                .iter()
                .map(|f| match f.written {
                    true => format!("{} (written, {} lines)", f.path, f.added),
                    false => format!("{} +{} -{}", f.path, f.added, f.removed),
                })
                .collect();
            let head = match files.len() {
                1 => "edited a file".to_owned(),
                n => format!("edited {n} files"),
            };
            ("✎", head, list.join("\n"))
        },
        ThreadItem::Notice { tag, summary, .. } => ("⚙", tag.clone(), summary.clone()),
        ThreadItem::Overlap { file, other } => {
            ("!", "overlap".to_owned(), format!("{} also changed {file}", other.title))
        },
        ThreadItem::Handoff { ask, to, text, state, reply, error, forwarded, .. } => {
            let state = serde_json::to_value(state).unwrap_or_default();
            let mut body = text.clone();
            if let Some(reply) = reply {
                body.push_str(&format!("\n‹ {}: {reply}", to.title));
            }
            if let Some(error) = error {
                body.push_str(&format!("\n({error})"));
            }
            if let Some(forwarded) = forwarded {
                body.push_str(&format!("\n(brought back: ask {forwarded})"));
            }
            let head =
                format!("to {} (ask {ask}), {}", to.title, state.as_str().unwrap_or_default());
            ("→", head, body)
        },
        ThreadItem::Permission { id, summary, state, .. } => {
            let state = serde_json::to_value(state).unwrap_or_default();
            let state = state.as_str().unwrap_or_default().to_owned();
            ("⚑", format!("permission {id}, {state}"), summary.clone())
        },
        ThreadItem::Waiting { message } => ("!", "waiting".to_owned(), message.clone()),
        ThreadItem::Session { event } => ("·", format!("session {event}"), String::new()),
        ThreadItem::Interrupted { reason } => ("✕", "interrupted".to_owned(), reason.clone()),
    };
    let mut lines = text.lines();
    let first = lines.next().map(|l| format!(": {l}")).unwrap_or_default();
    let head = if entry.rewound { format!("{head} (rewound)") } else { head };
    let mut out = format!("{}  {mark} {head}{first}\n", clock(entry.at_ms));
    for line in lines {
        out.push_str(&format!("          {line}\n"));
    }
    out
}

/// A Unix time in milliseconds as local "HH:MM".
fn clock(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: `tm` is written by localtime_r before it's read.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "--:--".to_owned();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn print_approval(approval: &ApprovalInfo) {
    let state = serde_json::to_value(approval.state).unwrap_or_default();
    println!(
        "{:>14}  pane {:<3} {:<14} {:>5}  {}",
        approval.id,
        approval.pane,
        state.as_str().unwrap_or(""),
        duration(approval.age_secs),
        approval.summary
    );
    for (index, option) in approval.options.iter().enumerate() {
        println!("{:>14}  --always {index}: {option}", "");
    }
}

/// An agent's hook event, as JSON on stdin.
fn hook_input() -> Option<Value> {
    let mut input = String::new();
    // Big enough for a file's worth of tool input.
    let _ = std::io::stdin().take(16 << 20).read_to_string(&mut input);
    serde_json::from_str(&input).ok()
}

/// Notifications that mean the agent needs you: all but those (Claude
/// Code's) known not to, so a new kind still shows.
fn needs_you(kind: Option<&str>) -> bool {
    !matches!(
        kind,
        Some(
            "idle_prompt"
                | "auth_success"
                | "agent_completed"
                | "elicitation_complete"
                | "elicitation_response"
        )
    )
}

/// `artty approve --hook`: an agent's permission request, put to artty
/// while the agent shows its own prompt. Prints a decision only when
/// someone made one in artty; anything else (the prompt answered there,
/// artty not running, an error) prints nothing, and the agent's prompt
/// decides. Exits 0 regardless: a failing hook mustn't decide either.
fn approve(agent: Option<String>) -> Result<i32, String> {
    // An agent run by `artty acp`: its requests come from that.
    if std::env::var_os(crate::acp_host::HOST_ENV).is_some() {
        return Ok(0);
    }
    let Some(event) = hook_input() else { return Ok(0) };
    let tool_name = event.get("tool_name").and_then(Value::as_str).unwrap_or("a tool").to_owned();
    let tool_input = event.get("tool_input").cloned().unwrap_or(Value::Null);
    let suggestions =
        event.get("permission_suggestions").and_then(Value::as_array).cloned().unwrap_or_default();
    let request = Request::Approve { tab: own_tab(), agent, tool_name, tool_input, suggestions };
    let Ok(Response::Decision { decision: Some(decision) }) = send(&request, None) else {
        return Ok(0);
    };
    let decision = if decision.allow && !decision.permissions.is_empty() {
        json!({ "behavior": "allow", "updatedPermissions": decision.permissions })
    } else if decision.allow {
        json!({ "behavior": "allow" })
    } else {
        let message = decision.message.unwrap_or_else(|| "Denied in artty".to_owned());
        json!({ "behavior": "deny", "message": message })
    };
    let output = json!({
        "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
    });
    println!("{output}");
    Ok(0)
}

/// The message and session id of an agent's hook event on stdin (Claude
/// Code, Codex and Gemini CLI all send `session_id`), and what tells its
/// turns apart.
fn hook_event() -> (Option<String>, Option<String>, Option<HookEvent>) {
    let Some(event) = hook_input() else { return (None, None, None) };
    let text = |key: &str| event.get(key).and_then(Value::as_str).map(str::to_owned);
    let used = text("hook_event_name").as_deref() == Some("PostToolUse");
    let hook = text("hook_event_name").map(|name| HookEvent {
        event: name,
        prompt: text("prompt"),
        prompt_id: text("prompt_id"),
        // Claude Code's Stop, and Codex's.
        reply: text("last_assistant_message"),
        source: text("source"),
        // Which permission request a tool's use answers.
        tool_name: used.then(|| text("tool_name")).flatten(),
        tool_input: used.then(|| event.get("tool_input").cloned()).flatten(),
        notification_type: text("notification_type"),
    });
    (hook_message(&event), text("session_id"), hook)
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

pub(crate) fn socket() -> Result<PathBuf, String> {
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

/// Wait for an ask's reply, for at most `timeout_secs`.
fn wait_for_ask(id: u64, timeout_secs: Option<u64>) -> Result<Response, String> {
    let again = format!("artty ask --result {id}");
    keep_waiting(timeout_secs, &again, |left| Request::AskResult {
        id,
        wait: true,
        timeout_secs: left,
    })
}

/// Send what `request` makes (given the seconds left), waiting at most
/// `timeout_secs` in all. The server keeps asks across a hand-over to a new
/// build (which drops connections) and a restart: ask again once it's
/// back (`again`: how to, if it isn't).
fn keep_waiting(
    timeout_secs: Option<u64>,
    again: &str,
    request: impl Fn(Option<u64>) -> Request,
) -> Result<Response, String> {
    let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_secs());
        let wait = request(left);
        match send(&wait, left.map(|s| Duration::from_secs(s + 5))) {
            Err(err) if err.starts_with(CLOSED) => {},
            other => return other,
        }
        // Back within a few seconds, or not coming back.
        let back = Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if send(&Request::ServerStatus, Some(Duration::from_secs(2))).is_ok() {
                break;
            }
            if Instant::now() > back || deadline.is_some_and(|d| Instant::now() > d) {
                return Err(format!("{CLOSED}; {again} waits again"));
            }
        }
    }
}

const CLOSED: &str = "the server closed the connection";

/// `artty wait`: each ask, its answer (or how far it got); exit 0 once
/// what was waited for came (the first over, or `all`), else 3.
fn print_waited(response: Response, all: bool, json: bool) -> Result<i32, String> {
    let asks = match response {
        Response::Asks { asks } => asks,
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected answer: {other:?}")),
    };
    let over = |a: &AskInfo| a.state.finished();
    let came = if all { asks.iter().all(over) } else { asks.iter().any(over) };
    if json {
        println!("{}", serde_json::to_string_pretty(&asks).unwrap_or_default());
    } else {
        for ask in &asks {
            let state = serde_json::to_value(ask.state).unwrap_or_default();
            let state = state.as_str().unwrap_or_default().to_owned();
            println!("ask {} (pane {}): {state}", ask.id, ask.pane);
            let said = ask.reply.as_deref().or(ask.error.as_deref()).unwrap_or_default();
            for line in said.lines() {
                println!("  {line}");
            }
        }
    }
    Ok(if came { 0 } else { 3 })
}

/// `artty events`: thread entries as they come (every pane's, or `tab`'s),
/// a JSON object a line (`text`: as `artty thread` shows them). Without
/// `follow`, those after `after`; with it, on and on (from now, or after).
fn events(
    after: Option<u64>,
    tab: Option<SessionId>,
    follow: bool,
    text: bool,
) -> Result<i32, String> {
    use std::io::Write;
    let mut after = match (after, follow) {
        (Some(after), _) => after,
        (None, true) => {
            match send(&Request::Events { after: None, tab, timeout_secs: None }, None)? {
                Response::Events { last, .. } => last,
                Response::Error { message } => return Err(message),
                _ => 0,
            }
        },
        (None, false) => return Err("events: --after SEQ, or --follow".into()),
    };
    let mut names: std::collections::HashMap<SessionId, String> = Default::default();
    loop {
        let wait = if follow { Some(30) } else { Some(0) };
        let request = Request::Events { after: Some(after), tab, timeout_secs: wait };
        let response = match send(&request, Some(Duration::from_secs(40))) {
            Err(err) if follow && err.starts_with(CLOSED) => {
                // A hand-over: back in a moment, with the entries kept.
                std::thread::sleep(Duration::from_millis(500));
                continue;
            },
            other => other?,
        };
        let (found, last) = match response {
            Response::Events { events, last } => (events, last),
            Response::Error { message } => return Err(message),
            _ => return Err("unexpected answer".into()),
        };
        // Each pane's agent, by name (asked again for one not yet known).
        if text
            && found.iter().any(|e| !names.contains_key(&e.pane))
            && let Ok(Response::Agents { agents }) =
                send(&Request::Agents, Some(Duration::from_secs(5)))
        {
            names.extend(agents.into_iter().map(|a| (a.pane, a.agent)));
        }
        let mut out = std::io::stdout().lock();
        for event in &found {
            let line = match text {
                true => {
                    let agent = names.get(&event.pane).map_or("agent", String::as_str);
                    let entry = thread_entry(&event.entry, agent);
                    format!("pane {:>3}  {}", event.pane, entry.trim_end())
                },
                false => serde_json::to_string(event).unwrap_or_default(),
            };
            // A reader gone (a pipe closed): done.
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                return Ok(0);
            }
        }
        after = last.max(after);
        if !follow {
            return Ok(0);
        }
    }
}

pub(crate) fn send(request: &Request, timeout: Option<Duration>) -> Result<Response, String> {
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
    if reply.is_empty() {
        return Err(format!("{CLOSED} (stopped, or restarted)"));
    }
    serde_json::from_str(&reply).map_err(|err| format!("bad reply: {err}"))
}

/// `artty guide`: the guide for agents working with other agents; `install`
/// adds a Claude Code skill pointing to it, `uninstall` takes it out.
fn guide(action: Option<&str>) -> Result<i32, String> {
    use artty_core::guide;
    let skill = guide::skill_path().ok_or("no HOME")?;
    match action {
        None => {
            print!("{}", guide::GUIDE);
            Ok(0)
        },
        Some("install") => {
            let dir = skill.parent().unwrap();
            std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            std::fs::write(&skill, guide::SKILL)
                .map_err(|err| format!("{}: {err}", skill.display()))?;
            println!("Claude Code: added the artty skill ({})", skill.display());
            println!(
                "Claude Code asks before each artty command an agent runs. To let agents ask \
                 each other without that, allow them in its settings (permissions.allow):\n  \
                 \"Bash(artty agents:*)\", \"Bash(artty ask:*)\", \"Bash(artty thread:*)\", \
                 \"Bash(artty guide)\""
            );
            Ok(0)
        },
        Some("uninstall") => {
            match std::fs::remove_file(&skill) {
                Ok(()) => {
                    let _ = std::fs::remove_dir(skill.parent().unwrap());
                    println!("Claude Code: took out the artty skill");
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    println!("Claude Code: the artty skill isn't installed");
                },
                Err(err) => return Err(format!("{}: {err}", skill.display())),
            }
            Ok(0)
        },
        Some(other) => Err(format!("guide: unknown action {other} (install, uninstall)")),
    }
}

/// `artty hooks` subcommands: show, status, install, uninstall.
/// `artty remote`: other machines' servers, in the config file.
/// `artty attach`: a window of this machine's server (or another's, over
/// ssh) in this terminal, until detached.
fn attach(args: &[String]) -> Result<i32, String> {
    use artty_client::attach::{self, Ended, KeyPress, Options};
    let mut window = None;
    let (mut new, mut list) = (false, false);
    let mut prefix = std::env::var("ARTTY_PREFIX").ok().filter(|p| !p.is_empty());
    let mut server = artty_core::ServerConfig::default();
    let mut remote_name = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().cloned().ok_or(format!("attach: {arg} needs a value"));
        match arg.as_str() {
            // Windows are shared anyway (as they once weren't without it).
            "--share" => {},
            "--new" => new = true,
            "--list" | "-l" => list = true,
            "--prefix" => prefix = Some(value()?),
            "--ssh" => server.ssh = Some(value()?),
            "--artty" => server.artty = Some(value()?),
            "--remote" => remote_name = Some(value()?),
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(0);
            },
            flag if flag.starts_with('-') => return Err(format!("attach: unknown option {flag}")),
            number => {
                let number = number.parse().map_err(|_| format!("attach: not a window: {arg}"))?;
                window = Some(number);
            },
        }
    }
    let prefix = prefix.as_deref().unwrap_or("ctrl-b");
    let prefix = KeyPress::parse(prefix).ok_or(format!("attach: not a key: {prefix}"))?;
    if let Some(name) = remote_name {
        server = artty_core::Config::load()
            .0
            .servers
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("no server named {name} (see `artty remote list`)"))?;
    }
    let local = server.ssh.is_none() && server.command.is_empty();
    let link = if local {
        // Inside one of its panes, it would show itself.
        if own_tab().is_some() && !list {
            return Err(format!(
                "attach: inside artty already (to attach anyway, unset {SESSION_ENV})"
            ));
        }
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        if !artty_server::running() {
            artty_server::spawn(&exe, &[]).map_err(|err| format!("server: {err}"))?;
        }
        let stream = UnixStream::connect(artty_core::control::server_socket_path())
            .map_err(|err| format!("server: {err}"))?;
        artty_client::Link::socket(stream)?
    } else {
        let mut command = server.connect_command().ok_or("attach: no ssh or command")?;
        // ssh may ask for a password here, before the terminal is taken over.
        if let Some(at) = command.windows(2).position(|w| w[0] == "-o" && w[1] == "BatchMode=yes") {
            command.drain(at..at + 2);
        }
        artty_client::Link::command(&command)?
    };
    if list {
        for w in attach::windows(link)? {
            let shown = match w.apps {
                0 => "not shown".to_owned(),
                1 => "shown in 1 app".to_owned(),
                n => format!("shown in {n} apps"),
            };
            let tabs = if w.tabs == 1 { "1 tab".to_owned() } else { format!("{} tabs", w.tabs) };
            println!("{:>4}  {tabs}, {shown}: {}", w.window, w.titles.join(", "));
        }
        return Ok(0);
    }
    match attach::run(link, Options { window, new, prefix })? {
        Ended::Detached(window) => println!("[detached from window {window}]"),
        Ended::Closed(window) => println!("[window {window} closed]"),
    }
    Ok(0)
}

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
