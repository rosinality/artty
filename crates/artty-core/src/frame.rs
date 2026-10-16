use artty_vt::ansi::{CursorShape, Rgb};
use artty_vt::event::EventListener;
use artty_vt::grid::Dimensions;
use artty_vt::term::Term;
use artty_vt::term::cell::{Cell, Flags};

use crate::theme::{Theme, dim};

/// A renderer-independent snapshot of the visible screen.
///
/// Text is split into runs that a renderer can shape and draw at
/// `column * cell_width`. ASCII text is grouped into runs; every other
/// character gets its own run, so glyphs from fallback fonts (CJK, emoji,
/// symbols) can't push the rest of the line off the cell grid.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub columns: usize,
    pub lines: Vec<FrameLine>,
    pub cursor: Option<FrameCursor>,
    /// How many lines the view is scrolled up into history.
    pub display_offset: usize,
    pub background: Rgb,
    pub foreground: Rgb,
}

#[derive(Clone, Debug, Default)]
pub struct FrameLine {
    pub runs: Vec<FrameRun>,
    /// Cells whose background differs from the default background.
    pub backgrounds: Vec<FrameRect>,
    /// Selected cells, drawn over the backgrounds.
    pub selection: Vec<FrameRect>,
}

#[derive(Clone, Debug)]
pub struct FrameRun {
    pub column: usize,
    /// Number of cells the run covers (2 for a double-width character).
    pub cells: usize,
    pub text: String,
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameRect {
    pub column: usize,
    pub width: usize,
    pub color: Rgb,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameCursor {
    pub line: usize,
    pub column: usize,
    pub shape: CursorShape,
    /// The cursor is on a double-width character.
    pub wide: bool,
    pub blinking: bool,
    pub color: Rgb,
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

impl Frame {
    pub(crate) fn capture<T: EventListener>(term: &Term<T>, theme: &Theme) -> Self {
        let content = term.renderable_content();
        let colors = content.colors;
        let selection = content.selection;
        let (cursor_point, cursor_shape) = (content.cursor.point, content.cursor.shape);
        let display_offset = content.display_offset;
        let background = theme.resolve(
            artty_vt::ansi::Color::Named(artty_vt::ansi::NamedColor::Background),
            colors,
        );
        let foreground = theme.resolve(
            artty_vt::ansi::Color::Named(artty_vt::ansi::NamedColor::Foreground),
            colors,
        );

        let screen_lines = term.screen_lines();
        let mut lines = vec![FrameLine::default(); screen_lines];
        let mut builder: Option<(usize, LineBuilder)> = None;

        for indexed in content.display_iter {
            let line = (indexed.point.line.0 + display_offset as i32) as usize;
            if line >= screen_lines {
                continue;
            }
            match &mut builder {
                Some((current, _)) if *current == line => {}
                _ => {
                    if let Some((done, b)) = builder.take() {
                        lines[done] = b.finish();
                    }
                    builder = Some((line, LineBuilder::default()));
                }
            }
            let (_, b) = builder.as_mut().unwrap();
            let selected =
                selection.is_some_and(|s| s.contains_cell(&indexed, cursor_point, cursor_shape));
            if selected {
                b.select(indexed.point.column.0, indexed.cell.flags, theme.selection);
            }
            b.push(indexed.point.column.0, indexed.cell, theme, colors, background);
        }
        if let Some((done, b)) = builder.take() {
            lines[done] = b.finish();
        }

        let cursor_line = cursor_point.line.0 + display_offset as i32;
        let cursor = (content.cursor.shape != CursorShape::Hidden
            && (0..screen_lines as i32).contains(&cursor_line))
        .then(|| FrameCursor {
            line: cursor_line as usize,
            column: cursor_point.column.0,
            shape: content.cursor.shape,
            wide: term.grid()[cursor_point].flags.contains(Flags::WIDE_CHAR),
            blinking: term.cursor_style().blinking,
            color: colors[artty_vt::ansi::NamedColor::Cursor].unwrap_or(theme.cursor),
        });

        Frame { columns: term.columns(), lines, cursor, display_offset, background, foreground }
    }
}

#[derive(Default)]
struct LineBuilder {
    line: FrameLine,
    run: Option<(FrameRun, Style)>,
}

impl LineBuilder {
    fn select(&mut self, column: usize, flags: Flags, color: Rgb) {
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            return;
        }
        let width = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        match self.line.selection.last_mut() {
            Some(rect) if rect.column + rect.width == column => rect.width += width,
            _ => self.line.selection.push(FrameRect { column, width, color }),
        }
    }

    fn push(
        &mut self,
        column: usize,
        cell: &Cell,
        theme: &Theme,
        colors: &artty_vt::term::color::Colors,
        default_bg: Rgb,
    ) {
        let flags = cell.flags;
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            return;
        }

        let mut fg = theme.resolve(cell.fg, colors);
        let mut bg = theme.resolve(cell.bg, colors);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::DIM) {
            fg = dim(fg);
        }

        let width = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        if bg != default_bg {
            match self.line.backgrounds.last_mut() {
                Some(rect) if rect.color == bg && rect.column + rect.width == column => {
                    rect.width += width;
                }
                _ => self.line.backgrounds.push(FrameRect { column, width, color: bg }),
            }
        }

        if cell.c == ' ' && cell.zerowidth().is_none() || flags.contains(Flags::HIDDEN) {
            // Blank cells only matter as a gap: end the run unless it can absorb
            // the space and stay on the grid.
            if let Some((run, _)) = &mut self.run
                && run.column + run.text.len() == column {
                    run.text.push(' ');
                    return;
                }
            self.flush();
            return;
        }

        let style = Style {
            fg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: flags.contains(Flags::STRIKEOUT),
        };
        let ascii = cell.c.is_ascii() && cell.zerowidth().is_none();

        if ascii
            && let Some((run, run_style)) = &mut self.run {
                // `text.len()` equals the cell count only for all-ASCII runs,
                // which are the only runs we ever extend.
                if *run_style == style && run.column + run.text.len() == column {
                    run.text.push(cell.c);
                    return;
                }
            }

        self.flush();
        let mut text = String::from(cell.c);
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra);
        }
        let run = FrameRun {
            column,
            cells: width,
            text,
            fg: style.fg,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strikeout: style.strikeout,
        };
        if ascii {
            self.run = Some((run, style));
        } else {
            self.line.runs.push(run);
        }
    }

    fn flush(&mut self) {
        if let Some((mut run, _)) = self.run.take() {
            run.text.truncate(run.text.trim_end().len());
            run.cells = run.text.len();
            self.line.runs.push(run);
        }
    }

    fn finish(mut self) -> FrameLine {
        self.flush();
        self.line
    }
}

#[cfg(test)]
mod tests {
    use artty_vt::ansi::Processor;
    use artty_vt::event::VoidListener;
    use artty_vt::term::Config;

    use super::*;
    use crate::TerminalSize;

    fn frame_for(bytes: &[u8]) -> Frame {
        let size = TerminalSize { columns: 20, lines: 3, cell_width: 8, cell_height: 16 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        Processor::<artty_vt::ansi::StdSyncHandler>::new().advance(&mut term, bytes);
        Frame::capture(&term, &Theme::default())
    }

    fn texts(line: &FrameLine) -> Vec<(usize, &str)> {
        line.runs.iter().map(|r| (r.column, r.text.as_str())).collect()
    }

    #[test]
    fn groups_ascii_and_splits_on_style() {
        let frame = frame_for(b"ab \x1b[31mred\x1b[0m  x");
        assert_eq!(texts(&frame.lines[0]), vec![(0, "ab"), (3, "red"), (8, "x")]);
        assert_eq!(frame.lines[0].runs[1].fg, Theme::default().ansi[1]);
    }

    #[test]
    fn wide_and_non_ascii_chars_get_their_own_runs() {
        let frame = frame_for("a한b✳c".as_bytes());
        assert_eq!(texts(&frame.lines[0]), vec![(0, "a"), (1, "한"), (3, "b"), (4, "✳"), (5, "c")]);
        let cells: Vec<_> = frame.lines[0].runs.iter().map(|r| r.cells).collect();
        assert_eq!(cells, vec![1, 2, 1, 1, 1]);
    }

    #[test]
    fn only_non_default_backgrounds_are_reported() {
        let frame = frame_for(b"x\x1b[44mbg\x1b[0my");
        let rects: Vec<_> = frame.lines[0].backgrounds.iter().map(|r| (r.column, r.width)).collect();
        assert_eq!(rects, vec![(1, 2)]);
    }

    #[test]
    fn selection_becomes_rects() {
        use artty_vt::index::{Column, Line, Point, Side};
        use artty_vt::selection::{Selection, SelectionType};

        let size = TerminalSize { columns: 20, lines: 3, cell_width: 8, cell_height: 16 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        Processor::<artty_vt::ansi::StdSyncHandler>::new().advance(&mut term, b"hello world");
        let mut selection = Selection::new(SelectionType::Simple, Point::new(Line(0), Column(1)), Side::Left);
        selection.update(Point::new(Line(0), Column(4)), Side::Right);
        term.selection = Some(selection);

        let frame = Frame::capture(&term, &Theme::default());
        let rects: Vec<_> = frame.lines[0].selection.iter().map(|r| (r.column, r.width)).collect();
        assert_eq!(rects, vec![(1, 4)]);
        assert_eq!(term.selection_to_string().as_deref(), Some("ello"));
    }

    #[test]
    fn cursor_follows_output() {
        let frame = frame_for(b"hello\r\nab");
        let cursor = frame.cursor.unwrap();
        assert_eq!((cursor.line, cursor.column), (1, 2));
    }
}
