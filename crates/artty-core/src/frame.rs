use artty_vt::ansi::{CursorShape, Rgb};
use artty_vt::event::EventListener;
use artty_vt::grid::Dimensions;
use artty_vt::term::Term;
use std::collections::BTreeMap;
use std::sync::Arc;

use artty_vt::term::cell::{Cell, Flags, Image, ImageTile};
use artty_vt::term::placeholder::{PLACEHOLDER, PlaceholderLine};
use artty_vt::term::search::Match;

use crate::theme::{Theme, dim};

/// A renderer-independent snapshot of the visible screen.
///
/// Text is split into runs that a renderer can shape and draw at
/// `column * cell_width`. ASCII text is grouped into runs; every other
/// character gets its own run, so glyphs from fallback fonts (CJK, emoji,
/// symbols) can't push the rest of the line off the cell grid.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    pub columns: usize,
    pub lines: Vec<FrameLine>,
    pub cursor: Option<FrameCursor>,
    /// How many lines the view is scrolled up into history.
    pub display_offset: usize,
    pub background: Rgb,
    pub foreground: Rgb,
    /// The images shown, for sending each to a renderer once (by id).
    #[serde(skip)]
    pub images: Vec<Arc<Image>>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameLine {
    pub runs: Vec<FrameRun>,
    /// Cells whose background differs from the default background.
    pub backgrounds: Vec<FrameRect>,
    /// Selected cells, drawn over the backgrounds.
    pub selection: Vec<FrameRect>,
    /// Search matches, drawn under the selection (which marks the current one).
    pub matches: Vec<FrameRect>,
    /// Parts of images, drawn over the backgrounds and under the text.
    pub images: Vec<FrameImage>,
    /// Underlined cells, drawn over the text.
    pub underlines: Vec<FrameUnderline>,
}

/// Consecutive cells underlined alike.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameUnderline {
    pub column: usize,
    pub width: usize,
    pub style: UnderlineStyle,
    pub color: Rgb,
}

/// How a cell is underlined (`SGR 4`, `4:2` ... `4:5`, `21`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    fn of(flags: Flags) -> Option<Self> {
        Some(if flags.contains(Flags::UNDERCURL) {
            UnderlineStyle::Curly
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            UnderlineStyle::Double
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            UnderlineStyle::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            UnderlineStyle::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            UnderlineStyle::Single
        } else {
            return None;
        })
    }
}

/// Consecutive cells showing one row of an image: from its cell
/// (`tile_column`, `tile_row`) on, `cells` wide.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameImage {
    pub column: usize,
    pub cells: usize,
    pub image: u64,
    pub tile_column: u16,
    pub tile_row: u16,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameRun {
    pub column: usize,
    /// Number of cells the run covers (2 for a double-width character).
    pub cells: usize,
    pub text: String,
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub strikeout: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrameRect {
    pub column: usize,
    pub width: usize,
    pub color: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    strikeout: bool,
}

impl Frame {
    pub(crate) fn capture<T: EventListener>(
        term: &Term<T>,
        theme: &Theme,
        matches: &[Match],
    ) -> Self {
        let content = term.renderable_content();
        let colors = content.colors;
        let selection = content.selection;
        let (cursor_point, cursor_shape) = (content.cursor.point, content.cursor.shape);
        let display_offset = content.display_offset;
        let background = theme
            .resolve(artty_vt::ansi::Color::Named(artty_vt::ansi::NamedColor::Background), colors);
        let foreground = theme
            .resolve(artty_vt::ansi::Color::Named(artty_vt::ansi::NamedColor::Foreground), colors);

        let screen_lines = term.screen_lines();
        let mut lines = vec![FrameLine::default(); screen_lines];
        let mut builder: Option<(usize, LineBuilder)> = None;
        let mut images: BTreeMap<u64, Arc<Image>> = BTreeMap::new();
        // Kitty's Unicode placeholders on the current line, and the column of
        // the last one (they inherit from the cell to their left).
        let mut placeholders = PlaceholderLine::default();
        let mut last_placeholder: Option<usize> = None;

        for indexed in content.display_iter {
            let line = (indexed.point.line.0 + display_offset as i32) as usize;
            if line >= screen_lines {
                continue;
            }
            match &mut builder {
                Some((current, _)) if *current == line => {},
                _ => {
                    if let Some((done, b)) = builder.take() {
                        lines[done] = b.finish();
                    }
                    builder = Some((line, LineBuilder::default()));
                    placeholders = PlaceholderLine::default();
                    last_placeholder = None;
                },
            }
            let (_, b) = builder.as_mut().unwrap();
            let selected =
                selection.is_some_and(|s| s.contains_cell(&indexed, cursor_point, cursor_shape));
            if selected {
                b.select(indexed.point.column.0, indexed.cell.flags, theme.selection);
            }
            let column = indexed.point.column.0;
            if let Some(tile) = indexed.cell.image() {
                b.image(column, tile);
                images.entry(tile.image.id).or_insert_with(|| tile.image.clone());
            }
            if indexed.cell.c == PLACEHOLDER {
                let marks = indexed.cell.zerowidth().unwrap_or_default();
                let adjacent = last_placeholder.is_some_and(|last| last + 1 == column);
                last_placeholder = Some(column);
                let decoded = placeholders.decode(indexed.cell.fg, marks, adjacent);
                if let Some((placeholder, image)) =
                    decoded.and_then(|p| Some((p, term.placeholder_image(p.image)?)))
                {
                    let tile = ImageTile {
                        image,
                        column: placeholder.column as u16,
                        row: placeholder.row as u16,
                    };
                    b.image(column, &tile);
                    images.entry(tile.image.id).or_insert_with(|| tile.image.clone());
                }
            } else {
                placeholders.gap();
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

        // Search matches, in grid coordinates, onto the visible lines.
        let columns = term.columns();
        for found in matches {
            let (start, end) = (*found.start(), *found.end());
            for grid_line in start.line.0..=end.line.0 {
                let Ok(line) = usize::try_from(grid_line + display_offset as i32) else { continue };
                let Some(frame_line) = lines.get_mut(line) else { continue };
                let first = if grid_line == start.line.0 { start.column.0 } else { 0 };
                let last = if grid_line == end.line.0 { end.column.0 } else { columns - 1 };
                frame_line.matches.push(FrameRect {
                    column: first,
                    width: last + 1 - first,
                    color: theme.search_match,
                });
            }
        }

        Frame {
            columns,
            lines,
            cursor,
            display_offset,
            background,
            foreground,
            images: images.into_values().collect(),
        }
    }
}

#[derive(Default)]
struct LineBuilder {
    line: FrameLine,
    run: Option<(FrameRun, Style)>,
}

impl LineBuilder {
    /// A cell showing part of an image: extends the previous span when it
    /// continues it.
    fn image(&mut self, column: usize, tile: &ImageTile) {
        match self.line.images.last_mut() {
            Some(span)
                if span.image == tile.image.id
                    && span.tile_row == tile.row
                    && span.column + span.cells == column
                    && usize::from(span.tile_column) + span.cells == usize::from(tile.column) =>
            {
                span.cells += 1;
            },
            _ => self.line.images.push(FrameImage {
                column,
                cells: 1,
                image: tile.image.id,
                tile_column: tile.column,
                tile_row: tile.row,
            }),
        }
    }

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
                },
                _ => self.line.backgrounds.push(FrameRect { column, width, color: bg }),
            }
        }

        // Underlines are drawn on their own, so underlined spaces show too.
        if let Some(style) = UnderlineStyle::of(flags) {
            let color = cell.underline_color().map_or(fg, |color| theme.resolve(color, colors));
            match self.line.underlines.last_mut() {
                Some(u) if u.style == style && u.color == color && u.column + u.width == column => {
                    u.width += width
                },
                _ => self.line.underlines.push(FrameUnderline { column, width, style, color }),
            }
        }

        // Placeholders (kitty's, for images in tmux) show an image, not a glyph.
        // A tab's first cell keeps the tab (for copying) but shows a space: drawn
        // as a tab, it would push the rest of its run off the grid.
        if matches!(cell.c, ' ' | '\t') && cell.zerowidth().is_none()
            || flags.contains(Flags::HIDDEN)
            || cell.c == PLACEHOLDER
        {
            // Blank cells only matter as a gap: end the run unless it can absorb
            // the space and stay on the grid.
            if let Some((run, _)) = &mut self.run
                && run.column + run.text.len() == column
            {
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
            strikeout: flags.contains(Flags::STRIKEOUT),
        };
        let ascii = cell.c.is_ascii() && cell.zerowidth().is_none();

        if ascii && let Some((run, run_style)) = &mut self.run {
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
            strikeout: style.strikeout,
        };
        if ascii {
            self.run = Some((run, style));
        } else if is_regional_indicator(cell.c)
            && let Some(previous) = self.line.runs.last_mut()
            && previous.column + 1 == column
            && previous.cells == 1
            && previous.text.chars().count() == 1
            && previous.text.chars().all(is_regional_indicator)
        {
            // Two regional indicators are one flag, drawn across both cells.
            previous.text.push(cell.c);
            previous.cells = 2;
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

fn is_regional_indicator(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
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
        Frame::capture(&term, &Theme::default(), &[])
    }

    fn texts(line: &FrameLine) -> Vec<(usize, &str)> {
        line.runs.iter().map(|r| (r.column, r.text.as_str())).collect()
    }

    #[test]
    fn unicode_placeholders_show_their_images() {
        // As the protocol's example: image 42, a 2×2 virtual placement, then
        // its placeholders, row and column spelled out with marks.
        let frame = frame_for(
            "\x1b_Ga=T,U=1,f=32,s=2,v=2,i=42,c=2,r=2,q=2;AAAAAAAAAAAAAAAAAAAAAA\x1b\\\
             \x1b[38;5;42m\u{10eeee}\u{305}\u{305}\u{10eeee}\u{305}\u{30d}\x1b[39m\r\n\
             \x1b[38;5;42m\u{10eeee}\u{30d}\u{305}\u{10eeee}\u{30d}\u{30d}\x1b[39m"
                .as_bytes(),
        );
        let spans = |line: usize| {
            frame.lines[line]
                .images
                .iter()
                .map(|i| (i.column, i.cells, i.tile_column, i.tile_row))
                .collect::<Vec<_>>()
        };
        assert_eq!(spans(0), vec![(0, 2, 0, 0)]);
        assert_eq!(spans(1), vec![(0, 2, 0, 1)]);
        assert_eq!(frame.images.len(), 1);
        assert_eq!(frame.images[0].kitty_id, 42);
        // The placeholder character itself isn't drawn.
        assert!(frame.lines[0].runs.is_empty());

        // Marks left out after the first column: carried on from the left.
        let frame = frame_for(
            "\x1b_Ga=T,U=1,f=32,s=2,v=2,i=7,c=3,r=1,q=2;AAAAAAAAAAAAAAAAAAAAAA\x1b\\\
             \x1b[38;5;7m\u{10eeee}\u{305}\u{10eeee}\u{10eeee}"
                .as_bytes(),
        );
        assert_eq!(
            frame.lines[0].images.iter().map(|i| (i.column, i.cells)).collect::<Vec<_>>(),
            vec![(0, 3)]
        );
    }

    #[test]
    fn underlines_keep_their_style_and_color() {
        // Curly in red (as editors mark errors), then a double underline (4:2),
        // then a plain one across spaces.
        let frame =
            frame_for(b"\x1b[4:3;58:2::255:0:0mab\x1b[0m \x1b[4:2mc\x1b[0m \x1b[4m  \x1b[0m");
        let underlines: Vec<_> =
            frame.lines[0].underlines.iter().map(|u| (u.column, u.width, u.style)).collect();
        assert_eq!(
            underlines,
            vec![
                (0, 2, UnderlineStyle::Curly),
                (3, 1, UnderlineStyle::Double),
                (5, 2, UnderlineStyle::Single)
            ]
        );
        assert_eq!(frame.lines[0].underlines[0].color, Rgb { r: 255, g: 0, b: 0 });
        // Without a color of its own, an underline takes the text's.
        assert_eq!(frame.lines[0].underlines[1].color, frame.foreground);
    }

    #[test]
    fn groups_ascii_and_splits_on_style() {
        let frame = frame_for(b"ab \x1b[31mred\x1b[0m  x");
        assert_eq!(texts(&frame.lines[0]), vec![(0, "ab"), (3, "red"), (8, "x")]);
        assert_eq!(frame.lines[0].runs[1].fg, Theme::default().ansi[1]);
    }

    #[test]
    fn tabs_show_as_spaces_on_the_grid() {
        // The cell a tab starts on keeps the tab; drawn, it'd be wider than a
        // cell and push what follows in its run along.
        let frame = frame_for(b"ab\tc\td");
        assert_eq!(texts(&frame.lines[0]), vec![(0, "ab      c       d")]);
    }

    #[test]
    fn wide_and_non_ascii_chars_get_their_own_runs() {
        let frame = frame_for("a한b✳c".as_bytes());
        assert_eq!(texts(&frame.lines[0]), vec![(0, "a"), (1, "한"), (3, "b"), (4, "✳"), (5, "c")]);
        let cells: Vec<_> = frame.lines[0].runs.iter().map(|r| r.cells).collect();
        assert_eq!(cells, vec![1, 2, 1, 1, 1]);
    }

    #[test]
    fn flags_and_emoji_sequences_are_single_runs() {
        let frame = frame_for("🇰🇷🇯🇵 👨\u{200d}👩\u{200d}👧!".as_bytes());
        let runs: Vec<_> =
            frame.lines[0].runs.iter().map(|r| (r.column, r.cells, r.text.as_str())).collect();
        assert_eq!(
            runs,
            vec![(0, 2, "🇰🇷"), (2, 2, "🇯🇵"), (5, 2, "👨\u{200d}👩\u{200d}👧"), (7, 1, "!")]
        );
    }

    #[test]
    fn only_non_default_backgrounds_are_reported() {
        let frame = frame_for(b"x\x1b[44mbg\x1b[0my");
        let rects: Vec<_> =
            frame.lines[0].backgrounds.iter().map(|r| (r.column, r.width)).collect();
        assert_eq!(rects, vec![(1, 2)]);
    }

    #[test]
    fn selection_becomes_rects() {
        use artty_vt::index::{Column, Line, Point, Side};
        use artty_vt::selection::{Selection, SelectionType};

        let size = TerminalSize { columns: 20, lines: 3, cell_width: 8, cell_height: 16 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        Processor::<artty_vt::ansi::StdSyncHandler>::new().advance(&mut term, b"hello world");
        let mut selection =
            Selection::new(SelectionType::Simple, Point::new(Line(0), Column(1)), Side::Left);
        selection.update(Point::new(Line(0), Column(4)), Side::Right);
        term.selection = Some(selection);

        let frame = Frame::capture(&term, &Theme::default(), &[]);
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
