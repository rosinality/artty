//! Translate keystrokes into the bytes a terminal program expects.
//!
//! Plain text is not handled here: printable characters go through the macOS
//! input system (see `InputHandler` in `window_view.rs`) so IME composition works.

use artty_core::vt::term::TermMode;
use gpui::Keystroke;

/// Bytes for keys that aren't plain text, or `None` to let the key through
/// to app shortcuts or the input system.
///
/// With `option_as_meta`, Option sends an ESC prefix (⌥B → ESC b); without it,
/// Option+letter is left to macOS so it can type characters like `å`.
pub fn to_esc(keystroke: &Keystroke, mode: TermMode, option_as_meta: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    if mode.intersects(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC)
        && let Some(bytes) = kitty(keystroke, mode, option_as_meta)
    {
        return Some(bytes);
    }
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    let cursor = |c: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 { format!("\x1b[{n};{param}~") } else { format!("\x1b[{n}~") }.into_bytes()
    };
    let ss3 = |c: char| -> Vec<u8> {
        if param > 1 { format!("\x1b[1;{param}{c}") } else { format!("\x1bO{c}") }.into_bytes()
    };
    let meta_key = m.alt && option_as_meta;
    let meta = |bytes: &[u8]| -> Vec<u8> {
        if meta_key { [b"\x1b", bytes].concat() } else { bytes.to_vec() }
    };

    let bytes = match keystroke.key.as_str() {
        "enter" => meta(b"\r"),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => meta(b"\t"),
        "escape" => meta(b"\x1b"),
        "backspace" if m.control => meta(b"\x08"),
        "backspace" => meta(b"\x7f"),
        "space" if m.control => meta(b"\0"),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => ss3('P'),
        "f2" => ss3('Q'),
        "f3" => ss3('R'),
        "f4" => ss3('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        key if m.control => meta(&[control_byte(key)?]),
        key if meta_key && key.chars().count() == 1 => {
            let key = if m.shift { key.to_uppercase() } else { key.to_owned() };
            meta(key.as_bytes())
        }
        _ => return None,
    };
    Some(bytes)
}

/// The kitty keyboard protocol's `CSI code ; modifiers u` form, for keys the
/// legacy encoding can't tell apart (Shift+Enter, Ctrl+I vs Tab, Escape).
/// `None` means the legacy encoding is still right for this key.
///
/// <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
fn kitty(keystroke: &Keystroke, mode: TermMode, option_as_meta: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let all_keys = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    // Without Option as Meta, Option types characters (å) and isn't a modifier.
    let alt = m.alt && option_as_meta;
    let modifiers = 1 + m.shift as u8 + 2 * alt as u8 + 4 * m.control as u8;
    let csi_u = |code: u32| -> Vec<u8> {
        if modifiers == 1 { format!("\x1b[{code}u") } else { format!("\x1b[{code};{modifiers}u") }.into_bytes()
    };

    let key = keystroke.key.as_str();
    let code = match key {
        "escape" => return Some(csi_u(27)),
        "enter" => 13,
        "tab" => 9,
        "backspace" => 127,
        "space" => 32,
        _ => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            // Report the unshifted key; Shift is in the modifiers.
            c.to_lowercase().next()? as u32
        }
    };
    let text_key = !matches!(key, "enter" | "tab" | "backspace");
    // Unmodified keys keep their legacy bytes, and Shift+letter stays text,
    // unless the program asked for every key as an escape code.
    let only_shift = !m.control && !alt;
    if !all_keys && (modifiers == 1 || (text_key && only_shift)) {
        return None;
    }
    if m.alt && !option_as_meta && text_key && !m.control {
        return None;
    }
    Some(csi_u(code))
}

fn control_byte(key: &str) -> Option<u8> {
    let &[c] = key.as_bytes() else { return None };
    match c {
        b'a'..=b'z' => Some(c - b'a' + 1),
        b'@' | b'2' => Some(0),
        b'[' | b'3' => Some(0x1b),
        b'\\' | b'4' => Some(0x1c),
        b']' | b'5' => Some(0x1d),
        b'^' | b'6' => Some(0x1e),
        b'_' | b'-' | b'7' => Some(0x1f),
        b'?' | b'8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(key: &str) -> Option<Vec<u8>> {
        to_esc(&Keystroke::parse(key).unwrap(), TermMode::empty(), true)
    }

    #[test]
    fn named_keys() {
        assert_eq!(esc("enter").unwrap(), b"\r");
        assert_eq!(esc("backspace").unwrap(), b"\x7f");
        assert_eq!(esc("shift-tab").unwrap(), b"\x1b[Z");
        assert_eq!(esc("up").unwrap(), b"\x1b[A");
        assert_eq!(esc("ctrl-right").unwrap(), b"\x1b[1;5C");
        assert_eq!(esc("pageup").unwrap(), b"\x1b[5~");
        assert_eq!(esc("f5").unwrap(), b"\x1b[15~");
    }

    #[test]
    fn app_cursor_mode() {
        let up = to_esc(&Keystroke::parse("up").unwrap(), TermMode::APP_CURSOR, true);
        assert_eq!(up.unwrap(), b"\x1bOA");
    }

    #[test]
    fn control_and_meta() {
        assert_eq!(esc("ctrl-c").unwrap(), b"\x03");
        assert_eq!(esc("ctrl-[").unwrap(), b"\x1b");
        assert_eq!(esc("alt-b").unwrap(), b"\x1bb");
        assert_eq!(esc("ctrl-alt-a").unwrap(), b"\x1b\x01");
    }

    #[test]
    fn option_without_meta_types_characters() {
        let alt_b = Keystroke::parse("alt-b").unwrap();
        assert_eq!(to_esc(&alt_b, TermMode::empty(), false), None);
        let alt_left = Keystroke::parse("alt-left").unwrap();
        assert_eq!(to_esc(&alt_left, TermMode::empty(), false).unwrap(), b"\x1b[1;3D");
    }

    fn kitty(key: &str, mode: TermMode) -> Option<Vec<u8>> {
        to_esc(&Keystroke::parse(key).unwrap(), mode, true)
    }

    #[test]
    fn kitty_disambiguates() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(kitty("shift-enter", mode).unwrap(), b"\x1b[13;2u");
        assert_eq!(kitty("enter", mode).unwrap(), b"\r");
        assert_eq!(kitty("escape", mode).unwrap(), b"\x1b[27u");
        assert_eq!(kitty("ctrl-i", mode).unwrap(), b"\x1b[105;5u");
        assert_eq!(kitty("tab", mode).unwrap(), b"\t");
        assert_eq!(kitty("ctrl-shift-a", mode).unwrap(), b"\x1b[97;6u");
        assert_eq!(kitty("alt-b", mode).unwrap(), b"\x1b[98;3u");
        assert_eq!(kitty("up", mode).unwrap(), b"\x1b[A");
        // Plain and shifted text still goes through the input system.
        assert_eq!(kitty("a", mode), None);
        assert_eq!(kitty("shift-a", mode), None);
    }

    #[test]
    fn kitty_all_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(kitty("a", mode).unwrap(), b"\x1b[97u");
        assert_eq!(kitty("shift-a", mode).unwrap(), b"\x1b[97;2u");
        assert_eq!(kitty("enter", mode).unwrap(), b"\x1b[13u");
    }

    #[test]
    fn shift_enter_without_kitty_is_enter() {
        assert_eq!(esc("shift-enter").unwrap(), b"\r");
    }

    #[test]
    fn text_and_shortcuts_pass_through() {
        assert_eq!(esc("a"), None);
        assert_eq!(esc("cmd-t"), None);
    }
}
