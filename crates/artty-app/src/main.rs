mod boxdraw;
mod cli;
mod control;
mod keys;
mod mouse;
mod tab;
mod terminal;
mod workspace;

use artty_core::Config;
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, px, size,
};

use workspace::{
    CloseTab, Copy, DecreaseFontSize, Find, FindNext, FindPrevious, IncreaseFontSize,
    JumpToAttention, NewTab, NextTab, Paste, PreviousTab, ResetFontSize, Workspace,
};

actions!(artty, [Quit]);

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
        Some(other) => {
            eprintln!("artty: unknown argument {other:?}\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn main() {
    let command = parse_args();
    let (config, config_error) = Config::load();

    Application::new().run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        workspace::bind_keys(cx);
        cx.set_menus(vec![
            Menu { name: "artty".into(), items: vec![MenuItem::action("Quit artty", Quit)] },
            Menu {
                name: "Shell".into(),
                items: vec![
                    MenuItem::action("New Tab", NewTab),
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
                ],
            },
        ]);

        let window_background = workspace::background_appearance(&config);
        let bounds = Bounds::centered(None, size(px(1100.), px(680.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("artty".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(14.))),
                }),
                window_background,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(config, config_error, command, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
