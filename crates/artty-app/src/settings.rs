//! The Settings window: common options, written to the config file (which
//! every window reloads when it changes).

use artty_core::config_file::{ConfigFile, THEMES, ThemePreset};
use artty_core::{Config, CursorStyle};
use gpui::{
    AnyElement, App, AppContext, Bounds, Context, ElementId, FocusHandle, FontWeight, Hsla,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, div, point,
    prelude::*, px, rgba, size,
};

use crate::hooks::{self, Status};

#[path = "settings_keys.rs"]
mod keys;
#[path = "settings_servers.rs"]
mod servers;
#[path = "settings_themes.rs"]
mod themes;
use crate::terminal::hsla;

/// The page shown.
#[derive(Clone, Copy, PartialEq)]
enum Page {
    Appearance,
    Behavior,
    Keys,
    Sessions,
    Machines,
    Agents,
}

impl Page {
    const ALL: [Page; 6] = [
        Page::Appearance,
        Page::Behavior,
        Page::Keys,
        Page::Sessions,
        Page::Machines,
        Page::Agents,
    ];

    fn name(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Behavior => "Behavior",
            Page::Keys => "Keys",
            Page::Sessions => "Sessions",
            Page::Machines => "Machines",
            Page::Agents => "Agents",
        }
    }
}

/// A text field being typed into.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Typing {
    ThemeFilter,
    ActionFilter,
    KeyText,
}

/// A number setting edited with `−` and `+`, or typed in.
#[derive(Clone, Copy)]
struct Number {
    key: &'static str,
    value: f64,
    step: f64,
    min: f64,
    max: f64,
    format: fn(f64) -> String,
    integer: bool,
    /// Typed values are shown values: 85 for 85% is 0.85 times 100.
    scale: f64,
}

/// A number being typed into a stepper.
struct Editing {
    key: &'static str,
    text: String,
    /// Just opened: the first keystroke replaces the text, as when selected.
    fresh: bool,
    step: f64,
    min: f64,
    max: f64,
    integer: bool,
    scale: f64,
}

/// Font families offered in the picker: installed families whose names look
/// monospaced, plus the system monospaced font.
const MONO_HINTS: &[&str] = &[
    "Mono",
    "Code",
    "Menlo",
    "Monaco",
    "Courier",
    "Consolas",
    "Hack",
    "Iosevka",
    "Fira",
    "Cascadia",
    "JetBrains",
    "Hasklig",
    "Inconsolata",
    "Terminus",
    "Victor",
];

pub struct Settings {
    config: Config,
    error: Option<String>,
    fonts: Vec<String>,
    font_menu: bool,
    status: Option<String>,
    editing: Option<Editing>,
    /// The form adding another machine's server, when open.
    adding: Option<servers::AddServer>,
    /// A server whose Remove was clicked once; a second click removes it.
    removing: Option<String>,
    page: Page,
    typing: Option<Typing>,
    theme_filter: String,
    /// Ghostty's themes, once read (empty while reading, or without it).
    ghostty: Option<Vec<artty_core::theme_import::ImportedTheme>>,
    /// The theme last chosen here, by name.
    chosen_theme: Option<String>,
    /// The key binding being made, and its keys being recorded.
    adding_key: Option<keys::AddKey>,
    recording: Option<gpui::Subscription>,
    focus: FocusHandle,
}

/// Open the Settings window, or bring the open one forward.
pub fn open(
    existing: Option<WindowHandle<Settings>>,
    cx: &mut App,
) -> Option<WindowHandle<Settings>> {
    if let Some(handle) = existing
        && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return Some(handle);
    }
    let bounds = Bounds::centered(None, size(px(560.), px(680.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Settings".into()),
            appears_transparent: true,
            // Where macOS itself puts them in a 40 pt titlebar.
            traffic_light_position: Some(point(px(12.), px(13.))),
        }),
        window_background: gpui::WindowBackgroundAppearance::Blurred,
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    cx.open_window(options, |window, cx| cx.new(|cx| Settings::new(window, cx)))
        .inspect_err(|err| log::error!("opening settings: {err}"))
        .ok()
}

impl Settings {
    fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let (config, error) = Config::load();
        let mut fonts: Vec<String> = window
            .text_system()
            .all_font_names()
            .into_iter()
            .filter(|name| {
                !name.starts_with('.') && MONO_HINTS.iter().any(|hint| name.contains(hint))
            })
            .collect();
        fonts.push("SF Mono".to_owned());
        fonts.extend(config.font_family.iter().cloned());
        fonts.sort_by_key(|name| name.to_lowercase());
        fonts.dedup();
        Self {
            config,
            error,
            fonts,
            font_menu: false,
            status: None,
            editing: None,
            adding: None,
            removing: None,
            page: Page::Appearance,
            typing: None,
            theme_filter: String::new(),
            ghostty: None,
            chosen_theme: None,
            adding_key: None,
            recording: None,
            focus: cx.focus_handle(),
        }
    }

    /// Change the config file, then reload it.
    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ConfigFile)) {
        let result = ConfigFile::open().and_then(|mut file| {
            change(&mut file);
            file.save()
        });
        self.status = result.err().map(|err| format!("Couldn't save: {err}"));
        let (config, error) = Config::load();
        self.config = config;
        self.error = error;
        cx.notify();
    }

    fn current_theme(&self) -> Option<&'static ThemePreset> {
        let background = self.config.colors.background?;
        THEMES.iter().find(|theme| {
            let hex = format!("#{:02x}{:02x}{:02x}", background.r, background.g, background.b);
            theme.background.eq_ignore_ascii_case(&hex)
        })
    }

    // Controls.

    fn toggle(
        &self,
        id: &'static str,
        on: bool,
        key: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .w(px(38.))
            .h(px(22.))
            .flex_none()
            .rounded_full()
            .p(px(2.))
            .bg(if on { rgba(0x3478f6ff) } else { rgba(0xffffff26) })
            .flex()
            .when(on, |d| d.justify_end())
            .child(div().size(px(18.)).rounded_full().bg(rgba(0xffffffff)).shadow_sm())
            .on_click(
                cx.listener(move |this, _, _, cx| this.edit(cx, |file| file.set_bool(key, !on))),
            )
            .into_any_element()
    }

    /// Start typing a number in place of its stepper's value.
    fn start_editing(&mut self, number: &Number, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(cx);
        // Shown units, without float noise: 85, 1.2, 14.
        let shown = (number.value * number.scale * 1000.).round() / 1000.;
        self.editing = Some(Editing {
            key: number.key,
            text: shown.to_string(),
            fresh: true,
            step: number.step,
            min: number.min,
            max: number.max,
            integer: number.integer,
            scale: number.scale,
        });
        window.focus(&self.focus);
        cx.notify();
    }

    /// Save the typed number (clamped to its range), if any.
    fn commit_editing(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else { return };
        let Ok(typed) = editing.text.trim().parse::<f64>() else {
            self.status = Some(format!("“{}” isn't a number", editing.text.trim()));
            return cx.notify();
        };
        let value = (typed / editing.scale).clamp(editing.min, editing.max);
        let key = editing.key;
        self.edit(cx, |file| {
            if editing.integer {
                file.set_int(key, value.round() as i64)
            } else {
                file.set_float(key, value)
            }
        });
    }

    fn key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.server_form_key(event, cx)
            || self.theme_filter_key(event, cx)
            || self.key_form_key(event, cx)
        {
            return cx.stop_propagation();
        }
        let Some(editing) = self.editing.as_mut() else { return };
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" | "tab" => return self.commit_editing(cx),
            "escape" => self.editing = None,
            "backspace" => {
                if editing.fresh {
                    editing.text.clear();
                } else {
                    editing.text.pop();
                }
                editing.fresh = false;
            },
            "up" | "down" => {
                // Step from what's typed, staying in range.
                let shown = editing.text.trim().parse::<f64>().unwrap_or(0.);
                let delta =
                    editing.step * editing.scale * if keystroke.key == "up" { 1. } else { -1. };
                let next =
                    (shown + delta).clamp(editing.min * editing.scale, editing.max * editing.scale);
                editing.text = ((next * 1000.).round() / 1000.).to_string();
                editing.fresh = false;
            },
            _ => match keystroke.key_char.as_deref() {
                Some(c)
                    if !keystroke.modifiers.platform
                        && c.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') =>
                {
                    if editing.fresh {
                        editing.text.clear();
                        editing.fresh = false;
                    }
                    editing.text.push_str(c);
                },
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// `−  value  +` for a number setting; click the value to type one.
    fn stepper(&self, number: Number, cx: &mut Context<Self>) -> AnyElement {
        let Number { key, value, step, min, max, format, integer, .. } = number;
        let editing = self.editing.as_ref().filter(|e| e.key == key);
        let field = div()
            .id(ElementId::Name(format!("{key}-value").into()))
            .min_w(px(64.))
            .h(px(22.))
            .px(px(6.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .text_color(rgba(0xffffffee))
            .map(|d| match editing {
                Some(editing) => {
                    d.bg(rgba(0x0000004d)).border_1().border_color(rgba(0x8fb4ffcc)).child(
                        if editing.fresh {
                            div()
                                .px(px(1.))
                                .rounded(px(2.))
                                .bg(rgba(0x3478f6ff))
                                .child(editing.text.clone())
                        } else {
                            div().child(format!("{}▏", editing.text))
                        },
                    )
                },
                None => {
                    d.cursor_text().hover(|d| d.bg(rgba(0xffffff12))).child(format(value)).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.start_editing(&number, window, cx)
                        }),
                    )
                },
            });
        let button = |label: &'static str, next: f64, cx: &mut Context<Self>| {
            let next = next.clamp(min, max);
            let enabled = (next - value).abs() > f64::EPSILON;
            div()
                .id(ElementId::Name(format!("{key}-{label}").into()))
                .w(px(26.))
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .text_color(if enabled { rgba(0xffffffdd) } else { rgba(0xffffff40) })
                .when(enabled, |d| {
                    d.hover(|d| d.bg(rgba(0xffffff1a))).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.editing = None;
                            this.edit(cx, |file| {
                                if integer {
                                    file.set_int(key, next.round() as i64)
                                } else {
                                    file.set_float(key, next)
                                }
                            })
                        },
                    ))
                })
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(8.))
            .bg(rgba(0xffffff12))
            .child(button("−", value - step, cx))
            .child(field)
            .child(button("+", value + step, cx))
            .into_any_element()
    }

    fn button(
        &self,
        id: impl Into<ElementId>,
        label: &'static str,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .px_3()
            .py(px(4.))
            .rounded(px(7.))
            .bg(rgba(0xffffff1a))
            .hover(|d| d.bg(rgba(0xffffff2a)))
            .text_color(rgba(0xffffffee))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            .into_any_element()
    }

    /// List | Accordion | Switcher.
    fn sidebar_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.config.sidebar_style;
        let options = artty_core::SidebarStyle::ALL.map(|style| {
            let selected = style == current;
            let label = match style {
                artty_core::SidebarStyle::List => "List",
                artty_core::SidebarStyle::Accordion => "Accordion",
                artty_core::SidebarStyle::Switcher => "Switcher",
            };
            div()
                .id(ElementId::Name(format!("sidebar-{}", style.name()).into()))
                .px_2()
                .py(px(4.))
                .rounded(px(6.))
                .when(selected, |d| d.bg(rgba(0x3478f6ff)))
                .when(!selected, |d| d.hover(|d| d.bg(rgba(0xffffff1a))))
                .text_color(rgba(0xffffffee))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |file| file.set_string("sidebar-style", style.name()))
                }))
        });
        div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(rgba(0xffffff12))
            .children(options)
            .into_any_element()
    }

    /// Block | Bar | Underline | Hollow, with a preview of each.
    fn cursor_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.config.cursor_style;
        let options = CursorStyle::ALL.map(|style| {
            let selected = style == current;
            let glyph = div().w(px(9.)).h(px(15.)).map(|d| match style {
                CursorStyle::Block => d.bg(rgba(0xffffffdd)),
                CursorStyle::Bar => d.child(div().w(px(2.)).h_full().bg(rgba(0xffffffdd))),
                CursorStyle::Underline => {
                    d.flex().items_end().child(div().w_full().h(px(2.)).bg(rgba(0xffffffdd)))
                },
                CursorStyle::Hollow => d.border_1().border_color(rgba(0xffffffdd)),
            });
            let label = match style {
                CursorStyle::Block => "Block",
                CursorStyle::Bar => "Bar",
                CursorStyle::Underline => "Underline",
                CursorStyle::Hollow => "Hollow",
            };
            div()
                .id(ElementId::Name(format!("cursor-{}", style.name()).into()))
                .flex()
                .items_center()
                .gap(px(6.))
                .px_2()
                .py(px(4.))
                .rounded(px(6.))
                .when(selected, |d| d.bg(rgba(0x3478f6ff)))
                .when(!selected, |d| d.hover(|d| d.bg(rgba(0xffffff1a))))
                .text_color(rgba(0xffffffee))
                .child(glyph)
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.edit(cx, |file| file.set_string("cursor-style", style.name()))
                }))
        });
        div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(rgba(0xffffff12))
            .children(options)
            .into_any_element()
    }

    /// One row per agent: whether artty's hooks are installed, and buttons
    /// to install, update or remove them.
    fn agent_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        hooks::AGENTS
            .iter()
            .map(|agent| {
                let status = agent.status();
                let (label, color) = match &status {
                    Status::Installed => ("Installed", rgba(0x9ece6aff)),
                    Status::Outdated => ("Outdated", rgba(0xe0af68ff)),
                    Status::NotInstalled => ("Not installed", rgba(0xffffff80)),
                    Status::AgentNotFound => ("Agent not found", rgba(0xffffff50)),
                    Status::Error(_) => ("Can't read settings", rgba(0xf7768eff)),
                };
                let id = agent.id;
                let install = |label: &'static str, cx: &mut Context<Self>| {
                    self.button(
                        ElementId::Name(format!("install-{id}").into()),
                        label,
                        cx,
                        move |this, cx| {
                            let report = hooks::agent(id).and_then(|agent| agent.install());
                            this.status = Some(report.unwrap_or_else(|err| err));
                            cx.notify();
                        },
                    )
                };
                let uninstall = |cx: &mut Context<Self>| {
                    self.button(
                        ElementId::Name(format!("uninstall-{id}").into()),
                        "Uninstall",
                        cx,
                        move |this, cx| {
                            let report = hooks::agent(id).and_then(|agent| agent.uninstall());
                            this.status = Some(report.unwrap_or_else(|err| err));
                            cx.notify();
                        },
                    )
                };
                let buttons: Vec<AnyElement> = match status {
                    Status::Installed => vec![uninstall(cx)],
                    Status::Outdated => vec![install("Update", cx), uninstall(cx)],
                    Status::NotInstalled => vec![install("Install", cx)],
                    Status::AgentNotFound | Status::Error(_) => vec![],
                };
                let hint: &'static str = match agent.id {
                    "claude" => "~/.claude/settings.json",
                    "codex" => "~/.codex/hooks.json · approve with /hooks",
                    _ => "~/.gemini/settings.json",
                };
                Self::row(
                    agent.name,
                    Some(hint),
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_size(px(12.)).text_color(color).child(label))
                        .children(buttons)
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn font_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let current =
            self.config.font_family.first().cloned().unwrap_or_else(|| "SF Mono".to_owned());
        let list = self.font_menu.then(|| {
            div()
                .id("font-list")
                .absolute()
                .top(px(30.))
                .right_0()
                .w(px(240.))
                .max_h(px(260.))
                .overflow_y_scroll()
                .p(px(4.))
                .rounded(px(8.))
                .bg(rgba(0x262a33fa))
                .border_1()
                .border_color(rgba(0xffffff26))
                .shadow_lg()
                .occlude()
                .children(self.fonts.iter().enumerate().map(|(i, font)| {
                    let name = font.clone();
                    let selected = *font == current;
                    div()
                        .id(("font", i))
                        .px_2()
                        .py(px(3.))
                        .rounded(px(5.))
                        .font_family(SharedString::from(font.clone()))
                        .text_color(if selected { rgba(0x8fb4ffff) } else { rgba(0xffffffdd) })
                        .hover(|d| d.bg(rgba(0x3478f6ff)).text_color(rgba(0xffffffff)))
                        .child(font.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.font_menu = false;
                            let mut families = vec![name.clone()];
                            if name != "Menlo" {
                                families.push("Menlo".to_owned());
                            }
                            this.edit(cx, |file| file.set_strings("font-family", &families));
                        }))
                }))
        });
        div()
            .relative()
            .child(
                div()
                    .id("font-button")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(4.))
                    .rounded(px(7.))
                    .bg(rgba(0xffffff1a))
                    .hover(|d| d.bg(rgba(0xffffff2a)))
                    .text_color(rgba(0xffffffee))
                    .child(current)
                    .child(div().text_color(rgba(0xffffff80)).child("▾"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.font_menu = !this.font_menu;
                        cx.notify();
                    })),
            )
            .children(list)
            .into_any_element()
    }

    fn theme_chips(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.current_theme().map(|theme| theme.name);
        let defaults = artty_core::Theme::default();
        let hex =
            |rgb: artty_core::vt::ansi::Rgb| format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b);
        let builtin_palette: Vec<String> = defaults.ansi.iter().map(|c| hex(*c)).collect();
        let chip = |id: usize,
                    name: &'static str,
                    background: String,
                    palette: Vec<String>,
                    selected: bool,
                    preset: Option<&'static ThemePreset>,
                    cx: &mut Context<Self>| {
            let color = |hex: &str| -> Hsla {
                let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
                gpui::rgb(value).into()
            };
            div()
                .id(("theme", id))
                .flex()
                .flex_col()
                .gap_1()
                .p(px(6.))
                .rounded(px(8.))
                .border_2()
                .border_color(if selected { rgba(0x3478f6ff) } else { rgba(0x00000000) })
                .hover(|d| d.bg(rgba(0xffffff10)))
                .child(
                    div()
                        .w(px(84.))
                        .h(px(40.))
                        .rounded(px(5.))
                        .bg(color(&background))
                        .flex()
                        .items_end()
                        .gap(px(3.))
                        .p(px(5.))
                        .children(
                            [1, 2, 3, 4, 5, 6]
                                .map(|i| div().size(px(8.)).rounded_full().bg(color(&palette[i]))),
                        ),
                )
                .child(div().text_size(px(11.5)).text_color(rgba(0xffffffcc)).child(name))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.edit(cx, |file| file.set_theme(preset))),
                )
        };
        let mut chips = vec![chip(
            0,
            "artty",
            hex(defaults.background),
            builtin_palette,
            current.is_none(),
            None,
            cx,
        )];
        for (i, theme) in THEMES.iter().enumerate() {
            let palette = theme.palette.iter().map(|s| s.to_string()).collect();
            chips.push(chip(
                i + 1,
                theme.name,
                theme.background.to_owned(),
                palette,
                current == Some(theme.name),
                Some(theme),
                cx,
            ));
        }
        div().flex().flex_wrap().gap_1().children(chips).into_any_element()
    }

    // Layout.

    fn section(title: &'static str, rows: Vec<AnyElement>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .px_1()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(0xffffff70))
                    .child(title.to_uppercase()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(10.))
                    .bg(rgba(0xffffff0d))
                    .border_1()
                    .border_color(rgba(0xffffff12))
                    .children(rows),
            )
            .into_any_element()
    }

    fn row(label: &'static str, hint: Option<&'static str>, control: AnyElement) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_3()
            .py(px(8.))
            .border_b_1()
            .border_color(rgba(0xffffff0d))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(rgba(0xffffffee)).child(label))
                    .when_some(hint, |d, hint| {
                        d.child(div().text_size(px(11.)).text_color(rgba(0xffffff70)).child(hint))
                    }),
            )
            .child(control)
            .into_any_element()
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = self.config.clone();
        let tint = Hsla { a: 0.88, ..hsla(c.theme().background) };
        let page = self.page;
        let imports =
            if page == Page::Appearance { self.theme_import_rows(cx) } else { Vec::new() };
        let mut theme_rows = vec![
            Self::row("Theme", None, div().into_any_element()),
            div().px_3().pb_2().child(self.theme_chips(cx)).into_any_element(),
        ];
        theme_rows.extend(imports);
        let themes = Self::section("Theme", theme_rows);

        let appearance = Self::section(
            "Text and window",
            vec![
                Self::row("Font", None, self.font_picker(cx)),
                Self::row(
                    "Font size",
                    None,
                    self.stepper(
                        Number {
                            key: "font-size",
                            value: c.font_size.into(),
                            step: 1.,
                            min: 8.,
                            max: 32.,
                            format: |v| format!("{v:.0} pt"),
                            integer: false,
                            scale: 1.,
                        },
                        cx,
                    ),
                ),
                Self::row(
                    "Line height",
                    None,
                    self.stepper(
                        Number {
                            key: "line-height",
                            value: c.line_height.into(),
                            step: 0.05,
                            min: 1.,
                            max: 2.,
                            format: |v| format!("{v:.2}×"),
                            integer: false,
                            scale: 1.,
                        },
                        cx,
                    ),
                ),
                Self::row(
                    "Background opacity",
                    Some("100% is opaque"),
                    self.stepper(
                        Number {
                            key: "background-opacity",
                            value: c.background_opacity.into(),
                            step: 0.05,
                            min: 0.3,
                            max: 1.,
                            format: |v| format!("{:.0}%", v * 100.),
                            integer: false,
                            scale: 100.,
                        },
                        cx,
                    ),
                ),
                Self::row(
                    "Blur behind the window",
                    None,
                    self.toggle("blur", c.background_blur, "background-blur", cx),
                ),
                Self::row("Cursor", None, self.cursor_picker(cx)),
                Self::row(
                    "Blinking cursor",
                    None,
                    self.toggle("blink", c.cursor_blink, "cursor-blink", cx),
                ),
            ],
        );
        let behavior = Self::section(
            "Behavior",
            vec![
                Self::row(
                    "Option key acts as Meta",
                    Some("Off: Option types characters like å"),
                    self.toggle("meta", c.option_as_meta, "option-as-meta", cx),
                ),
                Self::row(
                    "Copy on select",
                    None,
                    self.toggle("copy", c.copy_on_select, "copy-on-select", cx),
                ),
                Self::row(
                    "Confirm pasting several lines",
                    Some("Into programs that would run them one by one"),
                    self.toggle(
                        "confirm-paste",
                        c.confirm_multiline_paste,
                        "confirm-multiline-paste",
                        cx,
                    ),
                ),
                Self::row(
                    "Show typing at once on other machines",
                    Some("Before the echo comes back, as mosh does"),
                    self.toggle("local-echo", c.local_echo, "local-echo", cx),
                ),
                Self::row(
                    "Secure keyboard entry",
                    Some("Other apps can't read what you type here"),
                    self.toggle(
                        "secure-input",
                        c.secure_keyboard_entry,
                        "secure-keyboard-entry",
                        cx,
                    ),
                ),
                Self::row(
                    "Workspaces in the sidebar",
                    Some("All · only the current one open · only the current one, with a switcher"),
                    self.sidebar_picker(cx),
                ),
                Self::row(
                    "List a split tab's panes",
                    Some("Off: one line, for the focused pane"),
                    self.toggle("list-panes", c.list_panes, "list-panes", cx),
                ),
                Self::row(
                    "Flash on bell",
                    None,
                    self.toggle("vbell", c.visual_bell, "visual-bell", cx),
                ),
                Self::row(
                    "Sound on bell",
                    None,
                    self.toggle("sbell", c.bell_sound, "bell-sound", cx),
                ),
                Self::row(
                    "Notifications",
                    Some("When a tab needs you while you're elsewhere"),
                    self.toggle("notify", c.notifications, "notifications", cx),
                ),
                Self::row(
                    "Shell integration",
                    Some("zsh and bash report commands; fish does natively"),
                    self.toggle("si", c.shell_integration, "shell-integration", cx),
                ),
            ],
        );
        let sessions = Self::section(
            "Sessions",
            vec![
                Self::row(
                    "Reopen tabs on launch",
                    None,
                    self.toggle("restore", c.restore_sessions, "restore-sessions", cx),
                ),
                Self::row(
                    "Resume coding agents",
                    None,
                    self.toggle("resume", c.resume_agents, "resume-agents", cx),
                ),
                Self::row(
                    "Output kept per pane",
                    Some("Saved for reopened tabs; 0 saves none"),
                    self.stepper(
                        Number {
                            key: "restore-scrollback",
                            value: c.restore_scrollback as f64,
                            step: 500.,
                            min: 0.,
                            max: 10_000.,
                            format: |v| format!("{v:.0} lines"),
                            integer: true,
                            scale: 1.,
                        },
                        cx,
                    ),
                ),
            ],
        );
        let mut agent_rows = vec![
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_size(px(11.5))
                .text_color(rgba(0xffffff80))
                .child("Status hooks let agents tell artty when they're working, need you, or are done, and which conversation to resume.")
                .into_any_element(),
        ];
        agent_rows.extend(self.agent_rows(cx));
        agent_rows.push(Self::row(
            "Configuration file",
            Some("Every setting, including colors and agents"),
            self.button("open-config", "Open", cx, |_, _| {
                if let Some(path) = Config::path() {
                    if !path.exists() {
                        let _ = ConfigFile::open().and_then(|file| file.save());
                    }
                    let _ = std::process::Command::new("open").arg("-t").arg(path).spawn();
                }
            }),
        ));
        let agents = Self::section("Agents", agent_rows);
        let keys = Self::section("Your key bindings", self.keys_page(cx));
        let tabs = Page::ALL.map(|p| {
            div()
                .id(p.name())
                .px(px(10.))
                .py(px(4.))
                .rounded(px(6.))
                .when(p == page, |d| d.bg(rgba(0xffffff26)).text_color(rgba(0xffffffee)))
                .when(p != page, |d| {
                    d.text_color(rgba(0xffffffa0)).hover(|d| d.bg(rgba(0xffffff12)))
                })
                .child(p.name())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.commit_editing(cx);
                    this.page = p;
                    this.typing = None;
                    cx.notify();
                }))
        });
        let content: Vec<AnyElement> = match page {
            Page::Appearance => vec![themes, appearance],
            Page::Behavior => vec![behavior],
            Page::Keys => vec![
                keys,
                div()
                    .text_size(px(11.5))
                    .text_color(rgba(0xffffff70))
                    .child(
                        "Several keys pressed in turn make a sequence (⌘E then 1). They're \
                         saved in [keys] of the config file, which also takes Ghostty's way \
                         (cmd+e>1 = text:è).",
                    )
                    .into_any_element(),
            ],
            Page::Sessions => vec![sessions],
            Page::Machines => vec![self.servers_section(cx)],
            Page::Agents => vec![agents],
        };

        div()
            .id("settings")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // Clicking elsewhere saves a number being typed.
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| this.commit_editing(cx)))
            .size_full()
            .bg(tint)
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgba(0xffffffb8))
                    .child("Settings"),
            )
            .child(div().flex_none().flex().justify_center().gap_1().pb_3().children(tabs))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_5()
                    .pb_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .when_some(self.error.clone(), |d, error| {
                        d.child(div().text_size(px(12.)).text_color(rgba(0xf7768eff)).child(format!("Config error: {error}")))
                    })
                    .when_some(self.status.clone(), |d, status| {
                        d.child(div().text_size(px(12.)).text_color(rgba(0xffffffb0)).child(status))
                    })
                    .children(content),
            )
            // A theme file dropped here: imported.
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                if let Some(path) = paths.paths().first() {
                    this.import_theme(path, cx);
                }
            }))
    }
}
