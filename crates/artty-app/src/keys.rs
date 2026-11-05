//! Translate keystrokes into the bytes a terminal program expects (see
//! `artty_core::keys`, shared with other apps).
//!
//! Plain text is not handled here: printable characters go through the macOS
//! input system (see `InputHandler` in `window_view.rs`) so IME composition works.

use artty_core::keys::Key;
use artty_core::vt::term::TermMode;
use gpui::Keystroke;

/// Bytes for keys that aren't plain text, or `None` to let the key through
/// to app shortcuts or the input system.
pub fn to_esc(keystroke: &Keystroke, mode: TermMode, option_as_meta: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let key = Key {
        key: &keystroke.key,
        shift: m.shift,
        alt: m.alt,
        control: m.control,
        platform: m.platform,
    };
    artty_core::keys::to_esc(&key, mode, option_as_meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystrokes_as_keys() {
        let esc = |key| to_esc(&Keystroke::parse(key).unwrap(), TermMode::empty(), true);
        assert_eq!(esc("ctrl-right").unwrap(), b"\x1b[1;5C");
        assert_eq!(esc("alt-b").unwrap(), b"\x1bb");
        assert_eq!(esc("cmd-t"), None);
    }
}
