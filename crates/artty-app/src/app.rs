//! The app (macOS): windows, menus, default keys, and starting up.

use artty_core::protocol::PaneSpec;

use crate::{app_state, cli, keymap, notify, secure_input};
use gpui::{App, Application, KeyBinding, Menu, MenuItem, actions};

use crate::window_view::{
    ClosePane, CloseTab, Copy, CopyCommandOutput, CopyMode, DecreaseFontSize, EqualizePanes, Find,
    FindNext, FindPrevious, FocusDown, FocusLeft, FocusRight, FocusUp, IncreaseFontSize,
    JumpToAttention, JumpToNextPrompt, JumpToPreviousPrompt, MoveTabDown, MoveTabUp, NewTab,
    NewWorkspace, NextPane, NextTab, NextWorkspace, OpenWorkspace, PaneToNewTab, Paste,
    PreviousPane, PreviousTab, PreviousWorkspace, RenameTab, ReopenClosedTab, ResetFontSize,
    ResizePaneDown, ResizePaneLeft, ResizePaneRight, ResizePaneUp, ShowActivity, SplitDown,
    SplitRight, SwapPaneDown, SwapPaneLeft, SwapPaneRight, SwapPaneUp, ToggleBroadcast, ToggleZoom,
};

actions!(
    artty,
    [
        Quit,
        QuitAndEndSessions,
        NewWindow,
        ReopenClosedWindows,
        OpenSettings,
        RestartToUpdate,
        RestartServer,
        ToggleSecureKeyboardEntry
    ]
);

pub const USAGE: &str = "usage: artty [-e command [args...]]\n       artty help    (commands for controlling a running artty)";

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
        },
        Some("-h" | "--help" | "help") => {
            std::process::exit(cli::main(vec!["help".into(), "--help".into()]));
        },
        Some(command) if cli::COMMANDS.contains(&command) => {
            std::process::exit(cli::main(std::env::args().skip(1).collect()));
        },
        Some(other) if other.starts_with("-psn_") => None, // Launched from Finder.
        Some(other) => {
            eprintln!("artty: unknown argument {other:?}\n{USAGE}");
            std::process::exit(2);
        },
    }
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "artty".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Restart to Update", RestartToUpdate),
                MenuItem::action("Restart Server", RestartServer),
                MenuItem::separator(),
                MenuItem::action("Secure Keyboard Entry (On/Off)", ToggleSecureKeyboardEntry),
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
                MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
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
                MenuItem::separator(),
                MenuItem::action("Copy Mode", CopyMode),
                MenuItem::action("Copy Command Output", CopyCommandOutput),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Bigger", IncreaseFontSize),
                MenuItem::action("Smaller", DecreaseFontSize),
                MenuItem::action("Default Size", ResetFontSize),
                MenuItem::separator(),
                MenuItem::action("Previous Prompt", JumpToPreviousPrompt),
                MenuItem::action("Next Prompt", JumpToNextPrompt),
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
                MenuItem::separator(),
                MenuItem::action("Move Divider Left", ResizePaneLeft),
                MenuItem::action("Move Divider Right", ResizePaneRight),
                MenuItem::action("Move Divider Up", ResizePaneUp),
                MenuItem::action("Move Divider Down", ResizePaneDown),
                MenuItem::action("Make Panes Even", EqualizePanes),
                MenuItem::action("Swap with Pane Left", SwapPaneLeft),
                MenuItem::action("Swap with Pane Right", SwapPaneRight),
                MenuItem::action("Swap with Pane Above", SwapPaneUp),
                MenuItem::action("Swap with Pane Below", SwapPaneDown),
                MenuItem::action("Move Pane to New Tab", PaneToNewTab),
                MenuItem::separator(),
                MenuItem::action("Type into All Panes (On/Off)", ToggleBroadcast),
            ],
        },
    ]
}

/// The app's own default bindings (windows' are in `window_view`).
pub fn bind_default_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-n", NewWindow, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
    ]);
}

pub fn main() {
    artty_core::logging::init();
    let command = parse_args();
    keep_awake_in_background();
    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| {
            crate::forward::stop_everything(cx);
            crate::askpass::stop(cx);
            cx.quit()
        });
        crate::askpass::start(cx);
        cx.on_action(|_: &QuitAndEndSessions, cx| app_state::quit_and_end_sessions(cx));
        cx.on_action(|_: &NewWindow, cx| app_state::new_window(cx));
        cx.on_action(|_: &ReopenClosedWindows, cx| app_state::reopen_closed_windows(cx));
        cx.on_action(|_: &OpenSettings, cx| app_state::open_settings(cx));
        cx.on_action(|_: &RestartToUpdate, cx| app_state::restart(cx));
        cx.on_action(|_: &RestartServer, cx| app_state::restart_server(cx));
        cx.on_action(|_: &ToggleSecureKeyboardEntry, _| {
            if let Err(err) = secure_input::toggle_setting() {
                log::error!("secure keyboard entry: {err}");
            }
        });
        keymap::refresh(cx);
        notify::init();
        app_state::init(PaneSpec { command, ..Default::default() }, cx);
        cx.activate(true);
    });
}

/// Keep App Nap away: in the background, macOS would stretch the app's
/// timers, and with them its pings to servers (which take a silent app as
/// gone) and its notice of lost connections. The Mac still sleeps when idle.
fn keep_awake_in_background() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str("Keeping connections to servers alive"),
    );
    // For the app's whole life.
    std::mem::forget(activity);
}
