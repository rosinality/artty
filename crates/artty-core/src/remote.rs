//! Reaching another machine's artty server: a command (ssh, usually)
//! whose stdin and stdout carry the connection, ending at `artty server
//! --stdio` there.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crate::ServerConfig;
use crate::protocol::{Hello, ServerMessage, read_message};

/// How [`open`]'s error starts when artty isn't there.
pub const NOT_INSTALLED: &str = "artty isn't installed there";

/// A started connection: its process, its two ends, and the greeting.
pub struct Opened {
    pub child: std::process::Child,
    pub reader: std::io::BufReader<std::process::ChildStdout>,
    pub writer: std::process::ChildStdin,
    pub welcome: ServerMessage,
}

/// How ssh may ask for what logging in needs (a password, a code, whether
/// to trust a machine's key).
pub enum Prompts<'a> {
    /// Not at all: ssh fails instead.
    Never,
    /// On the terminal this runs in (the `artty` command).
    Terminal,
    /// Through a program ssh runs for each question (`SSH_ASKPASS`): the
    /// app's, which asks in a window.
    Helper(&'a Askpass),
}

/// The app's way of asking for ssh: `program` run as `SSH_ASKPASS`, told
/// where to ask (`socket`, with `token` as proof it's ssh started by the
/// app) and for what (`name`); `key` tells this connection's questions
/// apart, so its wait for a greeting pauses while one is open
/// ([`set_prompting`]).
pub struct Askpass {
    pub program: std::path::PathBuf,
    pub socket: std::path::PathBuf,
    pub token: String,
    pub name: String,
    pub key: u64,
    /// Told what logging in says, as it goes.
    pub watch: Option<LoginWatch>,
}

/// What logging in says, as it goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Login {
    /// A line from ssh (or from the server, through it).
    Said(String),
    /// It said something, then nothing for a while, without getting
    /// through: it waits for someone — a sign-in in the browser, a push to
    /// approve, a key to touch. What it said (without ssh's routine
    /// notices), the address to sign in at if it gave one, and its
    /// process (to end the attempt).
    Stalled { said: Vec<String>, url: Option<String>, pid: i32 },
}

/// Where what logging in says goes.
pub type LoginWatch = Arc<dyn Fn(Login) + Send + Sync>;

/// How long after its last word a login that hasn't got through is taken
/// to be waiting for someone (a banner is followed by more, at once).
const STALL: Duration = Duration::from_secs(3);

/// How long an attempt waits for the greeting: in all, and while a login
/// waits for someone who's been told.
const GREETING_TIME: Duration = Duration::from_secs(30);
const WAITING_FOR_YOU: Duration = Duration::from_secs(600);

/// How [`open`]'s error starts when the attempt got no greeting in time.
pub const TIMED_OUT: &str = "no answer in time";

/// ssh's routine word, not a login waiting for anything.
pub fn routine(line: &str) -> bool {
    line.trim().is_empty() || line.starts_with("Warning: Permanently added")
}

/// The address in what ssh passes on from the server while logging in,
/// when it asks for a sign-in in the browser — Tailscale SSH's check mode:
/// "To authenticate, visit: https://login.tailscale.com/a/…". It waits
/// (without a word more) until that's done.
pub fn sign_in_url(line: &str) -> Option<&str> {
    let lower = line.to_lowercase();
    if !["visit", "authenticate", "sign in", "log in", "login"].iter().any(|w| lower.contains(w)) {
        return None;
    }
    let at = line.find("https://")?;
    let url = line[at..].split_whitespace().next()?;
    Some(url.trim_end_matches(['.', ',', ')', '"', '\'']))
}

/// Connections with a question open, by [`Askpass::key`].
static PROMPTING: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

/// Whether the connection `key` has a question open (the user may take a
/// while to answer: its wait for a greeting pauses meanwhile).
pub fn set_prompting(key: u64, open: bool) {
    let mut prompting = PROMPTING.lock().unwrap_or_else(|p| p.into_inner());
    prompting.retain(|&k| k != key);
    if open {
        prompting.push(key);
    }
}

fn prompting(key: u64) -> bool {
    PROMPTING.lock().unwrap_or_else(|p| p.into_inner()).contains(&key)
}

/// The environment telling the askpass helper (`artty` run by ssh) where to
/// ask; [`askpass_request`] reads it back.
pub const ASKPASS_SOCKET: &str = "ARTTY_ASKPASS_SOCKET";
const ASKPASS_TOKEN: &str = "ARTTY_ASKPASS_TOKEN";
const ASKPASS_NAME: &str = "ARTTY_ASKPASS_NAME";
const ASKPASS_KEY: &str = "ARTTY_ASKPASS_KEY";

/// A question ssh asks, as the helper passes it to the app.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AskpassRequest {
    pub token: String,
    /// The machine's name, as shown.
    pub name: String,
    pub key: u64,
    pub prompt: String,
    /// `SSH_ASKPASS_PROMPT`: `confirm` (yes or no), `none` (only a notice,
    /// as "touch your security key"), or empty (an answer to type).
    pub kind: String,
}

/// The app's answer: what to tell ssh, or nothing (cancelled).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AskpassAnswer {
    pub answer: Option<String>,
}

/// Running as ssh's askpass helper (with `prompt`): the question, with
/// where to ask it, if this process is one.
pub fn askpass_request(prompt: &str) -> Option<(std::path::PathBuf, AskpassRequest)> {
    let socket = std::env::var_os(ASKPASS_SOCKET)?;
    std::env::var_os("SSH_ASKPASS")?;
    let request = AskpassRequest {
        token: std::env::var(ASKPASS_TOKEN).ok()?,
        name: std::env::var(ASKPASS_NAME).unwrap_or_default(),
        key: std::env::var(ASKPASS_KEY).ok()?.parse().ok()?,
        prompt: prompt.to_owned(),
        kind: std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default(),
    };
    Some((socket.into(), request))
}

/// A process running `command` (ssh, or what reaches a server), finding
/// programs where the user's shell would ([`crate::tool_path`]): ssh runs
/// a `ProxyCommand` with this PATH.
pub fn ssh_process(command: &[String]) -> std::process::Command {
    let mut process = std::process::Command::new(&command[0]);
    process.args(&command[1..]).env("PATH", crate::tool_path());
    process
}

/// `command` with ssh allowed to ask as `prompts` says.
fn prompting_command(mut command: Vec<String>, prompts: &Prompts) -> std::process::Command {
    if !matches!(prompts, Prompts::Never)
        && let Some(at) = command.windows(2).position(|w| w[0] == "-o" && w[1] == "BatchMode=yes")
    {
        command.drain(at..at + 2);
    }
    let mut process = ssh_process(&command);
    if let Prompts::Helper(askpass) = prompts {
        process
            .env("SSH_ASKPASS", &askpass.program)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ASKPASS_SOCKET, &askpass.socket)
            .env(ASKPASS_TOKEN, &askpass.token)
            .env(ASKPASS_NAME, &askpass.name)
            .env(ASKPASS_KEY, askpass.key.to_string());
    }
    process
}

/// What a login has said that may be for someone.
#[derive(Default)]
struct Talk {
    lines: Vec<String>,
    /// When it last said something.
    last: Option<std::time::Instant>,
    /// How many lines someone has been told.
    told: usize,
}

/// Run the command reaching the server and wait for its greeting; ssh may
/// ask for a login as `prompts` says.
pub fn open(config: &ServerConfig, prompts: Prompts) -> Result<Opened, String> {
    use std::io::BufRead;
    use std::process::Stdio;
    let command = config.connect_command().ok_or("set ssh (or command) for it in the settings")?;
    let program = command[0].clone();
    let key = match &prompts {
        Prompts::Helper(askpass) => Some(askpass.key),
        _ => None,
    };
    let watch = match &prompts {
        Prompts::Helper(askpass) => askpass.watch.clone(),
        _ => None,
    };
    let terminal = matches!(prompts, Prompts::Terminal);
    let mut child = prompting_command(command, &prompts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{program}: {err}"))?;
    // What it says on stderr is the reason, when it fails (its last few
    // lines: fish follows its error with the command and a marker).
    let said = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    // And what it's said that may be for someone, and when it last did.
    let talk = Arc::new(std::sync::Mutex::new(Talk::default()));
    if let Some(stderr) = child.stderr.take() {
        let (said, talk, watch) = (said.clone(), talk.clone(), watch.clone());
        let name = config.name.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                log::info!("server {name}: {line}");
                if let Some(watch) = &watch {
                    watch(Login::Said(line.clone()));
                }
                if !line.trim().is_empty() {
                    let mut said = said.lock().unwrap();
                    if said.len() == 4 {
                        said.remove(0);
                    }
                    said.push(line.trim().to_owned());
                }
                if !routine(&line) {
                    let mut talk = talk.lock().unwrap();
                    if talk.lines.len() == 40 {
                        talk.lines.remove(0);
                    }
                    talk.lines.push(line.trim_start_matches(['#', ' ']).trim_end().to_owned());
                    talk.last = Some(std::time::Instant::now());
                }
            }
        });
    }
    // No greeting in time (a login asking for something, a hung network):
    // give up on this attempt. A login that went quiet after saying
    // something waits for someone: they're told (if anyone can be), and it
    // has longer.
    let greeted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (greeted, timed_out, pid) = (greeted.clone(), timed_out.clone(), child.id() as i32);
        let tellable = watch.is_some() || terminal;
        std::thread::spawn(move || {
            const TICK: Duration = Duration::from_millis(500);
            let (mut waited, mut waiting_for_you) = (Duration::ZERO, Duration::ZERO);
            while waited < GREETING_TIME && waiting_for_you < WAITING_FOR_YOU {
                std::thread::sleep(TICK);
                if greeted.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let stalled = {
                    let mut talk = talk.lock().unwrap();
                    let stalled = tellable && talk.last.is_some_and(|at| at.elapsed() >= STALL);
                    // Told once for each new word.
                    if stalled && talk.told < talk.lines.len() {
                        let new = &talk.lines[talk.told..];
                        if terminal {
                            for line in new {
                                eprintln!("{line}");
                            }
                        }
                        if let Some(watch) = &watch {
                            watch(Login::Stalled {
                                said: talk.lines.clone(),
                                url: talk
                                    .lines
                                    .iter()
                                    .rev()
                                    .find_map(|l| sign_in_url(l))
                                    .map(str::to_owned),
                                pid,
                            });
                        }
                        talk.told = talk.lines.len();
                    }
                    stalled
                };
                // (Not counting time a question waits for the user.)
                if stalled {
                    waiting_for_you += TICK;
                } else if !key.is_some_and(prompting) {
                    waited += TICK;
                }
            }
            timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
            // SAFETY: plain syscall on our own child.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        });
    }
    let mut writer = child.stdin.take().ok_or("no stdin")?;
    let mut reader = std::io::BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let hello = serde_json::to_string(&Hello::Attach {
        version: crate::protocol::protocol_version(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .map_err(|err| err.to_string())?;
    let welcome = writer
        .write_all(format!("{hello}\n").as_bytes())
        .and_then(|()| writer.flush())
        .and_then(|()| read_message::<ServerMessage>(&mut reader));
    greeted.store(true, std::sync::atomic::Ordering::SeqCst);
    match welcome {
        Ok(welcome) => Ok(Opened { child, reader, writer, welcome }),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            // Let the last of stderr arrive.
            std::thread::sleep(Duration::from_millis(100));
            let said = said.lock().unwrap().clone();
            // zsh, bash: "command not found" (or for a path, "no such
            // file"); fish: "Unknown command".
            let missing = said.iter().find(|line| {
                let line = line.to_lowercase();
                line.contains("artty")
                    && ["not found", "unknown command", "no such file"]
                        .iter()
                        .any(|missing| line.contains(missing))
            });
            if let Some(missing) = missing {
                return Err(format!("{NOT_INSTALLED} ({missing})"));
            }
            let timed_out = timed_out.load(std::sync::atomic::Ordering::SeqCst);
            match said.last() {
                Some(said) if timed_out => Err(format!("{TIMED_OUT}: {said}")),
                None if timed_out => Err(TIMED_OUT.to_owned()),
                Some(said) => Err(said.clone()),
                None => Err(format!("no answer: {err}")),
            }
        },
    }
}

/// After [`install`]: whether the login shell finds what was installed as
/// `artty` (else the config should name it).
pub struct Installed {
    pub on_path: bool,
}

/// Where [`install`] puts artty there.
pub const INSTALLED_AT: &str = "$HOME/.local/bin/artty";

impl Installed {
    /// What a server's `artty` setting should become so the connection
    /// runs what was installed, if it should change: unset when the login
    /// shell finds it as `artty`, else its path.
    pub fn artty_setting(&self, current: Option<&str>) -> Option<Option<String>> {
        let wanted = (!self.on_path).then(|| INSTALLED_AT.to_owned());
        (current.is_some_and(|c| c != INSTALLED_AT) || current.is_none() && wanted.is_some())
            .then_some(wanted)
    }
}

/// Installing artty on a machine, step by step, for whatever runs commands
/// there (`ssh` here; a phone's own ssh): run [`PROBE`], read it with
/// [`linux_cpu`], send the build for that CPU to [`UPLOAD`]'s stdin, run
/// [`MOVE_IN`], then [`FIND`], read with [`installed`]. Linux with glibc
/// 2.28 or later only.
pub mod installing {
    use super::Installed;

    /// What it runs: Linux on which CPU, with which C library.
    pub const PROBE: &str = "uname -sm; ldd --version 2>&1 | head -n 1";
    /// Takes the build on stdin.
    pub const UPLOAD: &str =
        "sh -c 'mkdir -p \"$HOME/.local/bin\" && cat > \"$HOME/.local/bin/artty.new\"'";
    /// Puts it in place, moving a running server onto it (its programs keep running).
    pub const MOVE_IN: &str = "sh -c 'set -e; b=\"$HOME/.local/bin\"; chmod 755 \"$b/artty.new\"; \
         mv \"$b/artty.new\" \"$b/artty\"; \
         if \"$b/artty\" server status >/dev/null 2>&1; then \"$b/artty\" server restart; fi'";
    /// Found as `artty` the way a connection looks for it?
    pub const FIND: &str = "exec \"$SHELL\" -lc 'command -v artty; echo \"$HOME\"'";

    /// From what [`PROBE`] printed: the CPU to send a build for (`x86_64`
    /// or `aarch64`), or why it can't take one.
    pub fn linux_cpu(probe: &str) -> Result<&'static str, String> {
        let mut lines = probe.lines();
        let system = lines.next().unwrap_or_default();
        let libc = lines.next().unwrap_or_default();
        let cpu = match system.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["Linux", "x86_64"] => "x86_64",
            ["Linux", "aarch64"] => "aarch64",
            _ => return Err(format!("it runs {system}: only Linux on x86_64 or aarch64")),
        };
        let glibc = ["GLIBC", "GNU libc", "GNU C Library"].iter().any(|name| libc.contains(name));
        let version = libc
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .filter_map(|word| {
                let (major, minor) = word.split_once('.')?;
                Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
            })
            .next_back();
        if !glibc || version.is_none_or(|version| version < (2, 28)) {
            return Err(format!("its C library isn't glibc 2.28 or later ({libc})"));
        }
        Ok(cpu)
    }

    /// From what [`FIND`] printed.
    pub fn installed(found: &str) -> Installed {
        let mut lines = found.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>();
        let home = lines.pop().unwrap_or_default();
        let on_path = lines.first().is_some_and(|path| *path == format!("{home}/.local/bin/artty"));
        Installed { on_path }
    }
}

/// Install this build of artty on the machine `config` reaches by ssh: the
/// build for its CPU (from `build`, given `x86_64` or `aarch64`), into
/// `~/.local/bin` (see [`installing`]).
pub fn install(
    config: &ServerConfig,
    prompts: Prompts,
    build: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> Result<Installed, String> {
    // (Only this may need a login: the rest shares its connection.)
    let cpu = installing::linux_cpu(&run_asking(config, installing::PROBE, None, &prompts)?)?;
    let build = build(cpu).ok_or_else(|| format!("this app has no build for Linux on {cpu}"))?;
    let file = std::fs::File::open(&build).map_err(|err| format!("{}: {err}", build.display()))?;
    run(config, installing::UPLOAD, Some(file))?;
    run(config, installing::MOVE_IN, None)?;
    Ok(installing::installed(&run(config, installing::FIND, None)?))
}

/// Run `remote` there by ssh (with `stdin`); what it printed, or why it
/// failed.
fn run(
    config: &ServerConfig,
    remote: &str,
    stdin: Option<std::fs::File>,
) -> Result<String, String> {
    run_asking(config, remote, stdin, &Prompts::Never)
}

fn run_asking(
    config: &ServerConfig,
    remote: &str,
    stdin: Option<std::fs::File>,
    prompts: &Prompts,
) -> Result<String, String> {
    use std::process::Stdio;
    let command = config.ssh_command(remote).ok_or("it isn't reached by ssh")?;
    let output = prompting_command(command, prompts)
        .stdin(stdin.map_or_else(Stdio::null, Stdio::from))
        .output()
        .map_err(|err| format!("ssh: {err}"))?;
    if !output.status.success() {
        return Err(last_said(&String::from_utf8_lossy(&output.stderr)).unwrap_or("failed".into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The last thing a failed command said on stderr: its last line with words
/// — not the source line a shell quotes under its error, with `^~~^`
/// pointing into it (as fish does), nor an empty `fish:`.
pub fn last_said(stderr: &str) -> Option<String> {
    let mut lines: Vec<&str> =
        stderr.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    if lines.last().is_some_and(|line| line.chars().all(|c| matches!(c, '^' | '~' | ' '))) {
        lines.pop();
        lines.pop();
    }
    lines
        .into_iter()
        .rfind(|line| line.chars().any(char::is_alphanumeric) && !line.ends_with(':'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_failed_command_said() {
        let fish = "fish: Unknown command: ldd\nfish: \nuname -sm; ldd --version 2>&1\n           ^~^\n";
        assert_eq!(last_said(fish).as_deref(), Some("fish: Unknown command: ldd"));
        let bash = "bash: line 1: ldd: command not found\n";
        assert_eq!(last_said(bash).as_deref(), Some("bash: line 1: ldd: command not found"));
        assert_eq!(last_said("one\ntwo\n\n").as_deref(), Some("two"));
        assert_eq!(last_said("  \n"), None);
    }

    #[test]
    fn ssh_asks_only_when_it_may() {
        let config = ServerConfig { ssh: Some("me@devbox".into()), ..Default::default() };
        let args = |prompts: &Prompts| -> Vec<String> {
            let command = config.connect_command().unwrap();
            prompting_command(command, prompts)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert!(args(&Prompts::Never).iter().any(|a| a == "BatchMode=yes"));
        assert!(!args(&Prompts::Terminal).iter().any(|a| a == "BatchMode=yes"));
        let askpass = Askpass {
            program: "/a/artty".into(),
            socket: "/s".into(),
            token: "t".into(),
            name: "devbox".into(),
            key: 3,
            watch: None,
        };
        let command =
            prompting_command(config.connect_command().unwrap(), &Prompts::Helper(&askpass));
        let env: Vec<_> =
            command.get_envs().map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned()))).collect();
        assert!(
            env.iter()
                .any(|(k, v)| k == "SSH_ASKPASS_REQUIRE" && v.as_deref() == Some("force".as_ref()))
        );
        assert!(
            env.iter().any(|(k, v)| k == "ARTTY_ASKPASS_KEY" && v.as_deref() == Some("3".as_ref()))
        );
    }

    #[test]
    fn the_setting_follows_what_was_installed() {
        let on_path = Installed { on_path: true };
        let off_path = Installed { on_path: false };
        assert_eq!(on_path.artty_setting(None), None);
        assert_eq!(off_path.artty_setting(None), Some(Some(INSTALLED_AT.to_owned())));
        assert_eq!(off_path.artty_setting(Some(INSTALLED_AT)), None);
        // Named elsewhere: what was installed instead.
        assert_eq!(on_path.artty_setting(Some("/opt/artty")), Some(None));
        assert_eq!(off_path.artty_setting(Some("/opt/artty")), Some(Some(INSTALLED_AT.to_owned())));
    }

    #[test]
    fn a_sign_in_in_the_browser_is_noticed() {
        assert_eq!(
            sign_in_url("# To authenticate, visit: https://login.tailscale.com/a/l1da9c4b01451ee"),
            Some("https://login.tailscale.com/a/l1da9c4b01451ee")
        );
        assert_eq!(
            sign_in_url("Please log in at https://example.com/device."),
            Some("https://example.com/device")
        );
        // An address, but not asking for anything.
        assert_eq!(sign_in_url("See https://example.com for the docs"), None);
        assert_eq!(sign_in_url("Permission denied (publickey)."), None);
    }

    /// `open` of a "server" that's `script` (sh), told what it says.
    fn open_script(script: &str) -> (Result<Opened, String>, Vec<Login>) {
        let config = ServerConfig {
            name: "test".into(),
            command: vec!["sh".into(), "-c".into(), script.into()],
            ..Default::default()
        };
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let askpass = Askpass {
            program: "/nonexistent".into(),
            socket: "/nonexistent".into(),
            token: "t".into(),
            name: "test".into(),
            key: 990_001,
            watch: Some({
                let heard = heard.clone();
                Arc::new(move |login: Login| {
                    // Waiting for someone who isn't there: end it.
                    if let Login::Stalled { pid, .. } = &login {
                        // SAFETY: plain syscall on the test's own child.
                        unsafe { libc::kill(*pid, libc::SIGTERM) };
                    }
                    heard.lock().unwrap().push(login);
                })
            }),
        };
        let opened = open(&config, Prompts::Helper(&askpass));
        let heard = heard.lock().unwrap().clone();
        (opened, heard)
    }

    #[test]
    fn a_login_gone_quiet_after_a_word_waits_for_you() {
        let started = std::time::Instant::now();
        let (opened, heard) = open_script(
            "echo 'Warning: Permanently added x (ED25519) to the list of known hosts.' >&2; \
             echo '# To authenticate, visit: https://login.example.com/a/xyz' >&2; exec sleep 20",
        );
        assert!(opened.is_err());
        // Told after it went quiet, well before the greeting's time ran out.
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let stalled = heard.iter().find_map(|login| match login {
            Login::Stalled { said, url, .. } => Some((said.clone(), url.clone())),
            Login::Said(_) => None,
        });
        let (said, url) = stalled.expect("told it waits");
        // ssh's routine notice isn't what it waits on.
        assert_eq!(said, ["To authenticate, visit: https://login.example.com/a/xyz"]);
        assert_eq!(url.as_deref(), Some("https://login.example.com/a/xyz"));
        assert!(heard.iter().filter(|l| matches!(l, Login::Said(_))).count() == 2);
    }

    #[test]
    fn a_failure_is_not_a_wait() {
        let (opened, heard) = open_script("echo 'Permission denied (publickey).' >&2; exit 255");
        assert_eq!(opened.err().as_deref(), Some("Permission denied (publickey)."));
        assert!(!heard.iter().any(|l| matches!(l, Login::Stalled { .. })));
    }
}
