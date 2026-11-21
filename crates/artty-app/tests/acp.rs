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
        "[acp]\nclaude = \"ARTTY_BIN='{ARTTY}' python3 '{ADAPTER}'\"\n[resume.claude-acp]\nsession = \"'{ARTTY}' acp \
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
    // Started with a task, plain `claude` runs headless (`headless-tasks`).
    let out = scratch.artty(&["new", "--background", "--agent", "claude", "--task", "first"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pane = stdout(&out);
    wait_for("its answer", || {
        thread(&scratch, &pane).iter().any(|(k, v)| k == "Reply" && v["text"] == "echo: first")
    });
    assert!(stdout(&scratch.artty(&["agents"])).contains("claude-acp"));
}

#[test]
fn a_long_ask() {
    let (scratch, pane) = started();
    // Read in pieces: the paste's end may come split across them.
    let long: String = (0..400).map(|n| format!("line {n} of a long message\n")).collect();
    let long = long.trim_end();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "30", long]);
    assert_eq!(stdout(&out), format!("echo: {long}"), "{}", stderr(&out));
}

#[test]
fn its_own_turn_interrupted() {
    let (scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "bgslow"]);
    assert_eq!(stdout(&out), "started", "{}", stderr(&out));
    wait_for("its own turn", || thread(&scratch, &pane).iter().any(|(k, _)| k == "Notice"));
    let out = scratch.artty(&["send", "--tab", &pane, "--no-enter", "\x1b"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // Over: asks go on.
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "after"]);
    assert_eq!(stdout(&out), "echo: after", "{}", stderr(&out));
    assert!(thread(&scratch, &pane).iter().any(|(k, _)| k == "Interrupted"));
}

#[test]
fn its_hooks_keep_quiet() {
    let (scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "hook"]);
    assert_eq!(stdout(&out), "echo: hook", "{}", stderr(&out));
    let replies: Vec<Value> = thread(&scratch, &pane)
        .into_iter()
        .filter(|(k, _)| k == "Reply")
        .map(|(_, v)| v["text"].clone())
        .collect();
    assert_eq!(replies, ["echo: hook"]);
}

#[test]
fn an_adapter_that_fails_says_why() {
    let scratch =
        Scratch::with_config("[acp]\nclaude = \"echo no such adapter here >&2; exit 1\"\n");
    let out = scratch.artty(&["new", "--background", "--agent", "claude-acp"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pane = stdout(&out);
    // The tab stays, saying why.
    wait_for("the adapter's error", || {
        let screen = stdout(&scratch.artty(&["read", "--tab", &pane, "--screen"]));
        screen.contains("no such adapter here") && screen.contains("a key closes this")
    });
}

#[test]
fn what_it_is_doing() {
    let (scratch, pane) = started();
    no_wait(&scratch, &pane, "busy");
    // The tool in use, by its own title (not the generic first one).
    wait_for("its activity", || {
        stdout(&scratch.artty(&["agents", "--json"]))
            .contains("\"message\": \"cargo test --workspace\"")
    });
    scratch.artty(&["send", "--tab", &pane, "--no-enter", "\x1b"]);
    wait_for("it to stop", || {
        let agents = stdout(&scratch.artty(&["agents", "--json"]));
        agents.contains("\"state\": \"idle\"")
    });
}

#[test]
fn an_edit_shown_before_it_is_allowed() {
    let (scratch, pane) = started();
    let file = scratch.dir.join("draft.md");
    no_wait(&scratch, &pane, &format!("edit {}", file.display()));
    wait_for("its permission request", || {
        thread(&scratch, &pane).iter().any(|(k, _)| k == "Permission")
    });
    let entries = thread(&scratch, &pane);
    let (_, permission) = entries.iter().find(|(k, _)| k == "Permission").unwrap();
    assert_eq!(permission["input"]["new_string"], "edited\n", "{permission:?}");
}

#[test]
fn a_thread_cleared() {
    let (mut scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "to be forgotten"]);
    assert_eq!(stdout(&out), "echo: to be forgotten", "{}", stderr(&out));
    let out = scratch.artty(&["thread", "--clear", "--tab", &pane]);
    assert!(out.status.success(), "{}", stderr(&out));
    let left = thread(&scratch, &pane);
    assert!(left.is_empty(), "{left:?}");
    // Gone on disk too; the agent goes on.
    scratch.restart();
    wait_for("the agent to resume", || {
        stdout(&scratch.artty(&["agents", "--json"])).contains("claude-acp")
    });
    let entries = thread(&scratch, &pane);
    assert!(!entries.iter().any(|(_, v)| v.to_string().contains("forgotten")), "{entries:?}");
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "again"]);
    assert_eq!(stdout(&out), "echo: again", "{}", stderr(&out));
}

#[test]
fn a_copy_of_a_conversation() {
    let (scratch, pane) = started();
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "which"]);
    assert_eq!(stdout(&out), "session: fake-acp-session", "{}", stderr(&out));
    let before = thread(&scratch, &pane);

    let out = scratch.artty(&[
        "new", "--background", "--agent", "claude", "--fork", "fake-acp-session", "--task", "which",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let copy = stdout(&out);
    wait_for("the copy's answer", || {
        thread(&scratch, &copy)
            .iter()
            .any(|(k, v)| k == "Reply" && v["text"] == "session: fork-of-fake-acp-session")
    });
    // The conversation copied isn't given anything.
    assert_eq!(thread(&scratch, &pane), before);
    // The copy is listed under the agent it's a copy of, with its own id.
    let agents: Value =
        serde_json::from_str(&stdout(&scratch.artty(&["agents", "--json"]))).unwrap();
    let listed = agents.as_array().unwrap();
    let find = |p: &str| listed.iter().find(|a| a["pane"] == p.parse::<u64>().unwrap()).unwrap().clone();
    assert_eq!(find(&copy)["session"], "fork-of-fake-acp-session", "{listed:?}");
    assert_eq!(find(&copy)["parent"].to_string(), pane, "{listed:?}");
    assert_eq!(find(&pane)["session"], "fake-acp-session");

    // Not another agent's, nor anything but an id.
    let out = scratch.artty(&["new", "--background", "--agent", "codex", "--fork", "x"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("only Claude Code"), "{}", stderr(&out));
    let out = scratch.artty(&["new", "--background", "--agent", "claude", "--fork", "a b;c"]);
    assert!(!out.status.success());
}

#[test]
fn who_started_whom_kept_in_a_restart() {
    let (mut scratch, pane) = started();
    let out = scratch.artty(&["new", "--background", "--agent", "claude", "--fork", "fake-acp-session"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let copy = stdout(&out);
    let parent_of = |scratch: &Scratch| {
        let agents: Value =
            serde_json::from_str(&stdout(&scratch.artty(&["agents", "--json"]))).unwrap();
        agents
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["session"] == "fork-of-fake-acp-session")
            .map(|a| (a["pane"].to_string(), a["parent"].to_string()))
    };
    wait_for("the copy to start", || parent_of(&scratch).is_some());
    assert_eq!(parent_of(&scratch), Some((copy, pane)));

    scratch.restart();
    wait_for("both to resume", || {
        let agents = stdout(&scratch.artty(&["agents", "--json"]));
        agents.contains("\"fake-acp-session\"") && agents.contains("fork-of-fake-acp-session")
    });
    let (copy, parent) = parent_of(&scratch).unwrap();
    let agents: Value =
        serde_json::from_str(&stdout(&scratch.artty(&["agents", "--json"]))).unwrap();
    let source = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["session"] == "fake-acp-session")
        .map(|a| a["pane"].to_string())
        .unwrap();
    assert_ne!(copy, source);
    assert_eq!(parent, source);
}
