//! Color themes from other terminals' files: iTerm2's `.itermcolors`,
//! Ghostty's theme files, kitty's `.conf`, Alacritty's and WezTerm's TOML,
//! and Windows Terminal's JSON schemes. Told apart by what's in them.

use std::path::{Path, PathBuf};

use crate::vt::ansi::Rgb;

/// A theme read from a file: what artty's `[colors]` takes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedTheme {
    pub name: String,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub selection: Option<Rgb>,
    /// The 16 ANSI colors, as many as the file has.
    pub palette: Vec<Rgb>,
}

impl ImportedTheme {
    fn color(&mut self, index: usize, rgb: Rgb) {
        if index < 16 {
            if self.palette.len() <= index {
                self.palette.resize(index + 1, Rgb { r: 0, g: 0, b: 0 });
            }
            self.palette[index] = rgb;
        }
    }

    /// Enough to be a theme: a background and foreground, or colors.
    fn usable(&self) -> bool {
        self.background.is_some() && self.foreground.is_some() || self.palette.len() >= 8
    }
}

/// `#rrggbb`, `rrggbb`, `#rgb` or `0xrrggbb`.
pub fn parse_hex(text: &str) -> Option<Rgb> {
    let hex = text.trim().trim_matches(['\'', '"']);
    let hex = hex.strip_prefix('#').or_else(|| hex.strip_prefix("0x")).unwrap_or(hex);
    let value = |s: &str| u8::from_str_radix(s, 16).ok();
    match hex.len() {
        6 => Some(Rgb { r: value(&hex[0..2])?, g: value(&hex[2..4])?, b: value(&hex[4..6])? }),
        3 => {
            let one = |i: usize| value(&hex[i..=i]).map(|v| v * 17);
            Some(Rgb { r: one(0)?, g: one(1)?, b: one(2)? })
        },
        _ => None,
    }
}

/// Read the theme in the file at `path` (named after the file).
pub fn import(path: &Path) -> Result<ImportedTheme, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let name = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    parse(&name, &text)
}

/// Read a theme from a file's text, whichever terminal's it is.
pub fn parse(name: &str, text: &str) -> Result<ImportedTheme, String> {
    let trimmed = text.trim_start();
    let mut theme = if trimmed.starts_with("<?xml") || trimmed.contains("<plist") {
        iterm(text)?
    } else if trimmed.starts_with('{') {
        windows_terminal(text)?
    } else if text.contains("[colors") {
        toml_theme(text)?
    } else {
        lines(text)
    };
    if theme.name.is_empty() {
        theme.name = name.to_owned();
    }
    if !theme.usable() {
        return Err("no colors found in it (iTerm2, Ghostty, kitty, Alacritty, WezTerm or \
                    Windows Terminal themes are read)"
            .into());
    }
    Ok(theme)
}

/// iTerm2: a plist of `<key>Ansi 0 Color</key><dict>…Red Component…</dict>`.
fn iterm(text: &str) -> Result<ImportedTheme, String> {
    let mut theme = ImportedTheme::default();
    let mut rest = text;
    while let Some(at) = rest.find("<key>") {
        rest = &rest[at + 5..];
        let Some(end) = rest.find("</key>") else { break };
        let key = rest[..end].trim().to_owned();
        rest = &rest[end + 6..];
        // Colors are the keys whose value is a dict.
        let after = rest.trim_start();
        if !after.starts_with("<dict>") {
            continue;
        }
        let Some(close) = after.find("</dict>") else { break };
        let dict = &after[..close];
        let component = |name: &str| -> Option<u8> {
            let at = dict.find(&format!("<key>{name}</key>"))?;
            let after = &dict[at..];
            let start = after.find("<real>")? + 6;
            let end = after.find("</real>")?;
            let value: f64 = after[start..end].trim().parse().ok()?;
            Some((value.clamp(0., 1.) * 255.).round() as u8)
        };
        let rgb = (|| {
            Some(Rgb {
                r: component("Red Component")?,
                g: component("Green Component")?,
                b: component("Blue Component")?,
            })
        })();
        rest = &after[close..];
        let Some(rgb) = rgb else { continue };
        match key.as_str() {
            "Foreground Color" => theme.foreground = Some(rgb),
            "Background Color" => theme.background = Some(rgb),
            "Cursor Color" => theme.cursor = Some(rgb),
            "Selection Color" => theme.selection = Some(rgb),
            key => {
                if let Some(index) = key
                    .strip_prefix("Ansi ")
                    .and_then(|k| k.strip_suffix(" Color"))
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    theme.color(index, rgb);
                }
            },
        }
    }
    Ok(theme)
}

/// Windows Terminal: `{"name", "background", "black", …, "brightWhite"}`.
fn windows_terminal(text: &str) -> Result<ImportedTheme, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    // A settings file holds its schemes in `schemes`: the first.
    let scheme = value.get("schemes").and_then(|s| s.get(0)).unwrap_or(&value);
    let get = |key: &str| scheme.get(key).and_then(|v| v.as_str()).and_then(parse_hex);
    let mut theme = ImportedTheme {
        name: scheme.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
        foreground: get("foreground"),
        background: get("background"),
        cursor: get("cursorColor"),
        selection: get("selectionBackground"),
        palette: Vec::new(),
    };
    const NAMES: [&str; 16] = [
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "purple",
        "cyan",
        "white",
        "brightBlack",
        "brightRed",
        "brightGreen",
        "brightYellow",
        "brightBlue",
        "brightPurple",
        "brightCyan",
        "brightWhite",
    ];
    for (index, name) in NAMES.iter().enumerate() {
        let rgb = get(name).or_else(|| match *name {
            "purple" => get("magenta"),
            "brightPurple" => get("brightMagenta"),
            _ => None,
        });
        if let Some(rgb) = rgb {
            theme.color(index, rgb);
        }
    }
    Ok(theme)
}

/// Alacritty (`[colors.primary]`, `[colors.normal]`…) or WezTerm
/// (`[colors]` with `ansi` and `brights` lists).
fn toml_theme(text: &str) -> Result<ImportedTheme, String> {
    let value: toml::Value = toml::from_str(text).map_err(|err| err.to_string())?;
    let colors = value.get("colors").ok_or("no [colors]")?;
    let hex = |v: Option<&toml::Value>| v.and_then(|v| v.as_str()).and_then(parse_hex);
    let mut theme = ImportedTheme::default();
    const NAMES: [&str; 8] =
        ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if let Some(primary) = colors.get("primary") {
        // Alacritty.
        theme.foreground = hex(primary.get("foreground"));
        theme.background = hex(primary.get("background"));
        theme.cursor = hex(colors.get("cursor").and_then(|c| c.get("cursor")));
        theme.selection = hex(colors.get("selection").and_then(|c| c.get("background")));
        for (offset, table) in [(0, "normal"), (8, "bright")] {
            for (index, name) in NAMES.iter().enumerate() {
                if let Some(rgb) = hex(colors.get(table).and_then(|t| t.get(name))) {
                    theme.color(offset + index, rgb);
                }
            }
        }
    } else {
        // WezTerm.
        theme.foreground = hex(colors.get("foreground"));
        theme.background = hex(colors.get("background"));
        theme.cursor = hex(colors.get("cursor_bg"));
        theme.selection = hex(colors.get("selection_bg"));
        for (offset, list) in [(0, "ansi"), (8, "brights")] {
            for (index, color) in
                colors.get(list).and_then(|l| l.as_array()).into_iter().flatten().enumerate()
            {
                if let Some(rgb) = hex(Some(color)) {
                    theme.color(offset + index, rgb);
                }
            }
        }
        theme.name = value
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_owned();
    }
    Ok(theme)
}

/// Ghostty (`palette = 0=#…`, `background = …`) or kitty (`color0 #…`,
/// `background #…`): a key and a value a line.
fn lines(text: &str) -> ImportedTheme {
    let mut theme = ImportedTheme::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Ghostty: `key = value`; kitty: `key value`.
        let (key, value) = match line.split_once('=') {
            Some((key, value)) if !key.trim().contains(' ') => (key.trim(), value.trim()),
            _ => match line.split_once(char::is_whitespace) {
                Some((key, value)) => (key.trim(), value.trim()),
                None => continue,
            },
        };
        match key {
            "foreground" => theme.foreground = parse_hex(value),
            "background" => theme.background = parse_hex(value),
            "cursor-color" | "cursor" => theme.cursor = parse_hex(value),
            "selection-background" | "selection_background" => theme.selection = parse_hex(value),
            "palette" => {
                if let Some((index, color)) = value.split_once('=')
                    && let (Ok(index), Some(rgb)) =
                        (index.trim().parse::<usize>(), parse_hex(color))
                {
                    theme.color(index, rgb);
                }
            },
            key => {
                if let Some(index) = key.strip_prefix("color").and_then(|n| n.parse::<usize>().ok())
                    && let Some(rgb) = parse_hex(value)
                {
                    theme.color(index, rgb);
                }
            },
        }
    }
    theme
}

/// Ghostty's themes on this Mac (its app's, and the user's own), by name.
pub fn ghostty_themes() -> Vec<(String, PathBuf)> {
    let mut dirs =
        vec![PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty/themes")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/ghostty/themes"));
    }
    let mut themes: Vec<(String, PathBuf)> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
        .collect();
    themes.sort_by_key(|(name, _)| name.to_lowercase());
    themes.dedup_by(|a, b| a.0 == b.0);
    themes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: &str) -> Rgb {
        parse_hex(hex).unwrap()
    }

    #[test]
    fn ghostty() {
        let theme = parse(
            "Dracula",
            "palette = 0=#21222c\npalette = 1=#ff5555\npalette = 15=#ffffff\n\
             background = #282a36\nforeground = #f8f8f2\ncursor-color = #f8f8f2\n\
             selection-background = #44475a\n",
        )
        .unwrap();
        assert_eq!(theme.name, "Dracula");
        assert_eq!(theme.background, Some(rgb("#282a36")));
        assert_eq!(theme.selection, Some(rgb("#44475a")));
        assert_eq!(
            (theme.palette.len(), theme.palette[1], theme.palette[15]),
            (16, rgb("ff5555"), rgb("ffffff"))
        );
    }

    #[test]
    fn kitty() {
        let theme = parse(
            "x",
            "# A kitty theme\nforeground #c0caf5\nbackground #1a1b26\nselection_background #33467c\n\
             cursor #c0caf5\ncolor0 #15161e\ncolor1 #f7768e\n",
        )
        .unwrap();
        assert_eq!(theme.foreground, Some(rgb("c0caf5")));
        assert_eq!(theme.palette, [rgb("15161e"), rgb("f7768e")]);
    }

    #[test]
    fn alacritty_and_wezterm() {
        let alacritty = parse(
            "a",
            "[colors.primary]\nbackground = '#1e1e2e'\nforeground = '#cdd6f4'\n\
             [colors.cursor]\ncursor = '#f5e0dc'\n[colors.normal]\nblack = '#45475a'\nred = '#f38ba8'\n\
             [colors.bright]\nwhite = '#a6adc8'\n",
        )
        .unwrap();
        assert_eq!(alacritty.background, Some(rgb("1e1e2e")));
        assert_eq!(alacritty.cursor, Some(rgb("f5e0dc")));
        assert_eq!((alacritty.palette[1], alacritty.palette[15]), (rgb("f38ba8"), rgb("a6adc8")));
        let wezterm = parse(
            "w",
            "[colors]\nforeground = '#dcd7ba'\nbackground = '#1f1f28'\nselection_bg = '#2d4f67'\n\
             ansi = ['#090618', '#c34043', '#76946a', '#c0a36e', '#7e9cd8', '#957fb8', '#6a9589', '#c8c093']\n\
             brights = ['#727169']\n[metadata]\nname = 'Kanagawa'\n",
        )
        .unwrap();
        assert_eq!(wezterm.name, "Kanagawa");
        assert_eq!((wezterm.palette.len(), wezterm.palette[8]), (9, rgb("727169")));
    }

    #[test]
    fn iterm2_and_windows_terminal() {
        let component = |name: &str, value: f64| format!("<key>{name}</key><real>{value}</real>");
        let color = |key: &str, (r, g, b): (f64, f64, f64)| {
            format!(
                "<key>{key}</key><dict>{}{}{}</dict>",
                component("Blue Component", b),
                component("Green Component", g),
                component("Red Component", r)
            )
        };
        let plist = format!(
            "<?xml version=\"1.0\"?><plist><dict>{}{}{}</dict></plist>",
            color("Ansi 1 Color", (1., 0., 0.)),
            color("Background Color", (0., 0., 0.)),
            color("Foreground Color", (1., 1., 1.)),
        );
        let theme = parse("Solarized", &plist).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(theme.background, Some(rgb("000000")));
        assert_eq!(theme.foreground, Some(rgb("ffffff")));
        assert_eq!(theme.palette[1], rgb("ff0000"));
        let windows = parse(
            "w",
            r##"{"name": "One Half Dark", "background": "#282C34", "foreground": "#DCDFE4",
                "cursorColor": "#A3B3CC", "selectionBackground": "#474E5D",
                "black": "#282C34", "red": "#E06C75", "purple": "#C678DD", "brightWhite": "#DCDFE4"}"##,
        )
        .unwrap();
        assert_eq!(windows.name, "One Half Dark");
        assert_eq!(
            (windows.palette[1], windows.palette[5], windows.palette[15]),
            (rgb("e06c75"), rgb("c678dd"), rgb("dcdfe4"))
        );
    }

    #[test]
    fn not_a_theme() {
        assert!(parse("x", "just some text\n").is_err());
    }
}
