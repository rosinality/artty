//! The `queue` extension against a scratch server: what an app taking it
//! is sent as permission requests come and go. (A file of its own: each
//! scratch server copies the artty binary as it starts, and too many at once
//! in one test binary outrun the wait for them.)

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
    let dir = std::env::temp_dir().join(format!("artty-queue-{}", std::process::id()));
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

/// The queue comes to an app taking it, as it changes: a permission
/// request (asked from a pane, as an agent's hook would), answered with a
/// control request, then gone.
#[test]
fn the_queue_follows_permission_requests() {
    use artty_core::control::{Request, Response};
    use artty_core::protocol::QueueKind;
    let Some((link, _scratch)) = scratch() else { return };
    let (client, events) = Client::start(link, "test").unwrap();
    assert!(client.supports("queue"));
    let (_, pane) = new_window(&client, &events);
    let request = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"touch q"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch:*"}],"behavior":"allow","destination":"session"}]}"#;
    let command =
        format!("printf '%s' '{request}' | artty approve --hook --agent t > approved.json\r");
    client.input(pane, command.into_bytes()).unwrap();
    let item = until(&events, |e| match e {
        Event::Queue(update) => update.items.first().cloned(),
        _ => None,
    });
    assert_eq!((item.kind, item.pane, item.text.as_str()), (QueueKind::Approval, pane, "touch q"));
    assert_eq!(item.options, ["Always allow Bash(touch:*) for this session"]);
    let decide = Request::Decide { id: item.id, allow: true, message: None, always: Some(0) };
    assert!(matches!(client.control(&decide), Ok(Response::Approvals { .. })));
    until(&events, |e| match e {
        Event::Queue(update) if update.items.is_empty() => Some(()),
        _ => None,
    });
}
