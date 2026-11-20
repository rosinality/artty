//! Coding agents' conversations on this machine, searched: Claude Code's
//! (`~/.claude/projects/<project>/<session>.jsonl`) and Codex's
//! (`~/.codex/sessions/YYYY/MM/DD/rollout-…-<session>.jsonl`), newest first,
//! one hit each: its directory, first request, and the text around the
//! match. Lines without the text are passed over before being parsed, and
//! a search reads at most so many files and bytes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use artty_core::protocol::SessionHit;
use serde_json::Value;

const MAX_FILES: usize = 500;
const MAX_BYTES: u64 = 256 << 20;
const MAX_HITS: usize = 50;

/// Conversations mentioning `text` (case ignored unless it has capitals).
pub fn search(text: &str) -> Vec<SessionHit> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut files = transcripts();
    files.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));
    let mut hits = Vec::new();
    let mut read = 0;
    for (agent, path, modified) in files.into_iter().take(MAX_FILES) {
        if hits.len() >= MAX_HITS || read >= MAX_BYTES {
            break;
        }
        read += std::fs::metadata(&path).map_or(0, |m| m.len());
        if let Some(hit) = search_file(agent, &path, modified, text) {
            hits.push(hit);
        }
    }
    hits
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// A Claude Code conversation's title, as its transcript has it: the name
/// it was given (`/rename`), else the latest one Claude Code made up for it.
/// Read from the end (the transcript can be large), all of it only if need be.
pub fn claude_title(session: &str) -> Option<String> {
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".claude")))?;
    claude_title_in(&claude, session)
}

/// A Claude Code conversation's transcript, by its id.
fn claude_transcript(claude: &Path, session: &str) -> Option<PathBuf> {
    // (A session id names a file: nothing else in it.)
    if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let name = format!("{session}.jsonl");
    std::fs::read_dir(claude.join("projects"))
        .ok()?
        .flatten()
        .map(|dir| dir.path().join(&name))
        .find(|path| path.is_file())
}

/// The prompts (their ids) in a Claude Code conversation as it stands now,
/// back from the one `prompt` (its turn's): those it was rewound past (Esc
/// Esc, restored to before them) aren't. The transcript keeps every
/// message, each naming the one before it; a rewind starts a new line from
/// an earlier one.
pub fn claude_prompts_kept(
    session: &str,
    prompt: &str,
) -> Option<std::collections::HashSet<String>> {
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".claude")))?;
    claude_prompts_kept_in(&claude, session, prompt)
}

fn claude_prompts_kept_in(
    claude: &Path,
    session: &str,
    prompt: &str,
) -> Option<std::collections::HashSet<String>> {
    use std::io::Read;
    let path = claude_transcript(claude, session)?;
    let mut text = String::new();
    std::fs::File::open(path).ok()?.take(256 << 20).read_to_string(&mut text).ok()?;
    // Each message: the one before it, and its prompt.
    let mut before: std::collections::HashMap<String, (Option<String>, Option<String>)> =
        Default::default();
    let mut start = None;
    for line in text.lines().filter(|l| l.contains("\"uuid\"")) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
        let Some(uuid) = text("uuid") else { continue };
        // (Compacted: a new line, naming where the old one ended.)
        let parent = text("parentUuid").or_else(|| text("logicalParentUuid"));
        let prompt_id = text("promptId");
        if prompt_id.as_deref() == Some(prompt) && text("type").as_deref() == Some("user") {
            start = Some(uuid.clone());
        }
        before.insert(uuid, (parent, prompt_id));
    }
    let mut kept = std::collections::HashSet::new();
    let mut at = start?;
    // (A loop can't be, but the file isn't ours.)
    for _ in 0..before.len() {
        let Some((parent, prompt_id)) = before.get(&at) else { break };
        kept.extend(prompt_id.clone());
        match parent {
            Some(parent) => at = parent.clone(),
            None => break,
        }
    }
    Some(kept)
}

/// [`claude_title`], Claude Code's folder being `claude`.
fn claude_title_in(claude: &Path, session: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = claude_transcript(claude, session)?;
    let title_in = |text: &str| -> Option<String> {
        let mut found: (Option<String>, Option<String>) = (None, None);
        for line in text.lines().filter(|l| l.contains("-title\"")) {
            let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
            let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
            match entry.get("type").and_then(Value::as_str) {
                Some("custom-title") => found.0 = text("customTitle").or(found.0),
                Some("ai-title") => found.1 = text("aiTitle").or(found.1),
                _ => {},
            }
        }
        found.0.or(found.1).filter(|t| !t.trim().is_empty())
    };
    let mut file = std::fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    const TAIL: u64 = 512 << 10;
    if len > TAIL {
        file.seek(SeekFrom::Start(len - TAIL)).ok()?;
        let mut tail = Vec::new();
        file.by_ref().take(TAIL).read_to_end(&mut tail).ok()?;
        // (Its first line may be cut: a whole line or none matches.)
        if let Some(title) = title_in(&String::from_utf8_lossy(&tail)) {
            return Some(title);
        }
        file.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut all = String::new();
    file.take(64 << 20).read_to_string(&mut all).ok()?;
    title_in(&all)
}

/// Every transcript: its agent, path, and modification time.
fn transcripts() -> Vec<(&'static str, PathBuf, u64)> {
    let mut files = Vec::new();
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".claude")));
    if let Some(projects) = claude.map(|c| c.join("projects")) {
        collect(&projects, 2, "claude", &mut files);
    }
    let codex =
        std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| Some(home()?.join(".codex")));
    if let Some(sessions) = codex.map(|c| c.join("sessions")) {
        collect(&sessions, 4, "codex", &mut files);
    }
    files
}

/// `.jsonl` files up to `depth` folders down.
fn collect(
    dir: &Path,
    depth: usize,
    agent: &'static str,
    files: &mut Vec<(&'static str, PathBuf, u64)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };
        if metadata.is_dir() && depth > 1 {
            collect(&path, depth - 1, agent, files);
        } else if metadata.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            files.push((agent, path, modified));
        }
    }
}

/// What people and agents said in a transcript line: `text` and string
/// `content` fields, anywhere in it.
fn texts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match value {
                    Value::String(s) if key == "text" || key == "content" || key == "summary" => {
                        out.push(s.clone())
                    },
                    _ => texts(value, out),
                }
            }
        },
        Value::Array(items) => items.iter().for_each(|v| texts(v, out)),
        _ => {},
    }
}

/// A request a person typed (not a tool's result, nor what tools add
/// around it, which start with a tag).
fn request(value: &Value) -> Option<String> {
    let (role, content) = match value.get("type")?.as_str()? {
        // Claude Code.
        "user" => ("user", value.get("message")?.get("content")?),
        // Codex.
        "response_item" => {
            let payload = value.get("payload")?;
            (payload.get("role")?.as_str()?, payload.get("content")?)
        },
        _ => return None,
    };
    if role != "user" {
        return None;
    }
    let mut said = Vec::new();
    match content {
        Value::String(s) => said.push(s.clone()),
        other => texts(other, &mut said),
    }
    said.into_iter().map(|s| s.trim().to_owned()).find(|s| !s.is_empty() && !s.starts_with('<'))
}

fn search_file(agent: &str, path: &Path, modified: u64, text: &str) -> Option<SessionHit> {
    let exact = text.chars().any(char::is_uppercase);
    let wanted = if exact { text.to_owned() } else { text.to_lowercase() };
    let has =
        |s: &str| if exact { s.contains(&wanted) } else { s.to_lowercase().contains(&wanted) };
    let file = std::fs::File::open(path).ok()?;
    // Claude Code's are named by session; Codex's say in their first line.
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let mut session = (agent == "claude").then(|| stem.clone());
    let (mut cwd, mut title, mut snippet) = (None, None, None);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let interesting =
            cwd.is_none() || title.is_none() || session.is_none() || snippet.is_none();
        if !interesting {
            break;
        }
        // Parsed only when it may hold what's still wanted.
        let wants_text = snippet.is_none() && has(&line);
        if !wants_text && cwd.is_some() && title.is_some() && session.is_some() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .or_else(|| value.get("payload")?.get("cwd"))
                .and_then(Value::as_str)
                .map(PathBuf::from);
        }
        if session.is_none() && value.get("type").and_then(Value::as_str) == Some("session_meta") {
            session = value
                .get("payload")
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if title.is_none() {
            title = request(&value);
        }
        if wants_text {
            let mut said = Vec::new();
            texts(&value, &mut said);
            snippet = said.iter().find_map(|s| around(s, &wanted, exact));
        }
    }
    let snippet = snippet?;
    // (Else Codex's file name ends with it.)
    let session = session
        .unwrap_or_else(|| stem.get(stem.len().saturating_sub(36)..).unwrap_or(&stem).to_owned());
    Some(SessionHit {
        agent: agent.to_owned(),
        session,
        cwd,
        title: shorten(&title.unwrap_or_default(), 100),
        snippet,
        modified,
    })
}

/// `text` around the first `wanted` in it, on one line.
fn around(text: &str, wanted: &str, exact: bool) -> Option<String> {
    let haystack = if exact { text.to_owned() } else { text.to_lowercase() };
    let at = haystack.find(wanted)?;
    let chars: Vec<char> = text.chars().collect();
    let at = haystack[..at].chars().count();
    let start = at.saturating_sub(50);
    let end = (at + wanted.chars().count() + 70).min(chars.len());
    let mut snippet: String =
        chars[start..end].iter().map(|&c| if c.is_whitespace() { ' ' } else { c }).collect();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    Some(snippet)
}

fn shorten(text: &str, max: usize) -> String {
    let line: String = text.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    if line.chars().count() <= max {
        return line;
    }
    line.chars().take(max - 1).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_kept_after_a_rewind() {
        let dir = std::env::temp_dir().join(format!("artty-rewind-{}", std::process::id()));
        let project = dir.join("projects/-src");
        std::fs::create_dir_all(&project).unwrap();
        let user = |uuid: &str, parent: &str, prompt: &str| {
            format!(
                "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"parentUuid\":{parent},\"promptId\":\"{prompt}\"}}\n"
            )
        };
        let reply = |uuid: &str, parent: &str, prompt: &str| {
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"{uuid}\",\"parentUuid\":\"{parent}\",\"promptId\":\"{prompt}\"}}\n"
            )
        };
        // p1, p2, then rewound to before p2: p3 follows p1's reply.
        let transcript = [
            user("u1", "null", "p1"),
            reply("a1", "u1", "p1"),
            user("u2", "\"a1\"", "p2"),
            reply("a2", "u2", "p2"),
            user("u3", "\"a1\"", "p3"),
            reply("a3", "u3", "p3"),
        ]
        .concat();
        std::fs::write(project.join("s-1.jsonl"), transcript).unwrap();
        let kept = claude_prompts_kept_in(&dir, "s-1", "p3").unwrap();
        let mut kept: Vec<&str> = kept.iter().map(String::as_str).collect();
        kept.sort();
        assert_eq!(kept, ["p1", "p3"]);
        assert_eq!(claude_prompts_kept_in(&dir, "s-1", "nope"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_titles() {
        let dir = std::env::temp_dir().join(format!("artty-titles-{}", std::process::id()));
        let project = dir.join("projects/-src-api");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, key: &str, title: &str| {
            format!("{{\"type\":\"{kind}\",\"{key}\":\"{title}\",\"sessionId\":\"s\"}}\n")
        };
        // The latest made-up title; a name given wins over any.
        let made = [
            line("ai-title", "aiTitle", "Old"),
            "{\"type\":\"user\"}\n".into(),
            line("ai-title", "aiTitle", "Login fix"),
        ];
        std::fs::write(project.join("aaa-1.jsonl"), made.concat()).unwrap();
        let named =
            [line("custom-title", "customTitle", "Mine"), line("ai-title", "aiTitle", "Later")];
        std::fs::write(project.join("bbb-2.jsonl"), named.concat()).unwrap();
        // A long transcript: its title far from its end.
        let long = format!(
            "{}{}",
            line("ai-title", "aiTitle", "Early"),
            "{\"type\":\"user\"}\n".repeat(40_000)
        );
        std::fs::write(project.join("ccc-3.jsonl"), long).unwrap();
        let title = |session: &str| claude_title_in(&dir, session);
        assert_eq!(title("aaa-1").as_deref(), Some("Login fix"));
        assert_eq!(title("bbb-2").as_deref(), Some("Mine"));
        assert_eq!(title("ccc-3").as_deref(), Some("Early"));
        assert_eq!(title("none"), None);
        assert_eq!(title("../x"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_and_codex_transcripts() {
        let dir = std::env::temp_dir().join(format!("artty-sessions-{}", std::process::id()));
        let claude = dir.join(".claude/projects/-src-api");
        let codex = dir.join(".codex/sessions/2026/10/27");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            claude.join("0b5f2a1e-1111-2222-3333-444455556666.jsonl"),
            [
                r#"{"type":"user","cwd":"/src/api","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","cwd":"/src/api","message":{"role":"user","content":"Fix the flaky login test"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The retry in auth::refresh_token races with the expiry check."}]}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            codex.join("rollout-2026-10-27T10-00-00-0199aaaa-bbbb-cccc-dddd-eeeeffff0000.jsonl"),
            [
                r#"{"type":"session_meta","payload":{"id":"0199aaaa-bbbb-cccc-dddd-eeeeffff0000","cwd":"/src/web"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>…</environment_context>"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Why does refresh_token fail?"}]}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        // SAFETY: tests here don't read HOME concurrently.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", dir.join(".claude"));
            std::env::set_var("CODEX_HOME", dir.join(".codex"));
        }
        let mut hits = search("REFRESH_TOKEN".to_lowercase().as_str());
        hits.sort_by(|a, b| a.agent.cmp(&b.agent));
        assert_eq!(hits.len(), 2, "{hits:?}");
        let claude = &hits[0];
        assert_eq!(claude.agent, "claude");
        assert_eq!(claude.session, "0b5f2a1e-1111-2222-3333-444455556666");
        assert_eq!(claude.cwd.as_deref(), Some(Path::new("/src/api")));
        assert_eq!(claude.title, "Fix the flaky login test");
        assert!(claude.snippet.contains("auth::refresh_token races"));
        let codex = &hits[1];
        assert_eq!(codex.session, "0199aaaa-bbbb-cccc-dddd-eeeeffff0000");
        assert_eq!(codex.cwd.as_deref(), Some(Path::new("/src/web")));
        assert_eq!(codex.title, "Why does refresh_token fail?");
        // Capitals: exact.
        assert!(search("Refresh_token").is_empty());
        assert!(search("nothing like this").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
