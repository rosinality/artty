//! Files (⌘⇧E): a drawer over the terminal's right edge listing a folder on
//! the focused pane's machine — the pane's current directory, following it
//! as the shell moves, until another folder is opened. Files open here
//! (fetched first from another machine), download to ~/Downloads, or give
//! their path; files dropped on it go into the folder shown. It lies over
//! the terminal rather than narrowing it, so nothing running there resizes.

use std::path::PathBuf;

use artty_core::protocol::DirEntry;

use super::*;

const WIDTH: f32 = 300.;
/// How often a drawer following its pane looks again (the shell may have
/// moved, or files changed).
const REFRESH: Duration = Duration::from_secs(2);

pub(super) struct Files {
    /// The pane whose machine it lists (and whose directory it follows).
    pane: PaneId,
    /// The folder listed, once known.
    path: Option<PathBuf>,
    entries: Vec<DirEntry>,
    error: Option<String>,
    /// Lists the pane's current directory (else a folder opened here).
    follow: bool,
    hidden: bool,
    asked: Option<Instant>,
}

impl WindowView {
    pub(super) fn toggle_files(&mut self, cx: &mut Context<Self>) {
        if self.files.take().is_some() {
            return cx.notify();
        }
        let Some(pane) = self.focused_id() else { return };
        self.files = Some(Files {
            pane,
            path: None,
            entries: Vec::new(),
            error: None,
            follow: true,
            hidden: false,
            asked: None,
        });
        self.list_files(None, cx);
    }

    /// List `path` (`None`: the pane's current directory).
    fn list_files(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(files) = self.files.as_mut() else { return };
        files.asked = Some(Instant::now());
        let pane = files.pane;
        let answer = app_state::query(Query::ListDir { pane, path }, cx);
        cx.spawn(async move |this, cx| {
            let reply = answer.await;
            let _ = this.update(cx, |this, cx| {
                let Some(files) = this.files.as_mut().filter(|f| f.pane == pane) else { return };
                match reply {
                    Ok(Reply::Dir(Ok((path, entries)))) => {
                        if files.path.as_ref() != Some(&path) || files.entries != entries {
                            files.path = Some(path);
                            files.entries = entries;
                            cx.notify();
                        }
                        files.error = None;
                    },
                    Ok(Reply::Dir(Err(error))) => {
                        files.error = Some(error);
                        cx.notify();
                    },
                    _ => {},
                }
            });
        })
        .detach();
    }

    /// Each second: a drawer following its pane lists again now and then,
    /// and moves with the focus to another pane.
    pub(super) fn files_tick(&mut self, cx: &mut Context<Self>) {
        let focused = self.focused_id();
        let Some(files) = self.files.as_mut() else { return };
        let moved = focused.is_some_and(|pane| pane != files.pane);
        if moved && files.follow {
            files.pane = focused.unwrap_or(files.pane);
            files.entries.clear();
            files.path = None;
        }
        let due = files.asked.is_none_or(|at| at.elapsed() >= REFRESH);
        if moved && files.follow || due {
            let path = if files.follow { None } else { files.path.clone() };
            self.list_files(path, cx);
        }
    }

    fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(files) = self.files.as_mut() {
            files.follow = false;
        }
        self.list_files(Some(path), cx);
    }

    fn follow_pane(&mut self, cx: &mut Context<Self>) {
        let focused = self.focused_id();
        if let Some(files) = self.files.as_mut() {
            files.follow = true;
            files.pane = focused.unwrap_or(files.pane);
        }
        self.list_files(None, cx);
    }

    /// Open a file here: as it is on this Mac, else fetched first.
    fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(pane) = self.files.as_ref().map(|f| f.pane) else { return };
        let server = ids::server_of(pane);
        if server == LOCAL {
            let _ = std::process::Command::new("open").arg(&path).spawn();
            return;
        }
        let name = app_state::server_name(server, cx);
        let file = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.show_toast(format!("Fetching {file} from {name}…"), cx);
        cx.spawn(async move |this, cx| {
            let fetched = crate::transfer::fetch(pane, &name, &path, cx).await;
            let _ = this.update(cx, |this, cx| match fetched {
                Ok(here) => {
                    this.toast = None;
                    cx.notify();
                    let _ = std::process::Command::new("open").arg(here).spawn();
                },
                Err(err) => this.show_toast(format!("Couldn't fetch {file}: {err}"), cx),
            });
        })
        .detach();
    }

    /// Download a file to ~/Downloads and show it there; this Mac's is
    /// shown where it is.
    fn download_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(pane) = self.files.as_ref().map(|f| f.pane) else { return };
        if ids::server_of(pane) == LOCAL {
            let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
            return;
        }
        let file = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.show_toast(format!("Downloading {file}…"), cx);
        cx.spawn(async move |this, cx| {
            let downloaded = crate::transfer::download(pane, &path, cx).await;
            let _ = this.update(cx, |this, cx| match downloaded {
                Ok(here) => {
                    this.show_toast(format!("Downloaded {file}"), cx);
                    let _ = std::process::Command::new("open").arg("-R").arg(here).spawn();
                },
                Err(err) => this.show_toast(format!("Couldn't download {file}: {err}"), cx),
            });
        })
        .detach();
    }

    /// Files dropped on the drawer: into the folder it shows.
    fn drop_into_folder(&mut self, paths: &gpui::ExternalPaths, cx: &mut Context<Self>) {
        let Some((pane, Some(dir))) = self.files.as_ref().map(|f| (f.pane, f.path.clone())) else {
            return;
        };
        let paths = paths.paths().to_vec();
        let name = app_state::server_name(ids::server_of(pane), cx);
        let files =
            if paths.len() == 1 { "1 file".to_owned() } else { format!("{} files", paths.len()) };
        self.show_toast(format!("Sending {files} to {name}…"), cx);
        cx.spawn(async move |this, cx| {
            let sent = crate::transfer::send(pane, &paths, Some(dir), cx).await;
            let _ = this.update(cx, |this, cx| {
                match sent {
                    Ok(_) => this.show_toast(format!("Sent {files}"), cx),
                    Err(err) => this.show_toast(format!("Couldn't send: {err}"), cx),
                }
                let path = this.files.as_ref().and_then(|f| f.path.clone());
                this.list_files(path, cx);
            });
        })
        .detach();
    }

    pub(super) fn files_drawer(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let files = self.files.as_ref()?;
        let server = ids::server_of(files.pane);
        let path = files.path.clone();
        let shown = |path: &PathBuf| {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            match path.strip_prefix(
                home.as_deref().filter(|_| server == LOCAL).unwrap_or("/nowhere".as_ref()),
            ) {
                Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
                Ok(rest) => format!("~/{}", rest.display()),
                Err(_) => path.display().to_string(),
            }
        };
        let small_button = |id: SharedString, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex_none()
                .px(px(6.))
                .py(px(1.))
                .rounded(px(4.))
                .text_size(px(11.))
                .text_color(if on { rgba(0xffffffee) } else { rgba(0xffffff90) })
                .bg(if on { rgba(0xffffff1f) } else { rgba(0x00000000) })
                .hover(|d| d.bg(rgba(0xffffff26)))
                .child(label)
        };
        let header = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(10.))
            .py(px(8.))
            .border_b_1()
            .border_color(rgba(0xffffff14))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        Self::one_line(format!(
                            "FILES · {}",
                            app_state::server_name(server, cx).to_uppercase()
                        ))
                        .flex_1()
                        .min_w_0()
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgba(0xffffff70)),
                    )
                    .child(small_button("files-up".into(), "↑", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            let parent = this
                                .files
                                .as_ref()
                                .and_then(|f| f.path.as_ref()?.parent().map(PathBuf::from));
                            if let Some(parent) = parent {
                                this.open_folder(parent, cx);
                            }
                        },
                    )))
                    .child(
                        small_button("files-follow".into(), "Pane", files.follow)
                            .on_click(cx.listener(|this, _, _, cx| this.follow_pane(cx))),
                    )
                    .child(small_button("files-hidden".into(), ".*", files.hidden).on_click(
                        cx.listener(|this, _, _, cx| {
                            if let Some(files) = this.files.as_mut() {
                                files.hidden = !files.hidden;
                            }
                            cx.notify();
                        }),
                    ))
                    .child(
                        small_button("files-close".into(), "×", false)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_files(cx))),
                    ),
            )
            .child(
                Self::one_line(path.as_ref().map_or_else(|| "…".to_owned(), shown))
                    .text_size(px(11.5))
                    .text_color(rgba(0xffffffb0)),
            );

        let rows: Vec<gpui::AnyElement> = files
            .entries
            .iter()
            .filter(|entry| files.hidden || !entry.name.starts_with('.'))
            .enumerate()
            .map(|(i, entry)| {
                let full = path.as_ref().map(|p| p.join(&entry.name)).unwrap_or_default();
                let group: SharedString = format!("files-row-{i}").into();
                let dir = entry.dir;
                let open = full.clone();
                let copy = full.clone();
                let download = full.clone();
                div()
                    .id(("files-row", i))
                    .group(group.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(22.))
                    .px(px(10.))
                    .hover(|d| d.bg(rgba(0xffffff0c)))
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        if dir {
                            this.open_folder(open.clone(), cx);
                        } else if event.click_count() >= 2 {
                            this.open_file(open.clone(), cx);
                        }
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .text_color(if dir { rgba(0x7aa2f7ff) } else { rgba(0xffffff50) })
                            .child(if dir { "▸" } else { "·" }),
                    )
                    .child(
                        Self::one_line(if entry.link {
                            format!("{} ↗", entry.name)
                        } else {
                            entry.name.clone()
                        })
                        .flex_1()
                        .min_w_0()
                        .text_color(if entry.name.starts_with('.') {
                            rgba(0xffffff70)
                        } else {
                            rgba(0xffffffd8)
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(rgba(0xffffff50))
                            .group_hover(group.clone(), |s| s.opacity(0.))
                            .when(!dir, |d| d.child(crate::cli::bytes(entry.size))),
                    )
                    // On hover: what can be done with it.
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .absolute()
                            .right(px(8.))
                            .opacity(0.)
                            .group_hover(group, |s| s.opacity(1.))
                            .when(!dir, |d| {
                                d.child(
                                    small_button(
                                        format!("files-download-{i}").into(),
                                        if server == LOCAL { "Reveal" } else { "Download" },
                                        false,
                                    )
                                    .bg(rgba(0x1e2430ff))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.download_file(download.clone(), cx);
                                        cx.stop_propagation();
                                    })),
                                )
                            })
                            .child(
                                small_button(format!("files-copy-{i}").into(), "Copy Path", false)
                                    .bg(rgba(0x1e2430ff))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy.display().to_string(),
                                        ));
                                        this.show_toast("Path copied", cx);
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();

        Some(
            div()
                .id("files-drawer")
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(WIDTH))
                .flex()
                .flex_col()
                .bg(rgba(0x161a22f4))
                .border_l_1()
                .border_color(rgba(0xffffff1f))
                .shadow_lg()
                .text_size(px(12.5))
                // Clicks and scrolls here aren't the terminal's.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.bg(rgba(0x22304af4)))
                .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                    this.drop_into_folder(paths, cx);
                    cx.stop_propagation();
                }))
                .child(header)
                .when_some(files.error.clone(), |d, error| {
                    d.child(
                        div().px(px(10.)).py(px(6.)).text_size(px(11.5)).text_color(rgba(0xf7768eff)).child(error),
                    )
                })
                .child(
                    div()
                        .id("files-rows")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .py(px(4.))
                        .children(rows)
                        .when(empty && files.path.is_some(), |d| {
                            d.child(div().px(px(10.)).text_color(rgba(0xffffff50)).child("Nothing here"))
                        }),
                ),
        )
    }
}
