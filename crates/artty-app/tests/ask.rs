//! `artty ask` against a scratch server, with a stand-in agent
//! (`fake_agent.py`) that reports its turns through `artty status --hook`
//! as Claude Code's hooks do.

mod common;

use std::time::{Duration, Instant};

use common::{AGENT, ARTTY, Scratch, stderr, stdout};

#[test]
fn asks_and_replies() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "hello there"]);
    assert_eq!(stdout(&out), "echo: hello there", "{}", stderr(&out));
    // Its id comes first, for a caller cut short.
    assert!(stderr(&out).contains("artty ask --result"), "{}", stderr(&out));
    assert!(out.status.success());

    // Several lines, whole (a paste, not lines typed one by one).
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "one\ntwo"]);
    assert_eq!(stdout(&out), "echo: one\ntwo", "{}", stderr(&out));
}

#[test]
fn queued_behind_a_turn_and_picked_up_later() {
    let scratch = Scratch::new();
    let pane = scratch.agent("2");
    // Two at once: the second waits for the first's turn to end.
    let first = scratch.ask(&["--tab", &pane, "--no-wait", "first"]);
    let second = scratch.ask(&["--tab", &pane, "--no-wait", "second"]);
    assert_eq!(first.status.code(), Some(3), "{}", stderr(&first));
    assert_eq!(second.status.code(), Some(3), "{}", stderr(&second));
    let (first, second) = (stdout(&first), stdout(&second));

    let now = scratch.ask(&["--result", &second, "--timeout", "0", "--json"]);
    let info: serde_json::Value = serde_json::from_str(&stdout(&now)).unwrap();
    assert_eq!(info["state"], "queued");

    let out = scratch.ask(&["--result", &second, "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: second", "{}", stderr(&out));
    let out = scratch.ask(&["--result", &first]);
    assert_eq!(stdout(&out), "echo: first", "{}", stderr(&out));
}

#[test]
fn refuses_shells_commands_and_typing_twice() {
    let scratch = Scratch::new();
    // A shell: nothing typed into it.
    let out = scratch.artty(&["new", "--background", "--", "/bin/sh"]);
    let shell = stdout(&out);
    // (Once seen to be a shell: a pane just opened might be an agent
    // starting, and is waited for.)
    std::thread::sleep(Duration::from_millis(1500));
    let out = scratch.artty(&["ask", "--tab", &shell, "echo pwned"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no agent"), "{}", stderr(&out));

    let pane = scratch.agent("2");
    let out = scratch.ask(&["--tab", &pane, "/clear"]);
    assert!(stderr(&out).contains("--raw"), "{}", stderr(&out));

    // A queued ask can be taken back; one with its agent can't.
    let busy = scratch.ask(&["--tab", &pane, "--no-wait", "busy"]);
    let busy_id = stdout(&busy);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = scratch.ask(&["--result", &busy_id, "--timeout", "0", "--json"]);
        let info: serde_json::Value = serde_json::from_str(&stdout(&now)).unwrap();
        if info["state"] == "running" {
            break;
        }
        assert!(Instant::now() < deadline, "never running: {info}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let queued = scratch.ask(&["--tab", &pane, "--no-wait", "later"]);
    let listed = stdout(&scratch.ask(&["--list", "--tab", &pane]));
    assert!(listed.contains("later") && listed.contains("running"), "{listed}");
    let out = scratch.ask(&["--cancel", &stdout(&queued)]);
    assert_eq!(stdout(&out), "cancelled", "{}", stderr(&out));
    let again = scratch.ask(&["--tab", &pane, "--no-wait", "again"]);
    let out = scratch.ask(&["--cancel", "all", "--tab", &pane]);
    assert!(stdout(&out).contains(&stdout(&again)), "{}", stdout(&out));
    let out = scratch.ask(&["--cancel", &stdout(&busy)]);
    assert!(stderr(&out).contains("already with its agent"), "{}", stderr(&out));
    let out = scratch.ask(&["--result", &stdout(&busy), "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: busy", "{}", stderr(&out));
}

#[test]
fn nothing_typed_into_the_shell_an_agent_leaves() {
    let scratch = Scratch::new();
    // An agent started from a shell, quitting after one turn: the shell's
    // back, and a queued ask must not be typed into it.
    let out = scratch.artty(&["new", "--background", "--", "/bin/sh", "-i"]);
    let pane = stdout(&out);
    std::thread::sleep(Duration::from_millis(500));
    let command = format!("python3 {AGENT} {ARTTY} 1 1");
    scratch.artty(&["send", "--tab", &pane, &command]);
    let first = scratch.ask(&["--tab", &pane, "--no-wait", "first"]);
    let queued = scratch.ask(&["--tab", &pane, "--no-wait", "touch pwned"]);
    let out = scratch.ask(&["--result", &stdout(&first), "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: first", "{}", stderr(&out));
    // Failed at once (a shell is back), not left waiting for an agent.
    let out = scratch.artty(&["ask", "--result", &stdout(&queued), "--timeout", "5", "--json"]);
    let info: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(info["state"], "failed", "{info}");
    std::thread::sleep(Duration::from_secs(1));
    let screen = stdout(&scratch.artty(&["read", "--tab", &pane, "--screen"]));
    assert!(!screen.contains("touch pwned"), "typed into the shell:\n{screen}");
}

#[test]
fn abandoning_one_with_its_agent() {
    let scratch = Scratch::new();
    let pane = scratch.agent("3");
    let ask = scratch.ask(&["--tab", &pane, "--no-wait", "slow"]);
    let id = stdout(&ask);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = scratch.ask(&["--result", &id, "--timeout", "0", "--json"]);
        let info: serde_json::Value = serde_json::from_str(&stdout(&now)).unwrap();
        if info["state"] == "running" {
            break;
        }
        assert!(Instant::now() < deadline, "never running: {info}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = scratch.ask(&["--cancel", &id, "--abandon"]);
    assert_eq!(stdout(&out), "cancelled", "{}", stderr(&out));
    // The agent's turn goes on: the next waits for it, then is answered.
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "next"]);
    assert_eq!(stdout(&out), "echo: next", "{}", stderr(&out));
}

#[test]
fn replies_wait_in_the_queue_until_read() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "the build?"]);
    assert_eq!(stdout(&out), "echo: the build?", "{}", stderr(&out));
    let queue = |scratch: &Scratch| -> serde_json::Value {
        serde_json::from_str(&stdout(&scratch.artty(&["queue", "--json"]))).unwrap()
    };
    let items = queue(&scratch);
    let reply = items.as_array().unwrap().iter().find(|i| i["kind"] == "Reply").cloned();
    let reply = reply.unwrap_or_else(|| panic!("no reply queued: {items}"));
    assert_eq!(
        (reply["text"].as_str(), reply["context"].as_str()),
        (Some("echo: the build?"), Some("the build?"))
    );
    let id = reply["id"].to_string();
    assert!(scratch.artty(&["queue", "--dismiss", &id]).status.success());
    assert!(queue(&scratch).as_array().unwrap().iter().all(|i| i["kind"] != "Reply"));
}
