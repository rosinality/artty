mod app_state;
mod boxdraw;
mod cli;
mod control;
mod hooks;
mod keys;
mod mouse;
mod notify;
mod profile;
mod settings;
mod terminal;
mod workspace;

use artty_core::control::live_sockets;
use artty_core::{Config, SavedState};
use gpui::{App, Application, KeyBinding, Menu, MenuItem, actions};

use workspace::{
    ClosePane, CloseTab, Copy, DecreaseFontSize, Find, FindNext, FindPrevious, FocusDown,
    FocusLeft, FocusRight, FocusUp, IncreaseFontSize, JumpToAttention, MoveTabDown, MoveTabUp,
    NewTab, NextPane, NextTab, Paste, PreviousPane, PreviousTab, RenameTab, ResetFontSize,
    SplitDown, SplitRight, TabSpec, ToggleZoom,
};

actions!(artty, [Quit, NewWindow, OpenSettings, RestartToUpdate]);

const USAGE: &str = "usage: artty [-e command [args...]]\n       artty help    (commands for controlling a running artty)";

/// `artty -e command args...` runs a command in the first tab instead of the shell.
fn parse_args() -> Option<Vec<String>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => None,
        Some("-e") => {
            let command: Vec<String> = args.collect();
            if command.is_empty() {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            Some(command)
        }
        Some("-h" | "--help" | "help") => {
            std::process::exit(cli::main(vec!["help".into(), "--help".into()]));
        }
        Some(command) if cli::COMMANDS.contains(&command) => {
            std::process::exit(cli::main(std::env::args().skip(1).collect()));
        }
        Some(other) if other.starts_with("-psn_") => None, // Launched from Finder.
        Some(other) => {
            eprintln!("artty: unknown argument {other:?}\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "artty".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Restart to Update", RestartToUpdate),
                MenuItem::separator(),
                MenuItem::action("Quit artty", Quit),
            ],
        },
        Menu {
            name: "Shell".into(),
            items: vec![
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("New Tab", NewTab),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Close Pane", ClosePane),
                MenuItem::action("Close Tab", CloseTab),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
                MenuItem::separator(),
                MenuItem::action("Find…", Find),
                MenuItem::action("Find Next", FindNext),
                MenuItem::action("Find Previous", FindPrevious),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Bigger", IncreaseFontSize),
                MenuItem::action("Smaller", DecreaseFontSize),
                MenuItem::action("Default Size", ResetFontSize),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Show Next Tab", NextTab),
                MenuItem::action("Show Previous Tab", PreviousTab),
                MenuItem::action("Jump to Tab Needing Attention", JumpToAttention),
                MenuItem::separator(),
                MenuItem::action("Rename Tab…", RenameTab),
                MenuItem::action("Move Tab Up", MoveTabUp),
                MenuItem::action("Move Tab Down", MoveTabDown),
                MenuItem::separator(),
                MenuItem::action("Next Pane", NextPane),
                MenuItem::action("Previous Pane", PreviousPane),
                MenuItem::action("Select Pane Left", FocusLeft),
                MenuItem::action("Select Pane Right", FocusRight),
                MenuItem::action("Select Pane Above", FocusUp),
                MenuItem::action("Select Pane Below", FocusDown),
                MenuItem::action("Zoom Pane", ToggleZoom),
            ],
        },
    ]
}

fn main() {
    let command = parse_args();
    let (config, _) = Config::load();

    // Only one instance restores and saves tabs; later ones start fresh.
    let first_instance = live_sockets().is_empty();
    let save_enabled = config.restore_sessions && first_instance;
    let restore = (save_enabled && command.is_none()).then(SavedState::load).flatten();

    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &NewWindow, cx| app_state::new_window(cx));
        cx.on_action(|_: &OpenSettings, cx| app_state::open_settings(cx));
        cx.on_action(|_: &RestartToUpdate, cx| app_state::restart(cx));
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
        ]);
        workspace::bind_keys(cx);
        cx.set_menus(menus());
        app_state::init(save_enabled, cx);
        notify::init();

        let windows = restore.map(SavedState::into_windows).unwrap_or_default();
        if windows.is_empty() {
            app_state::open_window(None, TabSpec { command, ..Default::default() }, cx);
        }
        for saved in windows {
            app_state::open_window(Some(saved), TabSpec::default(), cx);
        }
        cx.activate(true);
    });
}
