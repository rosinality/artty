//! "Needs you", at the top of the sidebar: what agents on every server need
//! from the user (the servers' `queue`). A permission request is answered
//! on its card (Allow, an "always" option the agent offered, Deny); an agent
//! waiting, or a reply, opens its pane; a reply can also be put away.
//! Opening a pane marks its replies read (the server does, once it's seen).

use artty_core::protocol::{QueueItem, QueueKind};

use super::*;

/// Cards shown before "N more".
const SHOWN: usize = 4;

/// Colors: a card's edge by what it needs.
const WAITING: u32 = 0xe0af68ff;
const REPLY: u32 = 0x9ece6aff;
const DENY: u32 = 0xf7768eff;

/// Which item a card is: its server, kind and id.
type Key = (ServerId, QueueKind, u64);

fn key(item: &QueueItem) -> Key {
    (ids::server_of(item.pane), item.kind, item.id)
}

/// How long ago, shortly: "12s", "4m", "2h", "3d".
fn ago(since_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let secs = now.saturating_sub(since_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// `text` on one line, at most `chars` characters.
fn line(text: &str, chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = flat.chars().take(chars).collect();
    if short.len() < flat.len() {
        short.push('…');
    }
    short
}

impl WindowView {
    /// The section, or nothing when nothing needs the user.
    pub(super) fn needs_you(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let items = app_state::queue(cx);
        if items.is_empty() {
            return None;
        }
        let count = items.len();
        let shown = if self.queue_all { count } else { count.min(SHOWN) };
        let heading = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px_2()
            .pb_1()
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui(0xffffff70))
            .child(div().flex_1().child("NEEDS YOU"))
            .child(
                div()
                    .px(px(6.))
                    .rounded(px(8.))
                    .bg(ui(WAITING))
                    .text_color(ui(0x1a1b26ff))
                    .child(count.to_string()),
            );
        let cards: Vec<gpui::AnyElement> = items
            .iter()
            .take(shown)
            .map(|item| self.queue_card(item, cx).into_any_element())
            .collect();
        let more = (count > SHOWN).then(|| {
            let label = if self.queue_all {
                "Show fewer".to_owned()
            } else {
                format!("{} more…", count - SHOWN)
            };
            div()
                .id("queue-more")
                .px_2()
                .py(px(2.))
                .rounded(px(6.))
                .text_size(px(11.5))
                .text_color(ui(0xffffff70))
                .hover(|d| d.bg(ui(0xffffff0a)).text_color(ui(0xffffffc0)))
                .child(label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.queue_all = !this.queue_all;
                    cx.notify();
                }))
        });
        Some(
            div()
                .id("needs-you")
                .flex()
                .flex_col()
                .gap(px(4.))
                .mb(px(8.))
                // Never more than about half the sidebar: the tabs stay in reach.
                .max_h(gpui::relative(0.5))
                .overflow_y_scroll()
                .child(heading)
                .children(cards)
                .children(more)
                .into_any_element(),
        )
    }

    fn queue_card(&self, item: &QueueItem, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let key = key(item);
        let (edge, what) = match item.kind {
            QueueKind::Approval => (WAITING, "permission"),
            QueueKind::Waiting => (WAITING, "waiting"),
            QueueKind::Reply => (REPLY, "replied"),
        };
        // The tab's title, as the sidebar shows it, and the agent.
        let place = match item.agent.as_deref() {
            Some(agent) => format!("{} · {agent}", item.title),
            None => item.title.clone(),
        };
        let group: SharedString = format!("queue-{}-{}-{}", key.0, key.1 as u8, key.2).into();
        let top = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(11.))
            .text_color(ui(0xffffff80))
            .child(
                div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(place),
            )
            .child(div().flex_none().child(format!("{what} · {}", ago(item.since_ms))))
            .when(item.kind == QueueKind::Reply, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("{group}-dismiss")))
                        .flex_none()
                        .opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                        .px(px(3.))
                        .rounded(px(3.))
                        .hover(|d| d.bg(ui(0xffffff18)).text_color(ui(0xffffffee)))
                        .child("×")
                        .tooltip(|_, cx| cx.new(|_| Tooltip("Mark read".into())).into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.dismiss_reply(key, cx);
                        })),
                )
            });
        let body = match item.kind {
            QueueKind::Approval => div()
                .font_family(self.metrics.font.family.clone())
                .text_size(px(11.5))
                .text_color(ui(0xffffffe0))
                .child(line(&item.text, 160)),
            QueueKind::Waiting => {
                div().text_size(px(12.)).text_color(ui(0xffffffd0)).child(line(&item.text, 160))
            },
            QueueKind::Reply => div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .when(!item.context.trim().is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui(0xffffff70))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(format!("› {}", line(&item.context, 80))),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(ui(0xffffffd0))
                        .child(line(&item.text, 220)),
                ),
        };
        let card = div()
            .id(group.clone())
            .group(group.clone())
            .flex()
            .flex_col()
            .gap(px(4.))
            .mx_1()
            .px(px(7.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(ui(0xffffff0c))
            .border_l_2()
            .border_color(ui(edge))
            .child(top)
            .child(body);
        match item.kind {
            QueueKind::Approval => card.child(self.approval_actions(item, key, cx)),
            // The card opens its pane.
            _ => {
                let pane = item.pane;
                card.cursor_pointer()
                    .hover(|d| d.bg(ui(0xffffff14)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_queue_pane(pane, cx)))
            },
        }
    }

    /// Allow, Always…, Deny and Open; with Always… chosen, the agent's options.
    fn approval_actions(&self, item: &QueueItem, key: Key, cx: &mut Context<Self>) -> gpui::Div {
        let button = |id: String, label: &str, fg: u32, bg: u32| {
            div()
                .id(SharedString::from(id))
                .px(px(7.))
                .py(px(1.))
                .rounded(px(5.))
                .text_size(px(11.5))
                .bg(ui(bg))
                .text_color(ui(fg))
                .hover(|d| d.bg(ui(0xffffff24)))
                .child(label.to_owned())
        };
        let id = format!("approval-{}-{}", key.0, key.2);
        let choosing = self.queue_always == Some(key);
        let pane = item.pane;
        let mut row = div().flex().flex_wrap().items_center().gap(px(5.)).child(
            button(format!("{id}-allow"), "Allow", 0x1a1b26ff, REPLY)
                .on_click(cx.listener(move |this, _, _, cx| this.decide(key, true, None, cx))),
        );
        if !item.options.is_empty() {
            row = row.child(
                button(
                    format!("{id}-always"),
                    if choosing { "Always ▴" } else { "Always ▾" },
                    0xffffffd0,
                    0xffffff12,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.queue_always =
                        if this.queue_always == Some(key) { None } else { Some(key) };
                    cx.notify();
                })),
            );
        }
        row = row
            .child(
                button(format!("{id}-deny"), "Deny", DENY, 0xffffff12)
                    .on_click(cx.listener(move |this, _, _, cx| this.decide(key, false, None, cx))),
            )
            .child(div().flex_1())
            .child(
                button(format!("{id}-open"), "↗", 0xffffffb0, 0x00000000)
                    .tooltip(|_, cx| cx.new(|_| Tooltip("Show the agent's pane".into())).into())
                    .on_click(cx.listener(move |this, _, _, cx| this.open_queue_pane(pane, cx))),
            );
        let options = choosing.then(|| {
            div().flex().flex_col().gap(px(2.)).children(item.options.iter().enumerate().map(
                |(index, label)| {
                    div()
                        .id(SharedString::from(format!("{id}-option-{index}")))
                        .px(px(6.))
                        .py(px(3.))
                        .rounded(px(5.))
                        .text_size(px(11.5))
                        .text_color(ui(0xffffffd0))
                        .bg(ui(0xffffff0a))
                        .hover(|d| d.bg(ui(0xffffff1c)).text_color(ui(0xffffffee)))
                        .child(label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.decide(key, true, Some(index), cx)
                        }))
                },
            ))
        });
        div().flex().flex_col().gap(px(4.)).child(row).children(options)
    }

    /// Answer a permission request on its server.
    fn decide(&mut self, key: Key, allow: bool, always: Option<usize>, cx: &mut Context<Self>) {
        let (server, _, id) = key;
        self.queue_always = None;
        let request = Request::Decide { id, allow, message: None, always };
        self.queue_request(server, request, cx);
    }

    fn dismiss_reply(&mut self, key: Key, cx: &mut Context<Self>) {
        let (server, _, id) = key;
        self.queue_request(server, Request::QueueDismiss { id }, cx);
    }

    /// Send `request`, telling the user if it didn't work (answered at the
    /// agent's prompt meanwhile, say). The server's next queue shows the
    /// rest.
    fn queue_request(&mut self, server: ServerId, request: Request, cx: &mut Context<Self>) {
        let answer = app_state::control_on(server, request, cx);
        cx.spawn(async move |this, cx| {
            if let Ok(Response::Error { message }) = answer.await {
                let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// Show a queue item's pane: here, or brought here by its server.
    fn open_queue_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let here = self.tabs.iter().any(|tab| tab.root.panes().contains(&pane));
        if here {
            self.focus_pane(pane, cx);
        } else {
            let request = Request::Focus { tab: ids::local_id(pane) };
            drop(app_state::control_on(ids::server_of(pane), request, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(line("cargo  test\n -p x", 40), "cargo test -p x");
        assert_eq!(line("abcdef", 3), "abc…");
    }
}
