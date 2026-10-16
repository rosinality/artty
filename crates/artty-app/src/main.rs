mod keys;
mod terminal;
mod workspace;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, actions, point, px, size,
};

use workspace::{CloseTab, NewTab, NextTab, Paste, PreviousTab, Workspace};

actions!(artty, [Quit]);

fn main() {
    Application::new().run(|cx: &mut App| {
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
            Menu { name: "Edit".into(), items: vec![MenuItem::action("Paste", Paste)] },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("Show Next Tab", NextTab),
                    MenuItem::action("Show Previous Tab", PreviousTab),
                ],
            },
        ]);

        let bounds = Bounds::centered(None, size(px(1100.), px(680.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("artty".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(14.))),
                }),
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
