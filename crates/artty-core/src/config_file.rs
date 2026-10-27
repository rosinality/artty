//! Editing the config file in place, keeping the user's comments and layout.
//! Used by the Settings window; the app reloads the file when it changes.

use std::io;
use std::path::PathBuf;

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::{Config, ServerConfig};

pub struct ConfigFile {
    path: PathBuf,
    doc: DocumentMut,
}

impl ConfigFile {
    /// The config file, or an empty one if it doesn't exist yet.
    pub fn open() -> io::Result<Self> {
        let path = Config::path().ok_or_else(|| io::Error::other("no config directory"))?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        Self::parse(path, &text)
    }

    fn parse(path: PathBuf, text: &str) -> io::Result<Self> {
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(Self { path, doc })
    }

    pub fn set_bool(&mut self, key: &str, v: bool) {
        self.doc[key] = value(v);
    }

    pub fn set_float(&mut self, key: &str, v: f64) {
        // Two decimals are plenty for sizes and opacity; avoid 0.7000000001.
        self.doc[key] = value((v * 100.).round() / 100.);
    }

    pub fn set_string(&mut self, key: &str, v: &str) {
        self.doc[key] = value(v);
    }

    pub fn set_int(&mut self, key: &str, v: i64) {
        self.doc[key] = value(v);
    }

    pub fn set_strings(&mut self, key: &str, v: &[String]) {
        self.doc[key] = value(v.iter().collect::<Array>());
    }

    /// Use a theme's colors, or the built-in ones (`None`).
    pub fn set_theme(&mut self, theme: Option<&ThemePreset>) {
        let Some(theme) = theme else {
            self.doc.remove("colors");
            return;
        };
        let mut colors = Table::new();
        colors["foreground"] = value(theme.foreground);
        colors["background"] = value(theme.background);
        colors["cursor"] = value(theme.cursor);
        colors["selection"] = value(theme.selection);
        colors["palette"] = value(theme.palette.iter().copied().collect::<Array>());
        self.doc["colors"] = Item::Table(colors);
    }

    /// Use colors read from another terminal's theme.
    pub fn set_colors(&mut self, theme: &crate::theme_import::ImportedTheme) {
        let hex = |rgb: crate::vt::ansi::Rgb| format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b);
        let mut colors = Table::new();
        for (key, color) in [
            ("foreground", theme.foreground),
            ("background", theme.background),
            ("cursor", theme.cursor),
            ("selection", theme.selection),
        ] {
            if let Some(color) = color {
                colors[key] = value(hex(color));
            }
        }
        if !theme.palette.is_empty() {
            colors["palette"] = value(theme.palette.iter().map(|&c| hex(c)).collect::<Array>());
        }
        self.doc["colors"] = Item::Table(colors);
    }

    /// The `[keys]` table, made if missing.
    fn keys(&mut self) -> &mut Table {
        if !self.doc.get("keys").is_some_and(Item::is_table) {
            self.doc["keys"] = Item::Table(Table::new());
        }
        self.doc["keys"].as_table_mut().expect("just made")
    }

    /// Bind `keystrokes` to `action` (in `[keys]`), replacing what they did.
    pub fn set_key(&mut self, keystrokes: &str, action: &crate::KeyAction) {
        let item = match action {
            crate::KeyAction::Named(name) => value(name.as_str()),
            crate::KeyAction::Send { send } => {
                let mut table = toml_edit::InlineTable::new();
                table.insert("send", send.as_str().into());
                value(table)
            },
        };
        self.keys()[keystrokes] = item;
    }

    /// Take a binding of `keystrokes` out of `[keys]`.
    pub fn remove_key(&mut self, keystrokes: &str) {
        self.keys().remove(keystrokes);
    }

    /// Add another machine's server, or replace the one of the same name.
    pub fn add_server(&mut self, server: &ServerConfig) {
        let mut table = Table::new();
        table["name"] = value(&server.name);
        if let Some(ssh) = &server.ssh {
            table["ssh"] = value(ssh);
        }
        if let Some(artty) = &server.artty {
            table["artty"] = value(artty);
        }
        if !server.command.is_empty() {
            table["command"] = value(server.command.iter().collect::<Array>());
        }
        if !self.doc.get("servers").is_some_and(Item::is_array_of_tables) {
            self.doc["servers"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        let servers = self.doc["servers"].as_array_of_tables_mut().unwrap();
        let named = |t: &Table| t.get("name").and_then(Item::as_str) == Some(server.name.as_str());
        let index = servers.iter().position(named);
        match index {
            Some(index) => *servers.get_mut(index).unwrap() = table,
            None => servers.push(table),
        }
    }

    /// Remove the server named `name`; returns whether there was one.
    pub fn remove_server(&mut self, name: &str) -> bool {
        let Some(servers) = self.doc.get_mut("servers").and_then(Item::as_array_of_tables_mut)
        else {
            return false;
        };
        let before = servers.len();
        servers.retain(|t| t.get("name").and_then(Item::as_str) != Some(name));
        let removed = servers.len() < before;
        if servers.is_empty() {
            self.doc.remove("servers");
        }
        removed
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = self.path.with_extension("toml.tmp");
        std::fs::write(&temp, self.doc.to_string())?;
        std::fs::rename(temp, &self.path)
    }
}

/// A built-in color theme.
pub struct ThemePreset {
    pub name: &'static str,
    pub foreground: &'static str,
    pub background: &'static str,
    pub cursor: &'static str,
    pub selection: &'static str,
    pub palette: [&'static str; 16],
}

/// Themes to choose from (dark, then light: the app's own colors follow).
/// "artty" is the built-in default.
pub const THEMES: &[ThemePreset] = &[
    ThemePreset {
        name: "Tokyo Night",
        foreground: "#c0caf5",
        background: "#1a1b26",
        cursor: "#c0caf5",
        selection: "#33467c",
        palette: [
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    },
    ThemePreset {
        name: "Nord",
        foreground: "#d8dee9",
        background: "#2e3440",
        cursor: "#d8dee9",
        selection: "#434c5e",
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ],
    },
    ThemePreset {
        name: "Gruvbox",
        foreground: "#ebdbb2",
        background: "#282828",
        cursor: "#ebdbb2",
        selection: "#504945",
        palette: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
            "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ],
    },
    ThemePreset {
        name: "Solarized",
        foreground: "#93a1a1",
        background: "#002b36",
        cursor: "#93a1a1",
        selection: "#073642",
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#586e75", "#cb4b16", "#93a1a1", "#839496", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    ThemePreset {
        name: "Solarized Light",
        foreground: "#657b83",
        background: "#fdf6e3",
        cursor: "#586e75",
        selection: "#eee8d5",
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    ThemePreset {
        name: "GitHub Light",
        foreground: "#24292f",
        background: "#ffffff",
        cursor: "#0969da",
        selection: "#ddf4ff",
        palette: [
            "#24292f", "#cf222e", "#116329", "#4d2d00", "#0969da", "#8250df", "#1b7c83", "#6e7781",
            "#57606a", "#a40e26", "#1a7f37", "#633c01", "#218bff", "#a475f9", "#3192aa", "#8c959f",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_comments_and_parse() {
        let text = "# my settings\nfont-size = 13 # small\n\n[colors]\nforeground = \"#ffffff\"\n";
        let mut file = ConfigFile::parse(PathBuf::from("/x"), text).unwrap();
        file.set_float("font-size", 14.000001);
        file.set_bool("background-blur", false);
        file.set_float("background-opacity", 0.7000000001);
        let out = file.doc.to_string();
        assert!(out.starts_with("# my settings\n"), "{out}");
        assert!(out.contains("font-size = 14.0"), "{out}");
        assert!(out.contains("background-opacity = 0.7\n"), "{out}");

        let config = Config::parse(&out).unwrap();
        assert_eq!(config.font_size, 14.);
        assert!(!config.background_blur);
        assert_eq!(config.colors.foreground.map(|c| c.r), Some(255));
    }

    #[test]
    fn adding_and_removing_servers() {
        let mut file = ConfigFile::parse(PathBuf::from("/x"), "font-size = 13\n").unwrap();
        let devbox = ServerConfig {
            name: "devbox".into(),
            ssh: Some("me@devbox".into()),
            ..Default::default()
        };
        file.add_server(&devbox);
        file.add_server(&ServerConfig {
            name: "other".into(),
            ssh: Some("o".into()),
            ..Default::default()
        });
        // Same name: replaced in place.
        file.add_server(&ServerConfig { artty: Some("~/bin/artty".into()), ..devbox.clone() });
        file.set_bool("background-blur", false);
        let config = Config::parse(&file.doc.to_string()).unwrap();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].artty.as_deref(), Some("~/bin/artty"));
        assert!(!config.background_blur);

        assert!(file.remove_server("devbox"));
        assert!(!file.remove_server("devbox"));
        assert!(file.remove_server("other"));
        let config = Config::parse(&file.doc.to_string()).unwrap();
        assert!(config.servers.is_empty());
        assert_eq!(config.font_size, 13.);
    }

    #[test]
    fn themes_round_trip() {
        let mut file = ConfigFile::parse(PathBuf::from("/x"), "").unwrap();
        for theme in THEMES {
            file.set_theme(Some(theme));
            let config = Config::parse(&file.doc.to_string())
                .unwrap_or_else(|e| panic!("{}: {e}", theme.name));
            assert_eq!(config.colors.palette.len(), 16);
        }
        file.set_theme(None);
        assert!(Config::parse(&file.doc.to_string()).unwrap().colors.foreground.is_none());
    }

    #[test]
    fn keys_and_imported_colors_round_trip() {
        use crate::KeyAction;
        let mut file = ConfigFile::parse(PathBuf::from("/x"), "font-size = 14\n").unwrap();
        file.set_key("cmd+e>1", &KeyAction::Named("text:è".into()));
        file.set_key("cmd-e 2", &KeyAction::Send { send: "é".into() });
        file.set_key("cmd-shift-t", &KeyAction::Named("new-tab".into()));
        file.remove_key("cmd-shift-t");
        let theme = crate::theme_import::parse(
            "t",
            "background = #101010\nforeground = #eeeeee\npalette = 0=#000000\npalette = 1=#ff0000\n",
        )
        .unwrap();
        file.set_colors(&theme);
        let config = Config::parse(&file.doc.to_string()).unwrap();
        assert_eq!(config.keys.len(), 2);
        assert_eq!(config.keys["cmd+e>1"], KeyAction::Named("text:è".into()));
        assert_eq!(config.keys["cmd-e 2"], KeyAction::Send { send: "é".into() });
        assert_eq!(config.colors.background, crate::theme_import::parse_hex("#101010"));
        assert_eq!(config.colors.palette.len(), 2);
        assert_eq!(config.font_size, 14.);
    }
}
