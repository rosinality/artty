//! `artty attach` in a terminal (artty's own, on a PTY here), against a
//! scratch server: what it shows, read back from that terminal's screen,
//! as typing and the prefix's commands change it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use artty_core::{Session, SessionOptions, TerminalSize, Theme};

const ARTTY: &str = env!("CARGO_BIN_EXE_artty");
const COLUMNS: u16 = 100;
const LINES: u16 = 30;

/// What the artty the tests may run in tells its panes: not for these
/// (`artty` would reach it, not the scratch server).
const OUTSIDE: [&str; 2] = ["ARTTY_SOCKET", "ARTTY_SESSION_ID"];

/// A scratch server's home: the server stops (and it goes) when dropped,
/// a failed test's too.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        // One per test: tests run side by side. Short: the socket's in it.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("artty-at-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("t")).unwrap();
        Scratch(dir)
    }

    /// What `artty` runs with: this home.
    fn env(&self) -> Vec<(String, String)> {
        let d = self.0.display();
        vec![
            ("HOME".into(), d.to_string()),
            ("XDG_STATE_HOME".into(), format!("{d}/state")),
            ("TMPDIR".into(), format!("{d}/t/")),
            ("ARTTY_PREFIX".into(), String::new()),
        ]
    }

    fn artty(&self, args: &[&str]) -> String {
        let mut command = Command::new(ARTTY);
        command.args(args).envs(self.env());
        for name in OUTSIDE {
            command.env_remove(name);
        }
        let output = command.output().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `artty attach ARGS` in a terminal of its own.
    fn attach(&self, args: &[&str]) -> Terminal {
        let mut arguments = Vec::new();
        for name in OUTSIDE {
            arguments.extend(["-u".to_owned(), name.to_owned()]);
        }
        arguments.extend([ARTTY.to_owned(), "attach".to_owned()]);
        arguments.extend(args.iter().map(|a| a.to_string()));
        let options = SessionOptions {
            command: Some(("env".to_owned(), arguments)),
            working_directory: Some(self.0.clone()),
            env: self.env().into_iter().collect(),
            ..Default::default()
        };
        let size = TerminalSize { columns: COLUMNS, lines: LINES, cell_width: 8, cell_height: 16 };
        let session =
            Session::spawn(1, options, size, Arc::new(Theme::default()), Arc::new(|_, _| {}))
                .unwrap();
        Terminal(session)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.artty(&["server", "stop"]);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Terminal(Session);

impl Terminal {
    fn screen(&self) -> Vec<String> {
        self.0.screen_text().lines().map(str::to_owned).collect()
    }

    /// The screen once `want` likes it; panics (showing it) after a while.
    fn until(&self, what: &str, want: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let screen = self.screen();
            if want(&screen) {
                return screen;
            }
            if Instant::now() > deadline {
                panic!("not in time: {what}; the screen:\n{}", screen.join("\n"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn type_text(&self, text: &str) {
        self.0.write(text.as_bytes().to_vec());
    }

    /// The status line: the last of the screen.
    fn status(&self) -> String {
        let screen = self.0.screen_text();
        let lines: Vec<&str> = screen.lines().collect();
        lines.get(usize::from(LINES) - 1).unwrap_or(&"").to_string()
    }
}

/// Where `text` starts on the screen, in characters (cells, for ASCII).
fn find(screen: &[String], text: &str) -> Option<(usize, usize)> {
    screen.iter().enumerate().find_map(|(line, row)| {
        let at = row.find(text)?;
        Some((line, row[..at].chars().count()))
    })
}

fn has(screen: &[String], text: &str) -> bool {
    find(screen, text).is_some()
}

#[test]
fn splits_tabs_the_mouse_and_detaching() {
    let scratch = Scratch::new();
    let terminal = scratch.attach(&[]);
    terminal.until("the status line", |s| s.last().is_some_and(|l| l.contains(" 1:")));
    terminal.type_text("printf 'left-%d\\n' 1\r");
    terminal.until("the output", |s| has(s, "left-1"));

    // Split side by side: a border, the new pane focused on the right.
    terminal.type_text("\x02%");
    terminal.until("the border", |s| s.iter().filter(|l| l.contains('│')).count() > 20);
    terminal.type_text("printf 'right-%d\\n' 2\r");
    let screen = terminal.until("the right pane's output", |s| has(s, "right-2"));
    let (_, column) = find(&screen, "right-2").unwrap();
    assert!(column > usize::from(COLUMNS) / 2, "at {column}");
    assert_eq!(find(&screen, "left-1").unwrap().1, 0);

    // A click in the left pane focuses it: typing goes there.
    terminal.type_text("\x1b[<0;5;3M\x1b[<0;5;3m");
    std::thread::sleep(Duration::from_millis(300));
    terminal.type_text("printf 'again-%d\\n' 3\r");
    let screen = terminal.until("typed on the left", |s| has(s, "again-3"));
    assert!(find(&screen, "again-3").unwrap().1 < usize::from(COLUMNS) / 2);

    // A new tab, named.
    terminal.type_text("\x02c");
    terminal.until("a second tab", |_| terminal.status().contains(" 2:"));
    terminal.type_text("\x02,");
    terminal.until("asked for a name", |_| terminal.status().contains("rename tab:"));
    terminal.type_text("named\r");
    terminal.until("the name", |_| terminal.status().contains("2:named*"));

    // Detached: it says so, and the window's there, shown nowhere.
    terminal.type_text("\x02d");
    terminal.until("detached", |s| has(s, "[detached from window"));
    let listed = scratch.artty(&["attach", "--list"]);
    assert!(listed.contains("2 tabs, not shown"), "{listed}");

    // Attached again: as it was; the first tab, split, then zoomed.
    let terminal = scratch.attach(&[]);
    terminal.until("the tabs", |_| terminal.status().contains("2:named*"));
    terminal.type_text("\x02p");
    terminal.until("the first tab", |s| has(s, "left-1") && has(s, "right-2"));
    assert!(terminal.status().contains("1:") && terminal.status().contains("2:named "));
    terminal.type_text("\x02z");
    terminal.until("zoomed", |s| {
        terminal.status().contains("*Z") && !s.iter().any(|l| l.contains('│'))
    });
    terminal.type_text("\x02d");
    terminal.until("detached", |s| has(s, "[detached from window"));
}

#[test]
fn another_prefix_copy_mode_and_the_server_going() {
    let scratch = Scratch::new();
    let terminal = scratch.attach(&["--prefix", "ctrl-a"]);
    terminal.until("the status line", |_| terminal.status().contains(" 1:"));
    // Ctrl-b is the program's now.
    terminal.type_text("\x02c");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!terminal.status().contains(" 2:"));
    terminal.type_text("\x01c");
    terminal.until("a second tab", |_| terminal.status().contains(" 2:"));
    // Copy mode, and out of it.
    terminal.type_text("\x01[");
    terminal.until("copy mode", |s| has(s, "[copy 0]"));
    terminal.type_text("q");
    terminal.until("out of copy mode", |s| !has(s, "[copy"));
    // The keys, listed.
    terminal.type_text("\x01?");
    terminal.until("the keys", |s| has(s, "ctrl-a, then:") && has(s, "split side by side"));
    terminal.type_text("q");
    terminal.until("the keys gone", |s| !has(s, "ctrl-a, then:"));
    // The server stops: it ends, saying so.
    scratch.artty(&["server", "stop"]);
    terminal.until("ended", |s| has(s, "the server stopped") || has(s, "lost the server"));
}

#[test]
fn windows_shown_elsewhere_are_shared_only_if_asked() {
    let scratch = Scratch::new();
    let first = scratch.attach(&[]);
    first.until("the status line", |_| first.status().contains(" 1:"));
    let listed = scratch.artty(&["attach", "--list"]);
    let window = listed.split_whitespace().next().unwrap().to_owned();
    let second = scratch.attach(&[&window]);
    second.until("refused", |s| has(s, "--share shows it here too"));
    // Shared: the same tabs.
    first.type_text("\x02c");
    first.until("a second tab", |_| first.status().contains(" 2:"));
    let second = scratch.attach(&[&window, "--share"]);
    second.until("the same tabs", |_| second.status().contains(" 2:"));
    assert_eq!(scratch.artty(&["attach", "--list"]).matches("shown in 2 apps").count(), 1);
}
