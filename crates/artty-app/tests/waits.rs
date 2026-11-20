//! Waiting on several asks (`artty wait`), and following every agent's
//! thread (`artty events`), against a scratch server.

mod common;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Scratch, stderr, stdout};
use serde_json::Value;

#[test]
fn the_first_of_several_asks_or_all() {
    let scratch = Scratch::new();
    let (quick, slow) = (scratch.agent("0.3"), scratch.agent("4"));
    for pane in [&quick, &slow] {
        let id = stdout(&scratch.ask(&["--tab", pane, "--no-wait", "warm up"]));
        scratch.ask(&["--result", &id, "--timeout", "20"]);
    }
    let a = stdout(&scratch.ask(&["--tab", &quick, "--no-wait", "quick one"]));
    let b = stdout(&scratch.ask(&["--tab", &slow, "--no-wait", "slow one"]));

    // The first: the quick one's answer, the slow one still at it.
    let started = Instant::now();
    let out = scratch.artty(&["wait", &a, &b, "--timeout", "20", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    let asks: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(asks[0]["reply"], "echo: quick one", "{asks}");
    assert_ne!(asks[1]["state"], "answered", "{asks}");

    // Not over yet: exit 3, as it is.
    let out = scratch.artty(&["wait", &b, "--timeout", "0"]);
    assert_eq!(out.status.code(), Some(3), "{}", stdout(&out));

    // Every one.
    let out = scratch.artty(&["wait", &a, &b, "--all", "--timeout", "20"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("echo: quick one") && text.contains("echo: slow one"), "{text}");

    let out = scratch.artty(&["wait", "12345"]);
    assert!(stderr(&out).contains("no ask 12345"), "{}", stderr(&out));
}

#[test]
fn every_agents_thread_as_it_comes() {
    let scratch = Scratch::new();
    let agent = scratch.agent("0.3");
    let id = stdout(&scratch.ask(&["--tab", &agent, "--no-wait", "warm up"]));
    scratch.ask(&["--result", &id, "--timeout", "20"]);

    let mut follow =
        scratch.command(&["events", "--follow"]).stdout(Stdio::piped()).spawn().unwrap();
    let lines = BufReader::new(follow.stdout.take().unwrap()).lines();
    // (Following from now: what came before isn't repeated.)
    std::thread::sleep(Duration::from_millis(500));
    scratch.ask(&["--tab", &agent, "--timeout", "20", "what's new?"]);
    let mut seen = Vec::new();
    for line in lines {
        let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let reply = event["item"]["Reply"]["text"].as_str().map(str::to_owned);
        seen.push(event);
        if reply.is_some() {
            break;
        }
    }
    let _ = follow.kill();
    let _ = follow.wait();
    let kinds: Vec<String> = seen
        .iter()
        .map(|e| e["item"].as_object().unwrap().keys().next().unwrap().clone())
        .collect();
    assert_eq!(kinds, ["Prompt", "Reply"], "{seen:?}");
    assert_eq!(seen[0]["pane"].to_string(), agent);
    assert_eq!(seen[1]["item"]["Reply"]["text"], "echo: what's new?");

    // What came after an entry, and done.
    let after = seen[0]["seq"].to_string();
    let out = scratch.artty(&["events", "--after", &after]);
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains("echo: what's new?"));
}
