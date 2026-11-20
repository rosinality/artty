//! Agents asking agents, against a scratch server: an ask from a program in
//! one of artty's panes says which pane asked; asking your own is refused.

mod common;

use std::time::{Duration, Instant};

use common::{ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

/// Run `script` (sh) in a new pane titled `title`; what it printed, once it
/// wrote it (to `name` in the scratch folder).
fn in_a_pane(scratch: &Scratch, title: &str, name: &str, script: &str) -> String {
    let out = scratch.dir.join(name);
    let script = format!("{script} > '{}.tmp' 2>&1; mv '{0}.tmp' '{0}'; sleep 60", out.display());
    let opened =
        scratch.artty(&["new", "--background", "--title", title, "--", "sh", "-c", &script]);
    assert!(opened.status.success(), "{}", stderr(&opened));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(&out) {
            return text.trim().to_owned();
        }
        assert!(Instant::now() < deadline, "{title}: nothing written");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn an_agent_asking_another_is_named_and_its_own_pane_refused() {
    let scratch = Scratch::new();
    let agent = scratch.agent("0.3");
    // Wait for it to be up (its hooks reported).
    let out = scratch.ask(&["--tab", &agent, "--timeout", "20", "warm up"]);
    assert_eq!(stdout(&out), "echo: warm up", "{}", stderr(&out));

    let reply = in_a_pane(
        &scratch,
        "planner",
        "asked.txt",
        &format!("'{ARTTY}' ask --tab {agent} --timeout 20 'from the planner'"),
    );
    assert!(reply.ends_with("echo: from the planner"), "{reply}");

    // Its thread says who asked.
    let out = scratch.artty(&["thread", "--tab", &agent, "--json"]);
    let entries: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let prompts: Vec<&Value> =
        entries.as_array().unwrap().iter().filter_map(|e| e["item"].get("Prompt")).collect();
    assert_eq!(prompts.len(), 2, "{entries}");
    assert!(prompts[0]["from"].is_null(), "{entries}");
    assert_eq!(prompts[1]["from"]["title"], "planner", "{entries}");
    let text = stdout(&scratch.artty(&["thread", "--tab", &agent]));
    assert!(text.contains("planner (pane "), "{text}");
    // And the planner's own thread has the card, as the asker.
    let planner = prompts[1]["from"]["pane"].to_string();
    let card = handoffs(&scratch, &planner);
    assert_eq!(card.len(), 1, "{card:?}");
    assert_eq!(
        (card[0]["by_agent"].as_bool(), card[0]["state"].as_str()),
        (Some(true), Some("answered"))
    );

    // An ask of its own pane would wait for the turn it's in: refused by
    // the command, and (not knowing its pane) by the server.
    let refused = in_a_pane(
        &scratch,
        "loner",
        "self.txt",
        &format!("'{ARTTY}' ask --tab \"$ARTTY_SESSION_ID\" --timeout 5 'me?'"),
    );
    assert!(refused.contains("is your own"), "{refused}");
    let refused = in_a_pane(
        &scratch,
        "loner",
        "self2.txt",
        &format!(
            "id=$ARTTY_SESSION_ID; env -u ARTTY_SESSION_ID '{ARTTY}' ask --tab \"$id\" \
             --timeout 5 'me?'"
        ),
    );
    assert!(refused.contains("would wait for the turn"), "{refused}");
}

/// `pane`'s thread's handoff cards.
fn handoffs(scratch: &Scratch, pane: &str) -> Vec<Value> {
    let out = scratch.artty(&["thread", "--tab", pane, "--json"]);
    let entries: Value = serde_json::from_str(&stdout(&out)).unwrap();
    entries.as_array().unwrap().iter().filter_map(|e| e["item"].get("Handoff").cloned()).collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An ask written in `here`'s thread to `to` (as the app sends one).
fn ask_from_thread(scratch: &Scratch, to: &str, here: &str, text: &str, back: bool) -> String {
    let request = serde_json::json!({
        "cmd": "ask", "tab": to.parse::<u64>().unwrap(), "text": text, "wait": false,
        "thread": here.parse::<u64>().unwrap(), "back": back,
    });
    let reply: Value = serde_json::from_str(&scratch.control(&request.to_string())).unwrap();
    reply["ask"]["id"].to_string()
}

#[test]
fn answers_brought_back_to_the_thread_they_were_asked_from() {
    let scratch = Scratch::new();
    let (api, web) = (scratch.agent("0.3"), scratch.agent("0.3"));
    for pane in [&api, &web] {
        let out = scratch.ask(&["--tab", pane, "--timeout", "20", "warm up"]);
        assert_eq!(stdout(&out), "echo: warm up", "{}", stderr(&out));
    }

    // Written from api's thread to web, its answer to come back.
    ask_from_thread(&scratch, &web, &api, "which tests cover login?", true);
    wait_for("the answer to come back", || {
        handoffs(&scratch, &api).first().is_some_and(|c| !c["forwarded"].is_null())
    });
    let card = &handoffs(&scratch, &api)[0];
    assert_eq!(card["state"], "answered", "{card}");
    assert_eq!(card["reply"], "echo: which tests cover login?", "{card}");
    // api is given it, from web.
    let forwarded = card["forwarded"].to_string();
    let out = scratch.ask(&["--result", &forwarded, "--timeout", "20"]);
    let answer = stdout(&out);
    assert!(answer.starts_with("echo: Answer from "), "{answer}");
    assert!(
        answer.contains("> which tests cover login?")
            && answer.ends_with("echo: which tests cover login?"),
        "{answer}"
    );
    let out = scratch.artty(&["thread", "--tab", &api, "--json"]);
    assert!(stdout(&out).contains(&format!("\"pane\": {web}")), "from web");

    // Not asked to come back: brought back by hand, once.
    let id = ask_from_thread(&scratch, &web, &api, "and the logout ones?", false);
    let out = scratch.ask(&["--result", &id, "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: and the logout ones?", "{}", stderr(&out));
    // (Exit 3: the new ask isn't answered yet.)
    let out = scratch.artty(&["ask", "--bring-back", &id, "--json"]);
    let brought: Value = serde_json::from_str(&stdout(&out)).unwrap_or(Value::Null);
    assert_eq!(brought["pane"].to_string(), api, "{}", stderr(&out));
    let again = scratch.artty(&["ask", "--bring-back", &id]);
    assert!(stderr(&again).contains("already brought back"), "{}", stderr(&again));
    wait_for("the card to say so", || {
        handoffs(&scratch, &api).get(1).is_some_and(|c| !c["forwarded"].is_null())
    });
    // web's own thread has no cards: it was only asked.
    assert!(handoffs(&scratch, &web).is_empty());
}

#[test]
fn the_guide_and_its_skill() {
    let scratch = Scratch::new();
    let guide = stdout(&scratch.artty(&["guide"]));
    assert!(guide.contains("artty ask --tab N"), "{guide}");
    // The server wrote it beside the config file.
    let written = std::fs::read_to_string(scratch.dir.join("config/artty/AGENTS.md")).unwrap();
    assert_eq!(written.trim_end(), guide);

    let out = scratch.artty(&["guide", "install"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let skill = scratch.dir.join(".claude/skills/artty/SKILL.md");
    assert!(std::fs::read_to_string(&skill).unwrap().contains("Run `artty guide`"));
    scratch.artty(&["guide", "uninstall"]);
    assert!(!skill.exists());
}
