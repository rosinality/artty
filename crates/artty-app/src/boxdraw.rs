//! Draw box-drawing and block-element characters as shapes instead of glyphs.
//!
//! Font glyphs for these rarely fill the cell exactly, which leaves gaps
//! between lines and boxes once line spacing is added. TUIs (including Claude
//! Code's input box) rely on them joining up, so terminals draw them.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, fill, point, px, size};

#[derive(Clone, Copy, PartialEq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

/// Arms of a box character: up, right, down, left.
type Arms = [Weight; 4];

fn arms(c: char) -> Option<Arms> {
    use Weight::{Double as D, Heavy as H, Light as L, None as N};
    Some(match c {
        '─' => [N, L, N, L],
        '│' => [L, N, L, N],
        '┌' => [N, L, L, N],
        '┐' => [N, N, L, L],
        '└' => [L, L, N, N],
        '┘' => [L, N, N, L],
        '├' => [L, L, L, N],
        '┤' => [L, N, L, L],
        '┬' => [N, L, L, L],
        '┴' => [L, L, N, L],
        '┼' => [L, L, L, L],
        '━' => [N, H, N, H],
        '┃' => [H, N, H, N],
        '┏' => [N, H, H, N],
        '┓' => [N, N, H, H],
        '┗' => [H, H, N, N],
        '┛' => [H, N, N, H],
        '┣' => [H, H, H, N],
        '┫' => [H, N, H, H],
        '┳' => [N, H, H, H],
        '┻' => [H, H, N, H],
        '╋' => [H, H, H, H],
        '╴' => [N, N, N, L],
        '╵' => [L, N, N, N],
        '╶' => [N, L, N, N],
        '╷' => [N, N, L, N],
        '╸' => [N, N, N, H],
        '╹' => [H, N, N, N],
        '╺' => [N, H, N, N],
        '╻' => [N, N, H, N],
        '═' => [N, D, N, D],
        '║' => [D, N, D, N],
        '╔' => [N, D, D, N],
        '╗' => [N, N, D, D],
        '╚' => [D, D, N, N],
        '╝' => [D, N, N, D],
        '╠' => [D, D, D, N],
        '╣' => [D, N, D, D],
        '╦' => [N, D, D, D],
        '╩' => [D, D, N, D],
        '╬' => [D, D, D, D],
        _ => return None,
    })
}

/// Paint `c` into `cell` if it is a character drawn here. Returns whether it was.
pub fn paint(c: char, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) -> bool {
    if let Some(arms) = arms(c) {
        paint_lines(arms, cell, color, window);
        true
    } else if matches!(c, '╭' | '╮' | '╯' | '╰') {
        paint_rounded(c, cell, color, window);
        true
    } else {
        paint_block(c, cell, color, window)
    }
}

fn light_width(cell: Bounds<Pixels>) -> Pixels {
    px((f32::from(cell.size.width) / 8.).round().max(1.))
}

fn paint_lines(arms: Arms, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let light = light_width(cell);
    let (left, top) = (cell.origin.x, cell.origin.y);
    let (right, bottom) = (left + cell.size.width, top + cell.size.height);
    let center =
        point(left + (cell.size.width / 2.).floor(), top + (cell.size.height / 2.).floor());
    // The thickest arm crossing the center, so perpendicular arms meet it fully.
    let max = |a: Weight, b: Weight| {
        [a, b].into_iter().map(|w| width_of(w, light)).fold(px(0.), Pixels::max)
    };
    let vertical = max(arms[0], arms[2]);
    let horizontal = max(arms[1], arms[3]);

    for (index, weight) in arms.into_iter().enumerate() {
        if weight == Weight::None {
            continue;
        }
        let w = width_of(weight, light);
        let offsets: &[f32] = if weight == Weight::Double { &[-1., 1.] } else { &[0.] };
        for &offset in offsets {
            let shift = light * offset;
            let bounds = match index {
                // Up / down arms: vertical strokes reaching past the center.
                0 | 2 => {
                    let x =
                        center.x + shift - (if weight == Weight::Double { light } else { w }) / 2.;
                    let stroke = if weight == Weight::Double { light } else { w };
                    let (y0, y1) = if index == 0 {
                        (top, center.y + horizontal / 2.)
                    } else {
                        (center.y - horizontal / 2., bottom)
                    };
                    Bounds::new(point(x, y0), size(stroke, y1 - y0))
                },
                // Right / left arms: horizontal strokes.
                _ => {
                    let y =
                        center.y + shift - (if weight == Weight::Double { light } else { w }) / 2.;
                    let stroke = if weight == Weight::Double { light } else { w };
                    let (x0, x1) = if index == 1 {
                        (center.x - vertical / 2., right)
                    } else {
                        (left, center.x + vertical / 2.)
                    };
                    Bounds::new(point(x0, y), size(x1 - x0, stroke))
                },
            };
            window.paint_quad(fill(bounds, color));
        }
    }
}

fn width_of(weight: Weight, light: Pixels) -> Pixels {
    match weight {
        Weight::None => px(0.),
        Weight::Light => light,
        Weight::Heavy => light * 2.,
        // Two light strokes with a light-width gap.
        Weight::Double => light * 3.,
    }
}

fn paint_rounded(c: char, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let light = light_width(cell);
    let (left, top) = (cell.origin.x, cell.origin.y);
    let (right, bottom) = (left + cell.size.width, top + cell.size.height);
    // Stroke centers, matching the straight lines drawn by `paint_lines`.
    let cx = left + (cell.size.width / 2.).floor();
    let cy = top + (cell.size.height / 2.).floor();
    let (vertical_end, horizontal_end) = match c {
        '╭' => (bottom, right),
        '╮' => (bottom, left),
        '╯' => (top, left),
        _ => (top, right), // '╰'
    };
    let radius = (cell.size.width / 2.).min(cell.size.height / 2.);
    let towards = |from: Pixels, to: Pixels| if to > from { from + radius } else { from - radius };

    let mut path = PathBuilder::stroke(light);
    path.move_to(point(cx, vertical_end));
    path.line_to(point(cx, towards(cy, vertical_end)));
    path.curve_to(point(towards(cx, horizontal_end), cy), point(cx, cy));
    path.line_to(point(horizontal_end, cy));
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_block(c: char, cell: Bounds<Pixels>, color: Hsla, window: &mut Window) -> bool {
    let (w, h) = (cell.size.width, cell.size.height);
    let origin = cell.origin;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
        Bounds::new(point(origin.x + w * x0, origin.y + h * y0), size(w * (x1 - x0), h * (y1 - y0)))
    };
    let eighths = |n: u32| n as f32 / 8.;

    let mut paint = |bounds: Bounds<Pixels>, alpha: f32| {
        window.paint_quad(fill(bounds, Hsla { a: color.a * alpha, ..color }));
    };
    match c {
        '█' => paint(rect(0., 0., 1., 1.), 1.),
        '▀' => paint(rect(0., 0., 1., 0.5), 1.),
        '▔' => paint(rect(0., 0., 1., eighths(1)), 1.),
        '▕' => paint(rect(eighths(7), 0., 1., 1.), 1.),
        '▐' => paint(rect(0.5, 0., 1., 1.), 1.),
        '░' => paint(rect(0., 0., 1., 1.), 0.25),
        '▒' => paint(rect(0., 0., 1., 1.), 0.5),
        '▓' => paint(rect(0., 0., 1., 1.), 0.75),
        // Lower eighths: ▁▂▃▄▅▆▇
        '\u{2581}'..='\u{2587}' => {
            let n = c as u32 - 0x2580;
            paint(rect(0., 1. - eighths(n), 1., 1.), 1.);
        },
        // Left eighths: ▉▊▋▌▍▎▏ (7/8 down to 1/8)
        '\u{2589}'..='\u{258f}' => {
            let n = 8 - (c as u32 - 0x2588);
            paint(rect(0., 0., eighths(n), 1.), 1.);
        },
        // Quadrants.
        '▖' | '▗' | '▘' | '▝' | '▙' | '▚' | '▛' | '▜' | '▞' | '▟' => {
            // Bits: upper-left, upper-right, lower-left, lower-right.
            let quadrants: [bool; 4] = match c {
                '▖' => [false, false, true, false],
                '▗' => [false, false, false, true],
                '▘' => [true, false, false, false],
                '▝' => [false, true, false, false],
                '▙' => [true, false, true, true],
                '▚' => [true, false, false, true],
                '▛' => [true, true, true, false],
                '▜' => [true, true, false, true],
                '▞' => [false, true, true, false],
                _ => [false, true, true, true], // '▟'
            };
            for (i, on) in quadrants.into_iter().enumerate() {
                if on {
                    let (x, y) = ((i % 2) as f32 * 0.5, (i / 2) as f32 * 0.5);
                    paint(rect(x, y, x + 0.5, y + 0.5), 1.);
                }
            }
        },
        _ => return false,
    }
    true
}
