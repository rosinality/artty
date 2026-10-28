//! Settings: the theme's colors one by one — background, text, cursor,
//! selection and the 16 ANSI colors — each typed in as `#rrggbb`. Changing
//! one keeps the theme's name, marked edited.

use artty_core::theme_import::parse_hex;
use artty_core::vt::ansi::Rgb;

use super::*;

/// A color that can be changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Slot {
    Background,
    Foreground,
    Cursor,
    Selection,
    Palette(usize),
}

const PALETTE_NAMES: [&str; 16] = [
    "Black",
    "Red",
    "Green",
    "Yellow",
    "Blue",
    "Magenta",
    "Cyan",
    "White",
    "Bright black",
    "Bright red",
    "Bright green",
    "Bright yellow",
    "Bright blue",
    "Bright magenta",
    "Bright cyan",
    "Bright white",
];

fn hex(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)
}

fn color(rgb: Rgb) -> Hsla {
    gpui::rgb(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)).into()
}

impl Settings {
    pub(super) fn colors_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.config.theme();
        let mut rows = vec![
            Self::row("Background", None, self.color_field(Slot::Background, theme.background, cx)),
            Self::row("Text", None, self.color_field(Slot::Foreground, theme.foreground, cx)),
            Self::row("Cursor", None, self.color_field(Slot::Cursor, theme.cursor, cx)),
            Self::row("Selection", None, self.color_field(Slot::Selection, theme.selection, cx)),
        ];
        let typing = match self.typing {
            Some(Typing::Color(Slot::Palette(i))) => Some(i),
            _ => None,
        };
        let swatches = |range: std::ops::Range<usize>, cx: &mut Context<Self>| {
            div().flex().gap(px(4.)).children(range.map(|i| {
                div()
                    .id(("palette", i))
                    .size(px(18.))
                    .rounded(px(4.))
                    .bg(color(theme.ansi[i]))
                    .border_1()
                    .border_color(if typing == Some(i) { ui(0x8fb4ffff) } else { ui(0xffffff30) })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_color(Slot::Palette(i), window, cx)
                    }))
            }))
        };
        rows.push(Self::row(
            "Palette",
            Some("Normal colors, then bright"),
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(swatches(0..8, cx))
                .child(swatches(8..16, cx))
                .into_any_element(),
        ));
        if let Some(i) = typing {
            rows.push(Self::row(
                PALETTE_NAMES[i],
                None,
                self.color_field(Slot::Palette(i), theme.ansi[i], cx),
            ));
        }
        Self::section("Colors", rows)
    }

    /// A swatch and its `#rrggbb`; click to type another.
    fn color_field(&self, slot: Slot, rgb: Rgb, cx: &mut Context<Self>) -> AnyElement {
        let typing = self.typing == Some(Typing::Color(slot));
        let text = if typing {
            if self.color_fresh {
                // As selected: what's typed replaces it.
                hex(rgb)
            } else {
                format!("{}▏", self.color_text)
            }
        } else {
            hex(rgb)
        };
        div()
            .id(ElementId::Name(format!("color-{slot:?}").into()))
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .size(px(18.))
                    .rounded(px(4.))
                    .bg(color(rgb))
                    .border_1()
                    .border_color(ui(0xffffff30)),
            )
            .child(
                div()
                    .w(px(78.))
                    .px_2()
                    .py(px(3.))
                    .rounded(px(5.))
                    .bg(if typing && self.color_fresh { ui(0x3478f6ff) } else { ui(0x0000004d) })
                    .border_1()
                    .border_color(if typing { ui(0x8fb4ff99) } else { ui(0xffffff1a) })
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .text_color(ui(0xffffffee))
                    .child(text),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.start_color(slot, window, cx)))
            .into_any_element()
    }

    fn start_color(&mut self, slot: Slot, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(cx);
        self.typing = Some(Typing::Color(slot));
        self.color_text.clear();
        self.color_fresh = true;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Save the color typed, if one was.
    pub(super) fn commit_color(&mut self, cx: &mut Context<Self>) {
        let Some(Typing::Color(slot)) = self.typing else { return };
        self.typing = None;
        if self.color_fresh {
            return cx.notify();
        }
        let Some(rgb) = parse_hex(&self.color_text) else {
            self.status = Some(format!("“{}” isn't a color: #rrggbb", self.color_text));
            return cx.notify();
        };
        let theme = self.config.theme();
        let current = match slot {
            Slot::Background => theme.background,
            Slot::Foreground => theme.foreground,
            Slot::Cursor => theme.cursor,
            Slot::Selection => theme.selection,
            Slot::Palette(i) => theme.ansi[i],
        };
        if rgb == current {
            return cx.notify();
        }
        let name = self.config.colors.name.clone().map(|name| {
            if name.ends_with(" (edited)") { name } else { format!("{name} (edited)") }
        });
        self.edit(cx, |file| {
            match slot {
                Slot::Background => file.set_color("background", rgb),
                Slot::Foreground => file.set_color("foreground", rgb),
                Slot::Cursor => file.set_color("cursor", rgb),
                Slot::Selection => file.set_color("selection", rgb),
                Slot::Palette(i) => {
                    let mut palette = theme.ansi;
                    palette[i] = rgb;
                    file.set_palette(&palette);
                },
            }
            file.set_colors_name(name.as_deref());
        });
    }

    /// Typing a color. Returns whether the key was used.
    pub(super) fn color_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !matches!(self.typing, Some(Typing::Color(_))) {
            return false;
        }
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" | "tab" => {
                self.commit_color(cx);
                return true;
            },
            "escape" => self.typing = None,
            "backspace" => {
                self.color_text.pop();
                self.color_fresh = false;
            },
            _ => match keystroke.key_char.as_deref() {
                Some(c)
                    if !keystroke.modifiers.platform
                        && c.chars().all(|c| c.is_ascii_hexdigit() || c == '#') =>
                {
                    if self.color_fresh {
                        self.color_text.clear();
                        self.color_fresh = false;
                    }
                    if self.color_text.len() + c.len() <= 7 {
                        self.color_text.push_str(c);
                    }
                },
                _ => return false,
            },
        }
        cx.notify();
        true
    }
}
