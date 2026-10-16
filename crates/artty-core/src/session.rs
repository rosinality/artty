use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use artty_vt::event::{Event, EventListener, WindowSize};
use artty_vt::event_loop::{EventLoop, EventLoopSender, Msg};
use artty_vt::grid::{Dimensions, Scroll};
use artty_vt::index::{Column, Line, Point, Side};
use artty_vt::selection::{Selection, SelectionType};
use artty_vt::sync::FairMutex;
use artty_vt::term::{ClipboardType, Config, Term};
use artty_vt::tty;
use log::warn;

use crate::frame::Frame;
use crate::theme::Theme;

pub type SessionId = u64;

/// Terminal size in cells, plus the cell size in pixels (reported to programs
/// that ask for the text area size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub lines: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.lines as usize
    }

    fn columns(&self) -> usize {
        self.columns as usize
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        WindowSize {
            num_lines: size.lines,
            num_cols: size.columns,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// Program and arguments; `None` runs the user's login shell.
    pub command: Option<(String, Vec<String>)>,
    pub working_directory: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub scrollback: Option<usize>,
}

/// Something the UI should react to, produced by [`Session::handle`].
pub enum Notice {
    /// The screen changed and should be redrawn.
    Redraw,
    Title(String),
    ResetTitle,
    Bell,
    /// The program asked to copy text to the clipboard (OSC 52).
    ClipboardStore(ClipboardType, String),
    /// The program asked to read the clipboard (OSC 52); answer with
    /// [`Session::reply_clipboard`].
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),
    /// The shell exited; the session is finished.
    Exited,
}

/// Receives terminal events on the PTY thread and forwards them to the UI.
#[derive(Clone)]
pub struct EventProxy {
    id: SessionId,
    pty: Arc<OnceLock<EventLoopSender>>,
    sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            // Replies to queries (device attributes, cursor position, ...) go
            // straight back to the program without a round trip through the UI.
            Event::PtyWrite(text) => {
                if let Some(pty) = self.pty.get() {
                    let _ = pty.send(Msg::Input(text.into_bytes().into()));
                }
            }
            event => (self.sink)(self.id, event),
        }
    }
}

pub struct Session {
    id: SessionId,
    term: Arc<FairMutex<Term<EventProxy>>>,
    pty: EventLoopSender,
    size: TerminalSize,
    theme: Arc<Theme>,
}

impl Session {
    /// Start `options.command` (or the login shell) on a new PTY.
    ///
    /// `sink` is called on the PTY thread for every terminal event; pass each
    /// event back to [`Session::handle`] on the UI thread.
    pub fn spawn(
        id: SessionId,
        options: SessionOptions,
        size: TerminalSize,
        theme: Arc<Theme>,
        sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    ) -> io::Result<Self> {
        let proxy = EventProxy { id, pty: Arc::new(OnceLock::new()), sink };

        let mut config = Config::default();
        if let Some(scrollback) = options.scrollback {
            config.scrolling_history = scrollback;
        }
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        let mut env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "artty".to_owned()),
            ("TERM_PROGRAM_VERSION".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        ]);
        env.extend(options.env);
        let pty_options = tty::Options {
            shell: options.command.map(|(program, args)| tty::Shell::new(program, args)),
            working_directory: options.working_directory,
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&pty_options, size.into(), id)?;

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let sender = event_loop.channel();
        let _ = proxy.pty.set(sender.clone());
        event_loop.spawn();

        Ok(Self { id, term, pty: sender, size, theme })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Send input (keystrokes, pasted text) to the program.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        // Typing jumps back to the live screen, like every other terminal.
        self.term.lock().scroll_display(Scroll::Bottom);
        self.send(Msg::Input(bytes));
    }

    pub fn resize(&mut self, size: TerminalSize) {
        if size == self.size || size.columns == 0 || size.lines == 0 {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        self.send(Msg::Resize(size.into()));
    }

    /// Scroll the viewport through history; positive values scroll up.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Start a selection at a viewport cell (`line` 0 is the top of the view).
    pub fn start_selection(&self, kind: SelectionType, line: usize, column: usize, side: Side) {
        let mut term = self.term.lock();
        let point = viewport_point(&term, line, column);
        term.selection = Some(Selection::new(kind, point, side));
    }

    pub fn update_selection(&self, line: usize, column: usize, side: Side) {
        let mut term = self.term.lock();
        let point = viewport_point(&term, line, column);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    /// The selected text, or `None` if nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|text| !text.is_empty())
    }

    /// Snapshot of the visible screen for rendering.
    pub fn frame(&self) -> Frame {
        Frame::capture(&self.term.lock(), &self.theme)
    }

    /// Whether the program wants mouse/keyboard input in a special mode, e.g.
    /// application cursor keys.
    pub fn mode(&self) -> artty_vt::term::TermMode {
        *self.term.lock().mode()
    }

    /// Process an event forwarded from the PTY thread. Answers queries that
    /// need terminal state and returns what the UI should do, if anything.
    pub fn handle(&self, event: Event) -> Option<Notice> {
        match event {
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => {
                Some(Notice::Redraw)
            }
            Event::Title(title) => Some(Notice::Title(title)),
            Event::ResetTitle => Some(Notice::ResetTitle),
            Event::Bell => Some(Notice::Bell),
            Event::ClipboardStore(kind, text) => Some(Notice::ClipboardStore(kind, text)),
            Event::ClipboardLoad(kind, format) => Some(Notice::ClipboardLoad(kind, format)),
            Event::ColorRequest(index, format) => {
                let rgb = self.theme.color_for_index(index, self.term.lock().colors());
                self.send(Msg::Input(format(rgb).into_bytes().into()));
                None
            }
            Event::TextAreaSizeRequest(format) => {
                self.send(Msg::Input(format(self.size.into()).into_bytes().into()));
                None
            }
            Event::Exit | Event::ChildExit(_) => Some(Notice::Exited),
            Event::PtyWrite(text) => {
                self.send(Msg::Input(text.into_bytes().into()));
                None
            }
        }
    }

    /// Answer a [`Notice::ClipboardLoad`] request with the clipboard contents.
    pub fn reply_clipboard(
        &self,
        format: &Arc<dyn Fn(&str) -> String + Sync + Send + 'static>,
        text: &str,
    ) {
        self.send(Msg::Input(format(text).into_bytes().into()));
    }

    fn send(&self, msg: Msg) {
        if let Err(err) = self.pty.send(msg) {
            warn!("session {}: PTY is gone: {err}", self.id);
        }
    }
}

fn viewport_point<T>(term: &Term<T>, line: usize, column: usize) -> Point {
    let offset = term.grid().display_offset() as i32;
    let line = (line as i32).min(term.screen_lines() as i32 - 1) - offset;
    Point::new(Line(line), Column(column.min(term.columns() - 1)))
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.pty.send(Msg::Shutdown);
    }
}
