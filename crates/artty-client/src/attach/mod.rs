//! A server's window in this terminal, as tmux shows a session: the active
//! tab's panes as its layout places them, between borders, each drawn from
//! its screen; a status line with the workspace and its tabs (and the
//! server's `[status]`, after them and on the right); typing to the
//! focused pane, in its mode; a prefix key (Ctrl-b) for tabs, splits,
//! focus, zoom, copy mode and leaving; the mouse to focus, select and
//! scroll. Leaving (detaching) leaves everything running.

mod input;
mod screen;
mod term;

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use artty_core::TerminalSize;
use artty_core::layout::{self, Axis, Direction, PaneId, Rect};
use artty_core::protocol::{
    CopyModeOp, CopyMotion, PaneState, StatusLine, StatusSegment, TabLayout, WindowId,
    WindowLayout, WindowSummary, WorkspaceId, WorkspaceLayout,
};
use artty_core::vt::ansi::{CursorShape, Rgb};
use artty_core::vt::term::TermMode;

pub use input::KeyPress;
use input::{Decoder, Input, MouseEvent};
use screen::{Area, Cursor, Grid, Style};

use crate::{Client, Event, Link, MouseAction, MouseButton, SelectionKind, StartError};

/// What to show, and how.
pub struct Options {
    /// The window: `None` takes the newest one no app shows, else the
    /// newest of all, else a new one. One another app shows is shared (its
    /// panes take the size of the app used last).
    pub window: Option<WindowId>,
    /// A new window, whatever there is.
    pub new: bool,
    /// The key before a command (Ctrl-b).
    pub prefix: KeyPress,
}

/// How showing it ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Detached(WindowId),
    /// Its last tab closed.
    Closed(WindowId),
}

/// How long the server has to answer.
const WAIT: Duration = Duration::from_secs(10);
/// How long a sequence's first part waits for the rest (else it's Escape).
const ESCAPE_WAIT: Duration = Duration::from_millis(30);
/// How long the status line says something.
const MESSAGE_FOR: Duration = Duration::from_secs(4);
/// Lines a wheel's step scrolls.
const WHEEL_LINES: i32 = 3;

/// Colors before any screen has come.
const BACKGROUND: Rgb = Rgb { r: 0x1a, g: 0x1b, b: 0x26 };
const FOREGROUND: Rgb = Rgb { r: 0xc0, g: 0xca, b: 0xf5 };
/// The focused pane's border, the active tab.
const ACCENT: Rgb = Rgb { r: 0x7a, g: 0xa2, b: 0xf7 };

/// Greet the server over `link`, or say why not.
fn start(link: Link) -> Result<(Client, mpsc::Receiver<Event>), String> {
    Client::start(link, env!("CARGO_PKG_VERSION")).map_err(|err| match err {
        StartError::AnotherBuild { server_version } => format!(
            "the server runs another artty build ({server_version}): `artty server restart` \
             moves it to this one"
        ),
        StartError::Failed(reason) => format!("couldn't reach the server: {reason}"),
    })
}

/// The server's windows, shown or not.
pub fn windows(link: Link) -> Result<Vec<WindowSummary>, String> {
    let (client, events) = start(link)?;
    list(&client, &events)
}

fn list(client: &Client, events: &mpsc::Receiver<Event>) -> Result<Vec<WindowSummary>, String> {
    client.list_windows()?;
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left).map_err(|_| "the server didn't answer")? {
            Event::Windows(windows) => return Ok(windows),
            Event::Closed(why) => return Err(format!("lost the server: {why}")),
            _ => {},
        }
    }
}

/// Show a window here until detached (or it closes, or the server goes).
pub fn run(link: Link, options: Options) -> Result<Ended, String> {
    let (client, events) = start(link)?;
    let windows = list(&client, &events)?;
    // As tmux attaches: the window asked for, else the newest one no app
    // shows, else the newest of all; a new one only if there are none.
    let newest =
        |free: bool| windows.iter().filter(|w| !free || w.apps == 0).max_by_key(|w| w.window);
    let chosen = match options.window {
        Some(window) => Some(
            windows
                .iter()
                .find(|w| w.window == window)
                .ok_or(format!("no window {window} (artty attach --list shows them)"))?,
        ),
        None if options.new => None,
        None => newest(true).or_else(|| newest(false)),
    };
    let shared = chosen.is_some_and(|w| w.apps > 0);
    client.attach(chosen.map(|w| w.window), true)?;
    let deadline = Instant::now() + WAIT;
    let window = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left).map_err(|_| "the server didn't show the window")? {
            Event::Attached { window } => break window,
            Event::Closed(why) => return Err(format!("lost the server: {why}")),
            _ => {},
        }
    };
    client.focus(window, true)?;

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for event in events {
                if tx.send(Message::Server(event)).is_err() {
                    return;
                }
            }
        });
    }
    let ended = term::Terminal::enter().and_then(|terminal| {
        let signals = tx.clone();
        term::watch_signals(move |signal| signals.send(Message::Signal(signal)).is_ok())?;
        term::read_input(move |bytes| tx.send(Message::Typed(bytes)).is_ok());
        let ended = View::new(&client, window, options.prefix, shared).run(&rx);
        drop(terminal);
        ended
    });
    // Closed, there's nothing to let go of; else it's left running.
    if !matches!(ended, Ok(Ended::Closed(_))) {
        let _ = client.detach(window);
    }
    ended
}

enum Message {
    Typed(Vec<u8>),
    Server(Event),
    Signal(libc::c_int),
}

/// What keys do now.
#[derive(Clone, Debug, PartialEq)]
enum Mode {
    Normal,
    /// The prefix was pressed: a command next.
    Prefix,
    /// Asking for a name.
    Prompt(Prompt, String),
    /// Asking whether to (y or n).
    Confirm(Confirm),
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Prompt {
    RenameTab(WorkspaceId, usize),
    RenameWorkspace(WorkspaceId),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Confirm {
    ClosePane(PaneId),
    CloseTab(WorkspaceId, usize),
}

/// A mouse button held: where it went down, and what it does.
#[derive(Clone, Copy, Debug)]
struct Drag {
    pane: PaneId,
    area: Area,
    /// The program takes it (else it selects).
    program: bool,
    moved: bool,
}

/// The window shown here.
struct View<'a> {
    client: &'a Client,
    window: WindowId,
    prefix: KeyPress,
    layout: Option<WindowLayout>,
    panes: HashMap<PaneId, PaneState>,
    columns: usize,
    lines: usize,
    cell: (u16, u16),
    /// Each pane's size as last told the server.
    sizes: HashMap<PaneId, TerminalSize>,
    shown: Option<Grid>,
    shape: Option<(CursorShape, bool)>,
    decoder: Decoder,
    mode: Mode,
    message: Option<(String, Instant)>,
    /// The server's status line for the window (`[status]`).
    status: StatusLine,
    drag: Option<Drag>,
    /// The tab shown, and the one before.
    tab: Option<(WorkspaceId, usize)>,
    last_tab: Option<(WorkspaceId, usize)>,
    /// Where each tab is on the status line: its cells, and it.
    tab_spans: Vec<(usize, usize, WorkspaceId, usize)>,
    /// The server said it's going (to come back, if true).
    stopping: Option<bool>,
    dirty: bool,
    ended: Option<Ended>,
}

impl<'a> View<'a> {
    fn new(client: &'a Client, window: WindowId, prefix: KeyPress, shared: bool) -> Self {
        let (columns, lines, cell_width, cell_height) = term::size();
        let mut hint = format!("{} ? lists the keys", prefix.describe());
        if shared {
            hint = format!("shared with another app · {hint}");
        }
        View {
            client,
            window,
            prefix,
            layout: None,
            panes: HashMap::new(),
            columns,
            lines,
            cell: (cell_width, cell_height),
            sizes: HashMap::new(),
            shown: None,
            shape: None,
            decoder: Decoder::default(),
            mode: Mode::Normal,
            message: Some((hint, Instant::now())),
            status: StatusLine::default(),
            drag: None,
            tab: None,
            last_tab: None,
            tab_spans: Vec::new(),
            stopping: None,
            dirty: true,
            ended: None,
        }
    }

    fn run(&mut self, messages: &mpsc::Receiver<Message>) -> Result<Ended, String> {
        loop {
            let wait = if self.decoder.waiting() {
                ESCAPE_WAIT
            } else if let Some((_, at)) = &self.message {
                MESSAGE_FOR.saturating_sub(at.elapsed()) + Duration::from_millis(10)
            } else {
                Duration::from_secs(60)
            };
            match messages.recv_timeout(wait) {
                Ok(message) => self.handle(message)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    for input in self.decoder.flush() {
                        self.input(input)?;
                    }
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("lost the server".to_owned());
                },
            }
            while self.ended.is_none()
                && let Ok(message) = messages.try_recv()
            {
                self.handle(message)?;
            }
            if let Some(ended) = self.ended.take() {
                return Ok(ended);
            }
            if self.message.as_ref().is_some_and(|(_, at)| at.elapsed() >= MESSAGE_FOR) {
                self.message = None;
                self.dirty = true;
            }
            if self.dirty {
                self.dirty = false;
                self.draw();
            }
        }
    }

    fn handle(&mut self, message: Message) -> Result<(), String> {
        match message {
            Message::Typed(bytes) => {
                for input in self.decoder.feed(&bytes) {
                    self.input(input)?;
                    if self.ended.is_some() {
                        break;
                    }
                }
            },
            Message::Server(event) => self.event(event)?,
            Message::Signal(libc::SIGWINCH) => {
                let (columns, lines, cell_width, cell_height) = term::size();
                (self.columns, self.lines, self.cell) = (columns, lines, (cell_width, cell_height));
                // Whatever the terminal made of the old screen: all again.
                self.shown = None;
                self.resize()?;
                self.dirty = true;
            },
            Message::Signal(_) => self.ended = Some(Ended::Detached(self.window)),
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Layout { window, layout } if window == self.window => {
                let tab = layout.active_workspace().map(|w| (w.id, w.active));
                if tab != self.tab {
                    self.last_tab = self.tab.or(self.last_tab);
                    self.tab = tab;
                }
                self.layout = Some(layout);
                self.resize()?;
                self.dirty = true;
            },
            Event::Screen { pane } => {
                if self.placed().iter().any(|(p, _)| *p == pane) {
                    self.dirty = true;
                }
            },
            Event::Panes(states) => {
                self.panes = states.into_iter().map(|s| (s.info.id, s)).collect();
                self.dirty = true;
            },
            Event::Status { window, line } if window == self.window => {
                self.status = line;
                self.dirty = true;
            },
            Event::WindowClosed { window } if window == self.window => {
                self.ended = Some(Ended::Closed(window));
            },
            Event::Bell { pane } => {
                if self
                    .layout
                    .as_ref()
                    .is_some_and(|l| l.tabs().any(|(_, _, t)| t.root.contains(pane)))
                {
                    term::write_out("\x07");
                }
            },
            Event::Clipboard { text } => copy(&text),
            Event::Notify { title, body, window, .. }
                if window.is_none_or(|w| w == self.window) =>
            {
                let text = if title.is_empty() { body } else { format!("{title}: {body}") };
                self.say(text);
            },
            Event::Stopping { restart } => self.stopping = Some(restart),
            Event::Closed(why) => {
                return Err(match self.stopping {
                    Some(true) => "the server restarted: attach again".to_owned(),
                    Some(false) => "the server stopped".to_owned(),
                    None => format!("lost the server: {why}"),
                });
            },
            _ => {},
        }
        Ok(())
    }

    fn say(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    fn workspace(&self) -> Option<&WorkspaceLayout> {
        self.layout.as_ref()?.active_workspace()
    }

    fn active_tab(&self) -> Option<&TabLayout> {
        self.layout.as_ref()?.active_tab()
    }

    fn focused(&self) -> Option<PaneId> {
        self.active_tab().map(|tab| tab.focused)
    }

    /// Where the panes go: all but the status line.
    fn pane_area(&self) -> Area {
        Area { x: 0, y: 0, width: self.columns, height: self.lines.saturating_sub(1) }
    }

    /// The active tab's panes, and where each is, as its layout says (all
    /// of them, zoomed or not).
    fn laid_out(&self) -> Vec<(PaneId, Area)> {
        let Some(tab) = self.active_tab() else { return Vec::new() };
        let area = self.pane_area();
        let rect = Rect { x: 0., y: 0., width: area.width as f32, height: area.height as f32 };
        let (panes, _) = tab.root.layout(rect, 1.);
        panes
            .into_iter()
            .map(|(pane, r)| {
                let area = Area {
                    x: r.x as usize,
                    y: r.y as usize,
                    width: r.width as usize,
                    height: r.height as usize,
                };
                (pane, area)
            })
            .collect()
    }

    /// The panes shown, and where: the focused one alone when zoomed.
    fn placed(&self) -> Vec<(PaneId, Area)> {
        match self.active_tab() {
            Some(tab) if tab.zoomed => vec![(tab.focused, self.pane_area())],
            _ => self.laid_out(),
        }
    }

    /// Tell the server the panes' sizes here, those that changed.
    fn resize(&mut self) -> Result<(), String> {
        for (pane, area) in self.placed() {
            let size = TerminalSize {
                columns: area.width.max(1) as u16,
                lines: area.height.max(1) as u16,
                cell_width: self.cell.0,
                cell_height: self.cell.1,
            };
            if self.sizes.get(&pane) != Some(&size) {
                self.client.resize(pane, size)?;
                self.sizes.insert(pane, size);
            }
        }
        Ok(())
    }

    fn tab_title(&self, tab: &TabLayout) -> String {
        let title = tab.title.clone().or_else(|| {
            let state = self.panes.get(&tab.focused)?;
            Some(state.info.title.clone())
        });
        title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "shell".to_owned())
    }

    /// A mark for what a tab's most urgent pane is doing.
    fn tab_mark(&self, tab: &TabLayout) -> Option<char> {
        let state = tab
            .root
            .panes()
            .iter()
            .filter_map(|pane| self.panes.get(pane))
            .max_by_key(|state| state.urgency())?;
        match state.icon.as_str() {
            "waiting" | "attention" => Some('!'),
            "busy" => Some('~'),
            "failed" => Some('x'),
            "done" => Some('+'),
            _ => None,
        }
    }

    fn input(&mut self, input: Input) -> Result<(), String> {
        match input {
            Input::Focus(focused) => return self.client.focus(self.window, focused),
            Input::Mouse(event) => return self.mouse(event),
            _ => {},
        }
        match self.mode.clone() {
            Mode::Prompt(what, text) => self.prompt(what, text, input),
            Mode::Confirm(what) => {
                self.mode = Mode::Normal;
                self.dirty = true;
                if matches!(&input, Input::Text(t) if t == "y" || t == "Y") {
                    match what {
                        Confirm::ClosePane(pane) => self.client.close_pane(pane)?,
                        Confirm::CloseTab(workspace, tab) => {
                            self.client.close_tab(workspace, tab)?
                        },
                    }
                }
                Ok(())
            },
            Mode::Help => {
                self.mode = Mode::Normal;
                self.dirty = true;
                Ok(())
            },
            Mode::Prefix => {
                self.mode = Mode::Normal;
                self.dirty = true;
                self.command(input)
            },
            Mode::Normal => {
                if matches!(&input, Input::Key(key) if *key == self.prefix) {
                    self.mode = Mode::Prefix;
                    self.dirty = true;
                    return Ok(());
                }
                let Some(pane) = self.focused() else { return Ok(()) };
                let screen = self.client.screen(pane).unwrap_or_default();
                if screen.mode.contains(TermMode::VI) {
                    return self.copy_key(pane, input);
                }
                self.type_into(pane, input)
            },
        }
    }

    /// Typing, a key or a paste, into a pane (scrolled back down first).
    fn type_into(&mut self, pane: PaneId, input: Input) -> Result<(), String> {
        let screen = self.client.screen(pane).unwrap_or_default();
        if screen.frame.display_offset > 0 {
            self.client.scroll(pane, -(screen.frame.display_offset as i32))?;
        }
        match input {
            Input::Text(text) if screen.mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) => {
                for c in text.chars() {
                    let name = c.to_lowercase().to_string();
                    let key = artty_core::keys::Key {
                        key: &name,
                        shift: c.is_uppercase(),
                        ..Default::default()
                    };
                    if !self.client.key(pane, &key)? {
                        self.client.input(pane, c.to_string().into_bytes())?;
                    }
                }
                Ok(())
            },
            Input::Text(text) => self.client.input(pane, text.into_bytes()),
            Input::Key(key) => self.client.key(pane, &key.key()).map(|_| ()),
            Input::Paste(text) => self.client.paste(pane, &text),
            Input::Mouse(_) | Input::Focus(_) => Ok(()),
        }
    }

    /// What follows the prefix.
    fn command(&mut self, input: Input) -> Result<(), String> {
        let window = self.window;
        let Some(pane) = self.focused() else { return Ok(()) };
        let text = match &input {
            Input::Key(key) if *key == self.prefix => return self.type_into(pane, input),
            Input::Key(key) => {
                let direction = match key.name.as_str() {
                    "up" => Direction::Up,
                    "down" => Direction::Down,
                    "left" => Direction::Left,
                    "right" => Direction::Right,
                    "pageup" => {
                        self.client.copy_mode(pane, CopyModeOp::Enter)?;
                        return self.client.copy_mode(pane, CopyModeOp::Move(CopyMotion::PageUp));
                    },
                    _ => return Ok(()),
                };
                let panes: Vec<(PaneId, Rect)> = self
                    .laid_out()
                    .into_iter()
                    .map(|(pane, a)| {
                        let rect = Rect {
                            x: a.x as f32,
                            y: a.y as f32,
                            width: a.width as f32,
                            height: a.height as f32,
                        };
                        (pane, rect)
                    })
                    .collect();
                if let Some(next) = layout::neighbor(&panes, pane, direction) {
                    self.client.focus_pane(next)?;
                }
                return Ok(());
            },
            Input::Text(text) => text.as_str(),
            _ => return Ok(()),
        };
        let workspace = self.workspace().map(|w| (w.id, w.active, w.tabs.len()));
        let Some((workspace, tab, tabs)) = workspace else { return Ok(()) };
        match text {
            "c" => self.client.new_tab(window, None)?,
            "C" => self.client.new_workspace(window, None)?,
            "n" | "p" => {
                let all: Vec<(WorkspaceId, usize)> =
                    self.layout.iter().flat_map(|l| l.tabs().map(|(w, i, _)| (w, i))).collect();
                if let Some(at) = all.iter().position(|&t| t == (workspace, tab)) {
                    let step = if text == "n" { 1 } else { all.len() - 1 };
                    let (w, i) = all[(at + step) % all.len()];
                    self.client.select_tab(w, i)?;
                }
            },
            "l" => match self.last_tab {
                Some((w, i)) => self.client.select_tab(w, i)?,
                None => self.say("no last tab"),
            },
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                let n: usize = text.parse().unwrap_or(0);
                let index = if n == 0 { 9 } else { n - 1 };
                if index < tabs {
                    self.client.select_tab(workspace, index)?;
                } else {
                    self.say(format!("no tab {}", index + 1));
                }
            },
            "%" | "|" => self.client.split(pane, Axis::Horizontal)?,
            "\"" | "-" => self.client.split(pane, Axis::Vertical)?,
            "o" => {
                let panes = self.active_tab().map(|t| t.root.panes()).unwrap_or_default();
                if let Some(at) = panes.iter().position(|&p| p == pane) {
                    self.client.focus_pane(panes[(at + 1) % panes.len()])?;
                }
            },
            "x" => self.mode = Mode::Confirm(Confirm::ClosePane(pane)),
            "&" => self.mode = Mode::Confirm(Confirm::CloseTab(workspace, tab)),
            "z" => self.client.toggle_zoom(window)?,
            "E" => self.client.equalize(window)?,
            "!" => self.client.pane_to_new_tab(pane)?,
            "d" => self.ended = Some(Ended::Detached(window)),
            "," => {
                let title = self.active_tab().and_then(|t| t.title.clone()).unwrap_or_default();
                self.mode = Mode::Prompt(Prompt::RenameTab(workspace, tab), title);
            },
            "$" => {
                let name = self.workspace().and_then(|w| w.name.clone()).unwrap_or_default();
                self.mode = Mode::Prompt(Prompt::RenameWorkspace(workspace), name);
            },
            "(" | ")" => {
                let layout = self.layout.as_ref().unwrap();
                let count = layout.workspaces.len();
                let step = if text == ")" { 1 } else { count - 1 };
                let next = &layout.workspaces[(layout.active + step) % count];
                let (id, active) = (next.id, next.active);
                self.client.select_tab(id, active)?;
            },
            "[" => self.client.copy_mode(pane, CopyModeOp::Enter)?,
            "?" => self.mode = Mode::Help,
            _ => {},
        }
        self.dirty = true;
        Ok(())
    }

    fn prompt(&mut self, what: Prompt, mut text: String, input: Input) -> Result<(), String> {
        self.dirty = true;
        match input {
            Input::Text(more) | Input::Paste(more) => {
                text.extend(more.chars().filter(|c| !c.is_control()));
            },
            Input::Key(key) => match (key.name.as_str(), key.control) {
                ("enter", _) => {
                    self.mode = Mode::Normal;
                    let name = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
                    return match what {
                        Prompt::RenameTab(workspace, tab) => {
                            self.client.rename_tab(workspace, tab, name)
                        },
                        Prompt::RenameWorkspace(workspace) => {
                            self.client.rename_workspace(workspace, name)
                        },
                    };
                },
                ("escape", _) | ("c", true) | ("g", true) => {
                    self.mode = Mode::Normal;
                    return Ok(());
                },
                ("backspace", _) => _ = text.pop(),
                ("u", true) => text.clear(),
                _ => {},
            },
            Input::Mouse(_) | Input::Focus(_) => {},
        }
        self.mode = Mode::Prompt(what, text);
        Ok(())
    }

    /// Keys in copy mode: vi's motions, `v` (`V` lines, Ctrl-v a block) to
    /// select, `y` or Enter to copy and leave, `q` or Escape to leave.
    fn copy_key(&mut self, pane: PaneId, input: Input) -> Result<(), String> {
        use CopyMotion::*;
        let op = match &input {
            Input::Text(text) => match text.as_str() {
                "h" => CopyModeOp::Move(Left),
                "j" => CopyModeOp::Move(Down),
                "k" => CopyModeOp::Move(Up),
                "l" => CopyModeOp::Move(Right),
                "0" => CopyModeOp::Move(LineStart),
                "$" => CopyModeOp::Move(LineEnd),
                "^" => CopyModeOp::Move(FirstOccupied),
                "H" => CopyModeOp::Move(ScreenTop),
                "M" => CopyModeOp::Move(ScreenMiddle),
                "L" => CopyModeOp::Move(ScreenBottom),
                "w" => CopyModeOp::Move(WordRight),
                "b" => CopyModeOp::Move(WordLeft),
                "e" => CopyModeOp::Move(WordEnd),
                "W" => CopyModeOp::Move(BigWordRight),
                "B" => CopyModeOp::Move(BigWordLeft),
                "E" => CopyModeOp::Move(BigWordEnd),
                "%" => CopyModeOp::Move(Bracket),
                "{" => CopyModeOp::Move(ParagraphUp),
                "}" => CopyModeOp::Move(ParagraphDown),
                "g" => CopyModeOp::Move(Top),
                "G" => CopyModeOp::Move(Bottom),
                "[" => CopyModeOp::Move(PromptUp),
                "]" => CopyModeOp::Move(PromptDown),
                "v" | " " => CopyModeOp::Select(0),
                "V" => CopyModeOp::Select(1),
                "y" => return self.copy_and_leave(pane),
                "q" => CopyModeOp::Exit,
                _ => return Ok(()),
            },
            Input::Key(key) => match (key.name.as_str(), key.control) {
                ("up", _) => CopyModeOp::Move(Up),
                ("down", _) => CopyModeOp::Move(Down),
                ("left", _) => CopyModeOp::Move(Left),
                ("right", _) => CopyModeOp::Move(Right),
                ("home", _) => CopyModeOp::Move(LineStart),
                ("end", _) => CopyModeOp::Move(LineEnd),
                ("pageup", _) | ("b", true) => CopyModeOp::Move(PageUp),
                ("pagedown", _) | ("f", true) => CopyModeOp::Move(PageDown),
                ("u", true) => CopyModeOp::Move(HalfPageUp),
                ("d", true) => CopyModeOp::Move(HalfPageDown),
                ("v", true) => CopyModeOp::Select(2),
                ("enter", _) => return self.copy_and_leave(pane),
                ("escape", _) | ("c", true) => CopyModeOp::Exit,
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        self.client.copy_mode(pane, op)
    }

    fn copy_and_leave(&mut self, pane: PaneId) -> Result<(), String> {
        if let Some(text) = self.client.selection_text(pane)? {
            copy(&text);
            self.say(format!("copied {} characters", text.chars().count()));
        }
        self.client.copy_mode(pane, CopyModeOp::Exit)
    }

    fn mouse(&mut self, event: MouseEvent) -> Result<(), String> {
        let motion = event.code & 32 != 0;
        let wheel = event.code & 64 != 0;
        let button = match event.code & 3 {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::None,
        };
        // A button held goes where it went down.
        if let Some(mut drag) = self.drag
            && !wheel
            && (motion || event.release)
        {
            let line = event.line.clamp(drag.area.y, drag.area.y + drag.area.height - 1);
            let column = event.column.clamp(drag.area.x, drag.area.x + drag.area.width - 1);
            let (line, column) = (line - drag.area.y, column - drag.area.x);
            let action = if event.release { MouseAction::Release } else { MouseAction::Motion };
            if drag.program {
                self.client.mouse(drag.pane, button, action, line, column)?;
            } else if motion {
                drag.moved = true;
                self.client.select_update(drag.pane, line, column, false)?;
            } else if drag.moved {
                // Selected: copied, as a terminal does.
                if let Some(text) = self.client.selection_text(drag.pane)? {
                    copy(&text);
                }
            } else {
                self.client.select_clear(drag.pane)?;
            }
            self.drag = (!event.release).then_some(drag);
            return Ok(());
        }
        if event.release || motion {
            return Ok(());
        }
        if event.line + 1 == self.lines {
            // The status line: a tab clicked.
            let clicked = self.tab_spans.iter().find(|s| (s.0..s.1).contains(&event.column));
            if let (Some(&(_, _, workspace, tab)), false) = (clicked, wheel) {
                self.client.select_tab(workspace, tab)?;
            }
            return Ok(());
        }
        let placed = self.placed();
        let Some(&(pane, area)) = placed.iter().find(|(_, a)| a.contains(event.column, event.line))
        else {
            return Ok(());
        };
        let (line, column) = (event.line - area.y, event.column - area.x);
        if wheel {
            let lines = if event.code & 1 == 0 { WHEEL_LINES } else { -WHEEL_LINES };
            return self.client.wheel(pane, lines, line, column);
        }
        if Some(pane) != self.focused() {
            self.client.focus_pane(pane)?;
        }
        // Shift: selecting, whatever the program wants.
        let program = self.client.wants_mouse(pane) && event.code & 4 == 0;
        if program {
            self.client.mouse(pane, button, MouseAction::Press, line, column)?;
        } else if button == MouseButton::Left {
            self.client.select_start(pane, SelectionKind::Characters, line, column, false)?;
        } else {
            return Ok(());
        }
        self.drag = Some(Drag { pane, area, program, moved: false });
        Ok(())
    }

    /// The colors of the focused pane's terminal.
    fn colors(&self) -> (Rgb, Rgb) {
        self.focused()
            .and_then(|pane| self.client.screen(pane))
            .filter(|s| !s.frame.lines.is_empty())
            .map_or((BACKGROUND, FOREGROUND), |s| (s.frame.background, s.frame.foreground))
    }

    fn draw(&mut self) {
        let (background, foreground) = self.colors();
        let plain = Style::plain(foreground, background);
        let mut grid = Grid::new(self.columns, self.lines, plain);
        let focused = self.focused();
        let placed = self.placed();
        let mut cursor = None;
        let mut covered = vec![vec![false; self.columns]; self.lines.saturating_sub(1)];
        for &(pane, area) in &placed {
            for row in covered.iter_mut().skip(area.y).take(area.height) {
                for cell in row.iter_mut().skip(area.x).take(area.width) {
                    *cell = true;
                }
            }
            let Some(screen) = self.client.screen(pane) else { continue };
            grid.frame(area, &screen.frame);
            let offset = screen.frame.display_offset;
            let copying = screen.mode.contains(TermMode::VI);
            if offset > 0 || copying {
                // Scrolled back, as tmux says it: in the top right.
                let label =
                    if copying { format!("[copy {offset}]") } else { format!("[{offset}]") };
                let width = label.chars().count().min(area.width);
                let style = Style::plain(background, ACCENT);
                grid.text(area.x + area.width - width, area.y, width, &label, style);
            }
            if Some(pane) == focused
                && let Some(c) = screen.frame.cursor
                && c.line < area.height
                && c.column < area.width
            {
                let shape =
                    if c.shape == CursorShape::HollowBlock { CursorShape::Block } else { c.shape };
                cursor = Some(Cursor {
                    x: area.x + c.column,
                    y: area.y + c.line,
                    shape,
                    blinking: c.blinking,
                });
            }
        }
        // Borders: what no pane covers.
        let border: Vec<Vec<bool>> =
            covered.iter().map(|row| row.iter().map(|c| !c).collect()).collect();
        let around = focused.and_then(|f| placed.iter().find(|(p, _)| *p == f)).map(|(_, a)| *a);
        let dim = mix(background, foreground, 0.35);
        screen::borders(&mut grid, &border, |x, y| {
            let near = around.is_some_and(|a| {
                x + 1 >= a.x && x <= a.x + a.width && y + 1 >= a.y && y <= a.y + a.height
            });
            Style::plain(if near && placed.len() > 1 { ACCENT } else { dim }, background)
        });
        if let Some(c) = self.status(&mut grid, background, foreground) {
            cursor = Some(c);
        }
        if self.mode == Mode::Help {
            self.help(&mut grid, background, foreground);
            cursor = None;
        }
        let text = screen::draw(self.shown.as_ref(), &grid, cursor, &mut self.shape);
        term::write_out(&text);
        self.shown = Some(grid);
    }

    /// The status line; the cursor, if it's asking for something.
    fn status(&mut self, grid: &mut Grid, background: Rgb, foreground: Rgb) -> Option<Cursor> {
        let y = self.lines - 1;
        let width = self.columns;
        let bar = Style::plain(foreground, mix(background, foreground, 0.15));
        let strong = Style { bold: true, ..Style::plain(background, ACCENT) };
        grid.fill(Area { x: 0, y, width, height: 1 }, " ", bar);
        self.tab_spans.clear();
        let ask = match &self.mode {
            Mode::Prompt(Prompt::RenameTab(..), text) => Some(format!("rename tab: {text}")),
            Mode::Prompt(Prompt::RenameWorkspace(_), text) => {
                Some(format!("rename workspace: {text}"))
            },
            Mode::Confirm(Confirm::ClosePane(_)) => Some("close this pane? (y/n)".to_owned()),
            Mode::Confirm(Confirm::CloseTab(..)) => Some("close this tab? (y/n)".to_owned()),
            _ => None,
        };
        if let Some(ask) = ask {
            let at = grid.text(0, y, width, &ask, Style { bold: true, ..bar });
            let blinking = false;
            return Some(Cursor { x: at.min(width - 1), y, shape: CursorShape::Block, blinking });
        }
        let Some(layout) = self.layout.clone() else {
            grid.text(1, y, width.saturating_sub(1), "attaching…", bar);
            return None;
        };
        let mut x = 0;
        if let Some(workspace) = layout.active_workspace() {
            let name = workspace.name.clone().unwrap_or_else(|| workspace.title.clone());
            let label = match layout.workspaces.len() {
                1 => format!(" {} ", clip(&name, 24)),
                n => format!(" {} {}/{n} ", clip(&name, 24), layout.active + 1),
            };
            x += grid.text(x, y, width - x, &label, strong);
            x += grid.text(x, y, width - x, " ", bar);
            for (index, tab) in workspace.tabs.iter().enumerate() {
                let active = index == workspace.active;
                let mut label = format!(" {}:{}", index + 1, clip(&self.tab_title(tab), 24));
                if active {
                    label.push('*');
                }
                if tab.zoomed {
                    label.push('Z');
                }
                if let Some(mark) = self.tab_mark(tab).filter(|_| !active) {
                    label.push(mark);
                }
                label.push(' ');
                let style = if active { strong } else { bar };
                let start = x;
                x += grid.text(x, y, width - x, &label, style);
                self.tab_spans.push((start, x, workspace.id, index));
                if x >= width {
                    break;
                }
            }
        }
        // On the right: the prefix pressed, else something said, else the
        // server's status line; its left part after the tabs, where there's
        // room. All cut short to fit.
        let said = match (&self.mode, &self.message) {
            (Mode::Prefix, _) => Some((format!(" {} ", self.prefix.describe()), strong)),
            (_, Some((text, _))) => Some((format!(" {text} "), bar)),
            _ => None,
        };
        let right: Vec<(String, Style)> = match said {
            Some(said) => vec![said],
            None if self.status.right.is_empty() => Vec::new(),
            None => {
                let mut parts = styled(&self.status.right, bar);
                parts.push((" ".to_owned(), bar));
                parts
            },
        };
        let length: usize = right.iter().flat_map(|(t, _)| t.chars()).map(screen::char_width).sum();
        let room = width.saturating_sub(x + 1);
        let at = width.saturating_sub(length.min(room));
        let mut end = at;
        for (text, style) in right {
            end += grid.text(end, y, width - end, &text, style);
        }
        if !self.status.left.is_empty() && x + 1 < at {
            let mut left = x + 1;
            for (text, style) in styled(&self.status.left, bar) {
                left += grid.text(left, y, at.saturating_sub(left + 1), &text, style);
            }
        }
        None
    }

    /// The keys, in a box in the middle.
    fn help(&self, grid: &mut Grid, background: Rgb, foreground: Rgb) {
        let prefix = self.prefix.describe();
        let lines = [
            format!("{prefix}, then:"),
            String::new(),
            "c       new tab              n p      next, previous tab".to_owned(),
            "1-9 0   tab 1-9, 10          l        last tab".to_owned(),
            "%       split side by side   \"        split one above the other".to_owned(),
            "arrows  pane that way        o        next pane".to_owned(),
            "z       zoom the pane        E        panes evenly sized".to_owned(),
            "x       close the pane       &        close the tab".to_owned(),
            "!       pane into a tab      ,        rename the tab".to_owned(),
            "C       new workspace        ( )      previous, next workspace".to_owned(),
            "$       rename workspace     [ PgUp   copy mode, scroll back".to_owned(),
            "d       detach               ?        this".to_owned(),
            format!("{prefix:<8}{prefix} itself"),
            String::new(),
            "Copy mode: vi's keys; v (V, ctrl-v) selects, y copies, q leaves.".to_owned(),
            "The mouse: click to focus, drag to copy, the wheel scrolls.".to_owned(),
        ];
        let inner = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        let (width, height) = ((inner + 4).min(self.columns), (lines.len() + 2).min(self.lines));
        let area = Area {
            x: (self.columns - width) / 2,
            y: (self.lines.saturating_sub(1).saturating_sub(height)) / 2,
            width,
            height,
        };
        let style = Style::plain(foreground, mix(background, foreground, 0.1));
        grid.fill(area, " ", style);
        let edge: Vec<Vec<bool>> = (0..self.lines)
            .map(|y| {
                (0..self.columns)
                    .map(|x| {
                        area.contains(x, y)
                            && (x == area.x
                                || y == area.y
                                || x + 1 == area.x + width
                                || y + 1 == area.y + height)
                    })
                    .collect()
            })
            .collect();
        screen::borders(grid, &edge, |_, _| Style { fg: ACCENT, ..style });
        for (i, line) in lines.iter().enumerate().take(height.saturating_sub(2)) {
            grid.text(area.x + 2, area.y + 1 + i, width.saturating_sub(4), line, style);
        }
    }
}

/// A status line's pieces in this terminal's styles: their colors or the
/// bar's, dimmed toward the background.
fn styled(segments: &[StatusSegment], bar: Style) -> Vec<(String, Style)> {
    segments
        .iter()
        .map(|segment| {
            let bg = segment.bg.unwrap_or(bar.bg);
            let mut fg = segment.fg.unwrap_or(bar.fg);
            if segment.dim {
                fg = mix(bg, fg, 0.55);
            }
            let style = Style { bold: segment.bold, ..Style::plain(fg, bg) };
            (segment.text.clone(), style)
        })
        .collect()
}

/// Onto this terminal's clipboard (OSC 52), where it takes that.
fn copy(text: &str) {
    term::write_out(&format!("\x1b]52;c;{}\x07", screen::base64(text.as_bytes())));
}

/// `text`, at most `max` characters.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// `a` taking `share` of `b`.
fn mix(a: Rgb, b: Rgb, share: f32) -> Rgb {
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * share) as u8;
    Rgb { r: channel(a.r, b.r), g: channel(a.g, b.g), b: channel(a.b, b.b) }
}
