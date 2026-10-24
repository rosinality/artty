//! Images in the grid (artty): iTerm2's inline images (`OSC 1337;File=`),
//! and placing any image at the cursor.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;

use super::Term;
use super::cell::{Image, ImageFormat, ImageTile};
use crate::ansi::Handler;
use crate::event::EventListener;
use crate::grid::Dimensions;
use crate::index::Column;

/// Larger images (as sent, base64) are refused.
const MAX_ENCODED: usize = 48 << 20;
/// The most pixels an image may have (about 8K × 8K).
const MAX_PIXELS: u64 = 64 << 20;

/// A requested width or height (iTerm2's `width=` / `height=`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Dimension {
    Auto,
    Cells(f32),
    Pixels(f32),
    Percent(f32),
}

impl Dimension {
    fn parse(value: &str) -> Self {
        let value = value.trim();
        let number = |s: &str| s.trim().parse::<f32>().ok().filter(|n| n.is_finite() && *n > 0.);
        if value.eq_ignore_ascii_case("auto") {
            Dimension::Auto
        } else if let Some(n) = value.strip_suffix("px").and_then(number) {
            Dimension::Pixels(n)
        } else if let Some(n) = value.strip_suffix('%').and_then(number) {
            Dimension::Percent(n)
        } else {
            number(value).map_or(Dimension::Auto, Dimension::Cells)
        }
    }

    /// In pixels, given the cell size and the terminal's size (both pixels).
    fn pixels(self, cell: f32, whole: f32) -> Option<f32> {
        match self {
            Dimension::Auto => None,
            Dimension::Cells(n) => Some(n * cell),
            Dimension::Pixels(n) => Some(n),
            Dimension::Percent(n) => Some(n / 100. * whole),
        }
    }
}

/// How big to show an image, in pixels: as requested, keeping its aspect
/// ratio unless told not to, and no wider than the terminal when automatic.
fn display_size(
    natural: (f32, f32),
    width: Dimension,
    height: Dimension,
    keep_aspect: bool,
    cell: (f32, f32),
    terminal: (f32, f32),
) -> (f32, f32) {
    let (w, h) = natural;
    let requested = (width.pixels(cell.0, terminal.0), height.pixels(cell.1, terminal.1));
    let (mut w, mut h) = match requested {
        (None, None) => (w, h),
        (Some(rw), None) => (rw, if keep_aspect { h * rw / w } else { h }),
        (None, Some(rh)) => (if keep_aspect { w * rh / h } else { w }, rh),
        (Some(rw), Some(rh)) if keep_aspect => {
            let scale = (rw / w).min(rh / h);
            (w * scale, h * scale)
        },
        (Some(rw), Some(rh)) => (rw, rh),
    };
    // Never wider than the terminal, unless asked to be.
    if requested.0.is_none() && w > terminal.0 {
        let scale = terminal.0 / w;
        w *= scale;
        if keep_aspect {
            h *= scale;
        }
    }
    (w.max(1.), h.max(1.))
}

impl<T: EventListener> Term<T> {
    /// The cell size in pixels, for sizing images (artty).
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        if width > 0 && height > 0 {
            self.cell_size = (width, height);
        }
    }

    /// `OSC 1337` from iTerm2: inline images in one piece (`File=`) or in
    /// parts (`MultipartFile=`, `FilePart=`, `FileEnd`).
    pub(crate) fn iterm2(&mut self, params: &[&[u8]]) {
        let joined = params.join(&b";"[..]);
        if let Some(rest) = joined.strip_prefix(b"File=") {
            let split = rest.iter().position(|&b| b == b':').unwrap_or(rest.len());
            let args = String::from_utf8_lossy(&rest[..split]).into_owned();
            let payload = rest.get(split + 1..).unwrap_or_default();
            self.inline_image(&args, payload);
        } else if let Some(args) = joined.strip_prefix(b"MultipartFile=") {
            self.pending_image = Some((String::from_utf8_lossy(args).into_owned(), Vec::new()));
        } else if let Some(part) = joined.strip_prefix(b"FilePart=") {
            if let Some((_, data)) = self.pending_image.as_mut() {
                if data.len() + part.len() > MAX_ENCODED {
                    log::warn!("inline image too large; dropped");
                    self.pending_image = None;
                } else {
                    data.extend_from_slice(part);
                }
            }
        } else if joined.starts_with(b"FileEnd")
            && let Some((args, data)) = self.pending_image.take()
        {
            self.inline_image(&args, &data);
        }
    }

    fn inline_image(&mut self, args: &str, payload: &[u8]) {
        let mut inline = false;
        let (mut width, mut height, mut keep_aspect) = (Dimension::Auto, Dimension::Auto, true);
        for (key, value) in args.split(';').filter_map(|pair| pair.split_once('=')) {
            match key.trim() {
                "inline" => inline = value.trim() == "1",
                "width" => width = Dimension::parse(value),
                "height" => height = Dimension::parse(value),
                "preserveAspectRatio" => keep_aspect = value.trim() != "0",
                _ => {},
            }
        }
        // Not inline means "download": nothing to show.
        if !inline || payload.len() > MAX_ENCODED {
            return;
        }
        let payload: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
        let Ok(data) = Base64.decode(&payload) else {
            log::warn!("inline image: bad base64");
            return;
        };
        let Ok(size) = imagesize::blob_size(&data) else {
            log::warn!("inline image: not an image format we know");
            return;
        };
        let (pixel_w, pixel_h) = (size.width as u32, size.height as u32);
        if pixel_w == 0 || pixel_h == 0 || u64::from(pixel_w) * u64::from(pixel_h) > MAX_PIXELS {
            return;
        }
        let cell = (f32::from(self.cell_size.0), f32::from(self.cell_size.1));
        let terminal = (cell.0 * self.columns() as f32, cell.1 * self.screen_lines() as f32);
        let (w, h) = display_size((pixel_w as f32, pixel_h as f32), width, height, keep_aspect, cell, terminal);
        let image = Image::new(ImageFormat::Encoded, data.into(), (pixel_w, pixel_h), (w / cell.0, h / cell.1), 0);
        self.place_image(image, true);
    }

    /// Show `image` with its top left at the cursor, scrolling as text
    /// would. Afterwards the cursor is on the line below it (`move_cursor`)
    /// or back where it was.
    pub(crate) fn place_image(&mut self, image: Arc<Image>, move_cursor: bool) {
        let columns = (image.columns.ceil() as usize).max(1);
        let rows = (image.rows.ceil() as usize).max(1);
        if self.grid.cursor.input_needs_wrap {
            self.carriage_return();
            self.linefeed();
        }
        let start = self.grid.cursor.point;
        let mut scrolled: i32 = 0;
        for row in 0..rows {
            let line = self.grid.cursor.point.line;
            for column in 0..columns {
                let at = start.column.0 + column;
                if at >= self.columns() {
                    break;
                }
                let tile = ImageTile { image: image.clone(), column: column as u16, row: row as u16 };
                self.grid[line][Column(at)].set_image(tile);
            }
            if row + 1 < rows {
                let before = self.grid.cursor.point.line;
                self.linefeed();
                if self.grid.cursor.point.line == before {
                    scrolled += 1;
                }
            }
        }
        if move_cursor {
            self.carriage_return();
            self.linefeed();
        } else {
            // Where it was, as far as scrolling moved that line.
            self.grid.cursor.point.line = (start.line - scrolled).max(crate::index::Line(0));
            self.grid.cursor.point.column = start.column;
        }
        self.mark_fully_damaged();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        let cell = (10., 20.);
        let term = (800., 400.);
        let auto = Dimension::Auto;
        // Natural size, or scaled down to the terminal's width.
        assert_eq!(display_size((100., 50.), auto, auto, true, cell, term), (100., 50.));
        assert_eq!(display_size((1600., 400.), auto, auto, true, cell, term), (800., 200.));
        // Cells, pixels and percentages, keeping the aspect ratio.
        assert_eq!(display_size((100., 50.), Dimension::parse("20"), auto, true, cell, term), (200., 100.));
        assert_eq!(display_size((100., 50.), auto, Dimension::parse("10px"), true, cell, term), (20., 10.));
        assert_eq!(display_size((100., 50.), Dimension::parse("50%"), auto, true, cell, term), (400., 200.));
        // Both given: fit inside, unless told not to keep the ratio.
        assert_eq!(display_size((100., 50.), Dimension::parse("20"), Dimension::parse("2"), true, cell, term), (80., 40.));
        assert_eq!(display_size((100., 50.), Dimension::parse("20"), Dimension::parse("2"), false, cell, term), (200., 40.));
    }
}
