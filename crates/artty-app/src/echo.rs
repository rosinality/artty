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
/// without the server showing them were wrong — counted from when the
/// server was first heard from after they were typed: one stalling (seconds
/// with nothing back) shows them late, so they wait for it, up to
/// `STALL_WAIT`.
const ROUND_TRIPS: u32 = 4;
const MIN_WAIT: Duration = Duration::from_millis(300);
const STALL_WAIT: Duration = Duration::from_secs(30);

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
    /// When the server was first heard from since.
    heard: Option<Instant>,
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
                    self.guesses.push(Guess { line, column, character, at: now, heard: None });
                    column += 1;
                }
            },
        }
    }

    /// A new screen from the server. The guesses its cursor has gone past
    /// were echoed: each must show what was typed (then it's done with), or
    /// all were wrong. Ones still ahead of it wait, even if their cell
    /// already shows the character — that can be a shell's suggestion for
    /// the rest of the line, not the echo. Overdue, or the cursor off the
    /// line: wrong.
    pub fn confirm(&mut self, frame: &Frame, round_trip: Duration, now: Instant) {
        self.hear(now);
        let (Some(first), Some(last)) = (self.guesses.first(), self.guesses.last()) else { return };
        let Some(cursor) = frame.cursor.filter(|c| c.line == first.line) else {
            return self.wrong();
        };
        // Further on than typed: the shell is drawing elsewhere on the
        // line for a moment (a right prompt); judge a later screen.
        if cursor.column <= last.column + 1 {
            let echoed = self.guesses.iter().take_while(|g| g.column < cursor.column).count();
            if self.guesses[..echoed]
                .iter()
                .any(|g| cell(frame, g.line, g.column) != Some(g.character))
            {
                return self.wrong();
            }
            if echoed > 0 {
                self.guesses.drain(..echoed);
                self.trusted = true;
            }
        }
        if self.overdue(round_trip, now, now) {
            self.wrong();
        }
    }

    /// Time passing without screens, the server last heard from at
    /// `heard`: overdue guesses were wrong. Returns whether anything changed.
    pub fn expire(&mut self, round_trip: Duration, heard: Instant, now: Instant) -> bool {
        self.hear(heard);
        if self.overdue(round_trip, heard, now) {
            self.wrong();
            return true;
        }
        false
    }

    /// The server was heard from `at`: the guesses typed before then have
    /// their wait start, if it hadn't.
    fn hear(&mut self, at: Instant) {
        for guess in self.guesses.iter_mut().filter(|g| g.heard.is_none() && g.at <= at) {
            guess.heard = Some(at);
        }
    }

    /// Whether the server, heard from (lastly at `heard`) for long enough
    /// since the first guess, hasn't shown it; or it's waited for a stall
    /// too long.
    fn overdue(&self, round_trip: Duration, heard: Instant, now: Instant) -> bool {
        let wait = (round_trip * ROUND_TRIPS).max(MIN_WAIT);
        self.guesses.first().is_some_and(|g| {
            g.heard.is_some_and(|first| heard.saturating_duration_since(first) > wait)
                || now.saturating_duration_since(g.at) > STALL_WAIT
        })
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

    /// `typed` with a shell's suggestion for the rest after the cursor
    /// (zsh-autosuggestions, fish), as it draws it.
    fn suggested(typed: &str, suggestion: &str) -> Frame {
        let mut frame = screen(&format!("{typed}{suggestion}"));
        frame.cursor.as_mut().unwrap().column = typed.chars().count();
        frame
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

    /// Typing what the shell suggests: the suggestion's characters are
    /// already in the cells being guessed, which isn't the server's echo.
    /// (Taking it for one placed later guesses a cell behind: "herdrr".)
    #[test]
    fn a_suggestion_is_not_an_echo() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("$ "), TermMode::empty(), b"h", now);
        echo.confirm(&suggested("$ h", "erdr"), TRIP, now);
        // Typed faster than the echo: e, r, d, r, all in suggested cells.
        let stale = suggested("$ h", "erdr");
        echo.typed(&stale, TermMode::empty(), b"er", now);
        echo.confirm(&stale, TRIP, now);
        echo.typed(&stale, TermMode::empty(), b"dr", now);
        assert_eq!(shown(&echo, &stale), "$ herdr");
        // The server catches up a key at a time.
        for done in ["$ he", "$ her", "$ herd"] {
            let frame = suggested(done, &"$ herdr"[done.len()..]);
            echo.confirm(&frame, TRIP, now);
            assert_eq!(shown(&echo, &frame), "$ herdr", "at {done}");
        }
        echo.confirm(&screen("$ herdr"), TRIP, now);
        assert!(echo.is_empty());
        assert_eq!(shown(&echo, &screen("$ herdr")), "$ herdr");
    }

    /// The server's cursor went past a guess with something else there:
    /// wrong at once, not after waiting.
    #[test]
    fn a_different_echo_is_wrong_at_once() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("$ "), TermMode::empty(), b"a", now);
        echo.confirm(&screen("$ a"), TRIP, now);
        echo.typed(&screen("$ a"), TermMode::empty(), b"b", now);
        echo.confirm(&screen("$ aX"), TRIP, now);
        assert!(echo.is_empty());
        echo.typed(&screen("$ aX"), TermMode::empty(), b"c", now);
        assert!(echo.is_empty(), "off until Enter");
    }

    /// The server stalls (nothing at all comes back for seconds): the
    /// guesses stay, and its screens, when they come, confirm them.
    #[test]
    fn guesses_wait_out_a_stalled_server() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("$ "), TermMode::empty(), b"a", now);
        echo.confirm(&screen("$ a"), TRIP, now);
        // Typed as it stalls (last heard from just before): five seconds
        // of nothing.
        let typing = now + Duration::from_millis(10);
        echo.typed(&screen("$ a"), TermMode::empty(), b"b", typing);
        echo.typed(&screen("$ a"), TermMode::empty(), b"c", typing);
        echo.typed(&screen("$ a"), TermMode::empty(), b"d", typing);
        let back = now + Duration::from_secs(5);
        assert!(!echo.expire(TRIP, now, back));
        assert!(!echo.is_empty());
        // Then its screens, a character at a time.
        echo.confirm(&screen("$ ab"), TRIP, back);
        echo.confirm(&screen("$ abc"), TRIP, back + Duration::from_millis(20));
        assert!(!echo.is_empty(), "d still to come");
        assert!(!echo.expire(
            TRIP,
            back + Duration::from_millis(30),
            back + Duration::from_millis(30)
        ));
        echo.confirm(&screen("$ abcd"), TRIP, back + Duration::from_millis(40));
        assert!(echo.is_empty(), "all confirmed");
        echo.typed(&screen("$ abcd"), TermMode::empty(), b"e", back);
        assert!(!echo.is_empty(), "still guessing");
        // But not for ever.
        assert!(echo.expire(TRIP, now, back + Duration::from_secs(31)));
    }

    #[test]
    fn unconfirmed_guesses_turn_guessing_off_until_enter() {
        let now = Instant::now();
        let mut echo = Echo::default();
        echo.typed(&screen("$ "), TermMode::empty(), b"a", now);
        echo.confirm(&screen("$ a"), TRIP, now);
        // A program that doesn't echo: the guess never shows up.
        echo.typed(&screen("$ a"), TermMode::empty(), b"b", now);
        // Heard from (a ping's answer) just after, and since.
        let heard = now + Duration::from_millis(5);
        assert!(!echo.expire(TRIP, heard, heard));
        let (soon, later) = (now + Duration::from_millis(200), now + Duration::from_millis(500));
        assert!(!echo.expire(TRIP, soon, soon));
        assert!(echo.expire(TRIP, later, later));
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
