//! Translate keystrokes into the bytes a terminal program expects.
//!
//! Plain text is not handled here: printable characters go through the macOS
//! input system (see `InputHandler` in `workspace.rs`) so IME composition works.

use artty_core::vt::term::TermMode;
use gpui::Keystroke;

/// Bytes for keys that aren't plain text, or `None` to let the key through
/// to app shortcuts or the input system.
pub fn to_esc(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
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
    let meta = |bytes: &[u8]| -> Vec<u8> {
        if m.alt { [b"\x1b", bytes].concat() } else { bytes.to_vec() }
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
        // Option acts as Meta: ⌥B sends ESC b, as shells expect.
        key if m.alt && key.chars().count() == 1 => {
            let key = if m.shift { key.to_uppercase() } else { key.to_owned() };
            meta(key.as_bytes())
        }
        _ => return None,
    };
    Some(bytes)
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
        to_esc(&Keystroke::parse(key).unwrap(), TermMode::empty())
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
        let up = to_esc(&Keystroke::parse("up").unwrap(), TermMode::APP_CURSOR);
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
    fn text_and_shortcuts_pass_through() {
        assert_eq!(esc("a"), None);
        assert_eq!(esc("cmd-t"), None);
    }
}
