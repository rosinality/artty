//! Status line templates: text with values and styles, tmux-like.
//!
//! `{name}` puts in a value; `#[fg=COLOR]`, `#[bg=COLOR]`, `#[bold]`,
//! `#[dim]` style what follows (several at once: `#[fg=red,bold]`), and
//! `#[default]` goes back to plain (`fg=default`, `nobold` and the like for
//! one of them). Colors are the ANSI names (`red`, `brightblue`…) or
//! `#rrggbb`. `{{`, `}}` and `##` are `{`, `}` and `#`.
//!
//! Spaces: a word (text between spaces, values in it included) whose
//! values all came out empty is left out, with the spaces after it (or,
//! for the last word, before it). So `{git.branch}{git.dirty} {cmd:gpu}`
//! is `main* 31%`, `main 31%`, `31%` or `main`, never with a stray space;
//! and `✘{exit}` shows only with an exit code. Text without values always
//! shows.

use artty_vt::ansi::Rgb;

use crate::protocol::{StatusLine, StatusSegment};
use crate::theme::Theme;

/// A color as written: an ANSI one (0–15, in the theme) or exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Ansi(u8),
    Rgb(Rgb),
}

impl Color {
    pub fn parse(text: &str) -> Option<Color> {
        const NAMES: [&str; 8] =
            ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
        let text = text.trim().to_ascii_lowercase();
        if text.starts_with('#') {
            return text.parse().ok().map(Color::Rgb);
        }
        let (bright, name) = match text.strip_prefix("bright") {
            Some(name) => (8, name.trim_start_matches(['-', '_'])),
            None => (0, text.as_str()),
        };
        let index = NAMES.iter().position(|n| *n == name)?;
        Some(Color::Ansi(bright + index as u8))
    }

    fn resolve(self, theme: &Theme) -> Rgb {
        match self {
            Color::Ansi(index) => theme.ansi[usize::from(index) % 16],
            Color::Rgb(rgb) => rgb,
        }
    }
}

/// What a `#[…]` changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleChange {
    Fg(Option<Color>),
    Bg(Option<Color>),
    Bold(bool),
    Dim(bool),
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    Text(String),
    Value(String),
    Style(StyleChange),
}

/// A parsed template.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Template {
    pub tokens: Vec<Token>,
}

/// The style text is in, as it's being rendered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
}

impl Style {
    fn apply(&mut self, change: StyleChange) {
        match change {
            StyleChange::Fg(color) => self.fg = color,
            StyleChange::Bg(color) => self.bg = color,
            StyleChange::Bold(on) => self.bold = on,
            StyleChange::Dim(on) => self.dim = on,
            StyleChange::Default => *self = Style::default(),
        }
    }
}

/// A value as it's shown: its text, and whether it's dimmed (a command's
/// warning).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filled {
    pub text: String,
    pub dim: bool,
}

impl From<String> for Filled {
    fn from(text: String) -> Self {
        Filled { text, dim: false }
    }
}

impl Template {
    /// Parse a template; what it can't make out stays as text, and style
    /// words it doesn't know are logged and skipped.
    pub fn parse(text: &str) -> Template {
        let mut tokens = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            let escaped = ["{{", "}}", "##"].into_iter().find(|e| rest.starts_with(e));
            if let Some(escaped) = escaped {
                literal.push_str(&escaped[..1]);
                rest = &rest[2..];
                continue;
            }
            if c == '{'
                && let Some(end) = rest.find('}')
                && !rest[1..end].contains('{')
            {
                let name = rest[1..end].trim();
                if !name.is_empty() {
                    if !literal.is_empty() {
                        tokens.push(Token::Text(std::mem::take(&mut literal)));
                    }
                    tokens.push(Token::Value(name.to_owned()));
                    rest = &rest[end + 1..];
                    continue;
                }
            }
            if rest.starts_with("#[")
                && let Some(end) = rest.find(']')
            {
                if !literal.is_empty() {
                    tokens.push(Token::Text(std::mem::take(&mut literal)));
                }
                for word in rest[2..end].split([',', ' ']).filter(|w| !w.is_empty()) {
                    match style_change(word) {
                        Some(change) => tokens.push(Token::Style(change)),
                        None => log::warn!("status: unknown style #[{word}]"),
                    }
                }
                rest = &rest[end + 1..];
                continue;
            }
            literal.push(c);
            rest = &rest[c.len_utf8()..];
        }
        if !literal.is_empty() {
            tokens.push(Token::Text(literal));
        }
        Template { tokens }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The values it shows, by name (`git.branch`, `cmd:gpu`, `time:%H`).
    pub fn values(&self) -> impl Iterator<Item = &str> {
        self.tokens.iter().filter_map(|t| match t {
            Token::Value(name) => Some(name.as_str()),
            _ => None,
        })
    }

    /// The template with `value` filling in each value, its colors from
    /// `theme`; nothing at all if that's only spaces.
    pub fn render(
        &self,
        theme: &Theme,
        mut value: impl FnMut(&str) -> Filled,
    ) -> Vec<StatusSegment> {
        let mut atoms: Vec<Atom> = Vec::new();
        let mut style = Style::default();
        for token in &self.tokens {
            match token {
                Token::Style(change) => style.apply(*change),
                Token::Text(text) => {
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        let space = rest.starts_with(char::is_whitespace);
                        let end =
                            rest.find(|c: char| c.is_whitespace() != space).unwrap_or(rest.len());
                        let part = (rest[..end].to_owned(), style);
                        match atoms.last_mut() {
                            Some(Atom::Space(parts)) if space => parts.push(part),
                            Some(Atom::Word { parts, .. }) if !space => parts.push(part),
                            _ if space => atoms.push(Atom::Space(vec![part])),
                            _ => atoms.push(Atom::Word { parts: vec![part], values: 0, filled: 0 }),
                        }
                        rest = &rest[end..];
                    }
                },
                Token::Value(name) => {
                    let filled = value(name);
                    if !matches!(atoms.last(), Some(Atom::Word { .. })) {
                        atoms.push(Atom::Word { parts: Vec::new(), values: 0, filled: 0 });
                    }
                    let Some(Atom::Word { parts, values, filled: count }) = atoms.last_mut() else {
                        unreachable!()
                    };
                    *values += 1;
                    if !filled.text.is_empty() {
                        *count += 1;
                        parts.push((filled.text, Style { dim: style.dim || filled.dim, ..style }));
                    }
                },
            }
        }
        // Empty words out, each with the spaces after it (the last word's,
        // before it).
        let empty: Vec<bool> = atoms
            .iter()
            .map(|a| matches!(a, Atom::Word { values, filled: 0, .. } if *values > 0))
            .collect();
        let mut kept: Vec<bool> = empty.iter().map(|e| !e).collect();
        for i in (0..atoms.len()).filter(|&i| empty[i]) {
            let space = |j: &usize| matches!(atoms[*j], Atom::Space(_));
            let before = (0..i).rev().find(|&j| kept[j]).filter(space);
            let after = (i + 1..atoms.len()).find(|&j| kept[j]).filter(space);
            let last = !(i + 1..atoms.len()).any(|j| !empty[j] && !space(&j));
            let gone = if last { before.or(after) } else { after.or(before) };
            if let Some(j) = gone {
                kept[j] = false;
            }
        }
        let parts: Vec<(String, Style)> = atoms
            .into_iter()
            .zip(kept)
            .filter(|(_, kept)| *kept)
            .flat_map(|(atom, _)| match atom {
                Atom::Space(parts) | Atom::Word { parts, .. } => parts,
            })
            .collect();
        if parts.iter().all(|(text, _)| text.trim().is_empty()) {
            return Vec::new();
        }
        let mut segments: Vec<(String, Style)> = Vec::new();
        for (text, style) in parts {
            match segments.last_mut() {
                Some((last, s)) if *s == style => last.push_str(&text),
                _ => segments.push((text, style)),
            }
        }
        segments
            .into_iter()
            .map(|(text, style)| StatusSegment {
                text,
                fg: style.fg.map(|c| c.resolve(theme)),
                bg: style.bg.map(|c| c.resolve(theme)),
                bold: style.bold,
                dim: style.dim,
            })
            .collect()
    }
}

/// A run of spaces, or of the rest, as rendered: its pieces in their
/// styles; a word counts its values and those not empty.
enum Atom {
    Space(Vec<(String, Style)>),
    Word { parts: Vec<(String, Style)>, values: usize, filled: usize },
}

fn style_change(word: &str) -> Option<StyleChange> {
    let color = |text: &str| match text {
        "default" => Some(None),
        _ => Color::parse(text).map(Some),
    };
    Some(match word.trim() {
        "default" | "none" => StyleChange::Default,
        "bold" => StyleChange::Bold(true),
        "nobold" => StyleChange::Bold(false),
        "dim" => StyleChange::Dim(true),
        "nodim" => StyleChange::Dim(false),
        word => match word.split_once('=') {
            Some(("fg", c)) => StyleChange::Fg(color(c)?),
            Some(("bg", c)) => StyleChange::Bg(color(c)?),
            _ => return None,
        },
    })
}

/// Both sides.
pub fn line(
    left: &Template,
    right: &Template,
    theme: &Theme,
    mut value: impl FnMut(&str) -> Filled,
) -> StatusLine {
    StatusLine { left: left.render(theme, &mut value), right: right.render(theme, &mut value) }
}

/// A moment in local time, as `strftime` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1–12.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 is Sunday.
    pub weekday: u32,
}

impl LocalTime {
    pub fn now() -> LocalTime {
        // SAFETY: plain libc calls on a zeroed `tm` of our own.
        let tm = unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&now, &mut tm);
            tm
        };
        LocalTime {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
            second: tm.tm_sec as u32,
            weekday: tm.tm_wday as u32,
        }
    }

    /// `format` with `%H %M %S %d %m %Y %y %a %b %e %I %p %%` filled in;
    /// anything else stays as written.
    pub fn format(&self, format: &str) -> String {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTHS: [&str; 12] =
            ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let mut out = String::new();
        let mut chars = format.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let twelve = if self.hour.is_multiple_of(12) { 12 } else { self.hour % 12 };
            match chars.next() {
                Some('H') => out.push_str(&format!("{:02}", self.hour)),
                Some('M') => out.push_str(&format!("{:02}", self.minute)),
                Some('S') => out.push_str(&format!("{:02}", self.second)),
                Some('d') => out.push_str(&format!("{:02}", self.day)),
                Some('e') => out.push_str(&format!("{:2}", self.day)),
                Some('m') => out.push_str(&format!("{:02}", self.month)),
                Some('Y') => out.push_str(&self.year.to_string()),
                Some('y') => out.push_str(&format!("{:02}", self.year.rem_euclid(100))),
                Some('I') => out.push_str(&format!("{twelve:02}")),
                Some('p') => out.push_str(if self.hour < 12 { "AM" } else { "PM" }),
                Some('a') => out.push_str(DAYS[self.weekday as usize % 7]),
                Some('b') => out.push_str(MONTHS[(self.month as usize + 11) % 12]),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                },
                None => out.push('%'),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendered with these values (the rest empty): the text, and where
    /// each styled piece starts.
    fn render(template: &str, values: &[(&str, &str)]) -> Vec<StatusSegment> {
        Template::parse(template).render(&Theme::default(), |name| {
            let text = values.iter().find(|(n, _)| *n == name).map_or("", |(_, v)| v);
            Filled::from(text.to_owned())
        })
    }

    fn text(template: &str, values: &[(&str, &str)]) -> String {
        render(template, values).iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn values_fill_in() {
        let values = [("git.branch", "main"), ("git.dirty", "*"), ("cmd:gpu", "31%")];
        assert_eq!(text("{git.branch}{git.dirty} {cmd:gpu}", &values), "main* 31%");
        assert_eq!(text("on { git.branch }!", &values), "on main!");
        assert_eq!(text("{{literal}} ## {git.branch}", &values), "{literal} # main");
        // Not a value: as written.
        assert_eq!(text("a {} b { c", &values), "a {} b { c");
        assert_eq!(text("", &values), "");
    }

    #[test]
    fn empty_values_take_their_spaces_with_them() {
        let all = [("a", "A"), ("b", "B"), ("c", "C")];
        let t = "{a} {b} {c}";
        assert_eq!(text(t, &all), "A B C");
        assert_eq!(text(t, &[("b", "B"), ("c", "C")]), "B C");
        assert_eq!(text(t, &[("a", "A"), ("c", "C")]), "A C");
        assert_eq!(text(t, &[("a", "A"), ("b", "B")]), "A B");
        assert_eq!(text(t, &[("b", "B")]), "B");
        assert_eq!(text(t, &[]), "");
        assert_eq!(text("{a} {b} {c}", &[("a", "A")]), "A");
        // A word goes only if all its values are empty; text alone stays.
        assert_eq!(text("{a}{b} x", &[("b", "B")]), "B x");
        assert_eq!(text("{a}{b} x", &[]), "x");
        assert_eq!(text("✘{a} | {b}", &[("b", "B")]), "| B");
        assert_eq!(text("✘{a} | {b}", &[("a", "1")]), "✘1 |");
        // Padding around it is kept, but not on its own.
        assert_eq!(text("  {a}  {b} ", &[("a", "A")]), "  A ");
        assert_eq!(text("  {a}  {b} ", &[]), "");
        // A value's own spaces don't split it.
        assert_eq!(text("[{a}] {b}", &[("a", "x y")]), "[x y]");
    }

    #[test]
    fn styles() {
        let t = "{a} #[fg=yellow,bold]{b} #[default]{c} #[bg=#102030 dim]d";
        let segments = render(t, &[("a", "A"), ("b", "B"), ("c", "C")]);
        let theme = Theme::default();
        let plain = StatusSegment::default();
        assert_eq!(
            segments,
            vec![
                StatusSegment { text: "A ".into(), ..plain.clone() },
                StatusSegment {
                    text: "B ".into(),
                    fg: Some(theme.ansi[3]),
                    bold: true,
                    ..plain.clone()
                },
                StatusSegment { text: "C ".into(), ..plain.clone() },
                StatusSegment {
                    text: "d".into(),
                    bg: Some(Rgb { r: 0x10, g: 0x20, b: 0x30 }),
                    dim: true,
                    ..plain.clone()
                },
            ]
        );
        // `b` empty: it and its space, in its style, go.
        let segments = render(t, &[("a", "A"), ("c", "C")]);
        assert_eq!(segments[0].text, "A C ");
        assert_eq!(segments[0].fg, None);
        assert_eq!(Color::parse("brightred"), Some(Color::Ansi(9)));
        assert_eq!(Color::parse("bright-blue"), Some(Color::Ansi(12)));
        assert_eq!(Color::parse("chartreuse"), None);
        // Unknown style words are skipped; `fg=default` undoes one color.
        let t = Template::parse("#[sparkly,fg=red]x#[fg=default]y");
        assert_eq!(
            t.tokens,
            vec![
                Token::Style(StyleChange::Fg(Some(Color::Ansi(1)))),
                Token::Text("x".into()),
                Token::Style(StyleChange::Fg(None)),
                Token::Text("y".into()),
            ]
        );
    }

    #[test]
    fn dimmed_values() {
        let t = Template::parse("gpu {cmd:gpu}");
        let segments = t.render(&Theme::default(), |_| Filled { text: "⚠".into(), dim: true });
        assert_eq!(segments.len(), 2);
        assert!(!segments[0].dim && segments[1].dim);
        assert_eq!(segments[1].text, "⚠");
    }

    #[test]
    fn times() {
        let time =
            LocalTime { year: 2026, month: 11, day: 6, hour: 9, minute: 5, second: 7, weekday: 5 };
        assert_eq!(time.format("%H:%M:%S"), "09:05:07");
        assert_eq!(time.format("%a %d %b %Y"), "Fri 06 Nov 2026");
        assert_eq!(time.format("%m/%e/%y %I%p"), "11/ 6/26 09AM");
        assert_eq!(time.format("100%% %q %"), "100% %q %");
        let afternoon = LocalTime { hour: 12, ..time };
        assert_eq!(afternoon.format("%I %p"), "12 PM");
        let now = LocalTime::now();
        assert!(now.year >= 2024 && (1..=12).contains(&now.month) && now.hour < 24);
    }
}
