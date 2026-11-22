use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use artty_vt::ansi::Progress;
use artty_vt::event::{Event, EventListener, WindowSize};
use artty_vt::event_loop::{EventLoop, EventLoopSender, Msg};
use artty_vt::grid::{Dimensions, Scroll};
use artty_vt::index::{Boundary, Column, Direction, Line, Point, Side};
use artty_vt::selection::{Selection, SelectionType};
use artty_vt::sync::FairMutex;
use artty_vt::term::search::{RegexIter, RegexSearch};
use artty_vt::term::{ClipboardType, Config, Term};
use artty_vt::tty;
use log::warn;

use crate::frame::Frame;
use crate::process::{ProcessInfo, ProcessProbe, path_from_file_url};
use crate::protocol::{CopyModeOp, CopyMotion};
use crate::shell::{CommandRecord, ShellEvent, ShellPhase, ShellTracker};
use crate::theme::Theme;

pub type SessionId = u64;

/// A pane as one server build hands it to the next (see [`Session::handover`]).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHandover {
    pub id: SessionId,
    /// The PTY controller, inherited across `exec`.
    pub fd: i32,
    pub pid: u32,
    pub size: TerminalSize,
    shell: crate::shell::ShellState,
    reported_cwd: Option<PathBuf>,
    /// Where the prompt starts (see `Term::prompt_line`). Here, not in the
    /// binary snapshot, whose shape must stay put.
    #[serde(default)]
    prompt_line: Option<u64>,
}

/// Terminal size in cells, plus the cell size in pixels (reported to programs
/// that ask for the text area size).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Output shown before the program starts, e.g. a restored tab's history.
    pub initial_output: Option<Vec<u8>>,
    /// Cursor shape when the program doesn't choose one.
    pub cursor_shape: artty_vt::ansi::CursorShape,
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
    /// The program asked for a desktop notification (OSC 9 / 777 / 99).
    Notification {
        title: Option<String>,
        body: String,
    },
    Progress(Progress),
    /// A program's status (OSC 7501).
    ProgramStatus(artty_vt::program_status::StatusReport),
    /// The shell started a command (shell integration).
    CommandStarted(Option<String>),
    /// The shell finished a command (shell integration).
    CommandFinished(CommandRecord),
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
            },
            event => (self.sink)(self.id, event),
        }
    }
}

pub struct Session {
    id: SessionId,
    term: Arc<FairMutex<Term<EventProxy>>>,
    pty: EventLoopSender,
    probe: Option<ProcessProbe>,
    size: TerminalSize,
    theme: Arc<Theme>,
    shell: ShellTracker,
    /// Directory reported by the shell (OSC 7).
    reported_cwd: Option<PathBuf>,
    search: Option<(String, RegexSearch)>,
    term_config: Config,
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
        let (term, term_config) = new_term(&options, size, proxy.clone());

        let mut env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "artty".to_owned()),
            ("TERM_PROGRAM_VERSION".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        ]);
        if let Some((name, locale)) = crate::locale::pane_env() {
            env.insert(name.to_owned(), locale);
        }
        env.extend(options.env);
        let pty_options = tty::Options {
            shell: options.command.map(|(program, args)| tty::Shell::new(program, args)),
            working_directory: options.working_directory,
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&pty_options, size.into(), id)?;
        let probe = ProcessProbe::new(pty.file().as_raw_fd(), pty.pid());

        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let sender = event_loop.channel();
        let _ = proxy.pty.set(sender.clone());
        event_loop.spawn();

        Ok(Self {
            id,
            term,
            pty: sender,
            probe,
            size,
            theme,
            shell: ShellTracker::default(),
            reported_cwd: None,
            search: None,
            term_config,
        })
    }

    /// A terminal whose program runs elsewhere (a shell over ssh) instead of
    /// on a PTY here: its bytes go through the returned ends (see
    /// [`tty::RemoteEnds`]), its sizes to `resize`. Of `options`, only what
    /// the terminal takes counts (scrollback, cursor, output shown first).
    pub fn remote(
        id: SessionId,
        options: SessionOptions,
        size: TerminalSize,
        theme: Arc<Theme>,
        sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
        mut resize: Box<dyn FnMut(TerminalSize) + Send>,
    ) -> io::Result<(Self, tty::RemoteEnds)> {
        let proxy = EventProxy { id, pty: Arc::new(OnceLock::new()), sink };
        let (term, term_config) = new_term(&options, size, proxy.clone());
        let (remote, ends) = tty::Remote::new(Box::new(move |window: WindowSize| {
            resize(TerminalSize {
                columns: window.num_cols,
                lines: window.num_lines,
                cell_width: window.cell_width,
                cell_height: window.cell_height,
            })
        }))?;
        // What it said last (why it ended) is shown.
        let event_loop = EventLoop::new(term.clone(), proxy.clone(), remote, true, false)?;
        let sender = event_loop.channel();
        let _ = proxy.pty.set(sender.clone());
        event_loop.spawn();
        let session = Self {
            id,
            term,
            pty: sender,
            probe: None,
            size,
            theme,
            shell: ShellTracker::default(),
            reported_cwd: None,
            search: None,
            term_config,
        };
        Ok((session, ends))
    }

    /// Stop reading the PTY for a hand-over. Returns when everything read so
    /// far is parsed (or after a second, if the reader is stuck), with a
    /// sender that resumes the reader if the hand-over is called off. If the
    /// process is replaced instead, the reader never reads again.
    pub fn freeze(&self) -> std::sync::mpsc::Sender<()> {
        let (frozen, is_frozen) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        let _ = self.pty.send(Msg::Freeze(frozen, resumed));
        let _ = is_frozen.recv_timeout(std::time::Duration::from_secs(1));
        resume
    }

    /// What the next server build needs to carry on with this pane: the PTY
    /// descriptor it inherits across `exec` (which this clears close-on-exec
    /// on) and the rest, and the terminal's state, kept apart so a snapshot a
    /// newer build can't read costs only the screen. `None` without a PTY.
    pub fn handover(&self) -> Option<(SessionHandover, artty_vt::term::TermSnapshot)> {
        let probe = self.probe.as_ref()?;
        let fd = probe.pty_fd();
        // SAFETY: plain syscall on a descriptor we own.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let term = self.term.lock();
        let handover = SessionHandover {
            id: self.id,
            fd,
            pid: probe.shell_pid() as u32,
            size: self.size,
            shell: self.shell.state(),
            reported_cwd: self.reported_cwd.clone(),
            prompt_line: term.prompt_line(),
        };
        Some((handover, term.snapshot()))
    }

    /// Carry on with a pane a previous server build handed over: the shell
    /// keeps running, and the terminal is where it was. Without its snapshot
    /// the screen starts blank, and the program is asked to redraw.
    pub fn adopt(
        handover: SessionHandover,
        snapshot: Option<artty_vt::term::TermSnapshot>,
        options: SessionOptions,
        theme: Arc<Theme>,
        sink: Arc<dyn Fn(SessionId, Event) + Send + Sync>,
    ) -> io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let SessionHandover { id, fd, pid, size, shell, reported_cwd, prompt_line } = handover;
        let redraw = snapshot.is_none();
        let proxy = EventProxy { id, pty: Arc::new(OnceLock::new()), sink };
        let mut config = term_config();
        if let Some(scrollback) = options.scrollback {
            config.scrolling_history = scrollback;
        }
        config.default_cursor_style.shape = options.cursor_shape;
        let term_config = config.clone();
        let mut term = match snapshot {
            Some(snapshot) => Term::from_snapshot(config, snapshot, proxy.clone()),
            None => Term::new(config, &size, proxy.clone()),
        };
        term.set_cell_size(size.cell_width, size.cell_height);
        term.set_prompt_line(prompt_line);
        let term = Arc::new(FairMutex::new(term));
        // SAFETY: `fd` was inherited for exactly this, and nothing else owns it.
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        let pty = tty::Pty::adopt(master, pid)?;
        let probe = ProcessProbe::new(pty.file().as_raw_fd(), pid);
        let event_loop = EventLoop::new(term.clone(), proxy.clone(), pty, false, false)?;
        let sender = event_loop.channel();
        let _ = proxy.pty.set(sender.clone());
        event_loop.spawn();
        if redraw {
            // A size change makes full-screen programs and most shells repaint.
            let narrower = TerminalSize { columns: size.columns.saturating_sub(1).max(2), ..size };
            let _ = sender.send(Msg::Resize(narrower.into()));
            let _ = sender.send(Msg::Resize(size.into()));
        }
        Ok(Self {
            id,
            term,
            pty: sender,
            probe,
            size,
            theme,
            shell: ShellTracker::from_state(shell),
            reported_cwd,
            search: None,
            term_config,
        })
    }

    /// Change the cursor shape used when the program doesn't choose one.
    pub fn set_cursor_shape(&mut self, shape: artty_vt::ansi::CursorShape) {
        if self.term_config.default_cursor_style.shape != shape {
            self.term_config.default_cursor_style.shape = shape;
            let mut term = self.term.lock();
            term.set_options(self.term_config.clone());
            // A change in Settings shows now, over a shape a program chose.
            term.reset_cursor_style();
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
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
        // The terminal is never narrower than two columns (a wide character
        // has to fit); the shell is told the same size.
        let size = TerminalSize {
            columns: size.columns.max(artty_vt::term::MIN_COLUMNS as u16),
            lines: size.lines.max(1),
            ..size
        };
        if size == self.size {
            return;
        }
        self.size = size;
        let mut term = self.term.lock();
        term.resize(size);
        term.set_cell_size(size.cell_width, size.cell_height);
        drop(term);
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

    /// The shell prompt before (`older`) or after the stable line `from`.
    fn prompt_near(&self, from: i64, older: bool) -> Option<u64> {
        let prompts = &self.shell.prompts;
        if older {
            prompts.iter().rev().copied().find(|&p| (p as i64) < from)
        } else {
            prompts.iter().copied().find(|&p| (p as i64) > from)
        }
    }

    /// Scroll the previous (`older`) or next shell prompt to the top of the
    /// view; past the newest, back to the bottom. Returns whether it moved.
    pub fn jump_to_prompt(&self, older: bool) -> bool {
        let mut term = self.term.lock();
        let scrolled = term.grid().lines_scrolled() as i64;
        let offset = term.grid().display_offset() as i64;
        let history = term.history_size() as i64;
        let wanted = match self.prompt_near(scrolled - offset, older) {
            // The display offset that puts that line at the top.
            Some(prompt) => (scrolled - prompt as i64).clamp(0, history),
            None if older => return false,
            None => 0,
        };
        if wanted == offset {
            return false;
        }
        term.scroll_display(Scroll::Delta((wanted - offset) as i32));
        true
    }

    /// The command in view (the last one started at or above the top of a
    /// view scrolled back), or else the last one: its command line and output.
    pub fn command_output(&self) -> Option<(Option<String>, String)> {
        let term = self.term.lock();
        let offset = term.grid().display_offset() as i64;
        let history = &self.shell.history;
        let record = if offset == 0 {
            history.back()
        } else {
            let top = term.grid().lines_scrolled() as i64 - offset;
            history
                .iter()
                .rev()
                .find(|r| r.prompt.is_some_and(|p| p as i64 <= top))
                .or(history.front())
        }?;
        Some((record.command.clone(), record.output.clone()))
    }

    /// Copy mode: a keyboard cursor through the screen and history (the
    /// terminal's vi mode), selecting from it.
    pub fn copy_mode(&self, op: CopyModeOp) {
        let mut term = self.term.lock();
        let active = term.mode().contains(artty_vt::term::TermMode::VI);
        match op {
            CopyModeOp::Enter if !active => term.toggle_vi_mode(),
            CopyModeOp::Exit if active => {
                term.toggle_vi_mode();
                term.selection = None;
                term.scroll_display(Scroll::Bottom);
            },
            CopyModeOp::Select(kind) if active => {
                let ty = match kind {
                    1 => SelectionType::Lines,
                    2 => SelectionType::Block,
                    _ => SelectionType::Simple,
                };
                let point = term.vi_mode_cursor.point;
                // As in vi: the same kind again ends it, another changes it.
                match term.selection.as_mut().filter(|s| !s.is_empty()) {
                    Some(selection) if selection.ty == ty => term.selection = None,
                    Some(selection) => selection.ty = ty,
                    None => term.selection = Some(Selection::new(ty, point, Side::Left)),
                }
                if let Some(selection) = term.selection.as_mut() {
                    selection.include_all();
                }
            },
            CopyModeOp::Move(motion) if active => self.copy_motion(&mut term, motion),
            _ => {},
        }
    }

    fn copy_motion(&self, term: &mut Term<EventProxy>, motion: CopyMotion) {
        use artty_vt::vi_mode::ViMotion;
        let vi = |term: &mut Term<EventProxy>, motion| term.vi_motion(motion);
        let page = |term: &mut Term<EventProxy>, pages: f32| {
            let lines = (term.screen_lines() as f32 * pages) as i32;
            term.scroll_display(Scroll::Delta(lines));
            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, lines);
        };
        match motion {
            CopyMotion::Up => vi(term, ViMotion::Up),
            CopyMotion::Down => vi(term, ViMotion::Down),
            CopyMotion::Left => vi(term, ViMotion::Left),
            CopyMotion::Right => vi(term, ViMotion::Right),
            CopyMotion::LineStart => vi(term, ViMotion::First),
            CopyMotion::LineEnd => vi(term, ViMotion::Last),
            CopyMotion::FirstOccupied => vi(term, ViMotion::FirstOccupied),
            CopyMotion::ScreenTop => vi(term, ViMotion::High),
            CopyMotion::ScreenMiddle => vi(term, ViMotion::Middle),
            CopyMotion::ScreenBottom => vi(term, ViMotion::Low),
            CopyMotion::WordLeft => vi(term, ViMotion::SemanticLeft),
            CopyMotion::WordRight => vi(term, ViMotion::SemanticRight),
            CopyMotion::WordEnd => vi(term, ViMotion::SemanticRightEnd),
            CopyMotion::BigWordLeft => vi(term, ViMotion::WordLeft),
            CopyMotion::BigWordRight => vi(term, ViMotion::WordRight),
            CopyMotion::BigWordEnd => vi(term, ViMotion::WordRightEnd),
            CopyMotion::Bracket => vi(term, ViMotion::Bracket),
            CopyMotion::ParagraphUp => vi(term, ViMotion::ParagraphUp),
            CopyMotion::ParagraphDown => vi(term, ViMotion::ParagraphDown),
            CopyMotion::Top => {
                let top = term.topmost_line();
                term.vi_goto_point(Point::new(top, Column(0)));
            },
            CopyMotion::Bottom => {
                let bottom = term.bottommost_line();
                term.vi_goto_point(Point::new(bottom, Column(0)));
            },
            CopyMotion::HalfPageUp => page(term, 0.5),
            CopyMotion::HalfPageDown => page(term, -0.5),
            CopyMotion::PageUp => page(term, 1.),
            CopyMotion::PageDown => page(term, -1.),
            CopyMotion::PromptUp | CopyMotion::PromptDown => {
                let scrolled = term.grid().lines_scrolled() as i64;
                let from = scrolled + i64::from(term.vi_mode_cursor.point.line.0);
                let older = motion == CopyMotion::PromptUp;
                if let Some(prompt) = self.prompt_near(from, older) {
                    let line = (prompt as i64 - scrolled).max(i64::from(term.topmost_line().0));
                    term.vi_goto_point(Point::new(Line(line as i32), Column(0)));
                }
            },
        }
    }

    /// The selected text, or `None` if nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|text| !text.is_empty())
    }

    /// Snapshot of the visible screen for rendering, with every visible
    /// match of the current search.
    pub fn frame(&mut self) -> Frame {
        let term = self.term.lock();
        let mut matches = Vec::new();
        if let Some((_, regex)) = self.search.as_mut() {
            let top = Line(-(term.grid().display_offset() as i32));
            let bottom = Line(top.0 + term.screen_lines() as i32 - 1);
            let (start, end) = (Point::new(top, Column(0)), Point::new(bottom, term.last_column()));
            // Bounded: a pathological pattern can't stall rendering.
            matches.extend(RegexIter::new(start, end, Direction::Right, &term, regex).take(1000));
        }
        Frame::capture(&term, &self.theme, &matches)
    }

    /// Whether the program wants mouse/keyboard input in a special mode, e.g.
    /// application cursor keys.
    pub fn mode(&self) -> artty_vt::term::TermMode {
        *self.term.lock().mode()
    }

    /// The shell's process id.
    pub fn shell_pid(&self) -> Option<i32> {
        self.probe.as_ref().map(ProcessProbe::shell_pid)
    }

    /// Foreground process, for detecting agents and running commands.
    pub fn process(&self) -> ProcessInfo {
        self.probe.as_ref().map(ProcessProbe::info).unwrap_or_default()
    }

    /// The shell's working directory: what it reported, or what the OS says.
    pub fn cwd(&self) -> Option<PathBuf> {
        self.reported_cwd.clone().or_else(|| self.probe.as_ref()?.shell_cwd())
    }

    /// Whether the shell sends shell-integration marks.
    pub fn shell_integration(&self) -> bool {
        self.shell.phase != ShellPhase::Unknown
    }

    pub fn shell_phase(&self) -> ShellPhase {
        self.shell.phase
    }

    pub fn last_command(&self) -> Option<&CommandRecord> {
        self.shell.history.back()
    }

    /// The visible screen (ignoring scrollback position) as text.
    pub fn screen_text(&self) -> String {
        let term = self.term.lock();
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        trim_lines(term.bounds_to_string(start, end))
    }

    /// The last `lines` lines of output, including history.
    pub fn scrollback_text(&self, lines: usize) -> String {
        let term = self.term.lock();
        // Count from the last line with content, not the blank screen bottom.
        let blank = |line: Line| (0..term.columns()).all(|c| term.grid()[line][Column(c)].c == ' ');
        let bottom = (0..term.screen_lines() as i32)
            .rev()
            .map(Line)
            .find(|&line| !blank(line))
            .unwrap_or(Line(0));
        let top = (bottom.0 - lines.max(1) as i32 + 1).max(term.topmost_line().0);
        let text = term.bounds_to_string(
            Point::new(Line(top), Column(0)),
            Point::new(bottom, term.last_column()),
        );
        trim_lines(text)
    }

    /// The last `lines` lines of output with color codes, for saving.
    pub fn styled_scrollback(&self, lines: usize) -> String {
        crate::state::styled_text(&self.term.lock(), lines)
    }

    /// [`Session::search`], then where the match is among them all: its
    /// number from the top and how many there are (counting stops at so
    /// many); and whether there's history beyond the screen at all.
    pub fn search_counted(
        &mut self,
        query: &str,
        older: bool,
    ) -> Result<crate::protocol::SearchFound, String> {
        const MOST: usize = 10_000;
        let found = self.search(query, older)?;
        let (_, regex) = self.search.as_mut().expect("just searched");
        let term = self.term.lock();
        let current = term.selection.as_ref().and_then(|s| s.to_range(&term)).map(|r| r.start);
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        let (mut index, mut total) = (0, 0);
        for each in RegexIter::new(start, end, Direction::Right, &term, regex).take(MOST + 1) {
            total += 1;
            if Some(*each.start()) == current {
                index = total;
            }
        }
        let alternate = term.mode().contains(artty_vt::term::TermMode::ALT_SCREEN);
        let history = term.history_size() > 0 && !alternate;
        Ok(crate::protocol::SearchFound {
            found,
            index: if found { index } else { 0 },
            total: total.min(MOST),
            more: total > MOST,
            history,
        })
    }

    /// Find `query` (a regex) and select the match, starting from the current
    /// match or the bottom. `older` searches up through history. Returns
    /// whether something matched; an invalid regex is an error.
    pub fn search(&mut self, query: &str, older: bool) -> Result<bool, String> {
        if self.search.as_ref().is_none_or(|(q, _)| q != query) {
            let regex = RegexSearch::new(query).map_err(|err| err.to_string())?;
            self.search = Some((query.to_owned(), regex));
        }
        let (_, regex) = self.search.as_mut().unwrap();
        let mut term = self.term.lock();
        let current = term.selection.as_ref().and_then(|s| s.to_range(&term));
        let (origin, direction) = match (current, older) {
            (Some(range), true) => (range.start.sub(&*term, Boundary::None, 1), Direction::Left),
            (Some(range), false) => (range.end.add(&*term, Boundary::None, 1), Direction::Right),
            (None, true) => {
                (Point::new(term.bottommost_line(), term.last_column()), Direction::Left)
            },
            (None, false) => (Point::new(term.topmost_line(), Column(0)), Direction::Right),
        };
        let side = if older { Side::Right } else { Side::Left };
        let Some(found) = term.search_next(regex, origin, direction, side, None) else {
            return Ok(false);
        };
        let (start, end) = (*found.start(), *found.end());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        term.scroll_to_point(start);
        Ok(true)
    }

    /// Lines with `text` in the output, history too, newest first, at most
    /// `max`: each its line from the top of the history, the match's column
    /// and length (in characters), and the line. Case is ignored unless
    /// `text` has capitals.
    pub fn find_lines(&self, text: &str, max: usize) -> Vec<(u64, usize, usize, String)> {
        let exact = text.chars().any(char::is_uppercase);
        let wanted = if exact { text.to_owned() } else { text.to_lowercase() };
        let term = self.term.lock();
        let history = term.history_size() as i64;
        let mut found = Vec::new();
        let mut line = term.bottommost_line();
        while line >= term.topmost_line() && found.len() < max {
            let row = term.bounds_to_string(
                Point::new(line, Column(0)),
                Point::new(line, term.last_column()),
            );
            let row = row.trim_end();
            let haystack = if exact { row.to_owned() } else { row.to_lowercase() };
            if let Some(at) = haystack.find(&wanted) {
                let column = haystack[..at].chars().count();
                let from_top = (i64::from(line.0) + history) as u64;
                found.push((from_top, column, wanted.chars().count(), row.to_owned()));
            }
            line -= 1;
        }
        found
    }

    /// Select what [`Session::find_lines`] found at `line` (from the top of
    /// the history), and scroll to it.
    pub fn reveal(&mut self, line: u64, column: usize, length: usize) {
        let mut term = self.term.lock();
        let history = term.history_size() as i64;
        let line = Line((line as i64 - history) as i32)
            .max(term.topmost_line())
            .min(term.bottommost_line());
        let last = term.last_column();
        let start = Point::new(line, Column(column.min(last.0)));
        let end = Point::new(line, Column((column + length.max(1) - 1).min(last.0)));
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        term.scroll_to_point(start);
    }

    /// Drop the scrollback and ask the program to redraw (Clear Buffer).
    pub fn clear_buffer(&self) {
        use artty_vt::ansi::{ClearMode, Handler};
        self.term.lock().clear_screen(ClearMode::Saved);
        // Ctrl-L: shells clear the screen and redraw their prompt.
        self.write(&b"\x0c"[..]);
    }

    pub fn end_search(&mut self) {
        self.search = None;
        self.clear_selection();
    }

    /// The link under a viewport cell: an OSC 8 hyperlink, or a URL in the text.
    pub fn link_at(&self, line: usize, column: usize) -> Option<String> {
        self.link_span(line, column).map(|(url, _)| url)
    }

    /// The link under a viewport cell and the columns it covers on that line.
    pub fn link_span(&self, line: usize, column: usize) -> Option<(String, Range<usize>)> {
        let term = self.term.lock();
        let point = viewport_point(&term, line, column);
        let row = &term.grid()[point.line];
        if let Some(link) = row[point.column].hyperlink() {
            let same =
                |c: usize| row[Column(c)].hyperlink().is_some_and(|other| other.id() == link.id());
            let start =
                (0..point.column.0).rev().take_while(|&c| same(c)).last().unwrap_or(point.column.0);
            let end = (point.column.0..term.columns())
                .take_while(|&c| same(c))
                .last()
                .unwrap_or(point.column.0)
                + 1;
            return Some((link.uri().to_owned(), start..end));
        }
        let text: Vec<char> = (0..term.columns()).map(|c| row[Column(c)].c).collect();
        url_at(&text, point.column.0)
    }

    /// Process an event forwarded from the PTY thread. Answers queries that
    /// need terminal state and returns what the UI should do, if anything.
    pub fn handle(&mut self, event: Event) -> Option<Notice> {
        match event {
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => {
                Some(Notice::Redraw)
            },
            Event::Title(title) => Some(Notice::Title(title)),
            Event::ResetTitle => Some(Notice::ResetTitle),
            Event::Bell => Some(Notice::Bell),
            Event::ClipboardStore(kind, text) => Some(Notice::ClipboardStore(kind, text)),
            Event::ClipboardLoad(kind, format) => Some(Notice::ClipboardLoad(kind, format)),
            Event::ColorRequest(index, format) => {
                let rgb = self.theme.color_for_index(index, self.term.lock().colors());
                self.send(Msg::Input(format(rgb).into_bytes().into()));
                None
            },
            Event::TextAreaSizeRequest(format) => {
                self.send(Msg::Input(format(self.size.into()).into_bytes().into()));
                None
            },
            Event::Exit | Event::ChildExit(_) => Some(Notice::Exited),
            Event::PtyWrite(text) => {
                self.send(Msg::Input(text.into_bytes().into()));
                None
            },
            Event::ShellMark { mark, line, column } => {
                let term = self.term.lock();
                match self.shell.mark(mark, line, column, &term)? {
                    ShellEvent::CommandStarted(command) => Some(Notice::CommandStarted(command)),
                    ShellEvent::CommandFinished(record) => Some(Notice::CommandFinished(record)),
                }
            },
            Event::Notification { title, body } => Some(Notice::Notification { title, body }),
            Event::WorkingDirectory(url) => {
                self.reported_cwd = path_from_file_url(&url);
                None
            },
            Event::Progress(progress) => Some(Notice::Progress(progress)),
            Event::ProgramStatus(report) => Some(Notice::ProgramStatus(report)),
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

/// A terminal for a new session, as `options` say, and its options.
fn new_term(
    options: &SessionOptions,
    size: TerminalSize,
    proxy: EventProxy,
) -> (Arc<FairMutex<Term<EventProxy>>>, Config) {
    let mut config = term_config();
    if let Some(scrollback) = options.scrollback {
        config.scrolling_history = scrollback;
    }
    config.default_cursor_style.shape = options.cursor_shape;
    let term_config = config.clone();
    let mut term = Term::new(config, &size, proxy);
    term.set_cell_size(size.cell_width, size.cell_height);
    if let Some(output) = &options.initial_output {
        artty_vt::ansi::Processor::<artty_vt::ansi::StdSyncHandler>::new()
            .advance(&mut term, output);
    }
    (Arc::new(FairMutex::new(term)), term_config)
}

/// Terminal options for every session: programs can opt into the kitty
/// keyboard protocol (Shift+Enter etc.), and copy mode's cursor is a steady block.
fn term_config() -> Config {
    Config {
        kitty_keyboard: true,
        vi_mode_cursor_style: Some(artty_vt::ansi::CursorStyle {
            shape: artty_vt::ansi::CursorShape::Block,
            blinking: false,
        }),
        ..Config::default()
    }
}

fn viewport_point<T>(term: &Term<T>, line: usize, column: usize) -> Point {
    let offset = term.grid().display_offset() as i32;
    let line = (line as i32).min(term.screen_lines() as i32 - 1) - offset;
    Point::new(Line(line), Column(column.min(term.columns() - 1)))
}

/// Drop trailing blank space from each line and trailing empty lines.
fn trim_lines(text: String) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let end = lines.iter().rposition(|line| !line.is_empty()).map_or(0, |i| i + 1);
    lines[..end].join("\n")
}

/// A URL in `line` covering `column`, and the columns it spans.
fn url_at(line: &[char], column: usize) -> Option<(String, Range<usize>)> {
    const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "mailto:"];
    let is_url_char = |c: char| !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '\0');

    // Expand around the column to the surrounding run of URL characters.
    if !is_url_char(*line.get(column)?) {
        return None;
    }
    let start = line[..column].iter().rposition(|&c| !is_url_char(c)).map_or(0, |i| i + 1);
    let end =
        line[column..].iter().position(|&c| !is_url_char(c)).map_or(line.len(), |i| column + i);
    let word: String = line[start..end].iter().collect();

    // The URL starts at a scheme inside the word (e.g. `(https://...)`).
    let (offset, _) = SCHEMES.iter().filter_map(|s| word.find(s).map(|i| (i, s))).min()?;
    let url = &word[offset..];
    if start + word[..offset].chars().count() > column {
        return None;
    }
    // Trailing punctuation usually belongs to the sentence, and a closing
    // paren only to the URL if it opened one.
    let mut url = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']).to_owned();
    while url.ends_with(')') && url.matches(')').count() > url.matches('(').count() {
        url.pop();
    }
    let first = start + word[..offset].chars().count();
    let span = first..first + url.chars().count();
    span.contains(&column).then_some((url, span))
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.pty.send(Msg::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str, column: usize) -> Option<String> {
        url_at(&text.chars().collect::<Vec<_>>(), column).map(|(url, _)| url)
    }

    #[test]
    fn finds_urls() {
        let text = "see (https://example.com/a_(b)) now.";
        assert_eq!(url(text, 10).as_deref(), Some("https://example.com/a_(b)"));
        assert_eq!(url(text, 1), None);
        assert_eq!(url("go to http://x.io/y.", 8).as_deref(), Some("http://x.io/y"));
        assert_eq!(url("no link here", 3), None);
        // Trailing punctuation isn't part of the link, or its span.
        assert_eq!(url("go to http://x.io/y.", 19), None);
        let (_, span) = url_at(&"see (https://a.io) x".chars().collect::<Vec<_>>(), 6).unwrap();
        assert_eq!(span, 5..17);
    }

    /// A session running `printf` with `text`, once its output has arrived.
    fn session_with(text: &str) -> Session {
        let size = TerminalSize { columns: 40, lines: 6, cell_width: 8, cell_height: 16 };
        let options = SessionOptions {
            command: Some(("/usr/bin/printf".into(), vec![text.into()])),
            ..Default::default()
        };
        let session =
            Session::spawn(1, options, size, Arc::new(Theme::default()), Arc::new(|_, _| {}))
                .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while session.screen_text().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        session
    }

    /// The text of a frame line, runs joined.
    fn line_text(frame: &Frame, line: usize) -> String {
        frame.lines[line].runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn prompts_command_output_and_copy_mode() {
        // Three commands of three lines of output each, then a prompt:
        // thirteen lines on a six-line screen.
        let mut script = String::new();
        for i in 1..=3 {
            script.push_str(&format!(
                "\\033]133;A\\007$ \\033]133;B\\007cmd{i}\\r\\n\\033]133;C\\007\
                 out{i}a\\r\\nout{i}b\\r\\nout{i}c\\r\\n\\033]133;D;0\\007"
            ));
        }
        script.push_str("\\033]133;A\\007$ ");
        let size = TerminalSize { columns: 40, lines: 6, cell_width: 8, cell_height: 16 };
        let options = SessionOptions {
            command: Some(("/usr/bin/printf".into(), vec![script])),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let sink: Arc<dyn Fn(SessionId, Event) + Send + Sync> =
            Arc::new(move |_, event| drop(tx.lock().unwrap().send(event)));
        let mut session =
            Session::spawn(1, options, size, Arc::new(Theme::default()), sink).unwrap();
        // The marks come as events, handled as the server would.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while session.shell.prompts.len() < 4 && std::time::Instant::now() < deadline {
            if let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
                session.handle(event);
            }
        }
        assert_eq!(session.shell.prompts.len(), 4);
        // At the bottom: the last command.
        assert_eq!(
            session.command_output(),
            Some((Some("cmd3".into()), "out3a\nout3b\nout3c".into()))
        );

        // cmd3's prompt is on screen already, so back goes to cmd2's, then cmd1's.
        assert!(session.jump_to_prompt(true));
        assert!(line_text(&session.frame(), 0).contains("cmd2"));
        assert_eq!(
            session.command_output(),
            Some((Some("cmd2".into()), "out2a\nout2b\nout2c".into()))
        );
        assert!(session.jump_to_prompt(true));
        assert!(line_text(&session.frame(), 0).contains("cmd1"));
        assert!(!session.jump_to_prompt(true));
        // Forward: cmd2's, then the bottom (cmd3's can't reach the top).
        assert!(session.jump_to_prompt(false));
        assert!(line_text(&session.frame(), 0).contains("cmd2"));
        assert!(session.jump_to_prompt(false));
        assert!(!session.jump_to_prompt(false));

        // Copy mode: up to cmd2's prompt, select its line, copy it.
        session.copy_mode(CopyModeOp::Enter);
        assert!(session.mode().contains(artty_vt::term::TermMode::VI));
        session.copy_mode(CopyModeOp::Move(CopyMotion::PromptUp));
        session.copy_mode(CopyModeOp::Move(CopyMotion::PromptUp));
        session.copy_mode(CopyModeOp::Select(1));
        assert_eq!(session.selection_text().as_deref(), Some("$ cmd2\n"));
        session.copy_mode(CopyModeOp::Move(CopyMotion::Down));
        session.copy_mode(CopyModeOp::Select(0));
        assert_eq!(session.selection_text().as_deref(), Some("$ cmd2\no"));
        session.copy_mode(CopyModeOp::Exit);
        assert!(!session.mode().contains(artty_vt::term::TermMode::VI));
        assert_eq!(session.selection_text(), None);
    }

    #[test]
    fn search_reading_and_links() {
        let mut session = session_with("foo bar\\nsee https://example.com/x\\nbaz foo\\n");
        assert_eq!(session.screen_text(), "foo bar\nsee https://example.com/x\nbaz foo");
        assert_eq!(session.scrollback_text(1), "baz foo");

        // Searching up from the bottom finds the last "foo", then the first.
        assert_eq!(session.search("fo+", true), Ok(true));
        assert_eq!(session.selection_text().as_deref(), Some("foo"));
        let frame = session.frame();
        assert_eq!(frame.lines[2].selection.first().map(|r| r.column), Some(4));
        assert_eq!(session.search("fo+", true), Ok(true));
        assert_eq!(session.frame().lines[0].selection.first().map(|r| r.column), Some(0));
        // Every visible match is highlighted.
        let lines = session.frame().lines;
        let matched: Vec<_> = lines
            .iter()
            .map(|l| l.matches.iter().map(|r| (r.column, r.width)).collect::<Vec<_>>())
            .collect();
        assert_eq!(&matched[..3], &[vec![(0, 3)], vec![], vec![(4, 3)]]);
        assert_eq!(session.search("nothing", true), Ok(false));
        assert!(session.search("(", true).is_err());

        assert_eq!(session.link_at(1, 10).as_deref(), Some("https://example.com/x"));
        assert_eq!(session.link_span(1, 10).map(|(_, span)| span), Some(4..25));
        assert_eq!(session.link_at(0, 1), None);
    }

    #[test]
    fn trims_text() {
        assert_eq!(trim_lines("a  \nb\n  \n\n".to_owned()), "a\nb");
    }
}
