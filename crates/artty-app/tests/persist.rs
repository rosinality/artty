//! Agents' threads and asks outlive their server, against scratch servers:
//! handed over to a new build (everything keeps running), and restarted
//! (tabs reopen, agents resumed).

mod common;

use std::time::{Duration, Instant};

use common::{AGENT, ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

/// Each entry as "Kind: detail".
fn thread(scratch: &Scratch, pane: &str) -> Vec<String> {
    let out = scratch.artty(&["thread", "--tab", pane, "--json"]);
    let entries: Value = serde_json::from_str(&stdout(&out)).unwrap();
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let (kind, item) = entry["item"].as_object().unwrap().iter().next().unwrap();
            let detail = match kind.as_str() {
                "Prompt" => item["text"].to_string(),
                "Reply" => item["text"].to_string(),
                "Session" => item["event"].to_string(),
                _ => item.to_string(),
            };
            format!("{kind}: {detail}")
        })
        .collect()
}

fn thread_listed(scratch: &Scratch, pane: &str) -> bool {
    let out = scratch.artty(&["list", "--json"]);
    stdout(&out).contains(&format!("\"id\": {pane},"))
}

/// An ask's state, as `artty ask --list` shows it.
fn ask_state(scratch: &Scratch, id: &str) -> String {
    let out = scratch.artty(&["ask", "--result", id, "--timeout", "0", "--json"]);
    let ask: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
    ask["state"].as_str().unwrap_or_default().to_owned()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The panes running agents (the stand-in's), by id.
fn agent_panes(scratch: &Scratch) -> Vec<String> {
    let out = scratch.artty(&["agents", "--json"]);
    let agents: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
    let agents = agents.as_array().cloned().unwrap_or_default();
    agents.iter().map(|a| a["pane"].to_string()).collect()
}

#[test]
fn a_hand_over_keeps_threads_and_asks_going() {
    let scratch = Scratch::new();
    let pane = scratch.agent("8");
    // Someone waiting for the reply: its connection goes with the old build.
    scratch.ask(&["--tab", &pane, "--timeout", "20", "warm up"]);
    let waiting = scratch
        .command(&["ask", "--tab", &pane, "--timeout", "30", "slow one"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut slow = String::new();
    wait_for("the ask to be under way", || {
        let out = scratch.artty(&["ask", "--list", "--json"]);
        let list: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
        let running = list.as_array().into_iter().flatten().find(|a| a["state"] == "running");
        slow = running.map(|a| a["id"].to_string()).unwrap_or_default();
        !slow.is_empty()
    });
    let queued = stdout(&scratch.ask(&["--tab", &pane, "--no-wait", "queued one"]));
    assert_eq!(ask_state(&scratch, &queued), "queued");

    scratch.hand_over();
    assert_eq!(ask_state(&scratch, &slow), "running", "handed over mid-turn");

    // The one under way is answered (to whoever waited for it), then the
    // queued one goes in.
    let out = waiting.wait_with_output().unwrap();
    assert_eq!(stdout(&out), "echo: slow one", "{}", stderr(&out));
    let out = scratch.ask(&["--result", &slow, "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: slow one", "{}", stderr(&out));
    let out = scratch.ask(&["--result", &queued, "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: queued one", "{}", stderr(&out));
    assert_eq!(
        thread(&scratch, &pane),
        [
            r#"Session: "started""#,
            r#"Prompt: "warm up""#,
            r#"Reply: "echo: warm up""#,
            r#"Prompt: "slow one""#,
            r#"Reply: "echo: slow one""#,
            r#"Prompt: "queued one""#,
            r#"Reply: "echo: queued one""#,
        ]
    );
}

#[test]
fn a_restart_brings_threads_back_to_their_reopened_panes() {
    let mut scratch = Scratch::with_config(&resumable("3"));
    // Pane ids start over after a restart: arranged so the agent's comes
    // back as another (1), and its old one (2) goes to the shell beside it.
    let gone = stdout(&scratch.artty(&["new", "--background"]));
    let pane = scratch.agent("3");
    scratch.artty(&["kill", &gone, "--pane"]);
    wait_for("the pane to close", || !thread_listed(&scratch, &gone));
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "hello"]);
    assert_eq!(stdout(&out), "echo: hello", "{}", stderr(&out));
    // A shell beside it, with nothing to take back.
    let shell = stdout(&scratch.artty(&["new", "--background"]));
    let cut = stdout(&scratch.ask(&["--tab", &pane, "--no-wait", "cut short"]));
    wait_for("the agent to take it", || ask_state(&scratch, &cut) == "running");
    let queued = stdout(&scratch.ask(&["--tab", &pane, "--no-wait", "after the restart"]));

    scratch.restart();

    // The agent was stopped mid-turn: that ask failed (and isn't sent
    // again); the queued one goes to the agent resumed in the reopened tab.
    let out = scratch.artty(&["ask", "--result", &cut, "--timeout", "0", "--json"]);
    let ask: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(ask["state"], "failed", "{ask}");
    assert!(ask["error"].as_str().unwrap().contains("restarted"), "{ask}");
    let out = scratch.ask(&["--result", &queued, "--timeout", "30"]);
    assert_eq!(stdout(&out), "echo: after the restart", "{}", stderr(&out));

    let panes = agent_panes(&scratch);
    let [reopened] = panes.as_slice() else { panic!("agents: {panes:?}") };
    assert_ne!(reopened, &pane, "a new pane, not the old one");
    assert_eq!(shell, "3");
    assert!(thread(&scratch, &pane).is_empty(), "the old id, now the shell's");
    assert_eq!(
        thread(&scratch, reopened),
        [
            r#"Session: "started""#,
            r#"Prompt: "hello""#,
            r#"Reply: "echo: hello""#,
            r#"Prompt: "cut short""#,
            r#"Session: "started""#,
            r#"Prompt: "after the restart""#,
            r#"Reply: "echo: after the restart""#,
        ]
    );
    // The other tab (the shell, reopened with the agent's old id) has none.
    let out = scratch.artty(&["list", "--json"]);
    let tabs: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let ids: Vec<String> = tabs.as_array().unwrap().iter().map(|t| t["id"].to_string()).collect();
    assert_eq!(ids.len(), 2, "{tabs}");
    for id in ids.iter().filter(|id| *id != reopened) {
        assert!(thread(&scratch, id).is_empty(), "{id}");
    }

    // And again: what it had, plus nothing new.
    scratch.restart();
    let panes = agent_panes(&scratch);
    wait_for("the agent to be resumed again", || agent_panes(&scratch).len() == 1);
    let reopened = &agent_panes(&scratch)[0];
    let entries = thread(&scratch, reopened);
    assert_eq!(
        entries[..7],
        [
            r#"Session: "started""#,
            r#"Prompt: "hello""#,
            r#"Reply: "echo: hello""#,
            r#"Prompt: "cut short""#,
            r#"Session: "started""#,
            r#"Prompt: "after the restart""#,
            r#"Reply: "echo: after the restart""#,
        ],
        "{panes:?}"
    );
}

/// How the stand-in agent is resumed, as artty resumes Claude Code.
fn resumable(delay: &str) -> String {
    format!("[resume.fake]\nlatest = \"python3 {AGENT} {ARTTY} {delay}\"\n")
}

/// The one agent's pane, once it's (re)started.
fn the_agent(scratch: &Scratch) -> String {
    wait_for("an agent", || agent_panes(scratch).len() == 1);
    agent_panes(scratch).remove(0)
}

#[test]
fn a_permission_request_keeps_its_answer() {
    let mut scratch = Scratch::with_config(&resumable("0.3"));
    let pane = scratch.agent("0.3");
    let ask = stdout(&scratch.ask(&["--tab", &pane, "--no-wait", "tool: touch x"]));
    let mut id = String::new();
    wait_for("the request", || {
        let out = scratch.artty(&["approvals", "--tab", &pane, "--json"]);
        let list: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
        id = list[0]["id"].to_string();
        list[0]["state"] == "pending"
    });
    scratch.artty(&["approval", "allow", &id]);
    let out = scratch.ask(&["--result", &ask, "--timeout", "20"]);
    assert_eq!(stdout(&out), "ran: touch x", "{}", stderr(&out));

    // Nobody looked at the thread since: it's on disk as answered all the same.
    scratch.restart();
    let entries = thread(&scratch, &the_agent(&scratch));
    let permission: Vec<&String> = entries.iter().filter(|e| e.starts_with("Permission")).collect();
    assert_eq!(permission.len(), 1, "{entries:?}");
    assert!(permission[0].contains(r#""state":"allowed""#), "{entries:?}");
}

#[test]
fn a_crash_doesnt_send_an_ask_twice() {
    let mut scratch = Scratch::with_config(&resumable("0.3"));
    let pane = scratch.agent("6");
    // The tabs are saved every few seconds.
    std::thread::sleep(Duration::from_secs(6));
    let ask = stdout(&scratch.ask(&["--tab", &pane, "--no-wait", "only once"]));
    wait_for("the agent to take it", || ask_state(&scratch, &ask) == "running");

    scratch.crash();

    let out = scratch.artty(&["ask", "--result", &ask, "--timeout", "0", "--json"]);
    let info: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(info["state"], "failed", "{info}");
    // The resumed agent is never given it again.
    let reopened = the_agent(&scratch);
    std::thread::sleep(Duration::from_secs(4));
    let entries = thread(&scratch, &reopened);
    let given = entries.iter().filter(|e| e.as_str() == r#"Prompt: "only once""#).count();
    assert_eq!(given, 1, "{entries:?}");
}
