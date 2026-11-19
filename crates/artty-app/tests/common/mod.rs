//! A scratch artty server for the tests, and the stand-in agent
//! (`fake_agent.py`) they run in its panes.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

pub const ARTTY: &str = env!("CARGO_BIN_EXE_artty");
pub const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_agent.py");

/// What the artty the tests may run in tells its panes: not for these.
const OUTSIDE: [&str; 2] = ["ARTTY_SOCKET", "ARTTY_SESSION_ID"];

/// A scratch server with a home of its own; stopped (and gone) when dropped.
pub struct Scratch {
    pub dir: PathBuf,
    server: Child,
}

impl Scratch {
    pub fn new() -> Scratch {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Short: the socket's in it.
        let dir = std::env::temp_dir().join(format!("artty-t-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("t")).unwrap();
        std::fs::create_dir_all(dir.join("config/artty")).unwrap();
        let mut command = Command::new(ARTTY);
        command.arg("server").envs(env(&dir));
        for name in OUTSIDE {
            command.env_remove(name);
        }
        let server = command.spawn().unwrap();
        let scratch = Scratch { dir, server };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !scratch.artty(&["server", "status"]).status.success() {
            assert!(Instant::now() < deadline, "the scratch server didn't start");
            std::thread::sleep(Duration::from_millis(50));
        }
        scratch
    }

    pub fn artty(&self, args: &[&str]) -> Output {
        let mut command = Command::new(ARTTY);
        command.args(args).envs(env(&self.dir));
        for name in OUTSIDE {
            command.env_remove(name);
        }
        command.output().unwrap()
    }

    /// A pane running the stand-in agent; a turn takes `delay` seconds.
    pub fn agent(&self, delay: &str) -> String {
        let out = self.artty(&["new", "--background", "--", "python3", AGENT, ARTTY, delay]);
        assert!(out.status.success(), "{}", stderr(&out));
        stdout(&out)
    }

    /// `artty ask ARGS`, once the pane's agent has started (its hooks
    /// reported): till then, it says there's no agent.
    pub fn ask(&self, args: &[&str]) -> Output {
        let mut all = vec!["ask"];
        all.extend(args);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.artty(&all);
            if !stderr(&out).contains("no agent is running") || Instant::now() > deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.artty(&["server", "stop"]);
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn env(dir: &std::path::Path) -> Vec<(String, String)> {
    let d = dir.display();
    vec![
        ("HOME".into(), d.to_string()),
        ("XDG_CONFIG_HOME".into(), format!("{d}/config")),
        ("XDG_STATE_HOME".into(), format!("{d}/state")),
        ("TMPDIR".into(), format!("{d}/t/")),
        ("SHELL".into(), "/bin/sh".into()),
    ]
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim_end().to_owned()
}
