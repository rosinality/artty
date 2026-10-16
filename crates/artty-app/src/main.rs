mod app_state;
mod boxdraw;
mod cli;
mod hooks;
mod keys;
mod mouse;
mod notify;
mod profile;
mod settings;
mod terminal;
mod window_view;

use artty_core::protocol::PaneSpec;
use gpui::{App, Application, KeyBinding, Menu, MenuItem, actions};

use window_view::{
    ClosePane, CloseTab, Copy, DecreaseFontSize, Find, FindNext, FindPrevious, FocusDown,
    FocusLeft, FocusRight, FocusUp, IncreaseFontSize, JumpToAttention, MoveTabDown, MoveTabUp,
    NewTab, NewWorkspace, NextPane, NextTab, NextWorkspace, OpenWorkspace, Paste, ShowActivity, PreviousPane, PreviousTab,
    PreviousWorkspace, RenameTab, ResetFontSize, SplitDown, SplitRight, ToggleZoom,
};

actions!(artty, [Quit, QuitAndEndSessions, NewWindow, ReopenClosedWindows, OpenSettings, RestartToUpdate, RestartServer]);

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
                MenuItem::action("Restart Server", RestartServer),
                MenuItem::separator(),
                MenuItem::action("Quit artty (Sessions Keep Running)", Quit),
                MenuItem::action("Quit and End Sessions", QuitAndEndSessions),
            ],
        },
        Menu {
            name: "Shell".into(),
            items: vec![
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("Reopen Closed Windows", ReopenClosedWindows),
                MenuItem::action("New Tab", NewTab),
                MenuItem::action("New Workspace", NewWorkspace),
                MenuItem::action("Open Workspace…", OpenWorkspace),
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
                MenuItem::action("Show Next Workspace", NextWorkspace),
                MenuItem::action("Show Previous Workspace", PreviousWorkspace),
                MenuItem::action("Jump to Tab Needing Attention", JumpToAttention),
                MenuItem::action("Activity", ShowActivity),
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
    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &QuitAndEndSessions, cx| app_state::quit_and_end_sessions(cx));
        cx.on_action(|_: &NewWindow, cx| app_state::new_window(cx));
        cx.on_action(|_: &ReopenClosedWindows, cx| app_state::reopen_closed_windows(cx));
        cx.on_action(|_: &OpenSettings, cx| app_state::open_settings(cx));
        cx.on_action(|_: &RestartToUpdate, cx| app_state::restart(cx));
        cx.on_action(|_: &RestartServer, cx| app_state::restart_server(cx));
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
        ]);
        window_view::bind_keys(cx);
        cx.set_menus(menus());
        notify::init();
        app_state::init(PaneSpec { command, ..Default::default() }, cx);
        cx.activate(true);
    });
}
