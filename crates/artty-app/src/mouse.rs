//! Mouse events for programs that enable mouse reporting (see
//! `artty_core::mouse`, shared with other apps).

pub use artty_core::mouse::{Action, Button, Wheel};
use artty_core::vt::term::TermMode;

pub fn button(button: gpui::MouseButton) -> Option<Button> {
    match button {
        gpui::MouseButton::Left => Some(Button::Left),
        gpui::MouseButton::Middle => Some(Button::Middle),
        gpui::MouseButton::Right => Some(Button::Right),
        _ => None,
    }
}

fn modifiers(modifiers: &gpui::Modifiers) -> artty_core::mouse::Modifiers {
    artty_core::mouse::Modifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

/// The report for a mouse event at viewport cell (`line`, `column`), or
/// `None` if the program didn't ask for this kind of event.
pub fn report(
    button: Button,
    action: Action,
    line: usize,
    column: usize,
    held: &gpui::Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    artty_core::mouse::report(button, action, line, column, &modifiers(held), mode)
}

/// What turning the wheel `lines` over cell (`line`, `column`) does.
pub fn wheel(
    lines: i32,
    line: usize,
    column: usize,
    held: &gpui::Modifiers,
    mode: TermMode,
) -> Wheel {
    artty_core::mouse::wheel(lines, line, column, &modifiers(held), mode)
}
