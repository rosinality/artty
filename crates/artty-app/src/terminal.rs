//! Painting a terminal [`Frame`] onto the cell grid.

use artty_core::vt::ansi::{CursorShape, Rgb};
use artty_core::{Config, Frame, FrameCursor, FrameLine, FrameRun};
use crate::boxdraw;
use gpui::{
    App, BorderStyle, Bounds, Font, FontStyle, FontWeight, Hsla, Pixels, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, fill, font, outline, point, px, size,
};

/// The system's monospaced font (SF Mono), which isn't listed as a regular
/// font family.
const SYSTEM_MONO_FONT: &str = ".AppleSystemUIFontMonospaced";
const FALLBACK_FONT: &str = "Menlo";

/// Font and cell geometry shared by layout, painting, and the IME.
#[derive(Clone)]
pub struct Metrics {
    pub font: Font,
    pub font_size: Pixels,
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window, config: &Config, font_size: f32) -> Self {
        let font = pick_font(window, &config.font_family);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, px(font_size), 'm')
            .map(|advance| advance.width)
            .unwrap_or(px(font_size * 0.6));
        Self {
            font,
            font_size: px(font_size),
            cell_width,
            line_height: px((font_size * config.line_height).round()),
        }
    }

    pub fn cell_bounds(&self, origin: gpui::Point<Pixels>, line: usize, column: usize, columns: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(origin.x + self.cell_width * column as f32, origin.y + self.line_height * line as f32),
            size(self.cell_width * columns as f32, self.line_height),
        )
    }
}

/// The first configured family that exists. "SF Mono" maps to the system
/// monospaced font.
fn pick_font(window: &Window, families: &[String]) -> Font {
    let text_system = window.text_system();
    // `resolve_font` silently falls back, so detect a miss by comparing with
    // a family that certainly doesn't exist.
    let missing = text_system.resolve_font(&font("artty-missing-font"));
    let candidates = families
        .iter()
        .map(|family| if family == "SF Mono" { SYSTEM_MONO_FONT } else { family.as_str() })
        .chain([SYSTEM_MONO_FONT, FALLBACK_FONT]);
    for family in candidates {
        let candidate = font(family.to_owned());
        if text_system.resolve_font(&candidate) != missing {
            return candidate;
        }
        log::warn!("font {family:?} not found");
    }
    font(FALLBACK_FONT)
}

pub fn hsla(rgb: Rgb) -> Hsla {
    gpui::rgb(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)).into()
}

/// Per-pane drawing state beyond the frame itself.
pub struct PaintState<'a> {
    pub focused: bool,
    /// False during the "off" phase of a blink.
    pub cursor_visible: bool,
    /// IME composition text, drawn at the cursor instead of it.
    pub marked_text: Option<&'a str>,
    /// How much to fade the pane's content (0 = none), for unfocused splits.
    pub fade: f32,
    /// A link to underline (⌘ held over it): line and columns.
    pub link: Option<(usize, std::ops::Range<usize>)>,
    /// Visual bell brightness, 0–1.
    pub flash: f32,
}

pub fn paint(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    metrics: &Metrics,
    state: PaintState<'_>,
    window: &mut Window,
    cx: &mut App,
) {
    let origin = bounds.origin;
    let PaintState { focused, cursor_visible, marked_text, fade, link, flash } = state;
    let faded = |color: Hsla| Hsla { a: color.a * (1. - fade), ..color };

    for (index, line) in frame.lines.iter().enumerate() {
        for rect in line.backgrounds.iter().chain(&line.matches).chain(&line.selection) {
            window.paint_quad(fill(metrics.cell_bounds(origin, index, rect.column, rect.width), faded(hsla(rect.color))));
        }
    }

    for (index, line) in frame.lines.iter().enumerate() {
        for run in &line.runs {
            let cell = metrics.cell_bounds(origin, index, run.column, run.cells);
            paint_text(cell, &run.text, run, faded(hsla(run.fg)), metrics, window, cx);
        }
    }

    if let Some((line, columns)) = link {
        let cells = metrics.cell_bounds(origin, line, columns.start, columns.len());
        let y = cells.origin.y + cells.size.height - px(2.);
        let underline = Bounds::new(point(cells.origin.x, y), size(cells.size.width, px(1.)));
        window.paint_quad(fill(underline, hsla(frame.foreground)));
    }

    if let Some(text) = marked_text.filter(|text| !text.is_empty()) {
        paint_marked_text(origin, frame, text, metrics, window, cx);
    } else if let Some(cursor) = frame.cursor.filter(|_| cursor_visible || !focused) {
        paint_cursor(origin, frame, cursor, focused, metrics, window, cx);
    }

    if flash > 0. {
        window.paint_quad(fill(bounds, Hsla { a: 0.14 * flash, ..hsla(frame.foreground) }));
    }
}

fn styled_font(metrics: &Metrics, run: &FrameRun) -> Font {
    let mut font = metrics.font.clone();
    if run.bold {
        font.weight = FontWeight::BOLD;
    }
    if run.italic {
        font.style = FontStyle::Italic;
    }
    font
}

/// Paint `text` starting at the left of `cells`. A lone non-ASCII character
/// is centered in its cells, or drawn as a shape if it's a box character.
fn paint_text(
    cells: Bounds<Pixels>,
    text: &str,
    run: &FrameRun,
    color: Hsla,
    metrics: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let mut chars = text.chars();
    let single = chars.next().filter(|_| chars.next().is_none() && !text.is_ascii());
    if let Some(c) = single
        && boxdraw::paint(c, cells, color, window)
    {
        return;
    }
    let text_run = TextRun {
        len: text.len(),
        font: styled_font(metrics, run),
        color,
        background_color: None,
        underline: run.underline.then_some(UnderlineStyle { thickness: px(1.), color: Some(color), wavy: false }),
        strikethrough: run.strikeout.then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    };
    let shaped = window.text_system().shape_line(text.to_owned().into(), metrics.font_size, &[text_run], None);
    let mut at = cells.origin;
    if single.is_some() && shaped.width < cells.size.width {
        // CJK glyphs are narrower than two cells; center them like other terminals.
        at.x += ((cells.size.width - shaped.width) / 2.).floor();
    }
    let _ = shaped.paint(at, metrics.line_height, window, cx);
}

/// The run and character drawn at `column`, if any.
fn char_at(line: &FrameLine, column: usize) -> Option<(&FrameRun, String)> {
    line.runs.iter().find_map(|run| {
        if run.text.is_ascii() {
            let offset = column.checked_sub(run.column)?;
            let c = run.text.as_bytes().get(offset)?;
            Some((run, (*c as char).to_string()))
        } else {
            (run.column == column).then(|| (run, run.text.clone()))
        }
    })
}

fn paint_cursor(
    origin: gpui::Point<Pixels>,
    frame: &Frame,
    cursor: FrameCursor,
    focused: bool,
    metrics: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let cell = metrics.cell_bounds(origin, cursor.line, cursor.column, if cursor.wide { 2 } else { 1 });
    let mut color = hsla(cursor.color);
    // Bars and underlines stay thin but readable at any font size.
    let thickness = px((f32::from(metrics.cell_width) * 0.14).round().max(2.));

    // Unfocused: blocks become outlines; thin shapes are drawn dimmed.
    let shape = match cursor.shape {
        CursorShape::Block if !focused => CursorShape::HollowBlock,
        shape => shape,
    };
    if !focused {
        color.a *= 0.5;
    }
    match shape {
        CursorShape::Beam => {
            window.paint_quad(fill(Bounds::new(cell.origin, size(thickness, cell.size.height)), color));
        }
        CursorShape::Underline => {
            let y = cell.origin.y + cell.size.height - thickness;
            window.paint_quad(fill(Bounds::new(point(cell.origin.x, y), size(cell.size.width, thickness)), color));
        }
        CursorShape::HollowBlock => window.paint_quad(outline(cell, color, BorderStyle::Solid)),
        CursorShape::Block => {
            window.paint_quad(fill(cell, color));
            // Redraw the character under the cursor so it stays readable.
            if let Some((run, text)) = frame.lines.get(cursor.line).and_then(|line| char_at(line, cursor.column)) {
                paint_text(cell, &text, run, hsla(frame.background), metrics, window, cx);
            }
        }
        CursorShape::Hidden => {}
    }
}

/// IME composition text (e.g. a Hangul syllable being typed), drawn at the cursor.
fn paint_marked_text(
    origin: gpui::Point<Pixels>,
    frame: &Frame,
    text: &str,
    metrics: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(cursor) = frame.cursor else { return };
    let at = metrics.cell_bounds(origin, cursor.line, cursor.column, 1).origin;
    let color = hsla(frame.foreground);
    let run = TextRun {
        len: text.len(),
        font: metrics.font.clone(),
        color,
        background_color: Some(hsla(frame.background)),
        underline: Some(UnderlineStyle { thickness: px(1.), color: Some(color), wavy: false }),
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(text.to_owned().into(), metrics.font_size, &[run], None);
    let _ = shaped.paint_background(at, metrics.line_height, window, cx);
    let _ = shaped.paint(at, metrics.line_height, window, cx);
}
