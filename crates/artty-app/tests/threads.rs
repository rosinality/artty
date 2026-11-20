//! An agent's thread (`artty thread`), against a scratch server: what its
//! turns, asks and permission requests leave there, in order.

mod common;

use std::time::{Duration, Instant};

use common::{Scratch, stderr, stdout};
use serde_json::Value;

/// The pane's pending permission request's id, once there.
fn pending(scratch: &Scratch, pane: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = scratch.artty(&["approvals", "--tab", pane, "--json"]);
        let list: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
        if let Some(first) = list.as_array().and_then(|l| l.first())
            && first["state"] == "pending"
        {
            return first["id"].to_string();
        }
        assert!(Instant::now() < deadline, "no pending request: {list}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

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
                "Prompt" => format!("{} (ask: {})", item["text"], !item["ask"].is_null()),
                "Reply" => item["text"].to_string(),
                "Permission" => format!("{} {}", item["summary"], item["state"]),
                "Session" => item["event"].to_string(),
                _ => item.to_string(),
            };
            format!("{kind}: {detail}")
        })
        .collect()
}

#[test]
fn turns_asks_and_permissions_in_order() {
    let scratch = Scratch::new();
    let pane = scratch.agent("0.3");
    let out = scratch.ask(&["--tab", &pane, "--timeout", "20", "hello"]);
    assert_eq!(stdout(&out), "echo: hello", "{}", stderr(&out));

    // Allowed in artty.
    let ask = scratch.ask(&["--tab", &pane, "--no-wait", "tool: touch x"]);
    let id = pending(&scratch, &pane);
    scratch.artty(&["approval", "allow", &id]);
    scratch.ask(&["--result", &stdout(&ask), "--timeout", "20"]);

    // Turned down at the agent's prompt: the turn stops there.
    let ask = scratch.ask(&["--tab", &pane, "--no-wait", "tool: touch never"]);
    pending(&scratch, &pane);
    scratch.artty(&["send", "--tab", &pane, "--no-enter", "4"]);
    scratch.ask(&["--result", &stdout(&ask), "--timeout", "20"]);

    assert_eq!(
        thread(&scratch, &pane),
        [
            r#"Session: "started""#,
            r#"Prompt: "hello" (ask: true)"#,
            r#"Reply: "echo: hello""#,
            r#"Prompt: "tool: touch x" (ask: true)"#,
            r#"Permission: "touch x" "allowed""#,
            r#"Reply: "ran: touch x""#,
            r#"Prompt: "tool: touch never" (ask: true)"#,
            r#"Permission: "touch never" "dismissed""#,
            r#"Interrupted: {"reason":"a permission prompt was turned down"}"#,
        ]
    );

    // Listed among the agents, with its unread replies.
    let out = scratch.artty(&["agents", "--json"]);
    let agents: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let agent = &agents[0];
    assert_eq!((agent["pane"].to_string(), agent["agent"].as_str()), (pane.clone(), Some("fake")));
    assert_eq!(agent["unread"], 2, "{agent}");

    // The last few, and what came after one.
    let out = scratch.artty(&["thread", "--tab", &pane, "--lines", "2"]);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("⚑ permission") && text.contains("✕ interrupted"), "{text}");
}

#[test]
fn searched_across_agents() {
    let scratch = Scratch::new();
    let (api, web) = (scratch.agent("0.3"), scratch.agent("0.3"));
    let out = scratch.ask(&["--tab", &api, "--timeout", "20", "fix the Login redirect"]);
    assert_eq!(stdout(&out), "echo: fix the Login redirect", "{}", stderr(&out));
    let out = scratch.ask(&["--tab", &web, "--timeout", "20", "the login form's colors"]);
    assert_eq!(stdout(&out), "echo: the login form's colors", "{}", stderr(&out));

    let out = scratch.artty(&["thread", "--search", "login", "--json"]);
    let hits: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let found: Vec<(String, String)> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["pane"].to_string(), h["kind"].as_str().unwrap().to_owned()))
        .collect();
    // Newest first: web's reply and prompt, then api's.
    assert_eq!(
        found,
        [
            (web.clone(), "answered".to_owned()),
            (web.clone(), "asked".to_owned()),
            (api.clone(), "answered".to_owned()),
            (api.clone(), "asked".to_owned()),
        ]
    );
    // Capitals: exact; one agent's; none.
    let out = scratch.artty(&["thread", "--search", "Login", "--json"]);
    let hits: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 2, "{hits}");
    let out = scratch.artty(&["thread", "--search", "login", "--tab", &api]);
    assert_eq!(stdout(&out).lines().count(), 2, "{}", stdout(&out));
    let out = scratch.artty(&["thread", "--search", "nowhere"]);
    assert_eq!(out.status.code(), Some(1));
}
