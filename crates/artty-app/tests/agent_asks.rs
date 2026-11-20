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
