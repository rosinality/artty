//! Agents run through their Agent Client Protocol adapter (`artty acp`),
//! here a stand-in (`fake_acp.py`), against a scratch server: asks,
//! permission requests, edits, interrupts, and turns of the agent's own.

mod common;

use std::time::{Duration, Instant};

use common::{ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

const ADAPTER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp.py");

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Each entry's kind and item.
fn thread(scratch: &Scratch, pane: &str) -> Vec<(String, Value)> {
    let out = scratch.artty(&["thread", "--tab", pane, "--json"]);
    let entries: Value = serde_json::from_str(&stdout(&out)).unwrap();
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let (kind, item) = e["item"].as_object().unwrap().iter().next().unwrap();
            (kind.clone(), item.clone())
        })
        .collect()
}

/// An ask not waited for: its id.
fn no_wait(scratch: &Scratch, pane: &str, text: &str) -> String {
    let out = scratch.ask(&["--tab", pane, "--no-wait", text]);
    let said = stderr(&out);
    let id = said.split("ask ").nth(1).and_then(|s| s.split_whitespace().next());
    id.unwrap_or_else(|| panic!("no ask id: {said}")).to_owned()
}

fn started() -> (Scratch, String) {
    // (Resumed by this build: `artty` on the PATH may be another.)
    let scratch = Scratch::with_config(&format!(
        "[acp]\nclaude = \"python3 '{ADAPTER}'\"\n[resume.claude-acp]\nsession = \"'{ARTTY}' acp \
         claude --resume {{session}}\"\n"
    ));
    let out = scratch.artty(&["new", "--background", "--agent", "claude-acp"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pane = stdout(&out);
    wait_for("the agent to start", || {
        let agents = stdout(&scratch.artty(&["agents", "--json"]));
        agents.contains("claude-acp")
    });
    (scratch, pane)
}

#[test]
fn asked_through_the_adapter() {
    let (scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "hello there"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "echo: hello there");

    // Multi-line, typed in as a paste.
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "one\ntwo"]);
    assert_eq!(stdout(&out), "echo: one\ntwo");

    let kinds: Vec<String> = thread(&scratch, &pane).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, ["Session", "Prompt", "Reply", "Prompt", "Reply"], "{kinds:?}");
}

#[test]
fn permission_asked_in_artty() {
    let (scratch, pane) = started();
    let file = scratch.dir.join("notes.txt");
    let ask = format!("edit {}", file.display());
    let id = no_wait(&scratch, &pane, &ask);

    let mut request = String::new();
    wait_for("the permission request", || {
        let out = stdout(&scratch.artty(&["approvals", "--json"]));
        let list: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
        let pending = list
            .as_array()
            .and_then(|l| l.iter().find(|a| a["state"] == "pending" && a["tool_name"] == "Edit"));
        if let Some(found) = pending {
            request = found["id"].to_string();
        }
        pending.is_some()
    });
    let out = scratch.artty(&["approval", "allow", &request]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = scratch.artty(&["ask", "--result", &id, "--timeout", "20"]);
    assert_eq!(stdout(&out), "edited", "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited\n");
    let entries = thread(&scratch, &pane);
    let edited = entries.iter().find(|(k, _)| k == "Edited").expect("files changed");
    assert!(edited.1.to_string().contains("notes.txt"), "{edited:?}");
    let permission = entries.iter().find(|(k, _)| k == "Permission").expect("its request");
    assert_eq!(permission.1["state"], "allowed", "{permission:?}");
}

#[test]
fn interrupted_with_esc() {
    let (scratch, pane) = started();
    let id = no_wait(&scratch, &pane, "slow");
    wait_for("the turn to start", || thread(&scratch, &pane).iter().any(|(k, _)| k == "Prompt"));
    std::thread::sleep(Duration::from_millis(500));
    let out = scratch.artty(&["send", "--tab", &pane, "--no-enter", "\x1b"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = scratch.artty(&["ask", "--result", &id, "--timeout", "20"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("interrupted"), "{}", stderr(&out));
    assert!(thread(&scratch, &pane).iter().any(|(k, _)| k == "Interrupted"));

    // The next goes on.
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "still there?"]);
    assert_eq!(stdout(&out), "echo: still there?", "{}", stderr(&out));
}

#[test]
fn a_turn_of_its_own() {
    let (scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "bg"]);
    assert_eq!(stdout(&out), "started", "{}", stderr(&out));
    wait_for("the agent's own turn", || {
        let entries = thread(&scratch, &pane);
        entries.iter().any(|(k, _)| k == "Notice")
            && entries.last().is_some_and(|(k, v)| k == "Reply" && v["text"] == "background done")
    });
    // Its own turn is over: an ask is answered as usual.
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "after"]);
    assert_eq!(stdout(&out), "echo: after", "{}", stderr(&out));
}

#[test]
fn resumed_after_a_restart() {
    let (mut scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "before"]);
    assert_eq!(stdout(&out), "echo: before", "{}", stderr(&out));
    let before = thread(&scratch, &pane).len();

    scratch.restart();
    wait_for("the agent to resume", || {
        stdout(&scratch.artty(&["agents", "--json"])).contains("claude-acp")
    });
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "after"]);
    assert_eq!(stdout(&out), "echo: after", "{}", stderr(&out));
    // The conversation replayed as it loads isn't taken for new.
    let entries = thread(&scratch, &pane);
    assert!(!entries.iter().any(|(_, v)| v.to_string().contains("an old")), "{entries:?}");
    assert_eq!(entries.len(), before + 3, "{entries:?}");
}

#[test]
fn started_with_a_task() {
    let scratch = Scratch::with_config(&format!("[acp]\nclaude = \"python3 '{ADAPTER}'\"\n"));
    let out = scratch.artty(&["new", "--background", "--agent", "claude-acp", "--task", "first"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pane = stdout(&out);
    wait_for("its answer", || {
        thread(&scratch, &pane).iter().any(|(k, v)| k == "Reply" && v["text"] == "echo: first")
    });
}
