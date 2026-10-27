//! A question ssh asks while logging in (see `askpass`), over the window:
//! the machine, what it asks, and a field (unseen) or yes and no.

use crate::askpass::{Kind, Question};

use super::*;

/// The question shown, and what's typed so far.
pub(super) struct AskSheet {
    question: Question,
    input: String,
    remember: bool,
}

impl WindowView {
    pub fn show_ask(&mut self, question: Question, cx: &mut Context<Self>) {
        // One at a time: an earlier one waits.
        if self.asks.iter().any(|a| a.question.id == question.id) {
            return;
        }
        self.asks.push(AskSheet { question, input: String::new(), remember: false });
        self.marked_text = None;
        self.update_secure_input(cx);
        cx.notify();
    }

    pub fn dismiss_ask(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.asks.len();
        self.asks.retain(|a| a.question.id != id);
        if self.asks.len() != before {
            self.update_secure_input(cx);
            cx.notify();
        }
    }

    pub(super) fn asking(&self) -> bool {
        !self.asks.is_empty()
    }

    /// Answer the question shown (`None`: cancel).
    fn answer_ask(&mut self, answer: Option<String>, cx: &mut Context<Self>) {
        let Some(sheet) = self.asks.first() else { return };
        let (id, remember) = (sheet.question.id, sheet.remember);
        self.dismiss_ask(id, cx);
        // After this view's update: finishing updates every view (this one
        // too), and an entity can't be updated inside its own update.
        cx.defer(move |cx| crate::askpass::finish(id, answer, remember, cx));
    }

    fn submit_ask(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.asks.first() else { return };
        let answer = match sheet.question.kind {
            Kind::Secret => Some(sheet.input.clone()),
            Kind::YesNo => Some("yes".to_owned()),
            Kind::Notice => None,
        };
        self.answer_ask(answer, cx);
    }

    /// Keys while a question shows: they're its.
    pub(super) fn ask_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.answer_ask(None, cx),
            "enter" => self.submit_ask(cx),
            "backspace" => {
                if let Some(sheet) = self.asks.first_mut() {
                    sheet.input.pop();
                }
                cx.notify();
            },
            // Text arrives through the input handler.
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Typed text, for the question shown.
    pub(super) fn ask_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(sheet) = self.asks.first_mut().filter(|s| s.question.kind == Kind::Secret) {
            sheet.input.push_str(text);
            cx.notify();
        }
    }

    /// Whether what's typed is a secret (secure keyboard entry, meanwhile).
    pub(super) fn asking_secret(&self) -> bool {
        self.asks.first().is_some_and(|s| s.question.kind == Kind::Secret)
    }

    pub(super) fn ask_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let sheet = self.asks.first()?;
        let question = &sheet.question;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .bg(if primary { rgba(0x3478f6ff) } else { rgba(0xffffff14) })
                .hover(|d| d.bg(if primary { rgba(0x4a88ffff) } else { rgba(0xffffff26) }))
                .text_color(rgba(0xffffffe0))
                .child(label)
        };
        let buttons = match question.kind {
            Kind::Secret => vec![
                button("ask-cancel", "Cancel", false)
                    .on_click(cx.listener(|this, _, _, cx| this.answer_ask(None, cx))),
                button("ask-ok", "Log In", true)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_ask(cx))),
            ],
            Kind::YesNo => vec![
                button("ask-no", "Cancel", false).on_click(
                    cx.listener(|this, _, _, cx| this.answer_ask(Some("no".to_owned()), cx)),
                ),
                button("ask-yes", "Connect", true)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_ask(cx))),
            ],
            Kind::Notice => vec![
                button("ask-hide", "Hide", false)
                    .on_click(cx.listener(|this, _, _, cx| this.answer_ask(None, cx))),
            ],
        };
        let field = (question.kind == Kind::Secret).then(|| {
            let dots: String = "•".repeat(sheet.input.chars().count());
            div()
                .px_2()
                .py(px(4.))
                .rounded(px(5.))
                .bg(rgba(0x0000004d))
                .border_1()
                .border_color(rgba(0x8fb4ff99))
                .text_color(rgba(0xffffffee))
                .child(format!("{dots}▏"))
        });
        let remember = question.rememberable.then(|| {
            div()
                .id("ask-remember")
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgba(0xffffffb0))
                .child(div().child(if sheet.remember { "☑" } else { "☐" }))
                .child("Remember in Keychain")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sheet) = this.asks.first_mut() {
                        sheet.remember = !sheet.remember;
                    }
                    cx.notify();
                }))
        });
        let card = div()
            .w(px(420.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(px(10.))
            .bg(rgba(0x1e2430f8))
            .border_1()
            .border_color(rgba(0xffffff24))
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(0xffffffee))
                    .child(format!("{} asks", question.name)),
            )
            .child(div().text_color(rgba(0xffffffc0)).child(question.prompt.clone()))
            .children(field)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(remember.unwrap_or_else(|| div().id("ask-remember")))
                    .child(div().flex().gap_2().children(buttons)),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                // (Not stretched to the window's height.)
                .items_start()
                .pt(px(80.))
                .bg(rgba(0x00000066))
                .child(card),
        )
    }
}
