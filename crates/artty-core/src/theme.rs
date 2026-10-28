use artty_vt::ansi::{Color, NamedColor, Rgb};
use artty_vt::term::color::Colors;

/// Default colors, used wherever the running program has not overridden them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    /// Search matches other than the current one.
    pub search_match: Rgb,
    /// The 16 ANSI colors: normal 0–7, then bright 8–15.
    pub ansi: [Rgb; 16],
}

const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: rgb(0xd8dee9),
            background: rgb(0x161a22),
            cursor: rgb(0xe6e6e6),
            selection: rgb(0x3d5a8a),
            search_match: rgb(0x6b5a2a),
            ansi: [
                rgb(0x3b4252),
                rgb(0xe06c75),
                rgb(0x9ece6a),
                rgb(0xe0af68),
                rgb(0x7aa2f7),
                rgb(0xbb9af7),
                rgb(0x7dcfff),
                rgb(0xd8dee9),
                rgb(0x616e88),
                rgb(0xf7768e),
                rgb(0xb9f27c),
                rgb(0xffc777),
                rgb(0x8fb4ff),
                rgb(0xc7a9ff),
                rgb(0xa4e0ff),
                rgb(0xeceff4),
            ],
        }
    }
}

impl Theme {
    /// The same colors as `other`, but for search matches (no theme sets
    /// those).
    pub fn same_colors(&self, other: &Theme) -> bool {
        self.foreground == other.foreground
            && self.background == other.background
            && self.cursor == other.cursor
            && self.selection == other.selection
            && self.ansi == other.ansi
    }

    /// Resolve a cell color, preferring colors the program set via OSC 4/10/11.
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb,
            Color::Named(name) => overrides[name].unwrap_or_else(|| self.named(name)),
            Color::Indexed(index) => {
                overrides[index as usize].unwrap_or_else(|| self.indexed(index as usize))
            },
        }
    }

    /// The color for a palette index as reported to OSC 4/10/11 queries.
    pub fn color_for_index(&self, index: usize, overrides: &Colors) -> Rgb {
        if let Some(rgb) = overrides[index] {
            return rgb;
        }
        match index {
            0..=255 => self.indexed(index),
            256 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            _ => self.foreground,
        }
    }

    fn named(&self, name: NamedColor) -> Rgb {
        match name {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => dim(self.foreground),
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => dim(self.ansi[name as usize - NamedColor::DimBlack as usize]),
            _ => self.ansi[name as usize],
        }
    }

    /// The xterm 256-color palette: 16 theme colors, a 6×6×6 cube, and a gray ramp.
    fn indexed(&self, index: usize) -> Rgb {
        match index {
            0..16 => self.ansi[index],
            16..232 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
                Rgb { r: level(i / 36), g: level(i / 6 % 6), b: level(i % 6) }
            },
            _ => {
                let v = (8 + (index.min(255) - 232) * 10) as u8;
                Rgb { r: v, g: v, b: v }
            },
        }
    }
}

pub(crate) fn dim(rgb: Rgb) -> Rgb {
    rgb * 0.66
}
