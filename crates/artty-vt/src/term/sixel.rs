//! Sixel images (artty): `ESC P <P1>;<P2>;<P3> q <data> ESC \`, decoded to
//! RGBA and placed at the cursor like any other image.
//!
//! The data: `"Pan;Pad;Ph;Pv` raster attributes (the size, Ph × Pv), `#n`
//! selecting color register n, `#n;2;r;g;b` (percentages) or `#n;1;h;l;s`
//! defining it, `!count c` repeating sixel `c`, `$` returning to the left
//! edge and `-` moving down six rows. A sixel (`?` to `~`, minus 0x3F) gives
//! six vertical pixels, the lowest bit topmost.

use super::Term;
use super::cell::{Image, ImageFormat};
use crate::event::EventListener;

/// The largest image decoded, in pixels each way.
const MAX_SIDE: usize = 4096;

/// VT340's default color registers 0–15 (the rest start black).
const DEFAULT_PALETTE: [[u8; 3]; 16] = [
    [0, 0, 0],
    [51, 51, 204],
    [204, 36, 36],
    [51, 204, 51],
    [204, 51, 204],
    [51, 204, 204],
    [204, 204, 51],
    [120, 120, 120],
    [69, 69, 69],
    [87, 87, 153],
    [153, 69, 69],
    [87, 153, 87],
    [153, 87, 153],
    [87, 153, 153],
    [153, 153, 87],
    [204, 204, 204],
];

/// A decoded image: RGBA rows.
#[derive(Debug, PartialEq)]
pub(crate) struct Decoded {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// HLS (hue 0–360, lightness and saturation 0–100) as sixel defines it:
/// hue 0 is blue, not red.
fn hls(h: u16, l: u16, s: u16) -> [u8; 3] {
    let (h, l, s) = (((f32::from(h) + 240.) % 360.) / 360., f32::from(l.min(100)) / 100., f32::from(s.min(100)) / 100.);
    if s == 0. {
        let v = (l * 255.).round() as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.);
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        (v * 255.).round() as u8
    };
    [channel(h + 1. / 3.), channel(h), channel(h - 1. / 3.)]
}

/// Decode sixel data. `transparent`: unset pixels show through (P2 = 1);
/// otherwise they take color register 0, as on a VT340.
pub(crate) fn decode(data: &[u8], transparent: bool) -> Option<Decoded> {
    let mut palette: Vec<[u8; 3]> = DEFAULT_PALETTE.to_vec();
    palette.resize(256, [0, 0, 0]);
    // Pixels as palette entries plus one (0: unset), grown as drawn.
    let mut pixels: Vec<u16> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);
    let (mut x, mut y) = (0usize, 0usize);
    let mut color = 0u16;
    let mut i = 0;

    let number = |i: &mut usize| -> Option<u16> {
        let start = *i;
        while *i < data.len() && data[*i].is_ascii_digit() {
            *i += 1;
        }
        std::str::from_utf8(&data[start..*i]).ok()?.parse::<u32>().ok().map(|n| n.min(u32::from(u16::MAX)) as u16)
    };
    let numbers = |i: &mut usize| -> Vec<u16> {
        let mut values = vec![number(i).unwrap_or(0)];
        while *i < data.len() && data[*i] == b';' {
            *i += 1;
            values.push(number(i).unwrap_or(0));
        }
        values
    };

    // Grow the canvas to `w` × `h` (keeping what's drawn).
    let grow = |pixels: &mut Vec<u16>, width: &mut usize, height: &mut usize, w: usize, h: usize| {
        let (w, h) = (w.min(MAX_SIDE).max(*width), h.min(MAX_SIDE).max(*height));
        if (w, h) == (*width, *height) {
            return;
        }
        let mut grown = vec![0u16; w * h];
        for row in 0..*height {
            grown[row * w..row * w + *width].copy_from_slice(&pixels[row * *width..(row + 1) * *width]);
        }
        *pixels = grown;
        (*width, *height) = (w, h);
    };

    while i < data.len() {
        match data[i] {
            b'"' => {
                i += 1;
                let attributes = numbers(&mut i);
                if let (Some(&w), Some(&h)) = (attributes.get(2), attributes.get(3)) {
                    grow(&mut pixels, &mut width, &mut height, usize::from(w), usize::from(h));
                }
            },
            b'#' => {
                i += 1;
                let values = numbers(&mut i);
                color = values[0] % palette.len() as u16;
                match values.get(1..5) {
                    Some(&[2, r, g, b]) => {
                        let scale = |v: u16| (f32::from(v.min(100)) * 2.55).round() as u8;
                        palette[usize::from(color)] = [scale(r), scale(g), scale(b)];
                    },
                    Some(&[1, h, l, s]) => palette[usize::from(color)] = hls(h, l, s),
                    _ => {},
                }
            },
            b'$' => {
                x = 0;
                i += 1;
            },
            b'-' => {
                x = 0;
                y += 6;
                i += 1;
            },
            byte @ (b'!' | 0x3F..=0x7E) => {
                let (count, sixel) = if byte == b'!' {
                    i += 1;
                    let count = usize::from(number(&mut i).unwrap_or(1).max(1));
                    match data.get(i) {
                        Some(&c @ 0x3F..=0x7E) => (count, c),
                        _ => continue,
                    }
                } else {
                    (1, byte)
                };
                i += 1;
                let bits = sixel - 0x3F;
                let end = (x + count).min(MAX_SIDE);
                if bits != 0 && y < MAX_SIDE {
                    grow(&mut pixels, &mut width, &mut height, end, (y + 6).min(MAX_SIDE));
                    for bit in 0..6 {
                        let row = y + bit;
                        if bits & (1 << bit) != 0 && row < height {
                            pixels[row * width + x..row * width + end].fill(color + 1);
                        }
                    }
                }
                x = end;
            },
            _ => i += 1,
        }
    }
    if width == 0 || height == 0 {
        return None;
    }
    let background = [palette[0][0], palette[0][1], palette[0][2], if transparent { 0 } else { 255 }];
    let rgba = pixels
        .iter()
        .flat_map(|&p| match p {
            0 => background,
            p => {
                let [r, g, b] = palette[usize::from(p - 1)];
                [r, g, b, 255]
            },
        })
        .collect();
    Some(Decoded { width, height, rgba })
}

impl<T: EventListener> Term<T> {
    /// A sixel image (`params` are P1–P3): decoded and shown at the cursor.
    pub(crate) fn sixel(&mut self, params: &[u16], data: &[u8]) {
        let transparent = params.get(1) == Some(&1);
        let Some(image) = decode(data, transparent) else { return };
        let cell = (f32::from(self.cell_size.0), f32::from(self.cell_size.1));
        let size = (image.width as f32 / cell.0, image.height as f32 / cell.1);
        let image = Image::new(ImageFormat::Rgba, image.rgba.into(), (image.width as u32, image.height as u32), size, 0);
        self.place_image(image, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_colors_repeats_and_bands() {
        // Register 1 red, register 2 green (RGB percentages). Row band 1: two
        // red columns, all six pixels (`~`). Band 2, after `-`: one green
        // pixel at the top (`@`) in the first column.
        let image = decode(b"#1;2;100;0;0#2;2;0;100;0#1!2~-#2@", true).unwrap();
        assert_eq!((image.width, image.height), (2, 12));
        let pixel = |x: usize, y: usize| &image.rgba[(y * image.width + x) * 4..][..4];
        assert_eq!(pixel(1, 5), [255, 0, 0, 255]);
        assert_eq!(pixel(0, 6), [0, 255, 0, 255]);
        // Unset pixels are see-through when asked.
        assert_eq!(pixel(1, 6)[3], 0);
        // Otherwise they take register 0 (black on a VT340).
        let opaque = decode(b"#1;2;100;0;0#1~-#1@", false).unwrap();
        assert_eq!(&opaque.rgba[(6 * opaque.width) * 4..][..4], [255, 0, 0, 255]);
        assert_eq!(&opaque.rgba[(7 * opaque.width) * 4..][..4], [0, 0, 0, 255]);
    }

    #[test]
    fn raster_attributes_size_the_image() {
        let image = decode(b"\"1;1;5;3#0~", false).unwrap();
        assert_eq!((image.width, image.height), (5, 6));
    }

    #[test]
    fn hls_hue_starts_at_blue() {
        assert_eq!(hls(0, 50, 100), [0, 0, 255]);
        assert_eq!(hls(120, 50, 100), [255, 0, 0]);
        assert_eq!(hls(240, 50, 100), [0, 255, 0]);
    }
}
