//! Asking before closing a tab or a pane where something runs (a command,
//! an agent): what will end, Close or Cancel, Return or Escape. For ⇧⌘W,
//! ⌘W and the menus; a click on a tab's close button asks in its place
//! instead (see `tab_end`), by the same rule ([`PaneState::running`]).

use super::*;

/// What's to close, once confirmed.
pub enum Closing {
    /// The tab with this pane (found again then, should tabs have moved).
    Tab(PaneId),
    Pane(PaneId),
}

/// What's asked.
pub struct CloseSheet {
    closing: Closing,
    /// What runs there, by name: what closing ends.
    running: Vec<String>,
}

impl WindowView {
    /// What runs in `panes` that closing them would end, by name (an agent
    /// by its), each once.
    fn running_in(&self, panes: &[PaneId], cx: &App) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for state in panes.iter().filter_map(|&pane| self.pane_state(pane, cx)) {
            if !state.running() {
                continue;
            }
            let name = state.agent.clone().or_else(|| state.info.process.clone());
            let name = name.unwrap_or_else(|| "a program".to_owned());
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Close the tab at `index` in the sidebar, asking first if something
    /// runs in it.
    pub(super) fn close_tab_asking(&mut self, index: usize, cx: &mut Context<Self>) {
        let panes = self.tab_panes(index);
        let running = self.running_in(&panes, cx);
        match panes.first() {
            Some(&pane) if !running.is_empty() => {
                self.close_sheet = Some(CloseSheet { closing: Closing::Tab(pane), running });
                cx.notify();
            },
            _ => self.close_tab_at(index, cx),
        }
    }

    /// Close `pane`, asking first if something runs in it.
    pub(super) fn close_pane_asking(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let running = self.running_in(&[pane], cx);
        if running.is_empty() {
            self.send(ClientMessage::ClosePane { pane }, cx);
        } else {
            self.close_sheet = Some(CloseSheet { closing: Closing::Pane(pane), running });
            cx.notify();
        }
    }

    fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.close_sheet.take() else { return };
        match sheet.closing {
            Closing::Pane(pane) => self.send(ClientMessage::ClosePane { pane }, cx),
            Closing::Tab(pane) => {
                if let Some(index) =
                    (0..self.places.len()).find(|&i| self.tab_panes(i).contains(&pane))
                {
                    self.close_tab_at(index, cx);
                }
            },
        }
        cx.notify();
    }

    fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.close_sheet = None;
        cx.notify();
    }

    /// While it asks: Return closes, Escape doesn't, and nothing typed goes
    /// to the terminal behind it.
    pub(super) fn close_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.close_sheet.is_none() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.cancel_close(cx),
            "enter" => self.confirm_close(cx),
            _ => {},
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn close_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let sheet = self.close_sheet.as_ref()?;
        let title = match sheet.closing {
            Closing::Tab(_) => "Close this tab?",
            Closing::Pane(_) => "Close this pane?",
        };
        let note = match sheet.running.as_slice() {
            [one] => format!("{one} is running in it, and will end."),
            [rest @ .., last] => {
                format!("{} and {last} are running in it, and will end.", rest.join(", "))
            },
            [] => String::new(),
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex_none()
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .text_color(ui(0xffffffe0))
                .child(label)
        };
        let card = div()
            .w(px(380.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(px(10.))
            .bg(ui(0x1e2430f8))
            .border_1()
            .border_color(ui(0xffffff24))
            .shadow_lg()
            .text_size(px(13.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(ui(0xffffffee)).child(title))
            .child(div().text_size(px(12.)).text_color(ui(0xffffffa0)).child(note))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("close-cancel", "Cancel")
                            .bg(ui(0xffffff14))
                            .hover(|d| d.bg(ui(0xffffff26)))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_close(cx))),
                    )
                    .child(
                        button("close-confirm", "Close")
                            .bg(ui(0xe5484dff))
                            .hover(|d| d.bg(ui(0xf2555aff)))
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_close(cx))),
                    ),
            );
        Some(
            div()
                .id("close-sheet")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(ui(0x00000066))
                .occlude()
                // A click outside takes it back.
                .on_click(cx.listener(|this, _, _, cx| this.cancel_close(cx)))
                .child(card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())),
        )
    }
}
