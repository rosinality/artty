//! Agents started in worktrees of their own (`artty new --worktree`),
//! against a scratch server and a scratch repository.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{AGENT, ARTTY, Scratch, stderr, stdout};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A stand-in agent in worktree `name` of `repo`, given `task`: its pane,
/// once it answered.
fn start(scratch: &Scratch, repo: &Path, name: &str, task: &str) -> String {
    let repo = repo.display().to_string();
    let out = scratch.artty(&[
        "new",
        "--background",
        "--cwd",
        &repo,
        "--worktree",
        name,
        "--task",
        task,
        "--",
        "python3",
        AGENT,
        ARTTY,
        "0.3",
    ]);
    let pane = stdout(&out);
    let err = stderr(&out);
    let ask = err.split("is ask ").nth(1).expect(&err).split_whitespace().next().unwrap();
    let answer = scratch.ask(&["--result", ask, "--timeout", "30"]);
    assert_eq!(stdout(&answer), format!("echo: {task}"), "{}", stderr(&answer));
    pane
}

fn agent(scratch: &Scratch, pane: &str) -> Value {
    let out = scratch.artty(&["agents", "--json"]);
    let agents: Value = serde_json::from_str(&stdout(&out)).unwrap();
    agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["pane"].as_u64().map(|p| p.to_string()).as_deref() == Some(pane))
        .unwrap()
        .clone()
}

#[test]
fn agents_in_worktrees_of_their_own() {
    let scratch = Scratch::new();
    // (The server ends with its last tab.)
    scratch.artty(&["new", "--background"]);
    let repo: PathBuf = scratch.dir.join("notes");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("plan.md"), "plan\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    let beside = PathBuf::from(format!("{}.worktrees", repo.display()));

    // A new branch from HEAD, checked out beside the repository; the tab's
    // named after it.
    let pane = start(&scratch, &repo, "fix-intro", "tidy the intro");
    let tree = beside.join("fix-intro");
    assert!(tree.join("plan.md").exists());
    assert_eq!(git(&tree, &["branch", "--show-current"]), "fix-intro");
    let info = agent(&scratch, &pane);
    assert_eq!(info["title"], "fix-intro", "{info}");
    assert_eq!(info["cwd"].as_str().map(Path::new), Some(tree.as_path()), "{info}");

    // Another, left with a change: closing its tab keeps it.
    let other = start(&scratch, &repo, "draft", "draft the faq");
    std::fs::write(beside.join("draft/faq.md"), "q\n").unwrap();

    // Closed: the unchanged one goes (its branch stays), the changed one stays.
    for pane in [&pane, &other] {
        let out = scratch.artty(&["kill", pane, "--pane"]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    wait_for("the unchanged worktree to go", || !tree.exists());
    assert!(git(&repo, &["branch", "--list", "fix-intro"]).contains("fix-intro"));
    std::thread::sleep(Duration::from_secs(1));
    assert!(beside.join("draft/faq.md").exists(), "a worktree with changes stays");

    // Not a repository, or not a branch name: refused, nothing opened.
    let out = scratch.artty(&[
        "new",
        "--background",
        "--cwd",
        &scratch.dir.display().to_string(),
        "--worktree",
        "x",
        "--agent",
        "claude",
    ]);
    assert!(stderr(&out).contains("isn't in a git repository"), "{}", stderr(&out));
    let out = scratch.artty(&[
        "new",
        "--background",
        "--cwd",
        &repo.display().to_string(),
        "--worktree",
        "a b",
        "--agent",
        "claude",
    ]);
    assert!(stderr(&out).contains("a branch name"), "{}", stderr(&out));
}
