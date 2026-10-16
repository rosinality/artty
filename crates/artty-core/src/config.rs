//! User configuration, read from `~/.config/artty/config.toml`.
//!
//! Every key is optional; see `config.example.toml` at the repository root.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use artty_vt::ansi::Rgb;
use serde::{Deserialize, Deserializer};

use crate::theme::Theme;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Program to run in new tabs, e.g. `["/opt/homebrew/bin/fish", "-l"]`.
    /// Empty runs the account's login shell.
    pub shell: Vec<String>,
    /// Font family names, tried in order.
    pub font_family: Vec<String>,
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Opacity of the terminal background over the blurred desktop, 0–1.
    pub background_opacity: f32,
    pub background_blur: bool,
    pub scrollback: usize,
    /// Treat Option as Meta (⌥B sends ESC b). Turn off to type characters
    /// like `å` with Option.
    pub option_as_meta: bool,
    pub cursor_blink: bool,
    /// Cursor shape; programs can still change it while they run (vim).
    pub cursor_style: CursorStyle,
    /// Foreground process names treated as coding agents in the sidebar.
    pub agents: Vec<String>,
    /// macOS notifications when a tab needs attention while you're elsewhere.
    pub notifications: bool,
    /// Set up zsh and bash to report prompts and commands (fish does natively).
    pub shell_integration: bool,
    /// Flash the pane on the bell.
    pub visual_bell: bool,
    /// Play the system alert sound on the bell.
    pub bell_sound: bool,
    /// Copy text to the clipboard as soon as it's selected.
    pub copy_on_select: bool,
    /// Reopen the previous tabs on launch.
    pub restore_sessions: bool,
    /// Lines of each tab's output to save and show again; 0 saves none.
    pub restore_scrollback: usize,
    /// Resume coding agents that were running in restored tabs.
    pub resume_agents: bool,
    /// How to resume each agent, by process name.
    pub resume: BTreeMap<String, ResumeCommand>,
    pub colors: Colors,
}

/// Commands that resume an agent's conversation.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ResumeCommand {
    /// Resume a specific conversation; `{session}` is replaced by its id.
    pub session: Option<String>,
    /// Resume the latest conversation in the directory, when the id is unknown.
    pub latest: Option<String>,
}

impl ResumeCommand {
    /// The command line to run, if this agent can be resumed. Session ids are
    /// only used if they can't break out of the command.
    pub fn command(&self, session: Option<&str>) -> Option<String> {
        let safe = |id: &&str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        match (session.filter(safe), &self.session) {
            (Some(id), Some(template)) => Some(template.replace("{session}", id)),
            _ => self.latest.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorStyle {
    #[default]
    Block,
    /// A thin vertical line.
    Bar,
    Underline,
    /// An outlined block.
    Hollow,
}

impl CursorStyle {
    pub const ALL: [CursorStyle; 4] = [CursorStyle::Block, CursorStyle::Bar, CursorStyle::Underline, CursorStyle::Hollow];

    /// The name used in the config file.
    pub fn name(self) -> &'static str {
        match self {
            CursorStyle::Block => "block",
            CursorStyle::Bar => "bar",
            CursorStyle::Underline => "underline",
            CursorStyle::Hollow => "hollow",
        }
    }

    pub fn shape(self) -> artty_vt::ansi::CursorShape {
        use artty_vt::ansi::CursorShape;
        match self {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Bar => CursorShape::Beam,
            CursorStyle::Underline => CursorShape::Underline,
            CursorStyle::Hollow => CursorShape::HollowBlock,
        }
    }
}

/// Coding agents recognized by default.
pub const DEFAULT_AGENTS: &[&str] =
    &["claude", "codex", "gemini", "aider", "opencode", "amp", "goose", "cursor-agent", "qwen", "crush", "droid"];

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: Vec::new(),
            font_family: vec!["SF Mono".into(), "Menlo".into()],
            font_size: 13.,
            line_height: 1.35,
            background_opacity: 0.72,
            background_blur: true,
            scrollback: 10_000,
            option_as_meta: true,
            cursor_blink: true,
            cursor_style: CursorStyle::Block,
            agents: DEFAULT_AGENTS.iter().map(|&s| s.to_owned()).collect(),
            notifications: true,
            shell_integration: true,
            visual_bell: true,
            bell_sound: false,
            copy_on_select: false,
            restore_sessions: true,
            restore_scrollback: 1000,
            resume_agents: true,
            resume: BTreeMap::from([
                (
                    "claude".to_owned(),
                    ResumeCommand {
                        session: Some("claude --resume {session}".to_owned()),
                        latest: Some("claude --continue".to_owned()),
                    },
                ),
                (
                    "codex".to_owned(),
                    ResumeCommand {
                        session: Some("codex resume {session}".to_owned()),
                        latest: Some("codex resume --last".to_owned()),
                    },
                ),
                (
                    "gemini".to_owned(),
                    ResumeCommand {
                        session: Some("gemini --resume {session}".to_owned()),
                        latest: Some("gemini --resume latest".to_owned()),
                    },
                ),
            ]),
            colors: Colors::default(),
        }
    }
}

/// Color overrides on top of the built-in theme.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Colors {
    #[serde(deserialize_with = "color")]
    pub foreground: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub background: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub cursor: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub selection: Option<Rgb>,
    #[serde(deserialize_with = "color")]
    pub search_match: Option<Rgb>,
    /// The 16 ANSI colors, normal then bright; may be shorter than 16.
    #[serde(deserialize_with = "palette")]
    pub palette: Vec<Rgb>,
}

fn parse_rgb<E: serde::de::Error>(s: &str) -> Result<Rgb, E> {
    Rgb::from_str(s).map_err(|()| E::custom(format!("invalid color {s:?}, expected \"#rrggbb\"")))
}

fn color<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Rgb>, D::Error> {
    parse_rgb(&String::deserialize(d)?).map(Some)
}

fn palette<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Rgb>, D::Error> {
    let colors = Vec::<String>::deserialize(d)?;
    if colors.len() > 16 {
        return Err(serde::de::Error::custom("palette has more than 16 colors"));
    }
    colors.iter().map(|s| parse_rgb(s)).collect()
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("artty").join("config.toml"))
    }

    /// Load the config file. A missing file gives the defaults; a broken one
    /// gives the defaults plus the error, so the UI can report it.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else { return (Self::default(), None) };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(config) => (config, None),
                Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
        }
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        // Agents configured by the user add to the built-in ones.
        for (agent, command) in Self::default().resume {
            config.resume.entry(agent).or_insert(command);
        }
        Ok(config)
    }

    pub fn theme(&self) -> Theme {
        let mut theme = Theme::default();
        let colors = &self.colors;
        theme.foreground = colors.foreground.unwrap_or(theme.foreground);
        theme.background = colors.background.unwrap_or(theme.background);
        theme.cursor = colors.cursor.unwrap_or(theme.cursor);
        theme.selection = colors.selection.unwrap_or(theme.selection);
        theme.search_match = colors.search_match.unwrap_or(theme.search_match);
        theme.ansi[..colors.palette.len()].copy_from_slice(&colors.palette);
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.font_size, 13.);
        assert!(config.shell.is_empty());
    }

    #[test]
    fn colors_override_theme() {
        let config = Config::parse(
            r##"
            font-size = 14
            [colors]
            background = "#000000"
            palette = ["#010203", "#ff0000"]
            "##,
        )
        .unwrap();
        let theme = config.theme();
        assert_eq!(theme.background, Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(theme.ansi[1], Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(theme.ansi[2], Theme::default().ansi[2]);
    }

    #[test]
    fn resume_commands() {
        let config = Config::default();
        let claude = &config.resume["claude"];
        assert_eq!(claude.command(Some("0f1e-2d")).as_deref(), Some("claude --resume 0f1e-2d"));
        assert_eq!(claude.command(None).as_deref(), Some("claude --continue"));
        // An id that could inject shell syntax falls back to the latest.
        assert_eq!(claude.command(Some("x; rm -rf ~")).as_deref(), Some("claude --continue"));

        let custom = Config::parse("[resume.codex]\nlatest = \"codex resume --last\"").unwrap();
        assert_eq!(custom.resume["codex"].command(Some("abc")).as_deref(), Some("codex resume --last"));
        assert!(custom.resume.contains_key("claude"));
    }

    #[test]
    fn cursor_styles() {
        assert_eq!(Config::parse("").unwrap().cursor_style, CursorStyle::Block);
        for style in CursorStyle::ALL {
            let config = Config::parse(&format!("cursor-style = \"{}\"", style.name())).unwrap();
            assert_eq!(config.cursor_style, style);
        }
        assert!(Config::parse("cursor-style = \"beam\"").is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Config::parse("[colors]\nforeground = \"red\"").is_err());
        assert!(Config::parse("font-sise = 12").is_err());
    }
}

#[cfg(test)]
mod example_tests {
    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        let config = super::Config::parse(text).unwrap();
        assert_eq!(config.font_family, ["SF Mono", "Menlo"]);
    }
}
