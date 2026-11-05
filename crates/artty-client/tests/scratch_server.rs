//! Against a scratch server on this machine (the `artty` binary built
//! beside the tests), through a command as through ssh.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use artty_client::{Client, Event, Key, Link};
use artty_core::TerminalSize;

/// `artty server --stdio` with a home of its own; `None` without the binary.
fn scratch() -> Option<(Link, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let artty = exe.parent()?.parent()?.join("artty");
    if !artty.exists() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("artty-client-{}", std::process::id()));
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
    Some((Link::command(&command).unwrap(), dir))
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
    let Some((link, dir)) = scratch() else { return };
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
    drop(client);
    stop(&dir);
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
        .output();
    let _ = std::fs::remove_dir_all(dir);
}
