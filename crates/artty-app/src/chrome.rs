//! The app's own colors (sidebar, overlays, Settings), for a dark theme or a
//! light one. They're written for dark — light ink (`0xffffff` at some
//! alpha) over the theme's background, dark fields, dark panels — and
//! [`ui`] turns them into their light counterparts while the theme's
//! background is bright: dark ink, light fields and panels, accents dark
//! enough to read on white. macOS's own look (the window's blur, its
//! controls) follows too.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::Rgba;

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Whether the app's colors are the light ones now.
pub fn light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// Whether a theme with this background is a light one.
pub fn is_light_background(rgb: artty_core::vt::ansi::Rgb) -> bool {
    0.2126 * f32::from(rgb.r) + 0.7152 * f32::from(rgb.g) + 0.0722 * f32::from(rgb.b) > 140.
}

/// Follow the theme with this background (on the main thread: it sets
/// macOS's look for the app too).
pub fn follow(background: artty_core::vt::ansi::Rgb) {
    let light = is_light_background(background);
    if LIGHT.swap(light, Ordering::Relaxed) != light {
        set_appearance(light);
    }
}

/// macOS's light or dark look for every window of the app.
fn set_appearance(light: bool) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    let name =
        NSString::from_str(if light { "NSAppearanceNameAqua" } else { "NSAppearanceNameDarkAqua" });
    // SAFETY: plain AppKit calls on the main thread, with the documented
    // appearance names.
    unsafe {
        let appearance: Option<Retained<AnyObject>> =
            msg_send![class!(NSAppearance), appearanceNamed: &*name];
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, setAppearance: appearance.as_deref()];
        }
    }
}

/// An app color written for dark (`0xRRGGBBAA`), as it should be now.
pub fn ui(color: u32) -> Rgba {
    gpui::rgba(if light() { light_counterpart(color) } else { color })
}

fn light_counterpart(color: u32) -> u32 {
    let (rgb, alpha) = (color >> 8, color & 0xff);
    let with = |rgb: u32, alpha: u32| rgb << 8 | alpha.min(0xff);
    match rgb {
        // Ink: dark, a bit more of it (white on dark reads at less).
        0xffffff => with(0x1d2026, alpha + alpha / 8),
        // Shading (fields, scrims): light.
        0x000000 => with(0xffffff, if alpha == 0 { 0 } else { alpha * 2 }),
        // Panels: light.
        0x262a33 => with(0xf6f7f9, alpha),
        0x1e2430 => with(0xf3f4f7, alpha),
        0x161a22 => with(0xf4f5f8, alpha),
        0x2a3140 => with(0xe7eaf0, alpha),
        0x22304a => with(0xdbe6fb, alpha),
        // The selection blue, and its hover: pale, under dark ink.
        0x3478f6 => with(0xc6dafd, alpha),
        0x4a88ff => with(0xb3cdfc, alpha),
        // Accents: darker, to read on white.
        0x8fb4ff => with(0x2a63c9, alpha),
        0x7aa2f7 => with(0x2f5fd0, alpha),
        0xf7768e => with(0xc0304d, alpha),
        0xe0af68 => with(0x9a6400, alpha),
        0x9ece6a => with(0x3a7d2a, alpha),
        _ => color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_as_written_light_turned_over() {
        // Dark: as written.
        LIGHT.store(false, Ordering::Relaxed);
        assert_eq!(ui(0xffffff80), gpui::rgba(0xffffff80));
        // Light: ink dark, fields light, accents darker, the rest as written.
        assert_eq!(light_counterpart(0xffffff80), 0x1d202690);
        assert_eq!(light_counterpart(0x0000004d), 0xffffff9a);
        assert_eq!(light_counterpart(0x00000000), 0xffffff00);
        assert_eq!(light_counterpart(0x8fb4ffff), 0x2a63c9ff);
        assert_eq!(light_counterpart(0x123456ff), 0x123456ff);
        assert!(is_light_background(artty_core::vt::ansi::Rgb { r: 250, g: 250, b: 250 }));
        assert!(!is_light_background(artty_core::vt::ansi::Rgb { r: 33, g: 37, b: 43 }));
    }
}
