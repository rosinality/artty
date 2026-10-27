//! Local echo, as mosh does it: over a slow link, what you type shows at
//! once, drawn over the pane's last screen, before the server's echo comes
//! back. Each guess is checked against the screens that follow: one the
//! server shows is done with; one it doesn't show in time means the guessing
//! was wrong (a program that doesn't echo, or draws elsewhere), so the
//! guesses go and guessing waits for the next Enter.
//!
//! After an Enter, guesses stay hidden until the first is confirmed: the
//! next program might be asking for a password, which never echoes, so
//! nothing typed there is ever shown.

use std::time::{Duration, Instant};

use artty_core::Frame;
use artty_core::FrameRun;
use artty_core::vt::ansi::CursorShape;
use artty_core::vt::term::TermMode;

/// Guesses older than this many round trips (and at least `MIN_WAIT`)
/// without the server showing them were wrong.
const ROUND_TRIPS: u32 = 4;
const MIN_WAIT: Duration = Duration::from_millis(300);

/// A pane's guesses.
#[derive(Debug, Default)]
pub struct Echo {
    guesses: Vec<Guess>,
    /// Shown, not just tracked: a guess was confirmed since the last Enter.
    trusted: bool,
    /// Wrong lately: no guessing until the next Enter.
    off: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Guess {
    line: usize,
    column: usize,
    character: char,
    at: Instant,
}

impl Echo {
    /// `bytes` typed into a pane showing `frame`: guess what they show.
    pub fn typed(&mut self, frame: &Frame, mode: TermMode, bytes: &[u8], now: Instant) {
        let enter = bytes.contains(&b'\r');
        if enter {
            // A new command line: maybe a program that doesn't echo next.
            self.guesses.clear();
            self.trusted = false;
            self.off = false;
            return;
        }
        if self.off || !guessable(frame, mode) {
            return self.guesses.clear();
        }
        match bytes {
            // Backspace takes back the last guess (anything else, the
            // program decides).
            [0x7f] | [0x08] => {
                if self.guesses.pop().is_none() {
                    self.guesses.clear();
                }
            },
            _ => {
                let Some(text) =
                    std::str::from_utf8(bytes).ok().filter(|t| !t.chars().any(char::is_control))
                else {
                    // Arrows, ^C, escapes: no telling.
                    return self.guesses.clear();
                };
                let Some(cursor) = frame.cursor else { return };
                let (line, mut column) = self
                    .guesses
                    .last()
                    .map_or((cursor.line, cursor.column), |g| (g.line, g.column + 1));
                for character in text.chars() {
                    // Single-width only, and not past the line's end.
                    let single = character.is_ascii_graphic() || character == ' ';
                    if !single || column + 1 >= frame.columns {
                        return;
                    }
                    self.guesses.push(Guess { line, column, character, at: now });
                    column += 1;
                }
            },
        }
    }

    /// A new screen from the server: drop the guesses it shows; if one is
    /// overdue (or the cursor left the line), all were wrong.
    pub fn confirm(&mut self, frame: &Frame, round_trip: Duration, now: Instant) {
        if self.guesses.is_empty() {
            return;
        }
        let before = self.guesses.len();
        self.guesses.retain(|g| cell(frame, g.line, g.column) != Some(g.character));
        if self.guesses.len() < before {
            self.trusted = true;
        }
        let moved =
            frame.cursor.is_none_or(|c| self.guesses.first().is_some_and(|g| c.line != g.line));
        if moved || self.overdue(round_trip, now) {
            self.wrong();
        }
    }

    /// Time passing without screens: overdue guesses were wrong. Returns
    /// whether anything changed.
    pub fn expire(&mut self, round_trip: Duration, now: Instant) -> bool {
        if self.overdue(round_trip, now) {
            self.wrong();
            return true;
        }
        false
    }

    fn overdue(&self, round_trip: Duration, now: Instant) -> bool {
        let wait = (round_trip * ROUND_TRIPS).max(MIN_WAIT);
        self.guesses.first().is_some_and(|g| now.duration_since(g.at) > wait)
    }

    fn wrong(&mut self) {
        self.guesses.clear();
        self.trusted = false;
        self.off = true;
    }

    /// Draw the guesses into `frame` (a copy for painting), with the cursor
    /// after them.
    pub fn overlay(&self, frame: &mut Frame) {
        if !self.trusted || self.guesses.is_empty() {
            return;
        }
        for guess in &self.guesses {
            let Some(line) = frame.lines.get_mut(guess.line) else { continue };
            put(&mut line.runs, guess.column, guess.character, frame.foreground);
        }
        if let (Some(cursor), Some(last)) = (frame.cursor.as_mut(), self.guesses.last()) {
            cursor.line = last.line;
            cursor.column = last.column + 1;
            cursor.wide = false;
        }
    }

    /// Whether there's anything to draw or check.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.guesses.is_empty()
    }
}

/// Whether typing can be guessed: the live screen (not scrolled back or a
/// full-screen program) with a cursor showing.
fn guessable(frame: &Frame, mode: TermMode) -> bool {
    frame.display_offset == 0
        && !mode.intersects(TermMode::ALT_SCREEN | TermMode::VI)
        && frame.cursor.is_some_and(|c| c.shape != CursorShape::Hidden)
}

/// The character in a cell, if its run is single-width text.
fn cell(frame: &Frame, line: usize, column: usize) -> Option<char> {
    let run = frame
        .lines
        .get(line)?
        .runs
        .iter()
        .find(|r| r.column <= column && column < r.column + r.cells)?;
    (run.text.chars().count() == run.cells).then(|| run.text.chars().nth(column - run.column))?
}

/// Write `character` at `column`, splitting the run there.
fn put(runs: &mut Vec<FrameRun>, column: usize, character: char, fg: artty_core::vt::ansi::Rgb) {
    let mut kept = Vec::with_capacity(runs.len() + 2);
    for run in runs.drain(..) {
        let end = run.column + run.cells;
        if end <= column || run.column > column {
            kept.push(run);
            continue;
        }
        // Wide characters can't be split by cell: the run goes.
        if run.text.chars().count() != run.cells {
            continue;
        }
        let chars: Vec<char> = run.text.chars().collect();
        let split = column - run.column;
        if split > 0 {
            kept.push(FrameRun {
                cells: split,
                text: chars[..split].iter().collect(),
                ..run.clone()
            });
        }
        if split + 1 < chars.len() {
            kept.push(FrameRun {
                column: column + 1,
                cells: chars.len() - split - 1,
                text: chars[split + 1..].iter().collect(),
                ..run.clone()
            });
        }
    }
    kept.push(FrameRun {
        column,
        cells: 1,
        text: character.to_string(),
        fg,
        bold: false,
        italic: false,
        strikeout: false,
    });
    kept.sort_by_key(|run| run.column);
    *runs = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use artty_core::vt::ansi::Rgb;
    use artty_core::{FrameCursor, FrameLine};

    const WHITE: Rgb = Rgb { r: 255, g: 255, b: 255 };

    /// A screen with `text` on line 0 and the cursor after it.
    fn screen(text: &str) -> Frame {
        let mut line = FrameLine::default();
        if !text.is_empty() {
            line.runs.push(FrameRun {
                column: 0,
                cells: text.chars().count(),
                text: text.into(),
                fg: WHITE,
                bold: false,
                italic: false,
                strikeout: false,
            });
        }
        Frame {
            columns: 40,
            lines: vec![line, FrameLine::default()],
            cursor: Some(FrameCursor {
                line: 0,
                column: text.chars().count(),
                shape: CursorShape::Beam,
                wide: false,
                blinking: false,
                color: WHITE,
            }),
            foreground: WHITE,
            ..Default::default()
        }
    }

    fn shown(echo: &Echo, frame: &Frame) -> String {
        let mut frame = frame.clone();
        echo.overlay(&mut frame);
        frame.lines[0].runs.iter().map(|r| r.text.as_str()).collect()
    }

    const TRIP: Duration = Duration::from_millis(100);

    #[test]
    fn guesses_show_once_trusted_and_go_when_confirmed() {
        let now = Instant::now();
        let mut echo = Echo::default();
        let prompt = screen("$ ");
        echo.typed(&prompt, TermMode::empty(), b"l", now);
        // Not trusted yet (after an Enter, maybe a password prompt).
        assert_eq!(shown(&echo, &prompt), "$ ");
        // The server echoes it: trusted; the next keys show at once.
        echo.confirm(&screen("$ l"), TRIP, now);
        assert!(echo.is_empty());
        echo.typed(&screen("$ l"), TermMode::empty(), b"s -", now);
        assert_eq!(shown(&echo, &screen("$ l")), "$ ls -");
        // Backspace takes one back.
        echo.typed(&screen("$ l"), TermMode::empty(), &[0x7f], now);
        assert_eq!(shown(&echo, &screen("$ l")), "$ ls ");
        // The server catches up with part of it.
        echo.confirm(&screen("$ ls"), TRIP, now);
        assert_eq!(shown(&echo, &screen("$ ls")), "$ ls ");
        echo.confirm(&screen("$ ls "), TRIP, now);
        assert!(echo.is_empty());
    }

    #[test]
    fn unconfirmed_guesses_turn_guessing_off_until_enter() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("$ "), TermMode::empty(), b"a", now);
        echo.confirm(&screen("$ a"), TRIP, now);
        // A program that doesn't echo: the guess never shows up.
        echo.typed(&screen("$ a"), TermMode::empty(), b"b", now);
        assert!(!echo.expire(TRIP, now + Duration::from_millis(200)));
        assert!(echo.expire(TRIP, now + Duration::from_secs(1)));
        echo.typed(&screen("$ a"), TermMode::empty(), b"c", now);
        assert!(echo.is_empty(), "off until Enter");
        echo.typed(&screen("$ a"), TermMode::empty(), b"\r", now);
        echo.typed(&screen("$ "), TermMode::empty(), b"d", now);
        assert!(!echo.is_empty());
    }

    #[test]
    fn no_guessing_in_full_screen_programs_or_for_keys() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("x"), TermMode::ALT_SCREEN, b"a", now);
        assert!(echo.is_empty());
        echo.typed(&screen("x"), TermMode::empty(), b"a", now);
        echo.typed(&screen("x"), TermMode::empty(), b"\x1b[A", now);
        assert!(echo.is_empty(), "an arrow: no telling what happens");
    }

    #[test]
    fn writing_into_runs() {
        let mut runs = screen("abcd").lines[0].runs.clone();
        put(&mut runs, 1, 'X', WHITE);
        let text: Vec<(usize, &str)> = runs.iter().map(|r| (r.column, r.text.as_str())).collect();
        assert_eq!(text, [(0, "a"), (1, "X"), (2, "cd")]);
    }
}
