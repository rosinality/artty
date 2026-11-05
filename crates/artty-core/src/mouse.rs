//! Mouse events for programs that enable mouse reporting (vim, htop, tmux,
//! ...), for any app (the Mac's, a phone's): the reports' bytes, and what
//! the wheel does.

use crate::vt::term::TermMode;

/// Modifiers held with a mouse event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    /// Motion with no button held (any-event tracking).
    None = 3,
    WheelUp = 64,
    WheelDown = 65,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Release,
    Motion,
}

/// The report for a mouse event at viewport cell (`line`, `column`), or
/// `None` if the program didn't ask for this kind of event.
pub fn report(
    button: Button,
    action: Action,
    line: usize,
    column: usize,
    modifiers: &Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let wanted = match action {
        Action::Press | Action::Release => {
            mode.contains(TermMode::MOUSE_REPORT_CLICK)
                || mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
        },
        Action::Motion if button == Button::None => mode.contains(TermMode::MOUSE_MOTION),
        Action::Motion => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
    };
    if !wanted {
        return None;
    }

    let mut code = button as u8;
    if action == Action::Motion {
        code += 32;
    }
    code += 4 * modifiers.shift as u8 + 8 * modifiers.alt as u8 + 16 * modifiers.control as u8;

    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if action == Action::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{suffix}", column + 1, line + 1).into_bytes());
    }

    // Legacy encoding has no release button for wheel events, and reports
    // every release as button 3.
    if action == Action::Release {
        if matches!(button, Button::WheelUp | Button::WheelDown) {
            return None;
        }
        code = (code & !3) | 3;
    }
    let mut bytes = b"\x1b[M".to_vec();
    bytes.push(32 + code);
    for value in [column + 1, line + 1] {
        let value = 32 + value as u32;
        if mode.contains(TermMode::UTF8_MOUSE) {
            let mut buf = [0; 4];
            bytes.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut buf).as_bytes());
        } else if value <= 255 {
            bytes.push(value as u8);
        } else {
            // Out of range for the legacy encoding.
            return None;
        }
    }
    Some(bytes)
}

/// What turning the wheel `lines` (up, if positive) over cell (`line`,
/// `column`) does in a terminal in `mode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wheel {
    /// Bytes for the program: wheel reports, or arrow keys for a
    /// full-screen program that asked for them (alternate scroll).
    Send(Vec<u8>),
    /// Scroll the history.
    History(i32),
}

pub fn wheel(
    lines: i32,
    line: usize,
    column: usize,
    modifiers: &Modifiers,
    mode: TermMode,
) -> Wheel {
    if mode.intersects(TermMode::MOUSE_MODE) {
        let button = if lines > 0 { Button::WheelUp } else { Button::WheelDown };
        let report = report(button, Action::Press, line, column, modifiers, mode);
        return Wheel::Send(report.unwrap_or_default().repeat(lines.unsigned_abs() as usize));
    }
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        // Full-screen programs (less, vim) get arrow keys instead.
        let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA".as_slice(),
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Wheel::Send(key.repeat(lines.unsigned_abs() as usize));
    }
    Wheel::History(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers { control: false, alt: false, shift: false };

    #[test]
    fn sgr_click() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            report(Button::Left, Action::Press, 4, 9, &NONE, mode).unwrap(),
            b"\x1b[<0;10;5M"
        );
        assert_eq!(
            report(Button::Left, Action::Release, 4, 9, &NONE, mode).unwrap(),
            b"\x1b[<0;10;5m"
        );
    }

    #[test]
    fn legacy_click_and_release() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(report(Button::Left, Action::Press, 0, 0, &NONE, mode).unwrap(), b"\x1b[M !!");
        assert_eq!(report(Button::Left, Action::Release, 0, 0, &NONE, mode).unwrap(), b"\x1b[M#!!");
    }

    #[test]
    fn motion_needs_tracking_mode() {
        let click_only = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(report(Button::Left, Action::Motion, 0, 0, &NONE, click_only), None);
        let drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        assert_eq!(
            report(Button::Left, Action::Motion, 1, 2, &NONE, drag).unwrap(),
            b"\x1b[<32;3;2M"
        );
        assert_eq!(report(Button::None, Action::Motion, 1, 2, &NONE, drag), None);
    }

    #[test]
    fn wheel_and_modifiers() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let ctrl = Modifiers { control: true, ..NONE };
        assert_eq!(
            report(Button::WheelDown, Action::Press, 0, 0, &ctrl, mode).unwrap(),
            b"\x1b[<81;1;1M"
        );
    }

    #[test]
    fn the_wheel_goes_where_the_program_wants() {
        let report = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            wheel(2, 0, 0, &NONE, report),
            Wheel::Send(b"\x1b[<64;1;1M\x1b[<64;1;1M".to_vec())
        );
        let less = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(wheel(-1, 0, 0, &NONE, less), Wheel::Send(b"\x1b[B".to_vec()));
        assert_eq!(wheel(3, 0, 0, &NONE, TermMode::empty()), Wheel::History(3));
    }
}
