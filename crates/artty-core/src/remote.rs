//! Reaching another machine's artty server: a command (ssh, usually)
//! whose stdin and stdout carry the connection, ending at `artty server
//! --stdio` there.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crate::ServerConfig;
use crate::protocol::{Hello, ServerMessage, read_message};

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
    // What it says on stderr is the reason, when it fails.
    let said = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let said = said.clone();
        let name = config.name.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                log::info!("server {name}: {line}");
                if !line.trim().is_empty() {
                    *said.lock().unwrap() = line;
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
            Err(if said.is_empty() { format!("no answer: {err}") } else { said })
        },
    }
}
