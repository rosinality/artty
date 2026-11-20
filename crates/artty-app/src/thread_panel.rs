//! The thread panel (⌘⇧J): the focused pane's agent's thread beside its
//! terminal — what it was given and said, its permission requests (answered
//! there too), and a box to write to it in. It follows the focus from pane
//! to pane. The terminal narrows to make room (an agent's screen redraws to
//! fit), rather than going under it.
//!
//! The terminal keeps the keyboard until the box is clicked; then what's
//! typed goes there, till Esc (or a click in the terminal).

use super::*;

pub(super) const PANEL_WIDTH: f32 = 380.;
/// The terminal isn't narrowed below this for the panel (in a small
/// window, the panel goes over its right edge instead).
const MIN_TERMINAL: f32 = 420.;

#[derive(Default)]
pub(super) struct ThreadPanel {
    /// The agents (for @mentions), and the focused pane's thread.
    pub(super) agents: agents_view::Agents,
    /// The box was given the keyboard (it has it while there's a box: see
    /// `panel_typing`).
    pub(super) typing: bool,
}

impl ThreadPanel {
    /// The focused pane runs an agent (as last heard).
    fn has_agent(&self) -> bool {
        let agents = &self.agents;
        agents.selected.is_some_and(|p| agents.agents.iter().any(|(a, _)| a.pane == p))
    }
}

impl WindowView {
    /// Shown: open, and the terminal on screen (not Activity or Agents).
    pub(super) fn panel_shown(&self) -> bool {
        self.thread_panel.is_some() && !self.showing_view()
    }

    /// The panel's box has the keyboard: it was given it, the focused pane
    /// runs an agent (else there's no box), and nothing's open over the
    /// window that takes typing (the palette, a search, a rename, ...).
    pub(super) fn panel_typing(&self) -> bool {
        self.panel_shown()
            && self.thread_panel.as_ref().is_some_and(|p| p.typing && p.has_agent())
            && !self.asking()
            && self.palette.is_none()
            && self.search_all.is_none()
            && self.picker.is_none()
            && self.renaming.is_none()
            && self.search.is_none()
    }

    /// The terminal's width, with the panel open beside it.
    pub(super) fn beside_panel(&self, width: f32) -> f32 {
        if self.thread_panel.is_none() {
            return width;
        }
        (width - PANEL_WIDTH).max(width.min(MIN_TERMINAL))
    }

    /// ⌘⇧J: open the panel, its box taking the keyboard; from the terminal,
    /// back to the box; from the box, close it.
    pub(super) fn thread_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.thread_panel {
            Some(_) if self.panel_typing() => self.close_thread_panel(cx),
            Some(_) if self.panel_shown() => self.start_panel_typing(window, cx),
            _ => {
                self.open_thread_panel(cx);
                self.start_panel_typing(window, cx);
            },
        }
    }

    fn open_thread_panel(&mut self, cx: &mut Context<Self>) {
        // From Activity or Agents: back to the terminal, with it.
        if self.showing_agents() {
            self.agents = None;
        }
        if self.showing_activity() {
            self.toggle_activity(cx);
        }
        // One panel at the right edge at a time.
        if self.files.is_some() {
            self.toggle_files(cx);
        }
        if self.thread_panel.is_none() {
            self.thread_panel = Some(ThreadPanel::default());
        }
        self.follow_focus(cx);
        // The terminal's width changes.
        cx.notify();
    }

    pub(super) fn close_thread_panel(&mut self, cx: &mut Context<Self>) {
        self.thread_panel = None;
        self.marked_text = None;
        cx.notify();
    }

    /// Show the focused pane's thread (once it's another pane).
    pub(super) fn follow_focus(&mut self, cx: &mut Context<Self>) {
        let focused = self.focused_id();
        let Some(panel) = self.thread_panel.as_mut() else { return };
        // (Nothing to look up without a pane.)
        if panel.agents.selected == focused
            && (panel.agents.thread_of.is_some() || focused.is_none())
        {
            return;
        }
        if panel.agents.selected != focused {
            panel.agents.selected = focused;
            panel.agents.thread.clear();
            panel.agents.asks.clear();
            panel.agents.thread_of = None;
            panel.agents.pieces.borrow_mut().clear();
            panel.agents.text_sel = None;
            panel.agents.list.0.reset(1);
            panel.typing = false;
        }
        if self.panel_shown() {
            self.refresh_in(Shown::Panel, cx);
        }
    }

    /// The panel's agents were heard from: with no agent in the focused
    /// pane (it exited, say), the keyboard stays with the terminal.
    pub(super) fn panel_refreshed(&mut self) {
        if let Some(panel) = self.thread_panel.as_mut()
            && !panel.has_agent()
        {
            panel.typing = false;
        }
    }

    pub(super) fn start_panel_typing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        if let Some(panel) = self.thread_panel.as_mut() {
            panel.typing = true;
            self.marked_text = None;
        }
        cx.notify();
    }

    pub(super) fn stop_panel_typing(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.thread_panel.as_mut().filter(|p| p.typing) {
            panel.typing = false;
            self.marked_text = None;
            cx.notify();
        }
    }

    /// Keys while writing in the panel: the draft's, and Esc (with nothing
    /// written) gives the keyboard back to the terminal. None reach it.
    pub(super) fn panel_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.compose_key(event, cx) {
            return true;
        }
        if event.keystroke.key == "escape" {
            self.stop_panel_typing(cx);
            return true;
        }
        false
    }

    pub(super) fn thread_panel_view(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if !self.panel_shown() {
            return None;
        }
        let panel = self.thread_panel.as_ref()?;
        let agents = &panel.agents;
        let agent = agents
            .selected
            .and_then(|p| agents.agents.iter().find(|(a, _)| a.pane == p))
            .map(|(a, _)| a);
        let theme = crate::terminal::hsla(self.theme.background);
        let header = Self::titlebar("thread-titlebar")
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(TITLEBAR_HEIGHT))
            .px(px(12.))
            .border_b_1()
            .border_color(ui(0xffffff12))
            .child(
                Self::one_line("THREAD".to_owned())
                    .flex_1()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui(0xffffff70)),
            )
            .child(
                div()
                    .id("thread-close")
                    .px(px(6.))
                    .rounded(px(4.))
                    .text_size(px(12.))
                    .text_color(ui(0xffffff90))
                    .hover(|d| d.bg(ui(0xffffff14)).text_color(ui(0xffffffee)))
                    .child("✕")
                    .on_click(cx.listener(|this, _, _, cx| this.close_thread_panel(cx))),
            );
        let body = match agent {
            Some(agent) => self.agent_thread(agents, Some(agent), Shown::Panel, cx),
            None if !agents.loaded => div().flex_1(),
            None => div().flex_1().p(px(16.)).text_size(px(12.5)).text_color(ui(0xffffff90)).child(
                "No agent in this pane. Start Claude Code, Codex or Gemini CLI here (with \
                 artty's hooks installed: artty hooks install) and its thread shows here.",
            ),
        };
        Some(
            div()
                .id("thread-panel")
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(PANEL_WIDTH))
                .bg(theme)
                .border_l_1()
                .border_color(ui(0xffffff12))
                .text_size(px(13.))
                // Clicks and scrolls here aren't the terminal's.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .bg(ui(0xffffff0a))
                        .child(header)
                        .child(div().flex_1().min_h_0().flex().flex_col().child(body)),
                ),
        )
    }
}
