//! Settings: other machines' servers. Each is listed with a Remove button;
//! "Add a server" takes a name and an ssh destination and checks the
//! connection before saving it (or saves it anyway, on request).

use artty_core::ServerConfig;
use artty_core::protocol::ServerMessage;
use artty_core::remote::Prompts;

use super::*;

/// The add form.
#[derive(Default)]
pub(super) struct AddServer {
    name: String,
    ssh: String,
    /// The field typed into: 0 the name, 1 the destination.
    field: usize,
    checking: bool,
    error: Option<String>,
}

impl Settings {
    pub(super) fn servers_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> =
            self.config.servers.iter().map(|server| self.server_row(server, cx)).collect();
        rows.push(match &self.adding {
            Some(adding) => self.add_form(adding, cx),
            None => div()
                .id("add-server")
                .px_3()
                .py(px(8.))
                .text_color(ui(0x8fb4ffff))
                .hover(|d| d.bg(ui(0xffffff08)))
                .child("Add a server…")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.commit_editing(cx);
                    this.adding = Some(AddServer::default());
                    window.focus(&this.focus);
                    cx.notify();
                }))
                .into_any_element(),
        });
        Self::section("Other machines", rows)
    }

    fn server_row(&self, server: &ServerConfig, cx: &mut Context<Self>) -> AnyElement {
        let how = match (&server.ssh, server.command.is_empty()) {
            (_, false) => server.command.join(" "),
            (Some(ssh), true) => format!("ssh {ssh}"),
            (None, true) => "no ssh destination".to_owned(),
        };
        let name = server.name.clone();
        let armed = self.removing.as_deref() == Some(name.as_str());
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_3()
            .py(px(8.))
            .border_b_1()
            .border_color(ui(0xffffff0d))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().text_color(ui(0xffffffee)).child(server.name.clone()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui(0xffffff70))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(how),
                    ),
            )
            .child(
                Self::form_button(
                    ElementId::Name(format!("remove-{name}").into()),
                    if armed { "Remove?" } else { "Remove" },
                    armed,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    // Twice: once to arm, once to remove.
                    if this.removing.as_deref() == Some(name.as_str()) {
                        this.removing = None;
                        let name = name.clone();
                        this.edit(cx, |file| {
                            file.remove_server(&name);
                        });
                    } else {
                        this.removing = Some(name.clone());
                        cx.notify();
                    }
                })),
            )
            .into_any_element()
    }

    fn add_form(&self, adding: &AddServer, cx: &mut Context<Self>) -> AnyElement {
        let field = |index: usize, label: &'static str, text: &str, hint: &'static str| {
            let focused = adding.field == index && !adding.checking;
            div()
                .id(("server-field", index))
                .flex()
                .items_center()
                .gap_3()
                .child(div().w(px(90.)).flex_none().text_color(ui(0xffffffb0)).child(label))
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py(px(3.))
                        .rounded(px(5.))
                        .bg(ui(0x0000004d))
                        .border_1()
                        .border_color(if focused { ui(0x8fb4ff99) } else { ui(0xffffff1a) })
                        .text_color(if text.is_empty() { ui(0xffffff40) } else { ui(0xffffffee) })
                        .child(match (text.is_empty(), focused) {
                            (true, _) => hint.to_owned(),
                            (false, true) => format!("{text}▏"),
                            (false, false) => text.to_owned(),
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(adding) = this.adding.as_mut() {
                        adding.field = index;
                    }
                    cx.notify();
                }))
        };
        let can_add = !adding.name.trim().is_empty() && !adding.ssh.trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py(px(10.))
            .child(field(0, "Name", &adding.name, "devbox"))
            .child(field(1, "SSH", &adding.ssh, "user@host, or a Host from ~/.ssh/config"))
            .child(
                div().text_size(px(11.)).text_color(ui(0xffffff70)).child(
                    "artty must be installed there; ssh uses your keys (no password prompts).",
                ),
            )
            .when_some(adding.error.clone(), |d, error| {
                d.child(div().text_size(px(12.)).text_color(ui(0xf7768eff)).child(error))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::form_button("cancel-server".into(), "Cancel", false).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.adding = None;
                            cx.notify();
                        }),
                    ))
                    .when(adding.error.is_some() && !adding.checking, |d| {
                        d.child(
                            Self::form_button("add-anyway".into(), "Add Anyway", false).on_click(
                                cx.listener(|this, _, _, cx| this.submit_server(false, cx)),
                            ),
                        )
                    })
                    .child(
                        Self::form_button(
                            "add-server-button".into(),
                            if adding.checking { "Checking…" } else { "Add" },
                            can_add && !adding.checking,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.submit_server(true, cx))),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn form_button(
        id: ElementId,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .px(px(10.))
            .py(px(3.))
            .rounded(px(6.))
            .bg(if primary { ui(0x3478f6ff) } else { ui(0xffffff14) })
            .hover(|d| d.bg(if primary { ui(0x4a88ffff) } else { ui(0xffffff26) }))
            .text_color(ui(0xffffffe0))
            .child(label)
    }

    /// Add the server in the form: after checking that it answers (`check`),
    /// or as is.
    fn submit_server(&mut self, check: bool, cx: &mut Context<Self>) {
        let Some(adding) = self.adding.as_mut().filter(|a| !a.checking) else { return };
        let (name, ssh) = (adding.name.trim().to_owned(), adding.ssh.trim().to_owned());
        if name.is_empty() || ssh.is_empty() {
            adding.error = Some("Give it a name and an ssh destination.".to_owned());
            return cx.notify();
        }
        let server = ServerConfig { name, ssh: Some(ssh), ..Default::default() };
        if !check {
            return self.save_server(server, cx);
        }
        adding.checking = true;
        adding.error = None;
        cx.notify();
        let probe = server.clone();
        let askpass = crate::askpass::askpass(&server.name, cx);
        let checking = cx.background_spawn(async move {
            let prompts = askpass.as_ref().map_or(Prompts::Never, Prompts::Helper);
            let mut opened = artty_core::remote::open(&probe, prompts)?;
            let _ = opened.child.kill();
            let _ = opened.child.wait();
            match opened.welcome {
                ServerMessage::Welcome { version, .. }
                    if version == artty_core::protocol::protocol_version() =>
                {
                    Ok(())
                },
                ServerMessage::Welcome { server_version, .. } => Err(format!(
                    "It runs another artty build ({server_version}). Add it anyway, then \
                     click its header in the sidebar to install this one."
                )),
                _ => Err("It answered, but not as an artty server.".to_owned()),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = checking.await;
            let _ = this.update(cx, |this, cx| {
                let Some(adding) = this.adding.as_mut() else { return };
                adding.checking = false;
                match result {
                    Ok(()) => this.save_server(server, cx),
                    Err(error) if error.starts_with(artty_core::remote::NOT_INSTALLED) => {
                        adding.error = Some(
                            "artty isn't installed there. Add it anyway, then click its header \
                             in the sidebar to install it."
                                .to_owned(),
                        );
                        cx.notify();
                    },
                    Err(error) => {
                        adding.error = Some(format!("Couldn't connect: {error}"));
                        cx.notify();
                    },
                }
            });
        })
        .detach();
    }

    fn save_server(&mut self, server: ServerConfig, cx: &mut Context<Self>) {
        self.adding = None;
        self.edit(cx, |file| file.add_server(&server));
        if self.status.is_none() {
            self.status = Some(format!("Added {}: it shows in the sidebar.", server.name));
        }
    }

    /// Typing into the add form. Returns whether the key was used.
    pub(super) fn server_form_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(adding) = self.adding.as_mut().filter(|a| !a.checking) else { return false };
        let keystroke = &event.keystroke;
        let text = if adding.field == 0 { &mut adding.name } else { &mut adding.ssh };
        match keystroke.key.as_str() {
            "enter" => {
                self.submit_server(true, cx);
                return true;
            },
            "escape" => self.adding = None,
            "tab" => adding.field = 1 - adding.field,
            "backspace" => {
                text.pop();
            },
            _ => match keystroke.key_char.as_deref() {
                Some(c)
                    if !keystroke.modifiers.platform
                        && !keystroke.modifiers.control
                        && !c.chars().any(char::is_control) =>
                {
                    text.push_str(c)
                },
                _ => return false,
            },
        }
        cx.notify();
        true
    }
}
