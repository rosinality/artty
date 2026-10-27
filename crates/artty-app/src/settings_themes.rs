//! Settings: color themes from other terminals. A theme file — iTerm2's,
//! Ghostty's, kitty's, Alacritty's, WezTerm's, Windows Terminal's — chosen
//! (Import…) or dropped on the window; and Ghostty's own themes, if it's
//! installed, listed to try with a click.

use artty_core::theme_import::{self, ImportedTheme};

use super::*;

/// The most of Ghostty's themes listed at once.
const LISTED: usize = 60;

impl Settings {
    /// Use the theme in the file at `path`.
    pub(super) fn import_theme(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        match theme_import::import(path) {
            Ok(theme) => self.use_theme(&theme, cx),
            Err(err) => {
                self.status = Some(format!("Couldn't import {}: {err}", path.display()));
                cx.notify();
            },
        }
    }

    fn use_theme(&mut self, theme: &ImportedTheme, cx: &mut Context<Self>) {
        let name = theme.name.clone();
        self.edit(cx, |file| file.set_colors(theme));
        if self.status.is_none() {
            self.status = Some(format!("Theme: {name}"));
        }
        self.chosen_theme = Some(name);
    }

    fn choose_theme_file(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.first()
            {
                let _ = this.update(cx, |this, cx| this.import_theme(path, cx));
            }
        })
        .detach();
    }

    /// Ghostty's themes, read once in the background.
    fn load_ghostty_themes(&mut self, cx: &mut Context<Self>) {
        if self.ghostty.is_some() {
            return;
        }
        self.ghostty = Some(Vec::new());
        let reading = cx.background_spawn(async move {
            theme_import::ghostty_themes()
                .into_iter()
                .filter_map(|(name, path)| {
                    let mut theme = theme_import::import(&path).ok()?;
                    theme.name = name;
                    Some(theme)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let themes = reading.await;
            let _ = this.update(cx, |this, cx| {
                this.ghostty = Some(themes);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn theme_import_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.load_ghostty_themes(cx);
        let import = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py(px(6.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(rgba(0xffffffee)).child("From another terminal"))
                    .child(div().text_size(px(11.)).text_color(rgba(0xffffff70)).child(
                        "iTerm2, Ghostty, kitty, Alacritty, WezTerm or Windows Terminal: \
                         choose a theme file, or drop it here",
                    )),
            )
            .child(
                Self::form_button("import-theme".into(), "Import…", false)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_theme_file(cx))),
            )
            .into_any_element();
        let mut rows = vec![import];
        let themes = self.ghostty.as_deref().unwrap_or_default();
        if themes.is_empty() {
            return rows;
        }
        let focused = self.typing == Some(Typing::ThemeFilter);
        let filter = self.theme_filter.to_lowercase();
        rows.push(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .pt(px(6.))
                .child(div().flex_none().text_color(rgba(0xffffffee)).child("Ghostty's themes"))
                .child(
                    div()
                        .id("theme-filter")
                        .flex_1()
                        .px_2()
                        .py(px(3.))
                        .rounded(px(5.))
                        .bg(rgba(0x0000004d))
                        .border_1()
                        .border_color(if focused { rgba(0x8fb4ff99) } else { rgba(0xffffff1a) })
                        .text_color(if self.theme_filter.is_empty() {
                            rgba(0xffffff40)
                        } else {
                            rgba(0xffffffee)
                        })
                        .child(match (self.theme_filter.is_empty(), focused) {
                            (true, _) => format!("Filter {} themes", themes.len()),
                            (false, true) => format!("{}▏", self.theme_filter),
                            (false, false) => self.theme_filter.clone(),
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.typing = Some(Typing::ThemeFilter);
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );
        let hsla_of = |rgb: artty_core::vt::ansi::Rgb| -> Hsla {
            gpui::rgb(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)).into()
        };
        let shown: Vec<AnyElement> = themes
            .iter()
            .enumerate()
            .filter(|(_, theme)| filter.is_empty() || theme.name.to_lowercase().contains(&filter))
            .take(LISTED)
            .map(|(i, theme)| {
                let chosen = self.chosen_theme.as_deref() == Some(theme.name.as_str());
                let background = theme.background.map(hsla_of);
                let foreground = theme.foreground.map(hsla_of);
                div()
                    .id(("ghostty-theme", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(5.))
                    .when(chosen, |d| d.bg(rgba(0x3478f6ff)))
                    .when(!chosen, |d| d.hover(|d| d.bg(rgba(0xffffff10))))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .px(px(4.))
                            .py(px(3.))
                            .rounded(px(4.))
                            .when_some(background, |d, b| d.bg(b))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .when_some(foreground, |d, f| d.text_color(f))
                                    .child("Aa"),
                            )
                            .children(
                                theme
                                    .palette
                                    .iter()
                                    .skip(1)
                                    .take(6)
                                    .map(|&c| div().size(px(7.)).rounded_full().bg(hsla_of(c))),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(rgba(0xffffffd8))
                            .child(theme.name.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(theme) = this.ghostty.as_ref().and_then(|t| t.get(i)).cloned() {
                            this.use_theme(&theme, cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect();
        rows.push(
            div()
                .id("ghostty-themes")
                .mx_3()
                .my(px(6.))
                .max_h(px(220.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(shown)
                .into_any_element(),
        );
        rows
    }

    /// Typing into the theme filter. Returns whether the key was used.
    pub(super) fn theme_filter_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.typing != Some(Typing::ThemeFilter) {
            return false;
        }
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" | "enter" => self.typing = None,
            "backspace" => {
                self.theme_filter.pop();
            },
            _ => match keystroke.key_char.as_deref() {
                Some(c) if !keystroke.modifiers.platform && !c.chars().any(char::is_control) => {
                    self.theme_filter.push_str(c)
                },
                _ => return false,
            },
        }
        cx.notify();
        true
    }
}
