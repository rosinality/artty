//! The `peek` extension against a scratch server: a pane no window shows,
//! its screens sent while peeked at. (A file of its own: see `queue.rs`.)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use artty_client::{Client, Event, Link};
use artty_core::TerminalSize;

/// A scratch server's home: the server stops (and it goes) when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let exe = std::env::current_exe().unwrap();
        let artty = exe.parent().unwrap().parent().unwrap().join("artty");
        let d = self.0.display();
        let _ = std::process::Command::new(artty)
            .args(["server", "stop"])
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", format!("{d}/state"))
            .env("TMPDIR", format!("{d}/t/"))
            .env_remove("ARTTY_SOCKET")
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `artty server --stdio` with a home of its own; `None` without the binary.
fn scratch() -> Option<(Link, Scratch)> {
    let exe = std::env::current_exe().ok()?;
    let artty = exe.parent()?.parent()?.join("artty");
    if !artty.exists() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("artty-peek-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("t")).unwrap();
    let d = dir.display();
    let command = [
        "env".to_owned(),
        "-u".to_owned(),
        "ARTTY_SOCKET".to_owned(),
        "-u".to_owned(),
        "ARTTY_SESSION_ID".to_owned(),
        format!("HOME={d}"),
        format!("XDG_STATE_HOME={d}/state"),
        format!("TMPDIR={d}/t/"),
        artty.display().to_string(),
        "server".to_owned(),
        "--stdio".to_owned(),
    ];
    Some((Link::command(&command).unwrap(), Scratch(dir)))
}

/// Events until `want` takes one; panics after a while.
fn until<T>(
    events: &std::sync::mpsc::Receiver<Event>,
    mut want: impl FnMut(&Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        if let Some(found) = want(&event) {
            return found;
        }
    }
    panic!("not in time");
}

fn new_window(client: &Client, events: &std::sync::mpsc::Receiver<Event>) -> (u64, u64) {
    client.attach(None, false).unwrap();
    let window = until(events, |e| match e {
        Event::Attached { window } => Some(*window),
        _ => None,
    });
    let pane = until(events, |e| match e {
        Event::Layout { window: w, layout } if *w == window => {
            layout.active_tab().map(|t| t.focused)
        },
        _ => None,
    });
    let size = TerminalSize { columns: 60, lines: 12, cell_width: 8, cell_height: 16 };
    client.resize(pane, size).unwrap();
    (window, pane)
}

/// A tab no window shows gets no screens, until peeked at; then as it
/// changes, until no longer.
#[test]
fn screens_of_a_pane_peeked_at() {
    use artty_core::control::{Request, Response};
    let Some((link, _scratch)) = scratch() else { return };
    let (client, events) = Client::start(link, "test").unwrap();
    assert!(client.supports("peek"));
    let (_, shown) = new_window(&client, &events);
    let open = Request::New {
        cwd: None,
        command: vec!["sh".into()],
        title: None,
        focus: false,
        split: None,
        target: None,
        workspace: None,
        worktree: None,
        task: None,
        thread: None,
        back: false,
    };
    let Ok(Response::Tab { id: hidden }) = client.control(&open) else { panic!("no tab") };
    assert_ne!(hidden, shown);
    let screen_of =
        |pane| move |e: &Event| matches!(e, Event::Screen { pane: p } if *p == pane).then_some(());
    client.peek(Some(hidden)).unwrap();
    until(&events, screen_of(hidden));
    client.input(hidden, b"echo peeked-at\r".to_vec()).unwrap();
    until(&events, |e| {
        let has = |p: &u64| {
            client.screen(*p).is_some_and(|s| {
                s.frame.lines.iter().any(|l| l.runs.iter().any(|r| r.text.contains("peeked-at")))
            })
        };
        matches!(e, Event::Screen { pane } if *pane == hidden && has(pane)).then_some(())
    });

    // No longer: what it shows next isn't sent.
    client.peek(None).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    while events.try_recv().is_ok() {}
    client.input(hidden, b"echo again\r".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Ok(Event::Screen { pane }) = events.recv_timeout(left) {
            assert_ne!(pane, hidden, "screens of a pane no longer peeked at");
        }
    }
}
