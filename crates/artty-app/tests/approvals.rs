//! Agents' permission requests through `artty approve --hook`, against a
//! scratch server: answered in artty, or at the agent's own prompt (the
//! stand-in agent's, which acts as Claude Code's does).

mod common;

use std::time::{Duration, Instant};

use common::{ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

/// The pane's pending request, once its agent has asked.
fn pending(scratch: &Scratch, pane: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = scratch.artty(&["approvals", "--tab", pane, "--json"]);
        let list: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
        if let Some(first) = list.as_array().and_then(|l| l.first())
            && first["state"] == "pending"
        {
            return first.clone();
        }
        assert!(Instant::now() < deadline, "no pending request: {list}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn state(scratch: &Scratch, id: &str) -> String {
    let out = scratch.artty(&["approvals", "--json"]);
    let list: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let found = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"].as_u64().is_some_and(|n| n.to_string() == id))
        .cloned();
    found.map(|a| a["state"].as_str().unwrap().to_owned()).unwrap_or_default()
}

/// An agent asked to run `command` (its ask's id), and the request's id.
fn asking(scratch: &Scratch, pane: &str, command: &str) -> (String, String) {
    let message = format!("tool: {command}");
    let ask = scratch.ask(&["--tab", pane, "--no-wait", &message]);
    assert_eq!(ask.status.code(), Some(3), "{}", stderr(&ask));
    let request = pending(scratch, pane);
    assert_eq!(request["summary"], command);
    (stdout(&ask), request["id"].to_string())
}

fn reply(scratch: &Scratch, ask: &str) -> (Option<i32>, String, String) {
    let out = scratch.ask(&["--result", ask, "--timeout", "20"]);
    (out.status.code(), stdout(&out), stderr(&out))
}

#[test]
fn answered_in_artty() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");

    let (ask, id) = asking(&scratch, &pane, "touch allowed");
    let out = scratch.artty(&["approval", "allow", &id]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(reply(&scratch, &ask).1, "ran: touch allowed");
    assert_eq!(state(&scratch, &id), "allowed");
    // Once only.
    let again = scratch.artty(&["approval", "deny", &id]);
    assert!(stderr(&again).contains("already over"), "{}", stderr(&again));

    // Always: with the agent's own option, which goes back to it.
    let (ask, id) = asking(&scratch, &pane, "make test");
    let listed = stdout(&scratch.artty(&["approvals", "--tab", &pane]));
    assert!(
        listed.contains("--always 0: Always allow Bash(make test) for this session"),
        "{listed}"
    );
    let out = scratch.artty(&["approval", "allow", &id, "--always", "0"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(reply(&scratch, &ask).1, "ran (always): make test");

    let (ask, id) = asking(&scratch, &pane, "rm -rf /tmp/nothing");
    scratch.artty(&["approval", "deny", &id, "--message", "not now"]);
    assert_eq!(reply(&scratch, &ask).1, "denied: not now");
    assert_eq!(state(&scratch, &id), "denied");
}

#[test]
fn not_from_inside_a_pane() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");
    let (ask, id) = asking(&scratch, &pane, "touch mine");
    // A program in another pane (as an agent's would be) tries to approve,
    // and to type the answer at the prompt.
    let log = scratch.dir.join("inside.log");
    let script = format!(
        "{ARTTY} approval allow {id} > {log} 2>&1; {ARTTY} send --tab {pane} --no-enter 1 >> {log} 2>&1",
        log = log.display()
    );
    scratch.artty(&["new", "--background", "--", "/bin/sh", "-c", &script]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().count() >= 2 || Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(text.contains("can't be answered from inside artty's panes"), "{text}");
    assert_eq!(text.matches("inside artty's panes").count(), 2, "{text}");
    assert_eq!(state(&scratch, &id), "pending");
    // From outside (this test), it can.
    scratch.artty(&["approval", "allow", &id]);
    assert_eq!(reply(&scratch, &ask).1, "ran: touch mine");
}

#[test]
fn answered_at_the_agents_prompt() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");

    // Yes there: the tool's use answers the request; its hook is let go.
    let (ask, id) = asking(&scratch, &pane, "touch there");
    scratch.artty(&["send", "--tab", &pane, "--no-enter", "1"]);
    assert_eq!(reply(&scratch, &ask).1, "ran: touch there");
    assert_eq!(state(&scratch, &id), "allowed-there");

    // No there: the agent ends the hook, and stops its turn with no Stop.
    let (ask, id) = asking(&scratch, &pane, "touch never");
    scratch.artty(&["send", "--tab", &pane, "--no-enter", "4"]);
    let (code, _, err) = reply(&scratch, &ask);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("interrupted at a permission prompt"), "{err}");
    assert_eq!(state(&scratch, &id), "dismissed");
}
