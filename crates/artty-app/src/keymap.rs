//! Key bindings: the defaults, then the user's from `[keys]` in the config,
//! put in again whenever the config changes.
//!
//! An action is named in kebab case (`new-tab` is `artty::NewTab`);
//! `"none"` takes a default binding away; `{ send = "..." }` types text into
//! the focused pane.

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
            KeyAction::Named(name) => match cx.build_action(&action_name(name), None) {
                Ok(action) => action,
                Err(_) => {
                    problems.push(format!("{keystrokes}: no action {name:?}"));
                    continue;
                },
            },
            KeyAction::Send { send } => Box::new(SendText { text: send.clone() }),
        };
        match KeyBinding::load(keystrokes, action, None, false, None, cx.keyboard_mapper().as_ref())
        {
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
    let keys = Config::load().0.keys;
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
    fn kebab_names_map_to_actions() {
        assert_eq!(action_name("new-tab"), "artty::NewTab");
        assert_eq!(action_name("jump-to-attention"), "artty::JumpToAttention");
    }
}
