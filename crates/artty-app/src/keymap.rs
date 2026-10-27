//! Key bindings: the defaults, then the user's from `[keys]` in the config,
//! put in again whenever the config changes.
//!
//! An action is named in kebab case (`new-tab` is `artty::NewTab`);
//! `"none"` takes a default binding away; `{ send = "..." }` types text into
//! the focused pane. Ghostty's way is read too: `cmd+e>1` for the keys
//! (`cmd-e 1`), `"text:è"` for text (with its escapes).

use std::time::SystemTime;

use artty_core::{Config, KeyAction};
use gpui::{Action, App, Global, KeyBinding, NoAction};

use crate::window_view::SendText;

/// The config as of the last install, to notice changes.
#[derive(Default)]
struct Installed {
    modified: Option<SystemTime>,
    keys: Option<std::collections::BTreeMap<String, KeyAction>>,
}

impl Global for Installed {}

fn config_modified() -> Option<SystemTime> {
    std::fs::metadata(Config::path()?).ok()?.modified().ok()
}

/// `new-tab` → `artty::NewTab`.
fn action_name(name: &str) -> String {
    let pascal: String = name
        .split(['-', '_'])
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
        })
        .collect();
    format!("artty::{pascal}")
}

/// Ghostty's keystrokes (`super+e>one`) as GPUI's (`cmd-e 1`); others as
/// they are.
pub(crate) fn keystrokes(written: &str) -> String {
    if !written.contains(['+', '>']) {
        return written.to_owned();
    }
    // Chords, `>` between them (a `>` key ends with one: `shift+>`).
    let mut chords: Vec<String> = Vec::new();
    for part in written.split('>') {
        match chords.last_mut() {
            Some(last) if part.is_empty() => last.push('>'),
            _ => chords.push(part.to_owned()),
        }
    }
    chords
        .iter()
        .filter(|chord| !chord.is_empty())
        .map(|chord| {
            // The key is last (`+` itself, written `ctrl++`).
            let (modifiers, key) = match chord.strip_suffix("++") {
                Some(rest) => (rest, "+".to_owned()),
                None => match chord.rsplit_once('+') {
                    Some((modifiers, key)) => (modifiers, key.to_owned()),
                    None => ("", chord.clone()),
                },
            };
            let mut out: Vec<String> = modifiers
                .split('+')
                .filter(|m| !m.is_empty())
                .map(|m| match m.to_lowercase().as_str() {
                    "super" | "command" | "cmd" => "cmd".to_owned(),
                    "control" | "ctrl" => "ctrl".to_owned(),
                    "option" | "opt" | "alt" => "alt".to_owned(),
                    other => other.to_owned(),
                })
                .collect();
            out.push(key_name(&key));
            out.join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ghostty's key names as GPUI's.
fn key_name(key: &str) -> String {
    const DIGITS: [&str; 10] =
        ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine"];
    let lower = key.to_lowercase();
    if let Some(digit) = DIGITS.iter().position(|d| *d == lower) {
        return digit.to_string();
    }
    if let Some(digit) = lower.strip_prefix("digit_") {
        return digit.to_owned();
    }
    match lower.as_str() {
        "arrow_up" => "up",
        "arrow_down" => "down",
        "arrow_left" => "left",
        "arrow_right" => "right",
        "page_up" => "pageup",
        "page_down" => "pagedown",
        "return" => "enter",
        _ => return if key.chars().count() == 1 { key.to_owned() } else { lower },
    }
    .to_owned()
}

/// Ghostty's `text:` escapes: `\xHH` (bytes, of UTF-8), `\u{HEX}`, `\n`,
/// `\r`, `\t`, `\e`, `\\`, `\"`.
pub(crate) fn unescape(text: &str) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut chars = text.chars().peekable();
    let push = |bytes: &mut Vec<u8>, c: char| {
        let mut buffer = [0; 4];
        bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            push(&mut bytes, c);
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('t') => bytes.push(b'\t'),
            Some('e') => bytes.push(0x1b),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) => bytes.push(byte),
                    Err(_) => bytes.extend_from_slice(format!("\\x{hex}").as_bytes()),
                }
            },
            Some('u') if chars.peek() == Some(&'{') => {
                chars.next();
                let hex: String = chars.by_ref().take_while(|&c| c != '}').collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => push(&mut bytes, c),
                    None => bytes.extend_from_slice(format!("\\u{{{hex}}}").as_bytes()),
                }
            },
            Some(other) => push(&mut bytes, other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The defaults, then the user's bindings. Returns what couldn't be bound.
fn install(cx: &mut App, keys: &std::collections::BTreeMap<String, KeyAction>) -> Vec<String> {
    cx.clear_key_bindings();
    crate::bind_default_keys(cx);
    crate::window_view::bind_keys(cx);
    let mut problems = Vec::new();
    let mut bindings = Vec::new();
    for (keystrokes, what) in keys {
        let action: Box<dyn Action> = match what {
            KeyAction::Named(name) if name == "none" => Box::new(NoAction),
            KeyAction::Named(name) if name.starts_with("text:") => {
                Box::new(SendText { text: unescape(&name["text:".len()..]) })
            },
            KeyAction::Named(name) => match cx.build_action(&action_name(name), None) {
                Ok(action) => action,
                Err(_) => {
                    problems.push(format!("{keystrokes}: no action {name:?}"));
                    continue;
                },
            },
            KeyAction::Send { send } => Box::new(SendText { text: send.clone() }),
        };
        let keys = self::keystrokes(keystrokes);
        match KeyBinding::load(&keys, action, None, false, None, cx.keyboard_mapper().as_ref()) {
            Ok(binding) => bindings.push(binding),
            Err(err) => problems.push(format!("{keystrokes}: {err}")),
        }
    }
    cx.bind_keys(bindings);
    // Menus show their shortcuts as of when they're set.
    cx.set_menus(crate::menus());
    problems
}

/// Install the bindings if the config changed since (or on the first call).
pub fn refresh(cx: &mut App) {
    let modified = config_modified();
    let first = !cx.has_global::<Installed>();
    if !first && cx.global::<Installed>().modified == modified {
        return;
    }
    let config = Config::load().0;
    let timeout = config.key_sequence_timeout;
    gpui::set_pending_input_timeout(
        (timeout > 0.).then(|| std::time::Duration::from_secs_f32(timeout)),
    );
    let keys = config.keys;
    let unchanged = !first && cx.global::<Installed>().keys.as_ref() == Some(&keys);
    cx.set_global(Installed { modified, keys: Some(keys.clone()) });
    if unchanged {
        return;
    }
    let problems = install(cx, &keys);
    if !problems.is_empty() {
        log::warn!("key bindings: {}", problems.join("; "));
        crate::notify::post("artty: some key bindings were skipped", &problems.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_keys_and_text() {
        assert_eq!(keystrokes("cmd+e>1"), "cmd-e 1");
        assert_eq!(keystrokes("super+e>one"), "cmd-e 1");
        assert_eq!(keystrokes("ctrl+shift+arrow_up"), "ctrl-shift-up");
        assert_eq!(keystrokes("ctrl+a>digit_2"), "ctrl-a 2");
        assert_eq!(keystrokes("ctrl++"), "ctrl-+");
        assert_eq!(keystrokes("shift+>"), "shift->");
        // artty's own: as they are.
        assert_eq!(keystrokes("cmd-e 1"), "cmd-e 1");
        assert_eq!(unescape("è"), "è");
        assert_eq!(unescape("\\xc3\\xa8"), "è");
        assert_eq!(unescape("\\u{e9}"), "é");
        assert_eq!(unescape("a\\nb\\e[A"), "a\nb\x1b[A");
    }

    #[test]
    fn kebab_names_map_to_actions() {
        assert_eq!(action_name("new-tab"), "artty::NewTab");
        assert_eq!(action_name("jump-to-attention"), "artty::JumpToAttention");
    }
}
