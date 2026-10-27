//! Settings: key bindings. Yours (`[keys]`), each with Remove; and a new one:
//! record its keys (several make a sequence, as `⌘E 1`), then choose what it
//! does — an action, or text to type.

use artty_core::KeyAction;

use super::*;

/// A binding being made.
#[derive(Default)]
pub(super) struct AddKey {
    /// Keystrokes recorded, as GPUI writes them (`cmd-e`).
    keys: Vec<String>,
    /// Typing text to send (else choosing an action).
    text_mode: bool,
    /// What's typed: the action filter, or the text.
    filter: String,
    text: String,
    action: Option<String>,
}

/// `artty::NewTab` as `new-tab`.
fn kebab(action: &str) -> String {
    let name = action.rsplit("::").next().unwrap_or(action);
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Keystrokes as written in the config (artty's or Ghostty's way), shown
/// as macOS writes them: `⌘E 1`.
pub(super) fn shown_keys(written: &str) -> String {
    crate::keymap::keystrokes(written)
        .split(' ')
        .map(|chord| {
            let mut parts: Vec<&str> = chord.split('-').collect();
            // (A `-` key: `ctrl--`.)
            let key = if chord.ends_with("--") {
                parts.truncate(parts.len().saturating_sub(2));
                "-".to_owned()
            } else {
                parts.pop().unwrap_or_default().to_owned()
            };
            let mut out = String::new();
            for modifier in parts {
                out.push_str(match modifier {
                    "ctrl" => "⌃",
                    "alt" => "⌥",
                    "shift" => "⇧",
                    "cmd" => "⌘",
                    "fn" => "fn",
                    other => other,
                });
            }
            out.push_str(&match key.as_str() {
                "enter" => "↩".to_owned(),
                "escape" => "⎋".to_owned(),
                "tab" => "⇥".to_owned(),
                "space" => "Space".to_owned(),
                "backspace" => "⌫".to_owned(),
                "up" => "↑".to_owned(),
                "down" => "↓".to_owned(),
                "left" => "←".to_owned(),
                "right" => "→".to_owned(),
                key if key.chars().count() == 1 => key.to_uppercase(),
                key => key.to_owned(),
            });
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a binding does, said plainly.
fn shown_action(action: &KeyAction) -> String {
    match action {
        KeyAction::Named(name) if name == "none" => "nothing (default taken away)".to_owned(),
        KeyAction::Named(name) => match name.strip_prefix("text:") {
            Some(text) => format!("types “{}”", crate::keymap::unescape(text)),
            None => name.replace('-', " "),
        },
        KeyAction::Send { send } => format!("types “{}”", send.escape_debug()),
    }
}

impl Settings {
    pub(super) fn keys_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = self
            .config
            .keys
            .iter()
            .map(|(keys, action)| {
                let written = keys.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py(px(7.))
                    .border_b_1()
                    .border_color(ui(0xffffff0d))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(110.))
                            .px_2()
                            .py(px(2.))
                            .rounded(px(5.))
                            .bg(ui(0xffffff14))
                            .text_color(ui(0xffffffee))
                            .child(shown_keys(keys)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(ui(0xffffffb0))
                            .child(shown_action(action)),
                    )
                    .child(
                        Self::form_button(
                            ElementId::Name(format!("remove-key-{keys}").into()),
                            "Remove",
                            false,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let written = written.clone();
                            this.edit(cx, |file| file.remove_key(&written));
                        })),
                    )
                    .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py(px(8.))
                    .text_color(ui(0xffffff70))
                    .child("None yet: the defaults are as in the menus.")
                    .into_any_element(),
            );
        }
        // Ghostty's bindings, brought over with a click.
        let ghostty = self.ghostty_config.as_ref().map_or(0, |c| c.keybinds.len());
        if ghostty > 0 {
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py(px(7.))
                    .border_b_1()
                    .border_color(ui(0xffffff0d))
                    .child(div().text_color(ui(0xffffffb0)).child(if ghostty == 1 {
                        "Ghostty has 1 key binding".to_owned()
                    } else {
                        format!("Ghostty has {ghostty} key bindings")
                    }))
                    .child(
                        Self::form_button("ghostty-keys".into(), "Bring Over", false).on_click(
                            cx.listener(|this, _, _, cx| this.bring_over_ghostty_keys(cx)),
                        ),
                    )
                    .into_any_element(),
            );
        }
        rows.push(match &self.adding_key {
            Some(adding) => self.add_key_form(adding, cx),
            None => div()
                .id("add-key")
                .px_3()
                .py(px(8.))
                .text_color(ui(0x8fb4ffff))
                .hover(|d| d.bg(ui(0xffffff08)))
                .child("Add a key binding…")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.adding_key = Some(AddKey::default());
                    this.start_recording(window, cx);
                }))
                .into_any_element(),
        });
        rows
    }

    /// Take every key pressed in this window as the binding's, until Done.
    fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.weak_entity();
        let settings_window = window.window_handle();
        self.recording = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != settings_window {
                return;
            }
            let key = &event.keystroke.key;
            if ["shift", "control", "alt", "platform", "function"].contains(&key.as_str()) {
                return;
            }
            let chord = event.keystroke.unparse();
            let _ = entity.update(cx, |this, cx| {
                if let Some(adding) = this.adding_key.as_mut() {
                    adding.keys.push(chord);
                    cx.notify();
                }
            });
            cx.stop_propagation();
        }));
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        cx.notify();
    }

    fn add_key_form(&self, adding: &AddKey, cx: &mut Context<Self>) -> AnyElement {
        let recording = self.recording.is_some();
        let keys = if adding.keys.is_empty() {
            "Press the keys…".to_owned()
        } else {
            shown_keys(&adding.keys.join(" "))
        };
        let keys_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(70.)).flex_none().text_color(ui(0xffffffb0)).child("Keys"))
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(5.))
                    .bg(ui(0x0000004d))
                    .border_1()
                    .border_color(if recording { ui(0xf7768ecc) } else { ui(0xffffff1a) })
                    .text_color(if adding.keys.is_empty() {
                        ui(0xffffff50)
                    } else {
                        ui(0xffffffee)
                    })
                    .child(keys),
            )
            .child(if recording {
                Self::form_button("keys-done".into(), "Done", true)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_recording(cx)))
            } else {
                Self::form_button("keys-record".into(), "Record Again", false).on_click(
                    cx.listener(|this, _, window, cx| {
                        if let Some(adding) = this.adding_key.as_mut() {
                            adding.keys.clear();
                        }
                        this.start_recording(window, cx);
                    }),
                )
            });
        let mode = |id: &'static str, label: &'static str, on: bool, text: bool| {
            div()
                .id(id)
                .px_2()
                .py(px(3.))
                .rounded(px(6.))
                .when(on, |d| d.bg(ui(0x3478f6ff)))
                .when(!on, |d| d.hover(|d| d.bg(ui(0xffffff1a))))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(adding) = this.adding_key.as_mut() {
                        adding.text_mode = text;
                    }
                    this.typing = Some(if text { Typing::KeyText } else { Typing::ActionFilter });
                    cx.notify();
                }))
        };
        let does = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(70.)).flex_none().text_color(ui(0xffffffb0)).child("Does"))
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(8.))
                    .bg(ui(0xffffff12))
                    .child(mode("key-mode-action", "An action", !adding.text_mode, false))
                    .child(mode("key-mode-text", "Types text", adding.text_mode, true)),
            );
        let field = |typing: Typing, text: &str, hint: &'static str| {
            let focused = self.typing == Some(typing);
            div()
                .id(match typing {
                    Typing::KeyText => "key-text",
                    _ => "key-filter",
                })
                .flex_1()
                .px_2()
                .py(px(3.))
                .rounded(px(5.))
                .bg(ui(0x0000004d))
                .border_1()
                .border_color(if focused { ui(0x8fb4ff99) } else { ui(0xffffff1a) })
                .text_color(if text.is_empty() { ui(0xffffff40) } else { ui(0xffffffee) })
                .child(match (text.is_empty(), focused) {
                    (true, _) => hint.to_owned(),
                    (false, true) => format!("{text}▏"),
                    (false, false) => text.to_owned(),
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.typing = Some(typing);
                    cx.notify();
                }))
        };
        let detail: AnyElement = if adding.text_mode {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(70.)).flex_none())
                .child(field(
                    Typing::KeyText,
                    &adding.text,
                    "è  (escapes as Ghostty's: \\xc3\\xa8, \\n, \\e)",
                ))
                .into_any_element()
        } else {
            let filter = adding.filter.to_lowercase();
            let mut actions: Vec<String> = cx
                .all_action_names()
                .iter()
                .filter(|name| name.starts_with("artty::") && **name != "artty::SendText")
                .map(|name| kebab(name))
                .filter(|name| filter.is_empty() || name.contains(&filter.replace(' ', "-")))
                .collect();
            actions.sort();
            actions.dedup();
            let list = actions.into_iter().take(40).enumerate().map(|(i, name)| {
                let chosen = adding.action.as_deref() == Some(name.as_str());
                let label = name.replace('-', " ");
                div()
                    .id(("key-action", i))
                    .px_2()
                    .py(px(2.))
                    .rounded(px(4.))
                    .when(chosen, |d| d.bg(ui(0x3478f6ff)))
                    .when(!chosen, |d| d.hover(|d| d.bg(ui(0xffffff10))))
                    .text_color(ui(0xffffffd8))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(adding) = this.adding_key.as_mut() {
                            adding.action = Some(name.clone());
                        }
                        cx.notify();
                    }))
            });
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(70.)).flex_none())
                        .child(field(Typing::ActionFilter, &adding.filter, "Filter actions")),
                )
                .child(
                    div()
                        .id("key-actions")
                        .ml(px(78.))
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(list),
                )
                .into_any_element()
        };
        let ready = !adding.keys.is_empty()
            && if adding.text_mode { !adding.text.is_empty() } else { adding.action.is_some() };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py(px(10.))
            .child(keys_row)
            .child(does)
            .child(detail)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::form_button("key-cancel".into(), "Cancel", false).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.adding_key = None;
                            this.typing = None;
                            this.stop_recording(cx);
                        }),
                    ))
                    .child(
                        Self::form_button("key-save".into(), "Add", ready)
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    ),
            )
            .into_any_element()
    }

    /// Ghostty's key bindings into `[keys]`: text, and actions artty has.
    fn bring_over_ghostty_keys(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.ghostty_config.as_ref() else { return };
        let mut brought = Vec::new();
        let mut skipped = Vec::new();
        for (keys, action) in &config.keybinds {
            match artty_core::theme_import::ghostty_action(action) {
                Some(artty) => brought.push((keys.clone(), KeyAction::Named(artty))),
                None => skipped.push(format!("{} ({action})", shown_keys(keys))),
            }
        }
        let count = brought.len();
        self.edit(cx, |file| {
            for (keys, action) in &brought {
                file.set_key(keys, action);
            }
        });
        if self.status.is_none() {
            let mut status = format!("Brought over {count} from Ghostty");
            if !skipped.is_empty() {
                status.push_str(&format!("; no such action here for {}", skipped.join(", ")));
            }
            self.status = Some(status);
        }
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some(adding) = self.adding_key.as_ref() else { return };
        if adding.keys.is_empty() {
            return;
        }
        let action = match (&adding.action, adding.text_mode) {
            (_, true) if !adding.text.is_empty() => {
                KeyAction::Named(format!("text:{}", adding.text))
            },
            (Some(action), false) => KeyAction::Named(action.clone()),
            _ => return,
        };
        let keys = adding.keys.join(" ");
        self.adding_key = None;
        self.typing = None;
        self.recording = None;
        self.edit(cx, |file| file.set_key(&keys, &action));
    }

    /// Typing into the form. Returns whether the key was used.
    pub(super) fn key_form_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(typing), Some(adding)) = (self.typing, self.adding_key.as_mut()) else {
            return false;
        };
        let text = match typing {
            Typing::KeyText => &mut adding.text,
            Typing::ActionFilter => &mut adding.filter,
            _ => return false,
        };
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" => {
                self.save_key(cx);
                return true;
            },
            "escape" => self.typing = None,
            "backspace" => {
                text.pop();
            },
            _ => match keystroke.key_char.as_deref() {
                Some(c) if !keystroke.modifiers.platform && !c.chars().any(char::is_control) => {
                    text.push_str(c)
                },
                _ => return false,
            },
        }
        cx.notify();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_actions_shown() {
        assert_eq!(shown_keys("cmd-e 1"), "⌘E 1");
        assert_eq!(shown_keys("cmd+e>1"), "⌘E 1");
        assert_eq!(shown_keys("ctrl-shift-up"), "⌃⇧↑");
        assert_eq!(kebab("artty::NewTab"), "new-tab");
        assert_eq!(shown_action(&KeyAction::Named("text:\\xc3\\xa8".into())), "types “è”");
        assert_eq!(shown_action(&KeyAction::Named("new-tab".into())), "new tab");
    }
}
