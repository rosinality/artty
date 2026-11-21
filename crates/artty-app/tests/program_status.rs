//! Programs' status reports (OSC 7501) in a pane: Claude Code's, as a
//! stand-in sends them (`fake_status.py`), against a scratch server.

mod common;

use std::time::{Duration, Instant};

use common::{ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_status.py");

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn started(case: &str) -> (Scratch, String) {
    let scratch = Scratch::new();
    let out = scratch.artty(&["new", "--background", "--", "python3", FAKE, ARTTY, case]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pane = stdout(&out);
    wait_for("it to be ready", || {
        stdout(&scratch.artty(&["read", "--tab", &pane, "--screen"])).contains("ready")
    });
    (scratch, pane)
}

/// The pane's agent, as `artty agents` lists it.
fn agent(scratch: &Scratch, pane: &str) -> Option<Value> {
    let agents: Value =
        serde_json::from_str(&stdout(&scratch.artty(&["agents", "--json"]))).unwrap_or(Value::Null);
    agents
        .as_array()?
        .iter()
        .find(|a| a["pane"].as_u64().is_some_and(|p| p.to_string() == pane))
        .cloned()
}

#[test]
fn a_turn_cut_short_as_it_says() {
    let (scratch, pane) = started("interrupt");
    let at_work = agent(&scratch, &pane).expect("listed as an agent");
    assert_eq!(at_work["state"], "working", "{at_work}");
    // What it's doing: its own words.
    assert_eq!(at_work["message"], "Writing note.txt", "{at_work}");
    // And its tab says so (the sidebar shows it under the tab's title).
    let tabs: Value = serde_json::from_str(&stdout(&scratch.artty(&["list", "--json"]))).unwrap();
    let tab = tabs.as_array().unwrap().iter().find(|t| t["id"].as_u64().is_some_and(|id| id.to_string() == pane)).unwrap();
    assert_eq!(tab["detail"], "claude · Writing note.txt", "{tab}");

    // Idle, with no Stop: cut short (Esc).
    scratch.artty(&["send", "--tab", &pane, "go"]);
    wait_for("the turn cut short", || {
        let thread = stdout(&scratch.artty(&["thread", "--tab", &pane, "--json"]));
        thread.contains("Interrupted")
    });
    assert_eq!(agent(&scratch, &pane).unwrap()["state"], "idle");
}

#[test]
fn waiting_for_what_hooks_dont_tell() {
    let (scratch, pane) = started("blocked");
    // (Listed once what runs there's been looked at.)
    wait_for("it listed", || agent(&scratch, &pane).is_some());
    let waiting = agent(&scratch, &pane).unwrap();
    assert_eq!(waiting["agent"], "claude", "{waiting}");
    assert_eq!(waiting["state"], "waiting", "{waiting}");
    assert_eq!(waiting["message"], "Login expired", "{waiting}");
    scratch.artty(&["send", "--tab", &pane, "go"]);
    wait_for("it at work again", || {
        agent(&scratch, &pane)
            .is_some_and(|a| a["state"] == "working" && a["message"] == "Thinking")
    });
}

#[test]
fn an_idle_from_before_the_turn_cuts_nothing() {
    let (scratch, pane) = started("stale");
    wait_for("it listed", || agent(&scratch, &pane).is_some());
    std::thread::sleep(Duration::from_secs(5));
    let thread = stdout(&scratch.artty(&["thread", "--tab", &pane, "--json"]));
    assert!(!thread.contains("Interrupted"), "{thread}");
    assert_eq!(agent(&scratch, &pane).unwrap()["state"], "working");
}

#[test]
fn done_by_its_words_when_no_stop_comes() {
    let (scratch, pane) = started("nostop");
    wait_for("it listed", || agent(&scratch, &pane).is_some());
    assert_eq!(agent(&scratch, &pane).unwrap()["state"], "working");
    let deadline = Instant::now() + Duration::from_secs(20);
    while agent(&scratch, &pane).unwrap()["state"] != "done" {
        assert!(Instant::now() < deadline, "still {}", agent(&scratch, &pane).unwrap());
        std::thread::sleep(Duration::from_millis(250));
    }
}
