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

/// Run the command reaching the server and wait for its greeting.
pub fn open(config: &ServerConfig) -> Result<Opened, String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let command = config.connect_command().ok_or("set ssh (or command) for it in the settings")?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{}: {err}", command[0]))?;
    // What it says on stderr is the reason, when it fails (its last few
    // lines: fish follows its error with the command and a marker).
    let said = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    if let Some(stderr) = child.stderr.take() {
        let said = said.clone();
        let name = config.name.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                log::info!("server {name}: {line}");
                if !line.trim().is_empty() {
                    let mut said = said.lock().unwrap();
                    if said.len() == 4 {
                        said.remove(0);
                    }
                    said.push(line.trim().to_owned());
                }
            }
        });
    }
    // No greeting in time (a login asking for something, a hung network):
    // give up on this attempt.
    let greeted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (greeted, pid) = (greeted.clone(), child.id() as i32);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            if !greeted.load(std::sync::atomic::Ordering::SeqCst) {
                // SAFETY: plain syscall on our own child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
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
            match said.last() {
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

/// Install this build of artty on the machine `config` reaches by ssh: the
/// build for its CPU (from `build`, given `x86_64` or `aarch64`), into
/// `~/.local/bin`, moving a running server onto it (its programs keep
/// running). Linux with glibc 2.28 or later only.
pub fn install(
    config: &ServerConfig,
    build: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> Result<Installed, String> {
    // What it runs: Linux on which CPU, with which C library.
    let probe = run(config, "uname -sm; ldd --version 2>&1 | head -n 1", None)?;
    let mut lines = probe.lines();
    let system = lines.next().unwrap_or_default();
    let libc = lines.next().unwrap_or_default();
    let cpu = match system.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["Linux", cpu @ ("x86_64" | "aarch64")] => *cpu,
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
    let build = build(cpu).ok_or_else(|| format!("this app has no build for Linux on {cpu}"))?;
    let file = std::fs::File::open(&build).map_err(|err| format!("{}: {err}", build.display()))?;
    run(
        config,
        "sh -c 'mkdir -p \"$HOME/.local/bin\" && cat > \"$HOME/.local/bin/artty.new\"'",
        Some(file),
    )?;
    run(
        config,
        "sh -c 'set -e; b=\"$HOME/.local/bin\"; chmod 755 \"$b/artty.new\"; \
         mv \"$b/artty.new\" \"$b/artty\"; \
         if \"$b/artty\" server status >/dev/null 2>&1; then \"$b/artty\" server restart; fi'",
        None,
    )?;
    // Found as `artty` the way the connection looks for it?
    let found = run(config, "exec \"$SHELL\" -lc 'command -v artty; echo \"$HOME\"'", None)?;
    let mut lines = found.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>();
    let home = lines.pop().unwrap_or_default();
    let on_path = lines.first().is_some_and(|path| *path == format!("{home}/.local/bin/artty"));
    Ok(Installed { on_path })
}

/// Run `remote` there by ssh (with `stdin`); what it printed, or why it
/// failed.
fn run(
    config: &ServerConfig,
    remote: &str,
    stdin: Option<std::fs::File>,
) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let command = config.ssh_command(remote).ok_or("it isn't reached by ssh")?;
    let output = Command::new(&command[0])
        .args(&command[1..])
        .stdin(stdin.map_or_else(Stdio::null, Stdio::from))
        .output()
        .map_err(|err| format!("ssh: {err}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("failed");
        return Err(said.trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
