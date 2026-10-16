//! Painting a terminal [`Frame`] onto the cell grid.

use artty_core::vt::ansi::{CursorShape, Rgb};
use artty_core::{Frame, FrameCursor, FrameLine, FrameRun};
use gpui::{
    App, BorderStyle, Bounds, Font, FontStyle, FontWeight, Hsla, Pixels, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, fill, font, outline, point, px, size,
};

pub const MONO_FONT: &str = "Menlo";
pub const FONT_SIZE: f32 = 13.;

/// Font and cell geometry shared by layout, painting, and the IME.
#[derive(Clone)]
pub struct Metrics {
    pub font: Font,
    pub font_size: Pixels,
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window) -> Self {
        let font = font(MONO_FONT);
        let font_size = px(FONT_SIZE);
        let font_id = window.text_system().resolve_font(&font);
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(px(FONT_SIZE * 0.6));
        Self { font, font_size, cell_width, line_height: px((FONT_SIZE * 1.35).round()) }
    }

    pub fn cell_bounds(&self, origin: gpui::Point<Pixels>, line: usize, column: usize, columns: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(origin.x + self.cell_width * column as f32, origin.y + self.line_height * line as f32),
            size(self.cell_width * columns as f32, self.line_height),
        )
    }
}

pub fn hsla(rgb: Rgb) -> Hsla {
    gpui::rgb(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)).into()
}

pub fn paint(
    bounds: Bounds<Pixels>,
    frame: &Frame,
    metrics: &Metrics,
    focused: bool,
    marked_text: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    let origin = bounds.origin;

    for (index, line) in frame.lines.iter().enumerate() {
        for rect in &line.backgrounds {
            window.paint_quad(fill(metrics.cell_bounds(origin, index, rect.column, rect.width), hsla(rect.color)));
        }
    }

    for (index, line) in frame.lines.iter().enumerate() {
        for run in &line.runs {
            paint_text(metrics.cell_bounds(origin, index, run.column, 1).origin, &run.text, run, hsla(run.fg), metrics, window, cx);
        }
    }

    if let Some(text) = marked_text.filter(|text| !text.is_empty()) {
        paint_marked_text(origin, frame, text, metrics, window, cx);
    } else if let Some(cursor) = frame.cursor {
        paint_cursor(origin, frame, cursor, focused, metrics, window, cx);
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

fn paint_text(
    at: gpui::Point<Pixels>,
    text: &str,
    run: &FrameRun,
    color: Hsla,
    metrics: &Metrics,
    window: &mut Window,
    cx: &mut App,
) {
    let text_run = TextRun {
        len: text.len(),
        font: styled_font(metrics, run),
        color,
        background_color: None,
        underline: run.underline.then_some(UnderlineStyle { thickness: px(1.), color: Some(color), wavy: false }),
        strikethrough: run.strikeout.then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    };
    let shaped = window.text_system().shape_line(text.to_owned().into(), metrics.font_size, &[text_run], None);
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
    let color = hsla(cursor.color);

    if !focused {
        window.paint_quad(outline(cell, color, BorderStyle::Solid));
        return;
    }
    match cursor.shape {
        CursorShape::Beam => {
            window.paint_quad(fill(Bounds::new(cell.origin, size(px(2.), cell.size.height)), color));
        }
        CursorShape::Underline => {
            let y = cell.origin.y + cell.size.height - px(2.);
            window.paint_quad(fill(Bounds::new(point(cell.origin.x, y), size(cell.size.width, px(2.))), color));
        }
        CursorShape::HollowBlock => window.paint_quad(outline(cell, color, BorderStyle::Solid)),
        CursorShape::Block => {
            window.paint_quad(fill(cell, color));
            // Redraw the character under the cursor so it stays readable.
            if let Some((run, text)) = frame.lines.get(cursor.line).and_then(|line| char_at(line, cursor.column)) {
                paint_text(cell.origin, &text, run, hsla(frame.background), metrics, window, cx);
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
