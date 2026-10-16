use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
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
use artty_vt::term::search::RegexSearch;
use artty_vt::term::{ClipboardType, Config, Term};
use artty_vt::tty;
use log::warn;

use crate::frame::Frame;
use crate::process::{ProcessInfo, ProcessProbe, path_from_file_url};
use crate::shell::{CommandRecord, ShellEvent, ShellPhase, ShellTracker};
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
    /// Output shown before the program starts, e.g. a restored tab's history.
    pub initial_output: Option<Vec<u8>>,
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
    Notification { title: Option<String>, body: String },
    Progress(Progress),
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
            }
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
        let mut term = Term::new(config, &size, proxy.clone());
        if let Some(output) = &options.initial_output {
            artty_vt::ansi::Processor::<artty_vt::ansi::StdSyncHandler>::new().advance(&mut term, output);
        }
        let term = Arc::new(FairMutex::new(term));

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
        let probe = ProcessProbe::new(pty.file().as_raw_fd(), pty.child().id());

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
        })
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
        let text = term.bounds_to_string(Point::new(Line(top), Column(0)), Point::new(bottom, term.last_column()));
        trim_lines(text)
    }

    /// The last `lines` lines of output with color codes, for saving.
    pub fn styled_scrollback(&self, lines: usize) -> String {
        crate::state::styled_text(&self.term.lock(), lines)
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
            (None, true) => (Point::new(term.bottommost_line(), term.last_column()), Direction::Left),
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

    pub fn end_search(&mut self) {
        self.search = None;
        self.clear_selection();
    }

    /// The link under a viewport cell: an OSC 8 hyperlink, or a URL in the text.
    pub fn link_at(&self, line: usize, column: usize) -> Option<String> {
        let term = self.term.lock();
        let point = viewport_point(&term, line, column);
        if let Some(link) = term.grid()[point].hyperlink() {
            return Some(link.uri().to_owned());
        }
        let row = &term.grid()[point.line];
        let text: Vec<char> = (0..term.columns()).map(|c| row[Column(c)].c).collect();
        url_at(&text, point.column.0)
    }

    /// Process an event forwarded from the PTY thread. Answers queries that
    /// need terminal state and returns what the UI should do, if anything.
    pub fn handle(&mut self, event: Event) -> Option<Notice> {
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
            Event::ShellMark { mark, line, column } => {
                let term = self.term.lock();
                match self.shell.mark(mark, line, column, &term)? {
                    ShellEvent::CommandStarted(command) => Some(Notice::CommandStarted(command)),
                    ShellEvent::CommandFinished(record) => Some(Notice::CommandFinished(record)),
                }
            }
            Event::Notification { title, body } => Some(Notice::Notification { title, body }),
            Event::WorkingDirectory(url) => {
                self.reported_cwd = path_from_file_url(&url);
                None
            }
            Event::Progress(progress) => Some(Notice::Progress(progress)),
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

/// Drop trailing blank space from each line and trailing empty lines.
fn trim_lines(text: String) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let end = lines.iter().rposition(|line| !line.is_empty()).map_or(0, |i| i + 1);
    lines[..end].join("\n")
}

/// A URL in `line` covering `column`.
fn url_at(line: &[char], column: usize) -> Option<String> {
    const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "mailto:"];
    let is_url_char = |c: char| !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '\0');

    // Expand around the column to the surrounding run of URL characters.
    if !is_url_char(*line.get(column)?) {
        return None;
    }
    let start = line[..column].iter().rposition(|&c| !is_url_char(c)).map_or(0, |i| i + 1);
    let end = line[column..].iter().position(|&c| !is_url_char(c)).map_or(line.len(), |i| column + i);
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
    Some(url)
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
        url_at(&text.chars().collect::<Vec<_>>(), column)
    }

    #[test]
    fn finds_urls() {
        let text = "see (https://example.com/a_(b)) now.";
        assert_eq!(url(text, 10).as_deref(), Some("https://example.com/a_(b)"));
        assert_eq!(url(text, 1), None);
        assert_eq!(url("go to http://x.io/y.", 8).as_deref(), Some("http://x.io/y"));
        assert_eq!(url("no link here", 3), None);
    }

    /// A session running `printf` with `text`, once its output has arrived.
    fn session_with(text: &str) -> Session {
        let size = TerminalSize { columns: 40, lines: 6, cell_width: 8, cell_height: 16 };
        let options = SessionOptions {
            command: Some(("/usr/bin/printf".into(), vec![text.into()])),
            ..Default::default()
        };
        let session = Session::spawn(1, options, size, Arc::new(Theme::default()), Arc::new(|_, _| {})).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while session.screen_text().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        session
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
        assert_eq!(session.search("nothing", true), Ok(false));
        assert!(session.search("(", true).is_err());

        assert_eq!(session.link_at(1, 10).as_deref(), Some("https://example.com/x"));
        assert_eq!(session.link_at(0, 1), None);
    }

    #[test]
    fn trims_text() {
        assert_eq!(trim_lines("a  \nb\n  \n\n".to_owned()), "a\nb");
    }
}
