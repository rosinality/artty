//! Copy mode (⌘⇧C): the keyboard moves a cursor through the focused pane's
//! screen and history and selects from it, with vi's keys, as tmux's copy
//! mode does. The server's terminal keeps the cursor and selection; this
//! turns keys into its moves.

use artty_core::protocol::{CopyModeOp, CopyMotion};

use super::*;

impl WindowView {
    /// Enter copy mode in the focused pane, or leave it.
    pub(super) fn toggle_copy_mode(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.focused_id() else { return };
        let was = self.copy_mode;
        self.exit_copy_mode(cx);
        if was != Some(pane) {
            self.copy_mode = Some(pane);
            self.send(ClientMessage::CopyMode { pane, op: CopyModeOp::Enter }, cx);
        }
        cx.notify();
    }

    pub(super) fn exit_copy_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.copy_mode.take() {
            self.send(ClientMessage::CopyMode { pane, op: CopyModeOp::Exit }, cx);
            cx.notify();
        }
    }

    /// Whether keys go to copy mode: it's on in the focused pane. (On in
    /// another pane, it ends: focus moved away.)
    pub(super) fn in_copy_mode(&mut self, cx: &mut Context<Self>) -> bool {
        match self.copy_mode {
            Some(pane) if Some(pane) == self.focused_id() => true,
            Some(_) => {
                self.exit_copy_mode(cx);
                false
            },
            None => false,
        }
    }

    fn copy_op(&self, op: CopyModeOp, cx: &App) {
        if let Some(pane) = self.copy_mode {
            self.send(ClientMessage::CopyMode { pane, op }, cx);
        }
    }

    /// Copy the selection, if any, and leave copy mode.
    fn yank(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.copy_mode else { return };
        // Asked before leaving, which clears the selection; the server
        // answers in order.
        let answer = app_state::query(Query::SelectionText { pane }, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Reply::Text(Some(text))) = answer.await {
                let _ = this.update(cx, |this, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    this.show_toast("Copied", cx);
                });
            }
        })
        .detach();
        self.exit_copy_mode(cx);
    }

    /// Keys that aren't text (those come through the input handler, to
    /// [`Self::copy_mode_text`]). Returns whether the key was used.
    pub(super) fn copy_mode_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        use CopyMotion::*;
        let keystroke = &event.keystroke;
        let control = keystroke.modifiers.control;
        let op = match keystroke.key.as_str() {
            "escape" => {
                self.exit_copy_mode(cx);
                return true;
            },
            "c" if control => {
                self.exit_copy_mode(cx);
                return true;
            },
            "enter" => {
                self.yank(cx);
                return true;
            },
            "left" => CopyModeOp::Move(Left),
            "right" => CopyModeOp::Move(Right),
            "up" => CopyModeOp::Move(Up),
            "down" => CopyModeOp::Move(Down),
            "home" => CopyModeOp::Move(LineStart),
            "end" => CopyModeOp::Move(LineEnd),
            "pageup" => CopyModeOp::Move(PageUp),
            "pagedown" => CopyModeOp::Move(PageDown),
            "u" if control => CopyModeOp::Move(HalfPageUp),
            "d" if control => CopyModeOp::Move(HalfPageDown),
            "b" if control => CopyModeOp::Move(PageUp),
            "f" if control => CopyModeOp::Move(PageDown),
            "v" if control => CopyModeOp::Select(2),
            // Nothing else reaches the program while copy mode is on.
            _ if control || keystroke.modifiers.alt => return true,
            _ => return false,
        };
        self.copy_op(op, cx);
        true
    }

    /// Typed keys in copy mode, as in vi.
    pub(super) fn copy_mode_text(&mut self, text: &str, cx: &mut Context<Self>) {
        use CopyMotion::*;
        for key in text.chars() {
            let op = match key {
                'h' => CopyModeOp::Move(Left),
                'j' => CopyModeOp::Move(Down),
                'k' => CopyModeOp::Move(Up),
                'l' => CopyModeOp::Move(Right),
                '0' => CopyModeOp::Move(LineStart),
                '$' => CopyModeOp::Move(LineEnd),
                '^' => CopyModeOp::Move(FirstOccupied),
                'w' => CopyModeOp::Move(WordRight),
                'b' => CopyModeOp::Move(WordLeft),
                'e' => CopyModeOp::Move(WordEnd),
                'W' => CopyModeOp::Move(BigWordRight),
                'B' => CopyModeOp::Move(BigWordLeft),
                'E' => CopyModeOp::Move(BigWordEnd),
                'H' => CopyModeOp::Move(ScreenTop),
                'M' => CopyModeOp::Move(ScreenMiddle),
                'L' => CopyModeOp::Move(ScreenBottom),
                'g' => CopyModeOp::Move(Top),
                'G' => CopyModeOp::Move(Bottom),
                '%' => CopyModeOp::Move(Bracket),
                '{' => CopyModeOp::Move(ParagraphUp),
                '}' => CopyModeOp::Move(ParagraphDown),
                '[' => CopyModeOp::Move(PromptUp),
                ']' => CopyModeOp::Move(PromptDown),
                'v' => CopyModeOp::Select(0),
                'V' => CopyModeOp::Select(1),
                'y' => {
                    self.yank(cx);
                    return;
                },
                'q' => {
                    self.exit_copy_mode(cx);
                    return;
                },
                '/' | '?' => {
                    self.open_search(cx);
                    return;
                },
                _ => continue,
            };
            self.copy_op(op, cx);
        }
    }

    /// A badge at the bottom of the pane in copy mode, with its keys.
    pub(super) fn copy_mode_badge(&self) -> Option<impl IntoElement> {
        let pane = self.copy_mode.filter(|&p| Some(p) == self.focused_id())?;
        let rect = self.rect_of(pane)?;
        let container_right = self.area.x + self.area.width + PADDING;
        let container_bottom = self.area.y + self.area.height + PADDING;
        Some(
            div()
                .absolute()
                .bottom(px(container_bottom - (rect.y + rect.height) + 6.))
                .right(px(container_right - (rect.x + rect.width) + 4.))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(4.))
                .rounded(px(8.))
                .bg(ui(0x1e2430f2))
                .border_1()
                .border_color(ui(0x8fb4ff66))
                .shadow_md()
                .text_size(px(11.5))
                .child(div().text_color(ui(0x8fb4ffff)).child("Copy mode"))
                .child(
                    div()
                        .text_color(ui(0xffffff80))
                        .child("hjkl move · v select · y copy · [ ] prompts · / find · esc"),
                ),
        )
    }
}
