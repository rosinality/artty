//! A resumed Claude Code conversation's title in its tab, before Claude Code
//! titles the terminal itself, against a scratch server.

mod common;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Scratch, stdout};
use serde_json::Value;

fn title(scratch: &Scratch, pane: &str) -> String {
    let out = scratch.artty(&["list", "--json"]);
    let tabs: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let tab = tabs
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"].as_u64().map(|id| id.to_string()).as_deref() == Some(pane))
        .unwrap()
        .clone();
    tab["title"].as_str().unwrap_or_default().to_owned()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_resumed_conversation_by_its_title() {
    let scratch = Scratch::new();
    // Its transcript, as Claude Code keeps it.
    let project = scratch.dir.join(".claude/projects/-src");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = "{\"type\":\"ai-title\",\"aiTitle\":\"Login redirect fix\",\"sessionId\":\"sess-1\"}\n{\"type\":\"user\"}\n";
    std::fs::write(project.join("sess-1.jsonl"), transcript).unwrap();
    // Stands in for Claude Code: titles the terminal with its name at once,
    // with the conversation's (as given a message) a few seconds later.
    let script = "import sys,time\nsys.stdout.write('\\x1b]0;claude\\x07'); sys.stdout.flush()\ntime.sleep(4)\nsys.stdout.write('\\x1b]0;\\u2733 Login redirect fix, done\\x07'); sys.stdout.flush()\ntime.sleep(60)\n";
    let pane = stdout(&scratch.artty(&["new", "--background", "--", "python3", "-c", script]));
    // Its hook reports the conversation.
    let event = r#"{"hook_event_name":"SessionStart","session_id":"sess-1","source":"resume"}"#;
    let mut hook = scratch
        .command(&["status", "idle", "--hook", "--agent", "claude", "--tab", &pane])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(event.as_bytes()).unwrap();
    assert!(hook.wait().unwrap().success());

    // The conversation's title, though the terminal says "claude".
    wait_for("the conversation's title", || title(&scratch, &pane) == "Login redirect fix");
    // Then the title Claude Code gives the terminal.
    wait_for("the terminal's title", || title(&scratch, &pane) == "✳ Login redirect fix, done");
}
