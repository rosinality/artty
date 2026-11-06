//! Against a scratch server on this machine (the `artty` binary built
//! beside the tests), through a command as through ssh.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use artty_client::{Client, Event, Key, Link};
use artty_core::TerminalSize;

/// A scratch server's home: the server stops (and it goes) when dropped,
/// a failed test's too.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        stop(&self.0);
    }
}

/// `artty server --stdio` with a home of its own; `None` without the binary.
fn scratch() -> Option<(Link, Scratch)> {
    let exe = std::env::current_exe().ok()?;
    let artty = exe.parent()?.parent()?.join("artty");
    if !artty.exists() {
        return None;
    }
    // One per test: tests run side by side.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("artty-client-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("t")).unwrap();
    let d = dir.display();
    let command = [
        "env".to_owned(),
        format!("HOME={d}"),
        format!("XDG_STATE_HOME={d}/state"),
        format!("TMPDIR={d}/t/"),
        artty.display().to_string(),
        "server".to_owned(),
        "--stdio".to_owned(),
    ];
    Some((Link::command(&command).unwrap(), Scratch(dir)))
}

/// A scratch server with this config file (its own config home, and none
/// of the artty this may run in).
fn scratch_with(config: &str) -> Option<(Link, Scratch)> {
    let exe = std::env::current_exe().ok()?;
    let artty = exe.parent()?.parent()?.join("artty");
    if !artty.exists() {
        return None;
    }
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("artty-client-c{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("t")).unwrap();
    std::fs::create_dir_all(dir.join("config/artty")).unwrap();
    std::fs::write(dir.join("config/artty/config.toml"), config).unwrap();
    let d = dir.display();
    let command = [
        "env".to_owned(),
        "-u".to_owned(),
        "ARTTY_SOCKET".to_owned(),
        "-u".to_owned(),
        "ARTTY_SESSION_ID".to_owned(),
        format!("HOME={d}"),
        format!("XDG_CONFIG_HOME={d}/config"),
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
        if std::env::var_os("ARTTY_TEST_DEBUG").is_some() {
            eprintln!("{event:?}");
        }
        if let Some(found) = want(&event) {
            return found;
        }
    }
    panic!("not in time");
}

#[test]
fn shows_a_window_and_types_in_it() {
    let Some((link, _scratch)) = scratch() else { return };
    let (client, events) = Client::start(link, "test").unwrap();
    client.attach(None, false).unwrap();
    let window = until(&events, |e| match e {
        Event::Attached { window } => Some(*window),
        _ => None,
    });
    let pane = until(&events, |e| match e {
        Event::Layout { window: w, layout } if *w == window => {
            layout.active_tab().map(|t| t.focused)
        },
        _ => None,
    });
    let size = TerminalSize { columns: 60, lines: 12, cell_width: 8, cell_height: 16 };
    client.resize(pane, size).unwrap();
    client.focus(window, true).unwrap();
    // (In any shell: the user's runs, fish's included.)
    client.input(pane, b"printf 'client-%d\\n' 42".to_vec()).unwrap();
    assert!(client.key(pane, &Key::parse("enter")).unwrap());
    // The answer, on the screen kept for it, at the size asked.
    until(&events, |e| match e {
        Event::Screen { pane: p } if *p == pane => {
            let screen = client.screen(pane)?;
            let text: String = screen
                .frame
                .lines
                .iter()
                .flat_map(|line| line.runs.iter().map(|r| r.text.as_str()))
                .collect();
            // The answer, not the command typed.
            (screen.frame.columns == 60 && text.contains("client-42")).then_some(())
        },
        _ => None,
    });
    // It's listed, shown here.
    client.list_windows().unwrap();
    let apps = until(&events, |e| match e {
        Event::Windows(windows) => windows.iter().find(|w| w.window == window).map(|w| w.apps),
        _ => None,
    });
    assert_eq!(apps, 1);
}

/// The command an app connects with, for a second app on the same server.
fn link_to(dir: &std::path::Path) -> Link {
    let exe = std::env::current_exe().unwrap();
    let artty = exe.parent().unwrap().parent().unwrap().join("artty");
    let d = dir.display();
    Link::command(&[
        "env".to_owned(),
        format!("HOME={d}"),
        format!("XDG_STATE_HOME={d}/state"),
        format!("TMPDIR={d}/t/"),
        artty.display().to_string(),
        "server".to_owned(),
        "--stdio".to_owned(),
    ])
    .unwrap()
}

/// Shown a new window: it and its pane.
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

#[test]
fn notifications_go_to_the_app_in_use() {
    let Some((link, scratch)) = scratch() else { return };
    let (phone, phone_events) = Client::start(link, "test").unwrap();
    new_window(&phone, &phone_events);
    let (mac, mac_events) = Client::start(link_to(&scratch.0), "test").unwrap();
    let (window, pane) = new_window(&mac, &mac_events);
    // Typed at (so in use), then put away: a notification from it, a moment later.
    mac.input(pane, b"sleep 1; printf '\\033]9;from-the-pane\\007'".to_vec()).unwrap();
    assert!(mac.key(pane, &Key::parse("enter")).unwrap());
    mac.focus(window, false).unwrap();
    let (w, p) = until(&mac_events, |e| match e {
        Event::Notify { body, window, pane, .. } if body == "from-the-pane" => {
            Some((*window, *pane))
        },
        _ => None,
    });
    assert_eq!((w, p), (Some(window), Some(pane)));
    // Not the phone: its user is at the Mac.
    let deadline = Instant::now() + Duration::from_secs(1);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = phone_events.recv_timeout(left) else { break };
        assert!(!matches!(event, Event::Notify { .. }), "told the phone too");
    }
}

#[test]
fn images_arrive_and_go() {
    let Some((link, _scratch)) = scratch() else { return };
    let (client, events) = Client::start(link, "test").unwrap();
    let (_, pane) = new_window(&client, &events);
    // A 2×2 red square (kitty's graphics, RGBA), then the screen cleared.
    let pixels = "/wAA//8AAP//AAD//wAA/w==";
    let print = format!("printf '\\033_Gf=32,s=2,v=2,a=T,c=2,r=1;{pixels}\\033\\\\'");
    client.input(pane, print.into_bytes()).unwrap();
    assert!(client.key(pane, &Key::parse("enter")).unwrap());
    let id = until(&events, |e| match e {
        Event::Image { id } => Some(*id),
        _ => None,
    });
    let image = client.image(id).expect("kept");
    assert_eq!((image.width, image.height, image.data.len()), (2, 2, 16));
    let shown = |id| {
        let screen = client.screen(pane).unwrap();
        screen.frame.lines.iter().any(|l| l.images.iter().any(|i| i.image == id))
    };
    until(&events, |e| matches!(e, Event::Screen { .. } if shown(id)).then_some(()));
    // Gone from the screen: let go of.
    client.input(pane, b"clear".to_vec()).unwrap();
    assert!(client.key(pane, &Key::parse("enter")).unwrap());
    until(&events, |e| matches!(e, Event::Screen { .. } if !shown(id)).then_some(()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.image(id).is_some() && Instant::now() < deadline {
        let _ = events.recv_timeout(Duration::from_millis(100));
    }
    assert!(client.image(id).is_none(), "still kept");
}

#[test]
fn status_lines_come_rendered_and_follow_changes() {
    let dir = std::env::temp_dir().join(format!("artty-status-value-{}", std::process::id()));
    std::fs::write(&dir, "one\n").unwrap();
    let config = format!(
        "[status]\nleft = \"[{{user}}]\"\nright = \"#[fg=red,bold]n={{cmd:n}} {{nothing}} {{exit}}\"\n\
         [status.commands.n]\nrun = \"cat '{}'\"\nevery = \"300ms\"\n",
        dir.display()
    );
    let Some((link, _scratch)) = scratch_with(&config) else { return };
    let (client, events) = Client::start(link, "test").unwrap();
    let (window, _) = new_window(&client, &events);
    let text = |segments: &[artty_core::protocol::StatusSegment]| -> String {
        segments.iter().map(|s| s.text.as_str()).collect()
    };
    let line = until(&events, |e| match e {
        Event::Status { window: w, line } if *w == window && text(&line.right) == "n=one" => {
            Some(line.clone())
        },
        _ => None,
    });
    assert!(text(&line.left).starts_with('[') && text(&line.left).ends_with(']'));
    assert!(line.right.iter().all(|s| s.bold && s.fg.is_some()), "{line:?}");
    // The command's output changes: so does the line.
    std::fs::write(&dir, "two\n").unwrap();
    until(&events, |e| match e {
        Event::Status { window: w, line } if *w == window => {
            (text(&line.right) == "n=two").then_some(())
        },
        _ => None,
    });
    let _ = std::fs::remove_file(&dir);
}

fn stop(dir: &std::path::Path) {
    let exe = std::env::current_exe().unwrap();
    let artty = exe.parent().unwrap().parent().unwrap().join("artty");
    let d = dir.display();
    let _ = std::process::Command::new(artty)
        .args(["server", "stop"])
        .env("HOME", dir)
        .env("XDG_STATE_HOME", format!("{d}/state"))
        .env("TMPDIR", format!("{d}/t/"))
        // Run in artty, `artty` would reach its server, not this one.
        .env_remove("ARTTY_SOCKET")
        .output();
    let _ = std::fs::remove_dir_all(dir);
}
