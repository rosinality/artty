//! Status hooks in coding agents' settings: what's installed, installing,
//! updating and removing them. artty's hooks are recognized by running
//! `artty status`; everything else in the files is left alone.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// An agent artty can install status hooks for.
pub struct Agent {
    /// Short name used on the command line and in `artty status --agent`.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Hooks file, relative to the home directory; its `hooks` key is ours.
    pub file: &'static str,
    /// Hook event → the state it reports.
    events: &'static [(&'static str, &'static str)],
    /// The agent parses hook output as JSON (Gemini CLI).
    json_output: bool,
    /// Shown after installing.
    pub note: Option<&'static str>,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        name: "Claude Code",
        file: ".claude/settings.json",
        events: &[
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("PostToolUse", "working"),
            ("Notification", "waiting"),
            ("Stop", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: false,
        note: None,
    },
    Agent {
        id: "codex",
        name: "Codex",
        file: ".codex/hooks.json",
        events: &[
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("PostToolUse", "working"),
            ("PermissionRequest", "waiting"),
            ("Stop", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: false,
        note: Some("Approve the new hooks once with /hooks in Codex."),
    },
    Agent {
        id: "gemini",
        name: "Gemini CLI",
        file: ".gemini/settings.json",
        events: &[
            ("SessionStart", "idle"),
            ("BeforeAgent", "working"),
            ("AfterTool", "working"),
            ("Notification", "waiting"),
            ("AfterAgent", "done"),
            ("SessionEnd", "idle"),
        ],
        json_output: true,
        note: None,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The agent's config directory doesn't exist.
    AgentNotFound,
    NotInstalled,
    /// Some of artty's hooks are there, but not the current ones.
    Outdated,
    Installed,
    /// The settings file couldn't be read.
    Error(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::AgentNotFound => "agent not found".into(),
            Status::NotInstalled => "not installed".into(),
            Status::Outdated => "outdated (install to update)".into(),
            Status::Installed => "installed".into(),
            Status::Error(err) => format!("error: {err}"),
        }
    }
}

pub fn agent(id: &str) -> Result<&'static Agent, String> {
    AGENTS
        .iter()
        .find(|agent| agent.id == id)
        .ok_or_else(|| format!("unknown agent {id} (claude, codex or gemini)"))
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set".to_owned())
}

/// Whether a hook entry is one of artty's.
fn ours(entry: &Value) -> bool {
    entry.to_string().contains("artty status")
}

impl Agent {
    /// The hooks to add, keyed by event. They do nothing outside artty.
    pub fn entries(&self) -> Value {
        let command = |state: &str| {
            let report = format!(
                "[ -n \"$ARTTY_SOCKET\" ] && artty status {state} --hook --agent {}",
                self.id
            );
            if self.json_output {
                format!("{report} >/dev/null; echo '{{}}'")
            } else {
                format!("{report} || true")
            }
        };
        let mut hooks = Map::new();
        for (event, state) in self.events {
            hooks.insert(
                (*event).to_owned(),
                json!([{ "hooks": [{ "type": "command", "command": command(state) }] }]),
            );
        }
        Value::Object(hooks)
    }

    fn path_in(&self, home: &Path) -> PathBuf {
        home.join(self.file)
    }

    pub fn status(&self) -> Status {
        match home() {
            Ok(home) => self.status_in(&home),
            Err(err) => Status::Error(err),
        }
    }

    fn status_in(&self, home: &Path) -> Status {
        let path = self.path_in(home);
        if !path.parent().is_some_and(Path::is_dir) {
            return Status::AgentNotFound;
        }
        let settings = match read(&path) {
            Ok(settings) => settings,
            Err(err) => return Status::Error(err),
        };
        let hooks = settings.get("hooks").and_then(Value::as_object);
        let wanted = self.entries();
        let wanted = wanted.as_object().unwrap();
        let mut found = 0;
        let mut current = true;
        for (event, entries) in wanted {
            let installed: Vec<&Value> = hooks
                .and_then(|h| h.get(event))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|e| ours(e))
                .collect();
            found += installed.len();
            current &= installed.iter().copied().eq(entries.as_array().unwrap().iter());
        }
        // artty hooks under events we no longer use also make it outdated.
        let stray = hooks.into_iter().flatten().any(|(event, list)| {
            !wanted.contains_key(event) && list.as_array().is_some_and(|l| l.iter().any(ours))
        });
        match (found, current && !stray) {
            (0, _) if !stray => Status::NotInstalled,
            (_, true) => Status::Installed,
            _ => Status::Outdated,
        }
    }

    /// Add (or update) artty's hooks, backing the file up first.
    pub fn install(&self) -> Result<String, String> {
        self.install_in(&home()?)
    }

    fn install_in(&self, home: &Path) -> Result<String, String> {
        if self.status_in(home) == Status::Installed {
            return Ok(format!("{}: hooks are up to date", self.name));
        }
        let path = self.path_in(home);
        let mut settings = read(&path)?;
        let root =
            settings.as_object_mut().ok_or(format!("{} is not a JSON object", path.display()))?;
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let hooks = hooks
            .as_object_mut()
            .ok_or(format!("{}: \"hooks\" is not an object", path.display()))?;
        remove_ours(hooks);
        for (event, entries) in self.entries().as_object().unwrap() {
            let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
            let list = list
                .as_array_mut()
                .ok_or(format!("{}: hooks.{event} is not a list", path.display()))?;
            list.extend(entries.as_array().unwrap().iter().cloned());
        }
        let backup = write(&path, &settings)?;
        let mut report = format!("{}: installed hooks in {}", self.name, path.display());
        if let Some(backup) = backup {
            report.push_str(&format!(" (backup: {})", backup.display()));
        }
        if let Some(note) = self.note {
            report.push_str(&format!("\n{}: {note}", self.name));
        }
        Ok(report)
    }

    /// Remove artty's hooks, backing the file up first.
    pub fn uninstall(&self) -> Result<String, String> {
        self.uninstall_in(&home()?)
    }

    fn uninstall_in(&self, home: &Path) -> Result<String, String> {
        if matches!(self.status_in(home), Status::NotInstalled | Status::AgentNotFound) {
            return Ok(format!("{}: no artty hooks to remove", self.name));
        }
        let path = self.path_in(home);
        let mut settings = read(&path)?;
        let Some(root) = settings.as_object_mut() else {
            return Err(format!("{} is not a JSON object", path.display()));
        };
        if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            remove_ours(hooks);
            if hooks.is_empty() {
                root.remove("hooks");
            }
        }
        let backup = write(&path, &settings)?;
        let mut report = format!("{}: removed hooks from {}", self.name, path.display());
        if let Some(backup) = backup {
            report.push_str(&format!(" (backup: {})", backup.display()));
        }
        Ok(report)
    }
}

/// Drop artty's entries, and events left with none.
fn remove_ours(hooks: &mut Map<String, Value>) {
    for list in hooks.values_mut() {
        if let Some(list) = list.as_array_mut() {
            list.retain(|entry| !ours(entry));
        }
    }
    hooks.retain(|_, list| list.as_array().is_none_or(|l| !l.is_empty()));
}

fn read(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Write `settings`, backing up the existing file. Returns the backup path.
fn write(path: &Path, settings: &Value) -> Result<Option<PathBuf>, String> {
    let backup = if path.exists() {
        let backup = path.with_extension("json.artty-backup");
        std::fs::copy(path, &backup).map_err(|err| format!("backup failed: {err}"))?;
        Some(backup)
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        }
        None
    };
    let text = serde_json::to_string_pretty(settings).map_err(|err| err.to_string())?;
    std::fs::write(path, text + "\n").map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(backup)
}

fn select(ids: &[&str], default: impl Fn(&Agent) -> bool) -> Result<Vec<&'static Agent>, String> {
    if ids.is_empty() {
        Ok(AGENTS.iter().filter(|agent| default(agent)).collect())
    } else {
        ids.iter().map(|id| agent(id)).collect()
    }
}

/// Install for the named agents, or every installed agent.
pub fn install(ids: &[&str]) -> Result<String, String> {
    let agents = select(ids, |agent| agent.status() != Status::AgentNotFound)?;
    if agents.is_empty() {
        return Err("no supported agent found (~/.claude, ~/.codex or ~/.gemini)".into());
    }
    Ok(agents
        .iter()
        .map(|agent| agent.install().unwrap_or_else(|err| err))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Uninstall from the named agents, or wherever artty's hooks are.
pub fn uninstall(ids: &[&str]) -> Result<String, String> {
    let agents =
        select(ids, |agent| matches!(agent.status(), Status::Installed | Status::Outdated))?;
    if agents.is_empty() {
        return Ok("no artty hooks are installed".into());
    }
    Ok(agents
        .iter()
        .map(|agent| agent.uninstall().unwrap_or_else(|err| err))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("artty-hooks-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn commands_are_guarded_and_named() {
        let claude = agent("claude").unwrap().entries();
        let command = claude["Notification"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(
            command,
            "[ -n \"$ARTTY_SOCKET\" ] && artty status waiting --hook --agent claude || true"
        );
        let gemini = agent("gemini").unwrap().entries();
        let command = gemini["AfterAgent"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.ends_with(">/dev/null; echo '{}'"), "{command}");
    }

    #[test]
    fn install_status_uninstall() {
        let home = scratch_home("cycle");
        let claude = agent("claude").unwrap();
        assert_eq!(claude.status_in(&home), Status::AgentNotFound);

        // The user's own settings and hooks.
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let mine = json!({"theme": "dark", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}});
        std::fs::write(home.join(".claude/settings.json"), mine.to_string()).unwrap();
        assert_eq!(claude.status_in(&home), Status::NotInstalled);

        claude.install_in(&home).unwrap();
        assert_eq!(claude.status_in(&home), Status::Installed);
        assert!(claude.install_in(&home).unwrap().contains("up to date"));

        claude.uninstall_in(&home).unwrap();
        assert_eq!(claude.status_in(&home), Status::NotInstalled);
        let back: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(back, mine, "uninstall leaves exactly the user's settings");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn older_hooks_are_outdated_and_get_replaced() {
        let home = scratch_home("outdated");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let old = json!({"hooks": {"Stop": [{"hooks": [{"type": "command",
            "command": "[ -n \"$ARTTY_SOCKET\" ] && artty status done || true"}]}]}});
        std::fs::write(home.join(".claude/settings.json"), old.to_string()).unwrap();
        let claude = agent("claude").unwrap();
        assert_eq!(claude.status_in(&home), Status::Outdated);
        claude.install_in(&home).unwrap();
        assert_eq!(claude.status_in(&home), Status::Installed);
        let _ = std::fs::remove_dir_all(home);
    }
}
