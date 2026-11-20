//! Writing to agents from the Agents view or the thread panel: a draft per
//! agent (the same in both), sent as an
//! `artty ask` (given to the agent once it's between turns). Leading
//! @mentions send it to other agents instead, by their tab's title.
//!
//! Text comes through the window's input handler (so input methods work);
//! keys that edit or send come through `compose_key`.

use std::collections::HashMap;

use artty_core::control::{AskInfo, AskState};

use super::*;

/// A message being written, and where the cursor is in it (a byte index on
/// a character boundary).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Draft {
    pub text: String,
    pub cursor: usize,
}

impl Draft {
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some((at, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.replace_range(at..self.cursor, "");
            self.cursor = at;
        }
    }

    /// Delete back to the start of the cursor's line.
    pub fn delete_line_start(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn left(&mut self) {
        if let Some((at, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = at;
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1)
    }

    pub fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |at| self.cursor + at)
    }

    /// The @word the cursor is at the end of, without its @.
    pub fn mention(&self) -> Option<&str> {
        let before = &self.text[..self.cursor];
        let word = before.rsplit(char::is_whitespace).next()?;
        word.strip_prefix('@')
    }

    /// Replace the @word at the cursor with `@name `.
    pub fn complete(&mut self, name: &str) {
        let Some(partial) = self.mention().map(str::len) else { return };
        let start = self.cursor - partial - 1;
        let completed = format!("@{name} ");
        self.text.replace_range(start..self.cursor, &completed);
        self.cursor = start + completed.len();
    }
}

/// How an agent is mentioned: its tab's title, spaces taken out.
pub(super) fn handle(title: &str) -> String {
    title.split_whitespace().collect()
}

/// Who a message goes to and what it says: the agents its leading
/// @mentions name (by `handle`, ignoring case), else `selected`.
/// `agents`: each agent's pane and tab title.
pub(super) fn targets(
    text: &str,
    agents: &[(PaneId, String)],
    selected: Option<PaneId>,
) -> Result<(Vec<PaneId>, String), String> {
    let mut rest = text.trim_start();
    let mut chosen = Vec::new();
    while let Some(mention) = rest.strip_prefix('@') {
        let end = mention.find(char::is_whitespace).unwrap_or(mention.len());
        let name = &mention[..end];
        let found: Vec<PaneId> = agents
            .iter()
            .filter(|(_, title)| handle(title).eq_ignore_ascii_case(name))
            .map(|(pane, _)| *pane)
            .collect();
        match found.as_slice() {
            [] => return Err(format!("no agent's tab is called {name}")),
            [pane] => {
                if !chosen.contains(pane) {
                    chosen.push(*pane);
                }
            },
            _ => {
                return Err(format!(
                    "{} agents' tabs are called {name}: rename one to tell them apart",
                    found.len()
                ));
            },
        }
        rest = mention[end..].trim_start();
    }
    let message = rest.trim_end().to_owned();
    if message.is_empty() {
        return Err("nothing to send".into());
    }
    if chosen.is_empty() {
        chosen.push(selected.ok_or("no agent to send it to")?);
    }
    Ok((chosen, message))
}

/// Per agent's pane: its draft.
pub(super) type Drafts = HashMap<PaneId, Draft>;

impl WindowView {
    /// Where what's typed goes: the Agents view while shown, else the
    /// thread panel while it's being written in.
    pub(super) fn composing(&self) -> Option<Shown> {
        if self.showing_agents() {
            Some(Shown::View)
        } else if self.panel_typing() {
            Some(Shown::Panel)
        } else {
            None
        }
    }

    fn draft_mut(&mut self) -> Option<&mut Draft> {
        let pane = self.agents_in(self.composing()?)?.selected?;
        Some(self.drafts.entry(pane).or_default())
    }

    fn draft_in(&self, which: Shown) -> Option<&Draft> {
        self.drafts.get(&self.agents_in(which)?.selected?)
    }

    fn draft(&self) -> Option<&Draft> {
        self.draft_in(self.composing()?)
    }

    /// Text typed (or pasted, or from an input method) into the draft.
    pub(super) fn compose_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft_mut() {
            draft.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
            cx.notify();
        }
    }

    /// Keys that edit or send the draft. Returns whether the key was used;
    /// with an input method composing, it has the keys.
    pub(super) fn compose_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.marked_text.is_some() {
            return false;
        }
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let empty = self.draft().is_none_or(|d| d.text.is_empty());
        let Some(draft) = self.draft_mut() else { return false };
        match key {
            "enter" if modifiers.shift || modifiers.alt => draft.insert("\n"),
            "enter" if !empty => {
                self.send_draft(cx);
                return true;
            },
            "backspace" if modifiers.platform => draft.delete_line_start(),
            "backspace" => draft.backspace(),
            "left" if modifiers.platform => draft.cursor = draft.line_start(),
            "right" if modifiers.platform => draft.cursor = draft.line_end(),
            "left" => draft.left(),
            "right" => draft.right(),
            "escape" if !empty => *draft = Draft::default(),
            "tab" => {
                let first =
                    self.composing().and_then(|w| self.mention_matches(w).into_iter().next());
                let Some(first) = first else { return false };
                if let Some(draft) = self.draft_mut() {
                    draft.complete(&first);
                }
            },
            _ => return false,
        }
        cx.notify();
        true
    }

    /// Agents whose handle starts with the @word being typed.
    fn mention_matches(&self, which: Shown) -> Vec<String> {
        let (Some(agents), Some(partial)) =
            (self.agents_in(which), self.draft_in(which).and_then(Draft::mention))
        else {
            return Vec::new();
        };
        let partial = partial.to_lowercase();
        let mut found: Vec<String> = agents
            .agents
            .iter()
            .map(|(agent, _)| handle(&agent.title))
            .filter(|h| h.to_lowercase().starts_with(&partial))
            .collect();
        found.dedup();
        found
    }

    /// Send the draft: an ask to each agent it's for.
    fn send_draft(&mut self, cx: &mut Context<Self>) {
        let Some(agents) = self.composing().and_then(|w| self.agents_in(w)) else { return };
        let Some(draft) = self.draft() else { return };
        let list: Vec<(PaneId, String)> =
            agents.agents.iter().map(|(a, _)| (a.pane, a.title.clone())).collect();
        let (panes, text) = match targets(&draft.text, &list, agents.selected) {
            Ok(found) => found,
            Err(message) => return self.show_toast(message, cx),
        };
        if let Some(draft) = self.draft_mut() {
            *draft = Draft::default();
        }
        for pane in panes {
            let request = Request::Ask {
                tab: ids::local_id(pane),
                text: text.clone(),
                raw: false,
                wait: false,
                timeout_secs: None,
            };
            let answer = app_state::control_on(ids::server_of(pane), request, cx);
            cx.spawn(async move |this, cx| {
                if let Ok(Response::Error { message }) = answer.await {
                    let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
                }
                // Its queued ask shows at once, not in a second.
                let _ = this.update(cx, |this, cx| this.refresh_agents(cx));
            })
            .detach();
        }
        cx.notify();
    }

    /// Asks to the agent not yet taken (waiting for its turn to end), at the
    /// thread's end.
    pub(super) fn queued_asks(&self, asks: &[AskInfo]) -> impl IntoElement {
        div().flex().flex_col().gap(px(8.)).children(
            asks.iter().filter(|a| matches!(a.state, AskState::Queued | AskState::Delivering)).map(
                |ask| {
                    div().flex().justify_end().child(
                        div()
                            .max_w(gpui::relative(0.86))
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(3.))
                            .child(div().text_size(px(11.)).text_color(ui(0xffffff60)).child(
                                match ask.state {
                                    AskState::Delivering => "you · being given to it",
                                    _ => "you · queued until its turn ends",
                                },
                            ))
                            .child(
                                div()
                                    .px(px(11.))
                                    .py(px(8.))
                                    .rounded(px(10.))
                                    .border_1()
                                    .border_color(ui(0x3d59a1ff))
                                    .text_color(ui(0xffffffa0))
                                    .child(ask.text.clone()),
                            ),
                    )
                },
            ),
        )
    }

    /// The box to write in, with what @ would complete, and its keys.
    /// In the panel, it has the keyboard once clicked (till Esc, or a
    /// click in the terminal).
    pub(super) fn compose_box(
        &self,
        title: &str,
        which: Shown,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let active = self.composing() == Some(which);
        let draft = self.draft_in(which).cloned().unwrap_or_default();
        let marked = self.marked_text.clone().filter(|_| active).unwrap_or_default();
        let (before, after) = draft.text.split_at(draft.cursor);
        let empty = draft.text.is_empty() && marked.is_empty();
        let focus = self.focus.clone();
        let entity = cx.entity();
        let matches = if active { self.mention_matches(which) } else { Vec::new() };
        let cursor = if active { "▏" } else { "" };
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(16.))
            .pt(px(8.))
            .pb(px(10.))
            .border_t_1()
            .border_color(ui(0xffffff12))
            .when(!matches.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .children(matches.iter().enumerate().map(|(i, name)| {
                            div()
                                .px(px(6.))
                                .rounded(px(5.))
                                .bg(if i == 0 { ui(0x3d59a1ff) } else { ui(0xffffff10) })
                                .text_color(ui(0xffffffd0))
                                .child(format!("@{name}"))
                        }))
                        .child(div().text_color(ui(0xffffff50)).child("⇥ completes")),
                )
            })
            .child(
                div()
                    .id(match which {
                        Shown::View => "compose-view",
                        Shown::Panel => "compose-panel",
                    })
                    .relative()
                    .min_h(px(36.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(ui(0xffffff0c))
                    .border_1()
                    .border_color(if active || which == Shown::View {
                        ui(0xffffff22)
                    } else {
                        ui(0xffffff12)
                    })
                    .when(active && which == Shown::Panel, |d| d.border_color(ui(0x7aa2f7aa)))
                    .text_size(px(13.))
                    .text_color(ui(0xffffffe8))
                    .when(which == Shown::Panel, |d| {
                        d.cursor_text().on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.start_panel_typing(window, cx);
                                cx.stop_propagation();
                            }),
                        )
                    })
                    // The input handler: typing and input methods come here
                    // (only while it has the keyboard: else the terminal's).
                    .when(active, |d| {
                        d.child(
                            canvas(
                                |_, _, _| {},
                                move |bounds, _, window, cx| {
                                    window.handle_input(
                                        &focus,
                                        ElementInputHandler::new(bounds, entity),
                                        cx,
                                    );
                                },
                            )
                            .absolute()
                            .size_full(),
                        )
                    })
                    .child(if empty {
                        div().text_color(ui(0xffffff50)).child(format!("{cursor}Message {title}…"))
                    } else {
                        // The cursor as a bar; what an input method composes, before it.
                        div().child(format!("{before}{marked}{cursor}{after}"))
                    }),
            )
            .child(div().text_size(px(11.)).text_color(ui(0xffffff50)).child(match which {
                Shown::View => "⏎ send · ⇧⏎ new line · @tab sends to another agent · esc clears",
                Shown::Panel if active => "⏎ send · ⇧⏎ new line · @tab · esc back to the terminal",
                Shown::Panel => "click to write · ⏎ send",
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str, cursor: usize) -> Draft {
        Draft { text: text.into(), cursor }
    }

    #[test]
    fn editing() {
        let mut d = Draft::default();
        d.insert("한글 ok");
        assert_eq!(d.cursor, d.text.len());
        d.left();
        d.left();
        d.insert("!");
        assert_eq!(d.text, "한글 !ok");
        d.backspace();
        d.backspace();
        assert_eq!(d.text, "한글ok");
        d.cursor = 0;
        d.backspace();
        d.right();
        assert_eq!(d.cursor, "한".len());
        let mut d = draft("one\ntwo three", 13);
        assert_eq!((d.line_start(), d.line_end()), (4, 13));
        d.delete_line_start();
        assert_eq!(d.text, "one\n");
    }

    #[test]
    fn mentions() {
        let mut d = draft("@ser", 4);
        assert_eq!(d.mention(), Some("ser"));
        d.complete("server");
        assert_eq!(d.text, "@server ");
        assert_eq!(d.mention(), None);
        assert_eq!(draft("hi @a", 2).mention(), None);
    }

    #[test]
    fn who_gets_it() {
        let agents = vec![
            (1, "api".to_owned()),
            (2, "web app".to_owned()),
            (3, "API".to_owned()),
            (4, "docs".to_owned()),
        ];
        assert_eq!(targets("fix it", &agents, Some(4)), Ok((vec![4], "fix it".into())));
        assert_eq!(
            targets("@webapp @docs  fix it\n", &agents, Some(1)),
            Ok((vec![2, 4], "fix it".into()))
        );
        assert!(targets("@api x", &agents, None).unwrap_err().contains("2 agents"));
        assert!(targets("@nobody x", &agents, None).unwrap_err().contains("no agent"));
        assert!(targets("@docs", &agents, None).unwrap_err().contains("nothing"));
        assert!(targets("x", &agents, None).is_err());
    }
}
