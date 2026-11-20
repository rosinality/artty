//! The guide for agents working with other agents in artty: printed by
//! `artty guide`, written beside the config file (`AGENTS.md`, rewritten
//! when the server starts) for an agent told to read it, and pointed to by
//! a Claude Code skill `artty guide install` adds.

use std::io;
use std::path::PathBuf;

pub const GUIDE: &str = include_str!("agents_guide.md");

/// The skill: a description Claude Code goes by to load it when other
/// agents come up, and where the guide is (current, whatever the build).
pub const SKILL: &str = "\
---
name: artty
description: Work with other coding agents running in artty's panes — list them, ask one something and get its answer (artty ask), read what they were asked and said. Use when the user mentions other agents, tabs or panes in artty, or asks you to hand work to, check with, or coordinate with another agent.
---

Run `artty guide` and follow it: it says how to find the other agents, ask
one something and wait for its answer, and the rules for doing so.
";

/// `AGENTS.md`, beside the config file.
pub fn path() -> Option<PathBuf> {
    Some(crate::Config::path()?.with_file_name("AGENTS.md"))
}

/// Write `AGENTS.md` if it isn't the current guide.
pub fn write() -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::other("no HOME"))?;
    if std::fs::read_to_string(&path).is_ok_and(|text| text == GUIDE) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, GUIDE)
}

/// Where Claude Code looks for the user's skills: `skills/artty/SKILL.md`
/// in its config folder.
pub fn skill_path() -> Option<PathBuf> {
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")))?;
    Some(claude.join("skills").join("artty").join("SKILL.md"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_skill_is_well_formed() {
        let front = super::SKILL.strip_prefix("---\n").unwrap();
        let (front, body) = front.split_once("\n---\n").unwrap();
        assert!(front.starts_with("name: artty\ndescription: "));
        assert!(!front.contains('"') && front.lines().count() == 2);
        assert!(body.contains("artty guide"));
    }
}
