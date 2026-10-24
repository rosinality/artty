//! Commands seen through shell integration (OSC 133 marks).

use std::collections::VecDeque;
use std::time::Instant;

use artty_vt::ansi::ShellMark;
use artty_vt::grid::Dimensions;
use artty_vt::index::{Column, Line, Point};
use artty_vt::term::Term;

/// Finished commands kept per session.
const HISTORY: usize = 50;
/// Output kept per command; longer output keeps its end.
const MAX_OUTPUT: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct CommandRecord {
    /// The command line as typed, when the shell marks where it starts.
    pub command: Option<String>,
    pub output: String,
    /// Output was longer than kept, or scrolled out of history.
    pub truncated: bool,
    pub exit: Option<i32>,
    pub started: Instant,
    pub finished: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellPhase {
    /// No shell-integration marks seen yet.
    #[default]
    Unknown,
    /// At the prompt, waiting for a command.
    Prompt,
    /// A command is running.
    Running,
}

pub enum ShellEvent {
    CommandStarted(Option<String>),
    CommandFinished(CommandRecord),
}

#[derive(Default)]
pub(crate) struct ShellTracker {
    pub phase: ShellPhase,
    /// Where the user started typing (the `B` mark).
    input_start: Option<(u64, usize)>,
    /// Stable line where the running command's output starts (the `C` mark).
    output_start: Option<u64>,
    current: Option<CommandRecord>,
    pub history: VecDeque<CommandRecord>,
}

/// Grid point for a stable line, clamped into what the grid still holds.
/// Returns whether clamping cut off the top.
fn grid_point<T>(term: &Term<T>, stable: u64, column: usize) -> (Point, bool) {
    let grid = term.grid();
    let line = stable as i64 - grid.lines_scrolled() as i64;
    let top = grid.topmost_line().0 as i64;
    let bottom = grid.bottommost_line().0 as i64;
    let clamped = line.clamp(top, bottom);
    let column = if clamped == line { column.min(grid.last_column().0) } else { 0 };
    (Point::new(Line(clamped as i32), Column(column)), line < top)
}

/// Text from `start` up to, but not including, `end`.
fn text_between<T>(term: &Term<T>, start: Point, end: Point) -> String {
    if end <= start {
        return String::new();
    }
    let end = if end.column.0 == 0 {
        Point::new(end.line - 1, term.grid().last_column())
    } else {
        Point::new(end.line, end.column - 1)
    };
    if end < start {
        return String::new();
    }
    term.bounds_to_string(start, end)
}

/// What a shell tracker carries across a server hand-over: where the prompt
/// and the running command's output are. (Finished commands' output isn't
/// kept; `artty read --last` starts over.)
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ShellState {
    phase: ShellPhase,
    input_start: Option<(u64, usize)>,
    output_start: Option<u64>,
}

impl ShellTracker {
    pub(crate) fn state(&self) -> ShellState {
        ShellState { phase: self.phase, input_start: self.input_start, output_start: self.output_start }
    }

    pub(crate) fn from_state(state: ShellState) -> Self {
        Self { phase: state.phase, input_start: state.input_start, output_start: state.output_start, ..Self::default() }
    }

    pub(crate) fn mark<T>(&mut self, mark: ShellMark, line: u64, column: usize, term: &Term<T>) -> Option<ShellEvent> {
        match mark {
            ShellMark::PromptStart => {
                // A prompt without `D` means the command ended without a report.
                let finished = self.finish(None, line, 0, term);
                self.phase = ShellPhase::Prompt;
                self.input_start = None;
                finished
            }
            ShellMark::CommandStart => {
                self.phase = ShellPhase::Prompt;
                self.input_start = Some((line, column));
                None
            }
            ShellMark::CommandExecuted => {
                let command = self.input_start.take().map(|(input_line, input_column)| {
                    let (start, _) = grid_point(term, input_line, input_column);
                    let (end, _) = grid_point(term, line, column);
                    let text = text_between(term, start, end);
                    text.trim().to_owned()
                });
                let command = command.filter(|c| !c.is_empty());
                self.phase = ShellPhase::Running;
                self.output_start = Some(line);
                self.current = Some(CommandRecord {
                    command: command.clone(),
                    output: String::new(),
                    truncated: false,
                    exit: None,
                    started: Instant::now(),
                    finished: None,
                });
                Some(ShellEvent::CommandStarted(command))
            }
            ShellMark::CommandFinished(exit) => {
                let finished = self.finish(exit, line, column, term);
                self.phase = ShellPhase::Prompt;
                finished
            }
        }
    }

    fn finish<T>(&mut self, exit: Option<i32>, line: u64, column: usize, term: &Term<T>) -> Option<ShellEvent> {
        let mut record = self.current.take()?;
        let output_start = self.output_start.take()?;
        let (start, cut) = grid_point(term, output_start, 0);
        let (end, _) = grid_point(term, line, column);
        let mut output = text_between(term, start, end);
        if output.len() > MAX_OUTPUT {
            let mut cut_at = output.len() - MAX_OUTPUT;
            while !output.is_char_boundary(cut_at) {
                cut_at += 1;
            }
            output.drain(..cut_at);
            record.truncated = true;
        }
        record.output = output.trim_end_matches('\n').to_owned();
        record.truncated |= cut;
        record.exit = exit;
        record.finished = Some(Instant::now());
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(record.clone());
        Some(ShellEvent::CommandFinished(record))
    }
}

#[cfg(test)]
mod tests {
    use artty_vt::ansi::{Processor, StdSyncHandler};
    use artty_vt::event::{Event, EventListener};
    use artty_vt::term::Config;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::TerminalSize;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Event>>>);

    impl EventListener for Recorder {
        fn send_event(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Feed `bytes`, handling marks as the session would.
    fn run(chunks: &[&[u8]], lines: u16) -> Vec<CommandRecord> {
        let recorder = Recorder::default();
        let size = TerminalSize { columns: 30, lines, cell_width: 8, cell_height: 16 };
        let mut term = Term::new(Config::default(), &size, recorder.clone());
        let mut processor = Processor::<StdSyncHandler>::new();
        let mut tracker = ShellTracker::default();
        let mut finished = Vec::new();
        for chunk in chunks {
            processor.advance(&mut term, chunk);
            let events: Vec<_> = recorder.0.lock().unwrap().drain(..).collect();
            for event in events {
                if let Event::ShellMark { mark, line, column } = event
                    && let Some(ShellEvent::CommandFinished(record)) = tracker.mark(mark, line, column, &term)
                {
                    finished.push(record);
                }
            }
        }
        finished
    }

    const PROMPT: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07";

    #[test]
    fn captures_command_and_output() {
        let records = run(&[PROMPT, b"echo hi\r\n\x1b]133;C\x07", b"hi\r\nthere\r\n\x1b]133;D;0\x07", PROMPT], 10);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command.as_deref(), Some("echo hi"));
        assert_eq!(records[0].output, "hi\nthere");
        assert_eq!(records[0].exit, Some(0));
        assert!(!records[0].truncated);
    }

    #[test]
    fn output_that_scrolled_into_history() {
        let mut output = Vec::new();
        for i in 0..20 {
            output.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        let records = run(&[PROMPT, b"seq\r\n\x1b]133;C\x07", &output, b"\x1b]133;D;1\x07"], 5);
        let lines: Vec<_> = records[0].output.lines().collect();
        assert_eq!(lines.first(), Some(&"line 0"));
        assert_eq!(lines.last(), Some(&"line 19"));
        assert_eq!(records[0].exit, Some(1));
    }

    #[test]
    fn missing_finish_mark_ends_at_next_prompt() {
        let records = run(&[PROMPT, b"x\r\n\x1b]133;C\x07", b"out\r\n", PROMPT], 10);
        assert_eq!(records[0].output, "out");
        assert_eq!(records[0].exit, None);
    }
}
