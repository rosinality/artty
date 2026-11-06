//! What this terminal shows, cell by cell: panes painted from their frames,
//! borders, the status line; and the escape codes turning what's shown into
//! what should be (the cells that changed, in truecolor).

use std::fmt::Write as _;

use artty_core::Frame;
use artty_core::vt::ansi::{CursorShape, Rgb};

/// A cell's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
}

impl Style {
    pub fn plain(fg: Rgb, bg: Rgb) -> Self {
        Style { fg, bg, bold: false, italic: false, underline: false, strikeout: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// What it shows; empty for the right half of a wide character.
    pub text: String,
    pub style: Style,
}

/// A rectangle of cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Area {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Area {
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// The cursor, where it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    cells: Vec<Cell>,
}

impl Grid {
    pub fn new(width: usize, height: usize, style: Style) -> Self {
        let blank = Cell { text: " ".into(), style };
        Grid { width, height, cells: vec![blank; width * height] }
    }

    pub fn get(&self, x: usize, y: usize) -> Option<&Cell> {
        (x < self.width && y < self.height).then(|| &self.cells[y * self.width + x])
    }

    fn get_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        (x < self.width && y < self.height).then(|| &mut self.cells[y * self.width + x])
    }

    /// `text` at (`x`, `y`) taking `cells` cells, within `clip`: a wide
    /// character cut off by its edge shows as a space.
    fn put(&mut self, x: usize, y: usize, text: &str, cells: usize, style: Style, clip: Area) {
        if !clip.contains(x, y) {
            return;
        }
        if cells > 1 && !clip.contains(x + cells - 1, y) {
            if let Some(cell) = self.get_mut(x, y) {
                *cell = Cell { text: " ".into(), style };
            }
            return;
        }
        // Half of a wide character overwritten: the other half goes too.
        self.unwide(x, y);
        if cells > 1 {
            self.unwide(x + cells - 1, y);
        }
        if let Some(cell) = self.get_mut(x, y) {
            *cell = Cell { text: text.to_owned(), style };
        }
        for i in 1..cells {
            if let Some(cell) = self.get_mut(x + i, y) {
                *cell = Cell { text: String::new(), style };
            }
        }
    }

    fn unwide(&mut self, x: usize, y: usize) {
        let Some(cell) = self.get(x, y) else { return };
        if cell.text.is_empty() && x > 0 {
            if let Some(left) = self.get_mut(x - 1, y) {
                left.text = " ".into();
            }
        } else if self.get(x + 1, y).is_some_and(|right| right.text.is_empty())
            && let Some(right) = self.get_mut(x + 1, y)
        {
            right.text = " ".into();
        }
    }

    /// Text from (`x`, `y`) on, a cell a character (wide ones two), up to
    /// `width` cells; returns the cells used.
    pub fn text(&mut self, x: usize, y: usize, width: usize, text: &str, style: Style) -> usize {
        let clip = Area { x, y, width, height: 1 };
        let mut at = 0;
        for c in text.chars() {
            let cells = char_width(c);
            if cells == 0 {
                continue;
            }
            if at + cells > width {
                break;
            }
            self.put(x + at, y, c.encode_utf8(&mut [0; 4]), cells, style, clip);
            at += cells;
        }
        at
    }

    pub fn fill(&mut self, area: Area, text: &str, style: Style) {
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                self.put(x, y, text, 1, style, area);
            }
        }
    }

    /// Change the look of cells already drawn.
    pub fn restyle(&mut self, area: Area, change: impl Fn(&mut Style)) {
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = self.get_mut(x, y) {
                    change(&mut cell.style);
                }
            }
        }
    }

    /// A pane's screen in `area` (what doesn't fit is cut off; what's
    /// missing is its background).
    pub fn frame(&mut self, area: Area, frame: &Frame) {
        let blank = Style::plain(frame.foreground, frame.background);
        self.fill(area, " ", blank);
        for (row, line) in frame.lines.iter().enumerate().take(area.height) {
            let y = area.y + row;
            let span = |column: usize, width: usize| Area {
                x: area.x + column,
                y,
                width: width.min(area.width.saturating_sub(column)),
                height: 1,
            };
            for rect in line.backgrounds.iter().chain(&line.matches).chain(&line.selection) {
                self.restyle(span(rect.column, rect.width), |style| style.bg = rect.color);
            }
            for run in &line.runs {
                let mut column = run.column;
                // Runs of ASCII are a cell a character; others are one
                // character (with its marks) in `cells` cells.
                let ascii = run.text.is_ascii();
                let pieces: Vec<(&str, usize)> = if ascii {
                    run.text.char_indices().map(|(i, _)| (&run.text[i..i + 1], 1)).collect()
                } else {
                    vec![(run.text.as_str(), run.cells.max(1))]
                };
                for (text, cells) in pieces {
                    let x = area.x + column;
                    if let Some(cell) = self.get(x, y).filter(|_| area.contains(x, y)) {
                        let style = Style {
                            fg: run.fg,
                            bold: run.bold,
                            italic: run.italic,
                            strikeout: run.strikeout,
                            ..cell.style
                        };
                        self.put(x, y, text, cells, style, area);
                    }
                    column += cells;
                }
            }
            for underline in &line.underlines {
                self.restyle(span(underline.column, underline.width), |style| {
                    style.underline = true
                });
            }
        }
    }
}

/// Cells a character takes in a terminal: 0 for combining marks and
/// controls, 2 for East Asian wide ones and emoji, else 1.
pub fn char_width(c: char) -> usize {
    let c = c as u32;
    match c {
        0..=0x1f | 0x7f..=0x9f => 0,
        0x300..=0x36f | 0x200b..=0x200f | 0xfe00..=0xfe0f | 0x20d0..=0x20ff => 0,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

/// Escape codes turning `shown` (`None`: anything) into `next`, then the
/// cursor where it goes (hidden without one), its shape as `shape_shown`
/// says it is now (updated).
pub fn draw(
    shown: Option<&Grid>,
    next: &Grid,
    cursor: Option<Cursor>,
    shape_shown: &mut Option<(CursorShape, bool)>,
) -> String {
    let mut out = String::new();
    // Drawn at once (terminals that can), the cursor out of the way.
    out.push_str("\x1b[?2026h\x1b[?25l");
    let shown = shown.filter(|s| s.width == next.width && s.height == next.height);
    if shown.is_none() {
        out.push_str("\x1b[0m\x1b[2J");
    }
    let mut style: Option<Style> = None;
    // Where the terminal's cursor is, if known.
    let mut at: Option<(usize, usize)> = None;
    for y in 0..next.height {
        for x in 0..next.width {
            let cell = next.get(x, y).unwrap();
            // A wide character's right half changes with its left.
            if cell.text.is_empty() || shown.and_then(|s| s.get(x, y)) == Some(cell) {
                continue;
            }
            if at != Some((x, y)) {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            }
            if style != Some(cell.style) {
                sgr(&mut out, &cell.style);
                style = Some(cell.style);
            }
            out.push_str(&cell.text);
            let wide = next.get(x + 1, y).is_some_and(|c| c.text.is_empty());
            // Where a character other than ASCII leaves the cursor is the
            // terminal's guess: placed again after one.
            at = cell.text.is_ascii().then_some((x + 1 + wide as usize, y));
        }
    }
    out.push_str("\x1b[0m");
    if let Some(cursor) = cursor {
        let _ = write!(out, "\x1b[{};{}H", cursor.y + 1, cursor.x + 1);
        if *shape_shown != Some((cursor.shape, cursor.blinking)) {
            let n = match cursor.shape {
                CursorShape::Underline => 4,
                CursorShape::Beam => 6,
                _ => 2,
            } - cursor.blinking as u8;
            let _ = write!(out, "\x1b[{n} q");
            *shape_shown = Some((cursor.shape, cursor.blinking));
        }
        if cursor.shape != CursorShape::Hidden {
            out.push_str("\x1b[?25h");
        }
    }
    out.push_str("\x1b[?2026l");
    out
}

fn sgr(out: &mut String, style: &Style) {
    let Rgb { r, g, b } = style.fg;
    let _ = write!(out, "\x1b[0;38;2;{r};{g};{b}");
    let Rgb { r, g, b } = style.bg;
    let _ = write!(out, ";48;2;{r};{g};{b}");
    for (on, code) in
        [(style.bold, 1), (style.italic, 3), (style.underline, 4), (style.strikeout, 9)]
    {
        if on {
            let _ = write!(out, ";{code}");
        }
    }
    out.push('m');
}

/// Box-drawing lines through the cells in `border`, joined where they meet.
pub fn borders(grid: &mut Grid, border: &[Vec<bool>], style: impl Fn(usize, usize) -> Style) {
    let at = |x: isize, y: isize| {
        x >= 0
            && y >= 0
            && border.get(y as usize).and_then(|row| row.get(x as usize)).copied().unwrap_or(false)
    };
    for (y, row) in border.iter().enumerate() {
        for (x, &on) in row.iter().enumerate() {
            if !on {
                continue;
            }
            let (xi, yi) = (x as isize, y as isize);
            let (up, down, left, right) =
                (at(xi, yi - 1), at(xi, yi + 1), at(xi - 1, yi), at(xi + 1, yi));
            let c = match (up, down, left, right) {
                (true, true, true, true) => "┼",
                (true, true, true, false) => "┤",
                (true, true, false, true) => "├",
                (false, true, true, true) => "┬",
                (true, false, true, true) => "┴",
                (_, _, true, _) | (_, _, _, true) if !(up || down) => "─",
                (false, true, false, true) => "┌",
                (false, true, true, false) => "┐",
                (true, false, false, true) => "└",
                (true, false, true, false) => "┘",
                _ => "│",
            };
            let full = Area { x: 0, y: 0, width: grid.width, height: grid.height };
            grid.put(x, y, c, 1, style(x, y), full);
        }
    }
}

/// `text` as base64 (for OSC 52).
pub fn base64(text: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(text.len().div_ceil(3) * 4);
    for chunk in text.chunks(3) {
        let bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use artty_core::{FrameLine, FrameRect, FrameRun};

    const WHITE: Rgb = Rgb { r: 255, g: 255, b: 255 };
    const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
    const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };

    fn row(grid: &Grid, y: usize) -> String {
        (0..grid.width).map(|x| grid.get(x, y).unwrap().text.as_str()).collect()
    }

    fn run(column: usize, text: &str, cells: usize) -> FrameRun {
        FrameRun {
            column,
            cells,
            text: text.into(),
            fg: RED,
            bold: true,
            italic: false,
            strikeout: false,
        }
    }

    #[test]
    fn frames_paint_and_clip() {
        let mut grid = Grid::new(10, 3, Style::plain(WHITE, BLACK));
        let frame = Frame {
            columns: 8,
            lines: vec![
                FrameLine {
                    runs: vec![run(0, "hello", 5), run(6, "한", 2)],
                    backgrounds: vec![FrameRect { column: 1, width: 2, color: RED }],
                    ..Default::default()
                },
                FrameLine { runs: vec![run(0, "toolongline", 11)], ..Default::default() },
            ],
            background: BLACK,
            foreground: WHITE,
            ..Default::default()
        };
        grid.frame(Area { x: 1, y: 0, width: 7, height: 2 }, &frame);
        // The wide character doesn't fit in the last column: a space.
        assert_eq!(row(&grid, 0), " hello    ");
        assert_eq!(row(&grid, 1), " toolong  ");
        let cell = grid.get(2, 0).unwrap();
        assert_eq!((cell.style.fg, cell.style.bg, cell.style.bold), (RED, RED, true));
        assert_eq!(grid.get(1, 0).unwrap().style.bg, BLACK);
        // Fits: two cells, the second empty.
        grid.frame(Area { x: 0, y: 0, width: 10, height: 1 }, &frame);
        assert_eq!(row(&grid, 0), "hello 한  ");
        assert_eq!(grid.get(7, 0).unwrap().text, "");
    }

    #[test]
    fn only_changes_are_drawn() {
        let style = Style::plain(WHITE, BLACK);
        let mut grid = Grid::new(6, 2, style);
        grid.text(0, 0, 6, "abc", style);
        let mut shape = None;
        let full = draw(None, &grid, None, &mut shape);
        assert!(full.contains("\x1b[2J") && full.contains("abc"));
        let mut next = grid.clone();
        next.text(1, 1, 5, "x", style);
        let cursor = Cursor { x: 2, y: 1, shape: CursorShape::Beam, blinking: false };
        let diff = draw(Some(&grid), &next, Some(cursor), &mut shape);
        assert!(!diff.contains("abc") && !diff.contains("\x1b[2J"));
        assert!(diff.contains("\x1b[2;2H") && diff.contains('x'));
        assert!(
            diff.contains("\x1b[6 q") && diff.ends_with("\x1b[2;3H\x1b[6 q\x1b[?25h\x1b[?2026l")
        );
        // The shape is told once.
        assert!(!draw(Some(&next), &next, Some(cursor), &mut shape).contains(" q"));
    }

    #[test]
    fn borders_join() {
        let style = Style::plain(WHITE, BLACK);
        let mut grid = Grid::new(5, 3, style);
        // A line down the middle, one across the right half.
        let mut border = vec![vec![false; 5]; 3];
        for row in border.iter_mut() {
            row[2] = true;
        }
        border[1][3] = true;
        border[1][4] = true;
        borders(&mut grid, &border, |_, _| style);
        assert_eq!(row(&grid, 0), "  │  ");
        assert_eq!(row(&grid, 1), "  ├──");
        assert_eq!(row(&grid, 2), "  │  ");
    }

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("한".as_bytes()), "7ZWc");
    }
}
