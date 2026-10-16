//! Tests for artty's additions to the terminal core.

use std::sync::{Arc, Mutex};

use artty_vt::ansi::{Processor, Progress, ShellMark, StdSyncHandler};
use artty_vt::event::{Event, EventListener};
use artty_vt::grid::Dimensions;
use artty_vt::index::{Column, Line, Point};
use artty_vt::term::{Config, Term};

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

fn run(bytes: &[u8], lines: usize) -> (Term<Recorder>, Vec<Event>) {
    let recorder = Recorder::default();
    let mut term = Term::new(Config::default(), &Size(20, lines), recorder.clone());
    Processor::<StdSyncHandler>::new().advance(&mut term, bytes);
    let events = recorder.0.lock().unwrap().drain(..).collect();
    (term, events)
}

fn marks(events: &[Event]) -> Vec<(ShellMark, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ShellMark { mark, line, .. } => Some((*mark, *line)),
            _ => None,
        })
        .collect()
}

#[test]
fn shell_marks_carry_stable_lines() {
    let (_, events) = run(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\nb\r\n\x1b]133;D;0\x07", 10);
    assert_eq!(
        marks(&events),
        vec![
            (ShellMark::PromptStart, 0),
            (ShellMark::CommandStart, 0),
            (ShellMark::CommandExecuted, 1),
            (ShellMark::CommandFinished(Some(0)), 3),
        ]
    );
}

#[test]
fn stable_lines_survive_scrolling() {
    // 3-line screen: the mark at line 0 scrolls into history, but its stable
    // line still finds the same text.
    let (term, events) = run(b"\x1b]133;C\x07first\r\nb\r\nc\r\nd\r\ne", 3);
    let (_, stable) = marks(&events)[0];
    let line = stable as i64 - term.grid().lines_scrolled() as i64;
    assert_eq!(line, -2);
    let row = &term.grid()[Line(line as i32)];
    let text: String = (0..5).map(|c| row[Column(c)].c).collect();
    assert_eq!(text, "first");
    assert_eq!(term.grid().cursor.point, Point::new(Line(2), Column(1)));
}

#[test]
fn exit_codes_and_options() {
    let (_, events) = run(b"\x1b]133;D;127\x07\x1b]133;D\x07\x1b]133;A;cl=m\x07", 5);
    let kinds: Vec<_> = marks(&events).into_iter().map(|(m, _)| m).collect();
    assert_eq!(
        kinds,
        vec![ShellMark::CommandFinished(Some(127)), ShellMark::CommandFinished(None), ShellMark::PromptStart]
    );
}

#[test]
fn notifications() {
    let (_, events) = run(b"\x1b]9;Build done\x07\x1b]777;notify;Claude;Needs input\x1b\\\x1b]99;;Hi\x07", 5);
    let notes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Notification { title, body } => Some((title.clone(), body.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes,
        vec![
            (None, "Build done".to_owned()),
            (Some("Claude".to_owned()), "Needs input".to_owned()),
            (Some("Hi".to_owned()), String::new()),
        ]
    );
}

#[test]
fn progress_and_working_directory() {
    let (_, events) = run(b"\x1b]9;4;1;42\x07\x1b]9;4;3\x07\x1b]9;4;0\x07\x1b]7;file://host/Users/me/a%20b\x07", 5);
    let mut progress = Vec::new();
    let mut cwd = None;
    for event in &events {
        match event {
            Event::Progress(p) => progress.push(*p),
            Event::WorkingDirectory(url) => cwd = Some(url.clone()),
            _ => {}
        }
    }
    assert_eq!(progress, vec![Progress::Normal(42), Progress::Indeterminate, Progress::Remove]);
    assert_eq!(cwd.as_deref(), Some("file://host/Users/me/a%20b"));
}
