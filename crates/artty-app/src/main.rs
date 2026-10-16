mod boxdraw;
mod keys;
mod mouse;
mod terminal;
mod workspace;

use artty_core::Config;
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, actions, point, px, size,
};

use workspace::{
    CloseTab, Copy, DecreaseFontSize, IncreaseFontSize, NewTab, NextTab, Paste, PreviousTab,
    ResetFontSize, Workspace,
};

actions!(artty, [Quit]);

const USAGE: &str = "usage: artty [-e command [args...]]";

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
        Some("-h" | "--help") => {
            println!("{USAGE}");
            std::process::exit(0);
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
                items: vec![MenuItem::action("Copy", Copy), MenuItem::action("Paste", Paste)],
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
                ],
            },
        ]);

        let window_background = if config.background_opacity >= 1. {
            WindowBackgroundAppearance::Opaque
        } else if config.background_blur {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Transparent
        };
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
