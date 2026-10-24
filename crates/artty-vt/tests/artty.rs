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

#[test]
fn emoji_sequences_share_one_cell() {
    let (term, _) = run("👨\u{200d}👩\u{200d}👧x 👍🏽y".as_bytes(), 3);
    let row = &term.grid()[Line(0)];
    assert_eq!(row[Column(0)].c, '👨');
    assert_eq!(row[Column(0)].zerowidth(), Some(&['\u{200d}', '👩', '\u{200d}', '👧'][..]));
    assert_eq!(row[Column(2)].c, 'x');
    assert_eq!(row[Column(4)].c, '👍');
    assert_eq!(row[Column(4)].zerowidth(), Some(&['🏽'][..]));
    assert_eq!(row[Column(6)].c, 'y');
}

#[test]
fn variation_selector_16_makes_emoji_wide() {
    use artty_vt::term::cell::Flags;
    let (term, _) = run("❤\u{fe0f}x ⚠\u{fe0f}y".as_bytes(), 3);
    let row = &term.grid()[Line(0)];
    assert_eq!(row[Column(0)].c, '❤');
    assert!(row[Column(0)].flags.contains(Flags::WIDE_CHAR));
    assert_eq!(row[Column(0)].zerowidth(), Some(&['\u{fe0f}'][..]));
    assert!(row[Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(row[Column(2)].c, 'x');
    assert_eq!(row[Column(4)].c, '⚠');
    assert_eq!(row[Column(6)].c, 'y');

    // In the last column there's no room: it stays narrow, and nothing breaks.
    let (term, _) = run(format!("{}❤\u{fe0f}z", "a".repeat(19)).as_bytes(), 3);
    assert_eq!(term.grid()[Line(0)][Column(19)].c, '❤');
    assert_eq!(term.grid()[Line(1)][Column(0)].c, 'z');
}

fn kitty_term() -> Term<Recorder> {
    let config = Config { kitty_keyboard: true, ..Config::default() };
    Term::new(config, &Size(20, 3), Recorder::default())
}

#[test]
fn deep_keyboard_mode_pushes_are_capped_not_fatal() {
    // Upstream removed from the title stack here and panicked once it was
    // empty, freezing the pane.
    let mut term = kitty_term();
    Processor::<StdSyncHandler>::new().advance(&mut term, "\x1b[>1u".repeat(5000).as_bytes());
    assert!(term.mode().intersects(artty_vt::term::TermMode::KITTY_KEYBOARD_PROTOCOL));
}

#[test]
fn a_prompt_forgets_keyboard_modes_a_program_left() {
    let mut term = kitty_term();
    let mut parser = Processor::<StdSyncHandler>::new();
    // A program pushes modes on both screens and dies without popping them.
    parser.advance(&mut term, b"\x1b[>1u\x1b[?1049h\x1b[>3u");
    parser.advance(&mut term, b"\x1b[?1049l\x1b]133;A\x07");
    assert!(!term.mode().intersects(artty_vt::term::TermMode::KITTY_KEYBOARD_PROTOCOL));
    // Nothing comes back from either screen's stack.
    parser.advance(&mut term, b"\x1b[<u\x1b[?1049h\x1b[<u");
    assert!(!term.mode().intersects(artty_vt::term::TermMode::KITTY_KEYBOARD_PROTOCOL));
}

/// Random bytes, biased toward escape sequences, fed in uneven chunks with
/// resizes in between: nothing may panic. Seeded, so a failure reproduces.
#[test]
fn random_input_never_panics() {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    const PIECES: &[&str] = &[
        "\x1b[", "\x1b]", "\x1bP", "\x1b_", "\x1b^", "\x1b\\", "\x07", "\x1b", ";", "?", ">", "<", "=", ":", "0", "1",
        "9", "99", "65535", "4294967296", "m", "H", "J", "K", "r", "h", "l", "u", "q", "t", "@", "P", "X", "L", "M", "S",
        "T", "b", "c", "n", "s", "x", "z", "{", "~", "133;A", "133;D;1", "8;;https://x", "52;c;aGk=", "1337;", "9;4;1;50",
        "\r", "\n", "\t", "\x08", "\x7f", "a", "é", "한", "👨\u{200d}👩", "❤\u{fe0f}", "\u{fe0f}", "\u{200d}", "🏽",
        "\u{0301}", "\x0e", "\x0f", "\x1b(0", "\x1b(B", "\x1b#8", "\x1b7", "\x1b8", "\x1bM", "\x1bD", "\x1bE",
        // Images: sixel, kitty graphics, iTerm2.
        "\x1bPq", "\x1bP0;1q", "#1;2;100;0;0", "#2;1;120;50;100", "!999~", "~", "-", "$", "\"1;1;640;480",
        "\x1b_Ga=T,f=32,s=1,v=1,c=3,r=2;", "\x1b_Ga=T,f=100,m=1;", "\x1b_Gm=0;", "\x1b_Ga=d,d=A;", "AAAA",
        "\x1b_Ga=p,i=1,C=1;", "\x1b]1337;File=inline=1;width=3:", "iVBORw0KGgo",
    ];
    for round in 0..300 {
        let mut term = kitty_term();
        let mut parser = Processor::<StdSyncHandler>::new();
        let mut bytes = Vec::new();
        for _ in 0..600 {
            let roll = next();
            if roll % 5 == 0 {
                bytes.push((roll >> 8) as u8);
            } else {
                bytes.extend_from_slice(PIECES[(roll >> 8) as usize % PIECES.len()].as_bytes());
            }
        }
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let take = (next() as usize % 97 + 1).min(rest.len());
            parser.advance(&mut term, &rest[..take]);
            rest = &rest[take..];
            if next() % 13 == 0 {
                let columns = next() as usize % 120 + 1;
                let lines = next() as usize % 50 + 1;
                term.resize(Size(columns, lines));
            }
        }
        let _ = round;
    }
}

#[test]
fn a_one_column_resize_with_wide_characters_finishes() {
    // Reflowing a wide character into one column used to wrap it onto a new
    // row forever, allocating without end. The terminal keeps two columns.
    let (mut term, _) = run("한글 ❤\u{fe0f} 👍".as_bytes(), 3);
    term.resize(Size(1, 27));
    assert_eq!(term.columns(), 2);
    term.resize(Size(0, 0));
    assert_eq!((term.columns(), term.screen_lines()), (2, 1));
}

#[test]
fn snapshots_carry_a_terminal_over_exactly() {
    use artty_vt::term::TermSnapshot;
    let mut term = kitty_term();
    let mut parser = Processor::<StdSyncHandler>::new();
    // History, colors and a title on the main screen; then the alternate
    // screen with its own content, keyboard modes and a cursor elsewhere.
    for i in 0..10 {
        parser.advance(&mut term, format!("line {i} \x1b[1;31mred\x1b[0m 한\r\n").as_bytes());
    }
    parser.advance(&mut term, b"\x1b]0;my title\x07\x1b]4;1;#123456\x07\x1b[>1u\x1b[?1049h\x1b[2;5Halt\x1b[>3u\x1b[?2004h");

    let json = serde_json::to_string(&term.snapshot()).unwrap();
    let snapshot: TermSnapshot = serde_json::from_str(&json).unwrap();
    let copy = Term::from_snapshot(Config { kitty_keyboard: true, ..Config::default() }, snapshot, Recorder::default());

    assert_eq!(copy.mode(), term.mode());
    assert_eq!(copy.grid().cursor.point, term.grid().cursor.point);
    assert_eq!(copy.colors()[1], term.colors()[1]);
    let text = |t: &Term<Recorder>| {
        let grid = t.grid();
        (grid.topmost_line().0..grid.screen_lines() as i32)
            .map(|line| (0..grid.columns()).map(|c| grid[Line(line)][Column(c)].c).collect::<String>())
            .collect::<Vec<_>>()
    };
    assert_eq!(text(&copy), text(&term));
    // And back on the main screen, history and styles are intact.
    let mut copy = copy;
    parser.advance(&mut term, b"\x1b[?1049l");
    parser.advance(&mut copy, b"\x1b[?1049l");
    assert_eq!(text(&copy), text(&term));
    assert_eq!(copy.history_size(), term.history_size());
    let cell = |t: &Term<Recorder>| t.grid()[Line(-1)][Column(7)].clone();
    assert_eq!(cell(&copy), cell(&term));
    assert_eq!(copy.mode(), term.mode());
}

/// A 1×1 PNG.
const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

#[test]
fn inline_images_cover_cells_and_carry_over() {
    use artty_vt::term::TermSnapshot;
    let mut term = kitty_term();
    let mut parser = Processor::<StdSyncHandler>::new();
    // Four cells by two, not keeping the 1:1 aspect ratio, after some text.
    parser.advance(&mut term, format!("ab\x1b]1337;File=inline=1;width=4;height=2;preserveAspectRatio=0:{PIXEL_PNG}\x07").as_bytes());
    let grid = term.grid();
    let tile = |line, column| grid[Line(line)][Column(column)].image().map(|t| (t.column, t.row));
    assert_eq!(tile(0, 2), Some((0, 0)));
    assert_eq!(tile(0, 5), Some((3, 0)));
    assert_eq!(tile(1, 2), Some((0, 1)));
    assert_eq!(tile(1, 6), None);
    assert_eq!(grid[Line(0)][Column(2)].image().unwrap().image.width, 1);
    // The cursor goes below the image, at the start of the line.
    assert_eq!(grid.cursor.point, Point::new(Line(2), Column(0)));

    // Not inline: nothing shown.
    let before = term.grid().cursor.point;
    parser.advance(&mut term, format!("\x1b]1337;File=name=eA==:{PIXEL_PNG}\x07").as_bytes());
    assert_eq!(term.grid().cursor.point, before);

    // A snapshot keeps the image once, and the cells pointing at it.
    let json = serde_json::to_string(&term.snapshot()).unwrap();
    assert_eq!(json.matches("\"width\":1,").count(), 1, "one copy of the image");
    let snapshot: TermSnapshot = serde_json::from_str(&json).unwrap();
    let copy = Term::from_snapshot(Config::default(), snapshot, Recorder::default());
    assert_eq!(copy.grid()[Line(1)][Column(5)].image().map(|t| (t.column, t.row)), Some((3, 1)));

    // Text written over part of it replaces that part only.
    parser.advance(&mut term, b"\x1b[1;4HX");
    assert!(term.grid()[Line(0)][Column(3)].image().is_none());
    assert!(term.grid()[Line(0)][Column(4)].image().is_some());
}

fn replies(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PtyWrite(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn kitty_graphics_transmit_place_query_delete() {
    use base64::Engine;
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let recorder = Recorder::default();
    let mut term = Term::new(Config::default(), &Size(20, 6), recorder.clone());
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut run = |term: &mut Term<Recorder>, bytes: &str| {
        parser.advance(term, bytes.as_bytes());
        replies(&recorder.0.lock().unwrap().drain(..).collect::<Vec<_>>())
    };

    // Transmit and show a 2×2 RGBA image across 4×2 cells: answered OK.
    let rgba = b64(&[255u8; 16]);
    assert_eq!(run(&mut term, &format!("\x1b_Ga=T,f=32,s=2,v=2,i=7,c=4,r=2;{rgba}\x1b\\")), ["\x1b_Gi=7;OK\x1b\\"]);
    let tile = |term: &Term<Recorder>, line, column| {
        term.grid()[Line(line)][Column(column)].image().map(|t| (t.column, t.row, t.image.kitty_id))
    };
    assert_eq!(tile(&term, 0, 0), Some((0, 0, 7)));
    assert_eq!(tile(&term, 1, 3), Some((3, 1, 7)));
    assert_eq!(term.grid().cursor.point, Point::new(Line(2), Column(0)));

    // Shown again later by id, without moving the cursor.
    run(&mut term, "\x1b[3;10H\x1b_Ga=p,i=7,C=1,q=1;\x1b\\");
    assert_eq!(tile(&term, 2, 9), Some((0, 0, 7)));
    assert_eq!(term.grid().cursor.point, Point::new(Line(2), Column(9)));

    // In chunks, compressed, as RGB.
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(&[1, 2, 3], 6);
    let encoded = b64(&packed);
    let (a, b) = encoded.split_at(4);
    run(&mut term, "\x1b[5;1H");
    assert!(run(&mut term, &format!("\x1b_Ga=T,f=24,s=1,v=1,o=z,i=9,m=1;{a}\x1b\\")).is_empty());
    assert_eq!(run(&mut term, &format!("\x1b_Gm=0;{b}\x1b\\")), ["\x1b_Gi=9;OK\x1b\\"]);
    assert_eq!(tile(&term, 4, 0), Some((0, 0, 9)));

    // A query answers without showing anything; shared memory isn't supported.
    let before = term.grid().cursor.point;
    assert_eq!(run(&mut term, &format!("\x1b_Ga=q,f=32,s=2,v=2,i=31;{rgba}\x1b\\")), ["\x1b_Gi=31;OK\x1b\\"]);
    assert_eq!(term.grid().cursor.point, before);
    let shm = run(&mut term, &format!("\x1b_Ga=q,t=s,s=1,v=1,i=32;{}\x1b\\", b64(b"/x")));
    assert!(shm[0].starts_with("\x1b_Gi=32;EINVAL"), "{shm:?}");

    // Deleting image 7's placements leaves image 9 alone.
    run(&mut term, "\x1b_Ga=d,d=i,i=7\x1b\\");
    assert_eq!(tile(&term, 0, 0), None);
    assert_eq!(tile(&term, 2, 9), None);
    assert_eq!(tile(&term, 4, 0), Some((0, 0, 9)));
    // And freeing it means it can't be shown again.
    run(&mut term, "\x1b_Ga=d,d=I,i=7\x1b\\");
    let gone = run(&mut term, "\x1b_Ga=p,i=7;\x1b\\");
    assert!(gone[0].contains("ENOENT"), "{gone:?}");
}

#[test]
fn sixel_images_are_shown_and_advertised() {
    let (term, events) = run(b"\x1b[c\x1bP0;1q\"1;1;16;12#1;2;100;0;0#1!16~-!16~\x1b\\after", 6);
    // Programs learn they can send sixel (4) from the device attributes.
    assert!(replies(&events).contains(&"\x1b[?62;4;22c".to_owned()));
    // 16 × 12 pixels in 8 × 16 pixel cells: two cells by one.
    let tile = term.grid()[Line(0)][Column(1)].image().map(|t| (t.column, t.row, t.image.width, t.image.height));
    assert_eq!(tile, Some((1, 0, 16, 12)));
    assert!(term.grid()[Line(0)][Column(2)].image().is_none());
    // Text carries on below it.
    assert_eq!(term.grid()[Line(1)][Column(0)].c, 'a');
}

#[test]
fn a_prompt_forgets_the_cursor_shape_a_program_chose() {
    use artty_vt::ansi::CursorShape;
    let mut term = kitty_term();
    let mut parser = Processor::<StdSyncHandler>::new();
    // yazi and friends set a block and leave it when they exit.
    parser.advance(&mut term, b"\x1b[2 q");
    assert_eq!(term.cursor_style().shape, CursorShape::Block);
    parser.advance(&mut term, b"\x1b]133;A\x07");
    assert_eq!(term.cursor_style(), Config::default().default_cursor_style);
}

#[test]
fn termcap_and_version_queries_are_answered() {
    // XTGETTCAP for TN, Co and something unknown; then XTVERSION.
    let (_, events) = run(b"\x1bP+q544E;436F;7A7A\x1b\\\x1b[>q", 3);
    let replies = replies(&events);
    assert_eq!(replies[0], "\x1bP1+r544E=787465726D2D323536636F6C6F72\x1b\\");
    assert_eq!(replies[1], "\x1bP1+r436F=323536\x1b\\");
    assert_eq!(replies[2], "\x1bP0+r7A7A\x1b\\");
    assert!(replies[3].starts_with("\x1bP>|artty "));
}
