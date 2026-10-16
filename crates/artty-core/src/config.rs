//! User configuration, read from `~/.config/artty/config.toml`.
//!
//! Every key is optional; see `config.example.toml` at the repository root.

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
    pub colors: Colors,
}

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
        toml::from_str(text)
    }

    pub fn theme(&self) -> Theme {
        let mut theme = Theme::default();
        let colors = &self.colors;
        theme.foreground = colors.foreground.unwrap_or(theme.foreground);
        theme.background = colors.background.unwrap_or(theme.background);
        theme.cursor = colors.cursor.unwrap_or(theme.cursor);
        theme.selection = colors.selection.unwrap_or(theme.selection);
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
