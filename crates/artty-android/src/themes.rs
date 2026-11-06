//! The terminal's colors for plain ssh machines (an artty server keeps the
//! Mac app's): a few well-known themes, and any theme file another terminal
//! uses (iTerm2, Ghostty, kitty, Alacritty, WezTerm, Windows Terminal).

use artty_core::theme_import;
use artty_core::vt::ansi::Rgb;

use crate::ConnectError;

/// A theme: its colors as 0xRRGGBB, the 16 ANSI ones in order.
#[derive(uniffi::Record, Clone)]
pub struct TerminalTheme {
    pub name: String,
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: Vec<u32>,
}

/// The cursor's shape when a program doesn't choose one.
#[derive(uniffi::Enum, Clone, Copy)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
    Hollow,
}

/// Built in, as Ghostty writes themes.
const BUILT_IN: &[(&str, &str)] = &[
    (
        "Tokyo Night",
        "background = 1a1b26\nforeground = c0caf5\ncursor-color = c0caf5\nselection-background = 33467c\n\
      palette = 0=#15161e\npalette = 1=#f7768e\npalette = 2=#9ece6a\npalette = 3=#e0af68\npalette = 4=#7aa2f7\npalette = 5=#bb9af7\npalette = 6=#7dcfff\npalette = 7=#a9b1d6\n\
      palette = 8=#414868\npalette = 9=#f7768e\npalette = 10=#9ece6a\npalette = 11=#e0af68\npalette = 12=#7aa2f7\npalette = 13=#bb9af7\npalette = 14=#7dcfff\npalette = 15=#c0caf5\n",
    ),
    (
        "Catppuccin Mocha",
        "background = 1e1e2e\nforeground = cdd6f4\ncursor-color = f5e0dc\nselection-background = 585b70\n\
      palette = 0=#45475a\npalette = 1=#f38ba8\npalette = 2=#a6e3a1\npalette = 3=#f9e2af\npalette = 4=#89b4fa\npalette = 5=#f5c2e7\npalette = 6=#94e2d5\npalette = 7=#bac2de\n\
      palette = 8=#585b70\npalette = 9=#f38ba8\npalette = 10=#a6e3a1\npalette = 11=#f9e2af\npalette = 12=#89b4fa\npalette = 13=#f5c2e7\npalette = 14=#94e2d5\npalette = 15=#a6adc8\n",
    ),
    (
        "Gruvbox Dark",
        "background = 282828\nforeground = ebdbb2\ncursor-color = ebdbb2\nselection-background = 665c54\n\
      palette = 0=#282828\npalette = 1=#cc241d\npalette = 2=#98971a\npalette = 3=#d79921\npalette = 4=#458588\npalette = 5=#b16286\npalette = 6=#689d6a\npalette = 7=#a89984\n\
      palette = 8=#928374\npalette = 9=#fb4934\npalette = 10=#b8bb26\npalette = 11=#fabd2f\npalette = 12=#83a598\npalette = 13=#d3869b\npalette = 14=#8ec07c\npalette = 15=#ebdbb2\n",
    ),
    (
        "Nord",
        "background = 2e3440\nforeground = d8dee9\ncursor-color = d8dee9\nselection-background = 4c566a\n\
      palette = 0=#3b4252\npalette = 1=#bf616a\npalette = 2=#a3be8c\npalette = 3=#ebcb8b\npalette = 4=#81a1c1\npalette = 5=#b48ead\npalette = 6=#88c0d0\npalette = 7=#e5e9f0\n\
      palette = 8=#4c566a\npalette = 9=#bf616a\npalette = 10=#a3be8c\npalette = 11=#ebcb8b\npalette = 12=#81a1c1\npalette = 13=#b48ead\npalette = 14=#8fbcbb\npalette = 15=#eceff4\n",
    ),
    (
        "Dracula",
        "background = 282a36\nforeground = f8f8f2\ncursor-color = f8f8f2\nselection-background = 44475a\n\
      palette = 0=#21222c\npalette = 1=#ff5555\npalette = 2=#50fa7b\npalette = 3=#f1fa8c\npalette = 4=#bd93f9\npalette = 5=#ff79c6\npalette = 6=#8be9fd\npalette = 7=#f8f8f2\n\
      palette = 8=#6272a4\npalette = 9=#ff6e6e\npalette = 10=#69ff94\npalette = 11=#ffffa5\npalette = 12=#d6acff\npalette = 13=#ff92df\npalette = 14=#a4ffff\npalette = 15=#ffffff\n",
    ),
    (
        "Solarized Dark",
        "background = 002b36\nforeground = 839496\ncursor-color = 93a1a1\nselection-background = 073642\n\
      palette = 0=#073642\npalette = 1=#dc322f\npalette = 2=#859900\npalette = 3=#b58900\npalette = 4=#268bd2\npalette = 5=#d33682\npalette = 6=#2aa198\npalette = 7=#eee8d5\n\
      palette = 8=#002b36\npalette = 9=#cb4b16\npalette = 10=#586e75\npalette = 11=#657b83\npalette = 12=#839496\npalette = 13=#6c71c4\npalette = 14=#93a1a1\npalette = 15=#fdf6e3\n",
    ),
    (
        "Solarized Light",
        "background = fdf6e3\nforeground = 657b83\ncursor-color = 586e75\nselection-background = eee8d5\n\
      palette = 0=#073642\npalette = 1=#dc322f\npalette = 2=#859900\npalette = 3=#b58900\npalette = 4=#268bd2\npalette = 5=#d33682\npalette = 6=#2aa198\npalette = 7=#eee8d5\n\
      palette = 8=#002b36\npalette = 9=#cb4b16\npalette = 10=#586e75\npalette = 11=#657b83\npalette = 12=#839496\npalette = 13=#6c71c4\npalette = 14=#93a1a1\npalette = 15=#fdf6e3\n",
    ),
];

fn hex(rgb: Rgb) -> u32 {
    (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
}

fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

fn record(name: &str, theme: &artty_core::Theme) -> TerminalTheme {
    TerminalTheme {
        name: name.to_owned(),
        foreground: hex(theme.foreground),
        background: hex(theme.background),
        cursor: hex(theme.cursor),
        selection: hex(theme.selection),
        ansi: theme.ansi.iter().map(|c| hex(*c)).collect(),
    }
}

/// artty's own, then the built-in ones.
#[uniffi::export]
pub fn builtin_themes() -> Vec<TerminalTheme> {
    let mut themes = vec![record("artty", &artty_core::Theme::default())];
    for (name, text) in BUILT_IN {
        if let Ok(imported) = theme_import::parse(name, text) {
            themes.push(record(name, &imported.theme()));
        }
    }
    themes
}

/// A theme file another terminal uses (`name`, its file name, tells some apart).
#[uniffi::export]
pub fn import_theme(name: String, text: String) -> Result<TerminalTheme, ConnectError> {
    let imported = theme_import::parse(&name, &text)?;
    let shown = if imported.name.is_empty() {
        name.rsplit_once('.').map_or(name.as_str(), |(n, _)| n).to_owned()
    } else {
        imported.name.clone()
    };
    Ok(record(&shown, &imported.theme()))
}

/// The theme for the core (search matches as artty's, which themes don't set).
pub(crate) fn theme(theme: &TerminalTheme) -> artty_core::Theme {
    let mut out = artty_core::Theme {
        foreground: rgb(theme.foreground),
        background: rgb(theme.background),
        cursor: rgb(theme.cursor),
        selection: rgb(theme.selection),
        ..Default::default()
    };
    for (slot, color) in out.ansi.iter_mut().zip(&theme.ansi) {
        *slot = rgb(*color);
    }
    out
}

pub(crate) fn cursor(shape: CursorShape) -> artty_core::CursorStyle {
    match shape {
        CursorShape::Block => artty_core::CursorStyle::Block,
        CursorShape::Bar => artty_core::CursorStyle::Bar,
        CursorShape::Underline => artty_core::CursorStyle::Underline,
        CursorShape::Hollow => artty_core::CursorStyle::Hollow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_imported() {
        let themes = builtin_themes();
        assert_eq!(themes.len(), 1 + BUILT_IN.len(), "each built-in one reads");
        let tokyo = themes.iter().find(|t| t.name == "Tokyo Night").unwrap();
        assert_eq!((tokyo.background, tokyo.ansi[4], tokyo.ansi.len()), (0x1a1b26, 0x7aa2f7, 16));
        let kitty = import_theme(
            "x.conf".into(),
            "background #101010\nforeground #eeeeee\ncolor1 #ff0000\n".into(),
        )
        .unwrap();
        assert_eq!((kitty.background, kitty.ansi[1]), (0x101010, 0xff0000));
        assert_eq!(theme(&kitty).background, rgb(0x101010));
        assert!(import_theme("x".into(), "nonsense".into()).is_err());
    }
}
