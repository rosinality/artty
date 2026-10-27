//! Commands on the focused pane and its tab: jumping between shell prompts,
//! copying a command's output, resizing, swapping and moving panes, typing
//! into every pane at once, reopening a closed tab, dropping files, and
//! pasting with a check for several lines.

use artty_core::layout::{Axis, MIN_RATIO};

use super::*;

/// How far one keyboard resize moves a divider, as a share of its split.
const RESIZE_STEP: f32 = 0.05;
/// How long a toast stays.
pub(super) const TOAST_TIME: Duration = Duration::from_millis(2500);

impl WindowView {
    pub(super) fn jump_to_prompt(&mut self, older: bool, cx: &mut Context<Self>) {
        if let Some(pane) = self.focused_id() {
            self.send(ClientMessage::JumpToPrompt { pane, older }, cx);
        }
    }

    /// Copy the output of the command in view (or the last one).
    pub(super) fn copy_command_output(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.focused_id() else { return };
        let answer = app_state::query(Query::CommandOutput { pane }, cx);
        cx.spawn(async move |this, cx| {
            let reply = answer.await;
            let _ = this.update(cx, |this, cx| {
                let message = match reply {
                    Ok(Reply::Output(Some((command, output)))) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(output));
                        match command {
                            Some(command) => format!("Copied the output of {command}"),
                            None => "Copied the command's output".to_owned(),
                        }
                    },
                    _ => "No command output yet (needs shell integration)".to_owned(),
                };
                this.show_toast(message, cx);
            });
        })
        .detach();
    }

    /// A short message over the terminal, for a moment.
    pub(super) fn show_toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some((message.into(), Instant::now()));
        cx.notify();
    }

    pub(super) fn toast_overlay(&self) -> Option<impl IntoElement> {
        let (message, _) = self.toast.as_ref()?;
        Some(
            div().absolute().bottom(px(16.)).left_0().right_0().flex().justify_center().child(
                div()
                    .px_3()
                    .py(px(5.))
                    .rounded(px(8.))
                    .bg(rgba(0x1e2430f2))
                    .border_1()
                    .border_color(rgba(0xffffff24))
                    .shadow_md()
                    .text_size(px(12.))
                    .text_color(rgba(0xffffffd0))
                    .child(message.clone()),
            ),
        )
    }

    /// Move the divider beside the focused pane toward `direction`.
    pub(super) fn resize_pane(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let (Some(window), Some(tab)) = (self.window, self.tabs.get(self.active)) else { return };
        if tab.zoomed {
            return;
        }
        let (axis, step) = match direction {
            Direction::Left => (Axis::Horizontal, -RESIZE_STEP),
            Direction::Right => (Axis::Horizontal, RESIZE_STEP),
            Direction::Up => (Axis::Vertical, -RESIZE_STEP),
            Direction::Down => (Axis::Vertical, RESIZE_STEP),
        };
        if let Some((path, ratio)) = tab.root.enclosing_split(tab.focused, axis) {
            let ratio = (ratio + step).clamp(MIN_RATIO, 1. - MIN_RATIO);
            self.send(ClientMessage::SetRatio { window, path, ratio }, cx);
        }
    }

    /// The focused pane trades places with its neighbor in `direction`.
    pub(super) fn swap_pane(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(from) = self.focused_id() else { return };
        if let Some(to) = layout::neighbor(&self.pane_rects, from, direction) {
            self.send(ClientMessage::SwapPanes { a: from, b: to }, cx);
        }
    }

    pub(super) fn equalize_panes(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.window {
            self.send(ClientMessage::EqualizePanes { window }, cx);
        }
    }

    pub(super) fn pane_to_new_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.focused_id().filter(|_| self.split_tab()) {
            self.send(ClientMessage::PaneToNewTab { pane }, cx);
        }
    }

    /// Typing into every pane of the tab, on or off.
    pub(super) fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        let Some(window) = self.window else { return };
        if !self.split_tab() {
            self.show_toast("Typing into all panes needs a split tab", cx);
            return;
        }
        let on = self.tabs.get(self.active).is_some_and(|t| !t.broadcast);
        self.send(ClientMessage::ToggleBroadcast { window }, cx);
        self.show_toast(
            if on { "Typing goes to every pane of this tab" } else { "Typing goes to one pane" },
            cx,
        );
    }

    fn split_tab(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|t| matches!(t.root, layout::Node::Split { .. }))
    }

    pub(super) fn reopen_closed_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.window {
            self.send(ClientMessage::ReopenClosedTab { window }, cx);
        }
    }

    /// Files dropped on a pane: their paths, quoted for a shell, typed there.
    pub(super) fn drop_paths(
        &mut self,
        paths: &gpui::ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pane, _)) = self.pane_at(window.mouse_position()) else { return };
        let mut text = paths
            .paths()
            .iter()
            .map(|path| shell_quote(&path.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            return;
        }
        text.push(' ');
        if pane != self.focused_id().unwrap_or(pane) {
            self.focus_pane(pane, cx);
        }
        self.send(ClientMessage::Input { pane, bytes: text.into_bytes() }, cx);
        window.activate_window();
    }

    /// Paste into the focused pane. Several lines into a program that would
    /// take each as typed (no bracketed paste: run one by one) ask first.
    pub(super) fn paste_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.focused_id() else { return };
        let bracketed = self.mode(pane, cx).contains(TermMode::BRACKETED_PASTE);
        let lines = text.trim_end_matches(['\r', '\n']).lines().count();
        if bracketed || lines < 2 || !self.config.confirm_multiline_paste {
            self.paste_now(pane, &text, bracketed, cx);
            return;
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Paste {lines} lines?"),
            Some(
                "The program here hasn't asked for pastes whole, so each line may run as a \
                 command as it arrives.",
            ),
            &["Paste", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.paste_now(pane, &text, bracketed, cx));
            }
        })
        .detach();
    }

    fn paste_now(&mut self, pane: PaneId, text: &str, bracketed: bool, cx: &mut Context<Self>) {
        let bytes = if bracketed {
            // Strip any embedded end marker so pasted text can't escape the bracket.
            format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.send(ClientMessage::Input { pane, bytes: bytes.into_bytes() }, cx);
        self.cursor_visible = true;
        self.blink_hold = true;
    }
}

/// `text` as one shell word: as is when that's safe, else in single quotes.
pub(super) fn shell_quote(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_paths() {
        assert_eq!(shell_quote("/Users/me/a.txt"), "/Users/me/a.txt");
        assert_eq!(shell_quote("/tmp/My File.png"), "'/tmp/My File.png'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
