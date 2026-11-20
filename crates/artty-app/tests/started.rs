//! Agents started with a task (`artty new --task`), and the files agents
//! change, against a scratch server.

mod common;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{AGENT, ARTTY, Scratch, stderr, stdout};
use serde_json::{Value, json};

/// Each entry's kind and item.
fn thread(scratch: &Scratch, pane: &str) -> Vec<(String, Value)> {
    let out = scratch.artty(&["thread", "--tab", pane, "--json"]);
    let entries: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let entries = entries.as_array().unwrap().iter();
    entries
        .map(|e| {
            let (kind, item) = e["item"].as_object().unwrap().iter().next().unwrap();
            (kind.clone(), item.clone())
        })
        .collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn agents_start_agents_two_deep_at_most() {
    let scratch = Scratch::new();
    // Each level's pane starts the next (with a task), and says how it went.
    let script = scratch.dir.join("chain.sh");
    let dir = scratch.dir.display();
    std::fs::write(
        &script,
        format!(
            "'{ARTTY}' new --background --title \"level $1\" --task \"go on, level $1\" -- sh \
             '{}' $(( $1 + 1 )) > \"{dir}/level$1.tmp\" 2>&1; mv \"{dir}/level$1.tmp\" \
             \"{dir}/level$1.txt\"; sleep 120\n",
            script.display()
        ),
    )
    .unwrap();
    let first = stdout(&scratch.artty(&[
        "new",
        "--background",
        "--title",
        "top",
        "--",
        "sh",
        &script.display().to_string(),
        "1",
    ]));
    let level = |n: u32| std::fs::read_to_string(scratch.dir.join(format!("level{n}.txt")));
    wait_for("three levels", || level(3).is_ok());
    let started = |n: u32| level(n).unwrap();
    assert!(started(1).contains("its task is ask"), "{}", started(1));
    assert!(started(2).contains("its task is ask"), "{}", started(2));
    assert!(started(3).contains("2 deep at most"), "{}", started(3));

    // The first one's task came from the top pane, whose thread has its card.
    let child = started(1).lines().next().unwrap().trim().to_owned();
    let prompts: Vec<Value> = thread(&scratch, &child)
        .into_iter()
        .filter(|(kind, _)| kind == "Handoff")
        .map(|(_, item)| item)
        .collect();
    assert_eq!(prompts.len(), 1, "level 2's task, from level 1: {prompts:?}");
    let cards: Vec<Value> = thread(&scratch, &first)
        .into_iter()
        .filter(|(k, _)| k == "Handoff")
        .map(|(_, i)| i)
        .collect();
    assert_eq!(cards.len(), 1, "{cards:?}");
    assert_eq!(cards[0]["text"], "go on, level 1");
    assert_eq!(cards[0]["by_agent"], true);
}

#[test]
fn a_task_for_a_new_agent() {
    let scratch = Scratch::new();
    let out = scratch.artty(&[
        "new",
        "--background",
        "--task",
        "hello there",
        "--",
        "python3",
        AGENT,
        ARTTY,
        "0.3",
    ]);
    let pane = stdout(&out);
    let err = stderr(&out);
    let ask = err.split("is ask ").nth(1).unwrap().split_whitespace().next().unwrap();
    let out = scratch.ask(&["--result", ask, "--timeout", "30"]);
    assert_eq!(stdout(&out), "echo: hello there", "{}", stderr(&out));
    let kinds: Vec<String> = thread(&scratch, &pane).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, ["Session", "Prompt", "Reply"]);
    // No agent to give it to.
    let out = scratch.artty(&["new", "--background", "--task", "x"]);
    assert!(stderr(&out).contains("needs an agent"), "{}", stderr(&out));
}

/// Report an edit tool's use as pane `pane`'s agent's hook would.
fn edited(scratch: &Scratch, pane: &str, input: Value) {
    let event = json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Edit",
                       "tool_input": input});
    let mut child = scratch
        .command(&["status", "working", "--hook", "--agent", "fake", "--tab", pane])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn files_changed_in_a_turn_and_two_agents_on_one() {
    let scratch = Scratch::new();
    let (api, web) = (scratch.agent("3"), scratch.agent("0.3"));
    for pane in [&api, &web] {
        let out = scratch.ask(&["--tab", pane, "--timeout", "20", "--no-wait", "warm up"]);
        let id = stdout(&out);
        scratch.ask(&["--result", &id, "--timeout", "20"]);
    }
    let ask = stdout(&scratch.ask(&["--tab", &api, "--no-wait", "edit the plan"]));
    wait_for("its turn", || {
        thread(&scratch, &api).iter().filter(|(k, _)| k == "Prompt").count() == 2
    });
    let plan = json!({"file_path": "/docs/plan.md", "old_string": "a\nb", "new_string": "c"});
    edited(&scratch, &api, plan.clone());
    edited(
        &scratch,
        &api,
        json!({"file_path": "/docs/faq.md", "old_string": "", "new_string": "q"}),
    );
    edited(&scratch, &web, plan);
    let out = scratch.ask(&["--result", &ask, "--timeout", "20"]);
    assert_eq!(stdout(&out), "echo: edit the plan", "{}", stderr(&out));

    let entries = thread(&scratch, &api);
    let kinds: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds[kinds.len() - 4..], ["Prompt", "Overlap", "Edited", "Reply"], "{entries:?}");
    let files = &entries[entries.len() - 2].1["files"];
    assert_eq!(files[0]["path"], "/docs/plan.md");
    assert_eq!((files[0]["added"].as_u64(), files[0]["removed"].as_u64()), (Some(1), Some(2)));
    assert_eq!(files[1]["path"], "/docs/faq.md");
    // Both are told, each of the other.
    let overlap = |pane: &str| {
        thread(&scratch, pane).into_iter().find(|(k, _)| k == "Overlap").map(|(_, i)| i).unwrap()
    };
    assert_eq!(overlap(&api)["other"]["pane"].to_string(), web);
    assert_eq!(overlap(&web)["other"]["pane"].to_string(), api);
    assert_eq!(overlap(&web)["file"], "/docs/plan.md");
}
