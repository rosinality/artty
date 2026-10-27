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
