//! A machine's login, over the window, when it needs you: ssh said
//! something and went quiet (a sign-in in the browser, a push to approve, a
//! key to touch), or connecting keeps failing and you asked why. What ssh
//! said, as it said it, and what can be done: open the address it gave,
//! log in in a tab (where ssh can ask for anything), try again.

use super::*;

/// What's shown.
#[derive(Clone, Debug)]
pub enum Sheet {
    /// The login waits for you: what ssh said, the address to sign in at if
    /// it gave one, and the attempt's process.
    Waits { server: ServerId, name: String, said: Vec<String>, url: Option<String>, pid: i32 },
    /// Connecting failed: what went wrong, and what ssh said.
    Failed { server: ServerId, name: String, error: Option<String>, said: Vec<String> },
}

impl Sheet {
    fn server(&self) -> ServerId {
        match self {
            Sheet::Waits { server, .. } | Sheet::Failed { server, .. } => *server,
        }
    }
}

/// Show `sheet` in the window in front (or any).
pub fn show(sheet: Sheet, cx: &mut App) {
    let views = app_state::app_views(cx);
    let front = cx.active_window();
    let Some(view) = views
        .iter()
        .find(|view| Some(view.read(cx).window_handle()) == front)
        .or(views.first())
        .cloned()
    else {
        return;
    };
    // Shown in one window: elsewhere it goes.
    let server = sheet.server();
    for other in views.iter().filter(|v| **v != view) {
        other.update(cx, |other, cx| other.dismiss_login(server, cx));
    }
    let handle = view.read(cx).window_handle();
    view.update(cx, |view, cx| {
        view.login = Some(sheet);
        cx.notify();
    });
    let _ = handle.update(cx, |_, window, _| window.activate_window());
}

/// `server`'s sheet, wherever it shows, is moot.
pub fn dismiss(server: ServerId, cx: &mut App) {
    for view in app_state::app_views(cx) {
        view.update(cx, |view, cx| view.dismiss_login(server, cx));
    }
}

/// What ssh said that may be for someone (not its routine notices).
pub fn worth_showing(output: &[String]) -> Vec<String> {
    output
        .iter()
        .filter(|line| !artty_core::remote::routine(line))
        .map(|line| line.trim_start_matches(['#', ' ']).trim_end().to_owned())
        .collect()
}

impl WindowView {
    fn dismiss_login(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if self.login.as_ref().is_some_and(|sheet| sheet.server() == server) {
            self.login = None;
            cx.notify();
        }
    }

    /// A tab on this Mac running `command`, in front.
    pub fn open_command_tab(
        &mut self,
        command: Vec<String>,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(window) = self.window_on(LOCAL) {
            let spec = PaneSpec { command: Some(command), cwd: None, title: Some(title) };
            self.send(ClientMessage::NewTab { window, workspace: None, spec }, cx);
        }
    }

    /// Put the sheet away (a login waiting goes on waiting).
    fn hide_login(&mut self, cx: &mut Context<Self>) {
        self.login = None;
        cx.notify();
    }

    /// Its main button: the browser; a tab to log in in; trying again.
    fn login_primary(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.login.clone() else { return };
        match sheet {
            Sheet::Waits { url: Some(url), .. } => cx.open_url(&url),
            Sheet::Waits { server, pid, .. } => self.login_in_tab(server, Some(pid), cx),
            Sheet::Failed { server, .. } => {
                self.hide_login(cx);
                cx.defer(move |cx| app_state::connect_remote(server, cx));
            },
        }
    }

    fn login_in_tab(&mut self, server: ServerId, pid: Option<i32>, cx: &mut Context<Self>) {
        self.hide_login(cx);
        let view = cx.entity();
        // After this view's update: it opens a tab in this view.
        cx.defer(move |cx| app_state::login_in_tab(server, pid, &view, cx));
    }

    pub(super) fn login_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.login.is_none() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.hide_login(cx),
            "enter" => self.login_primary(cx),
            _ => return false,
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn login_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let sheet = self.login.as_ref()?;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .flex_none()
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .bg(if primary { ui(0x3478f6ff) } else { ui(0xffffff14) })
                .hover(|d| d.bg(if primary { ui(0x4a88ffff) } else { ui(0xffffff26) }))
                .text_color(ui(0xffffffe0))
                .child(label)
        };
        let (title, note, said, error) = match sheet {
            Sheet::Waits { name, said, url, .. } => (
                format!("{name} is waiting for you"),
                if url.is_some() {
                    "Sign in in the browser, and artty connects."
                } else {
                    "Do what it asks, and artty connects. If it wants something typed, log in in a tab."
                },
                said,
                None,
            ),
            Sheet::Failed { name, said, error, .. } => (
                format!("Couldn't connect to {name}"),
                "What ssh said, trying:",
                said,
                error.clone(),
            ),
        };
        let lines = div()
            .id("login-said")
            .max_h(px(220.))
            .overflow_y_scroll()
            .px_2()
            .py(px(6.))
            .rounded(px(6.))
            .bg(ui(0x0000004d))
            .font_family("Menlo")
            .text_size(px(11.5))
            .text_color(ui(0xffffffc8))
            .when(said.is_empty(), |d| d.text_color(ui(0xffffff60)).child("(nothing)"))
            .children(said.iter().map(|line| div().child(line.clone())));
        let mut buttons: Vec<gpui::Stateful<gpui::Div>> = Vec::new();
        match sheet {
            Sheet::Waits { server, pid, url, .. } => {
                let (server, pid) = (*server, *pid);
                buttons.push(button("login-cancel", "Cancel Login", false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.hide_login(cx);
                        cx.defer(move |cx| app_state::cancel_login(server, pid, cx));
                    },
                )));
                buttons.push(
                    button("login-hide", "Hide", false)
                        .on_click(cx.listener(|this, _, _, cx| this.hide_login(cx))),
                );
                buttons.push(button("login-tab", "Log In in a Tab", url.is_none()).on_click(
                    cx.listener(move |this, _, _, cx| this.login_in_tab(server, Some(pid), cx)),
                ));
                if url.is_some() {
                    buttons.push(
                        button("login-browser", "Open Browser", true)
                            .on_click(cx.listener(|this, _, _, cx| this.login_primary(cx))),
                    );
                }
            },
            Sheet::Failed { server, said, error, .. } => {
                let server = *server;
                let copied = error.iter().chain(said).cloned().collect::<Vec<_>>().join("\n");
                buttons.push(button("login-copy", "Copy", false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied.clone()));
                        this.show_toast("Copied", cx);
                    },
                )));
                buttons.push(button("login-scratch", "Reconnect from Scratch", false).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.hide_login(cx);
                        cx.defer(move |cx| app_state::reconnect_from_scratch(server, cx));
                    }),
                ));
                buttons.push(button("login-tab", "Log In in a Tab", false).on_click(
                    cx.listener(move |this, _, _, cx| this.login_in_tab(server, None, cx)),
                ));
                buttons.push(
                    button("login-close", "Close", false)
                        .on_click(cx.listener(|this, _, _, cx| this.hide_login(cx))),
                );
                buttons.push(
                    button("login-retry", "Try Again", true)
                        .on_click(cx.listener(|this, _, _, cx| this.login_primary(cx))),
                );
            },
        }
        let card = div()
            .w(px(520.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .rounded(px(10.))
            .bg(ui(0x1e2430f8))
            .border_1()
            .border_color(ui(0xffffff24))
            .shadow_lg()
            .text_size(px(13.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(ui(0xffffffee)).child(title))
            .when_some(error, |d, error| d.child(div().text_color(ui(0xf7768eff)).child(error)))
            .child(div().text_size(px(12.)).text_color(ui(0xffffffa0)).child(note))
            .child(lines)
            .child(div().flex().flex_wrap().justify_end().gap_2().children(buttons));
        Some(
            div()
                .id("login-sheet")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(ui(0x00000066))
                .occlude()
                .child(card),
        )
    }
}
