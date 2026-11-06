//! What this terminal sends, read back into what was done: text, keys (as
//! `artty_core::keys` names them, for encoding again in a pane's mode),
//! pastes, the mouse, and focus coming and going.

/// A key that isn't plain text, or text with Alt or Ctrl.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyPress {
    /// `enter`, `up`, `f5`, `a`.
    pub name: String,
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl KeyPress {
    fn named(name: &str) -> Self {
        KeyPress { name: name.to_owned(), ..Default::default() }
    }

    fn control(name: &str) -> Self {
        KeyPress { control: true, ..Self::named(name) }
    }

    /// `ctrl-b`, `alt-x`, `f12`: as [`artty_core::keys::Key::parse`] reads.
    pub fn parse(text: &str) -> Option<Self> {
        let key = artty_core::keys::Key::parse(text);
        let name = match key.key {
            "" => return None,
            name if name.chars().count() == 1 && (key.control || key.alt) => name.to_lowercase(),
            name => name.to_owned(),
        };
        Some(KeyPress { name, shift: key.shift, alt: key.alt, control: key.control })
    }

    pub fn key(&self) -> artty_core::keys::Key<'_> {
        artty_core::keys::Key {
            key: &self.name,
            shift: self.shift,
            alt: self.alt,
            control: self.control,
            platform: false,
        }
    }

    /// As [`KeyPress::parse`] reads it.
    pub fn describe(&self) -> String {
        let mut text = String::new();
        for (on, name) in [(self.control, "ctrl-"), (self.alt, "alt-"), (self.shift, "shift-")] {
            if on {
                text.push_str(name);
            }
        }
        text + &self.name
    }
}

/// A mouse event, as SGR reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    /// The report's button code: 0–2 buttons, 3 none, 64/65 the wheel, with
    /// 4 shift, 8 alt, 16 ctrl, 32 motion added.
    pub code: u16,
    /// 0-based cell.
    pub column: usize,
    pub line: usize,
    pub release: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Text(String),
    Key(KeyPress),
    Paste(String),
    Mouse(MouseEvent),
    Focus(bool),
}

/// Bytes into [`Input`]s; what's cut off (a sequence's first part) waits
/// for the rest, or for [`Decoder::flush`] (an Escape on its own).
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}

/// How a sequence at the start of the bytes reads.
enum Read {
    Done(usize, Option<Input>),
    /// Its rest hasn't come.
    Short,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            match read(&self.pending[at..]) {
                Read::Done(used, input) => {
                    at += used;
                    push(&mut out, input);
                },
                Read::Short => break,
            }
        }
        self.pending.drain(..at);
        out
    }

    /// Whether a sequence's start waits.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Nothing more came: what waits is as it stands (an Escape, Alt+[).
    pub fn flush(&mut self) -> Vec<Input> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        let mut at = 0;
        while at < pending.len() {
            let rest = &pending[at..];
            match read(rest) {
                Read::Done(used, input) => {
                    at += used;
                    push(&mut out, input);
                },
                Read::Short if rest[0] == 0x1b && rest.starts_with(b"\x1b[200~") => {
                    // A paste whose end never came.
                    let text = String::from_utf8_lossy(&rest[6..]).into_owned();
                    push(&mut out, Some(Input::Paste(text)));
                    break;
                },
                Read::Short if rest[0] == 0x1b => {
                    at += 1;
                    push(&mut out, Some(Input::Key(KeyPress::named("escape"))));
                },
                // Half a character.
                Read::Short => break,
            }
        }
        out
    }
}

/// Text after text joins it.
fn push(out: &mut Vec<Input>, input: Option<Input>) {
    match (out.last_mut(), input) {
        (Some(Input::Text(text)), Some(Input::Text(more))) => text.push_str(&more),
        (_, Some(input)) => out.push(input),
        (_, None) => {},
    }
}

fn read(bytes: &[u8]) -> Read {
    match bytes[0] {
        0x1b => escape(bytes),
        byte if byte < 0x20 || byte == 0x7f => Read::Done(1, Some(Input::Key(control(byte)))),
        _ => text(bytes),
    }
}

/// A control byte's key.
fn control(byte: u8) -> KeyPress {
    match byte {
        b'\r' => KeyPress::named("enter"),
        b'\t' => KeyPress::named("tab"),
        0x7f => KeyPress::named("backspace"),
        0 => KeyPress::control("space"),
        0x1b => KeyPress::named("escape"),
        0x1c => KeyPress::control("\\"),
        0x1d => KeyPress::control("]"),
        0x1e => KeyPress::control("^"),
        0x1f => KeyPress::control("_"),
        byte => KeyPress::control(&((b'a' + byte - 1) as char).to_string()),
    }
}

/// One character of UTF-8 text (or a byte that isn't, dropped).
fn text(bytes: &[u8]) -> Read {
    let length = match bytes[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Read::Done(1, None),
    };
    if bytes.len() < length {
        return Read::Short;
    }
    match std::str::from_utf8(&bytes[..length]) {
        Ok(text) => Read::Done(length, Some(Input::Text(text.to_owned()))),
        Err(_) => Read::Done(1, None),
    }
}

fn escape(bytes: &[u8]) -> Read {
    let Some(&next) = bytes.get(1) else { return Read::Short };
    match next {
        b'[' => csi(bytes),
        b'O' => {
            let Some(&last) = bytes.get(2) else { return Read::Short };
            let key = match last {
                b'A' => "up",
                b'B' => "down",
                b'C' => "right",
                b'D' => "left",
                b'H' => "home",
                b'F' => "end",
                b'P' => "f1",
                b'Q' => "f2",
                b'R' => "f3",
                b'S' => "f4",
                _ => return Read::Done(3, None),
            };
            Read::Done(3, Some(Input::Key(KeyPress::named(key))))
        },
        // Alt and a key.
        0x1b => Read::Done(1, Some(Input::Key(KeyPress::named("escape")))),
        byte if byte < 0x20 || byte == 0x7f => {
            Read::Done(2, Some(Input::Key(KeyPress { alt: true, ..control(byte) })))
        },
        _ => match text(&bytes[1..]) {
            Read::Done(used, Some(Input::Text(text))) => {
                let lower = text.to_lowercase();
                let shift = lower != text;
                let key = KeyPress { name: lower, shift, alt: true, control: false };
                Read::Done(1 + used, Some(Input::Key(key)))
            },
            Read::Done(used, _) => Read::Done(1 + used, None),
            Read::Short => Read::Short,
        },
    }
}

/// `ESC [ params final`.
fn csi(bytes: &[u8]) -> Read {
    let Some(end) = bytes.iter().skip(2).position(|b| (0x40..=0x7e).contains(b)) else {
        // Not a sequence after all, if it goes on and on.
        return if bytes.len() > 64 { Read::Done(2, None) } else { Read::Short };
    };
    let end = end + 2;
    let used = end + 1;
    let params = std::str::from_utf8(&bytes[2..end]).unwrap_or_default();
    let last = bytes[end];
    if let Some(mouse) = params.strip_prefix('<') {
        let numbers: Vec<usize> = mouse.split(';').filter_map(|n| n.parse().ok()).collect();
        let input = match (numbers.as_slice(), last) {
            (&[code, column, line], b'M' | b'm') => Some(Input::Mouse(MouseEvent {
                code: code as u16,
                column: column.saturating_sub(1),
                line: line.saturating_sub(1),
                release: last == b'm',
            })),
            _ => None,
        };
        return Read::Done(used, input);
    }
    if params == "200" && last == b'~' {
        let body = &bytes[used..];
        return match body.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
            Some(length) => {
                let text = String::from_utf8_lossy(&body[..length]).into_owned();
                Read::Done(used + length + PASTE_END.len(), Some(Input::Paste(text)))
            },
            None => Read::Short,
        };
    }
    let numbers: Vec<u32> = params.split(';').map(|n| n.parse().unwrap_or(0)).collect();
    let modifiers = numbers.get(1).copied().unwrap_or(1).saturating_sub(1);
    let key = match (last, numbers[0]) {
        (b'I', 0) if params.is_empty() => return Read::Done(used, Some(Input::Focus(true))),
        (b'O', 0) if params.is_empty() => return Read::Done(used, Some(Input::Focus(false))),
        (b'Z', _) => {
            let key = KeyPress { shift: true, ..KeyPress::named("tab") };
            return Read::Done(used, Some(Input::Key(key)));
        },
        (b'A', _) => "up",
        (b'B', _) => "down",
        (b'C', _) => "right",
        (b'D', _) => "left",
        (b'H', _) => "home",
        (b'F', _) => "end",
        (b'P', _) => "f1",
        (b'Q', _) => "f2",
        (b'R', _) => "f3",
        (b'S', _) => "f4",
        (b'~', 1 | 7) => "home",
        (b'~', 2) => "insert",
        (b'~', 3) => "delete",
        (b'~', 4 | 8) => "end",
        (b'~', 5) => "pageup",
        (b'~', 6) => "pagedown",
        (b'~', 15) => "f5",
        (b'~', 17) => "f6",
        (b'~', 18) => "f7",
        (b'~', 19) => "f8",
        (b'~', 20) => "f9",
        (b'~', 21) => "f10",
        (b'~', 23) => "f11",
        (b'~', 24) => "f12",
        _ => return Read::Done(used, None),
    };
    let key = KeyPress {
        name: key.to_owned(),
        shift: modifiers & 1 != 0,
        alt: modifiers & 2 != 0,
        control: modifiers & 4 != 0,
    };
    Read::Done(used, Some(Input::Key(key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> Input {
        Input::Key(KeyPress::parse(text).unwrap())
    }

    #[test]
    fn text_and_control_keys() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.feed(b"ls -l\r\x02\x7f\t"),
            vec![
                Input::Text("ls -l".into()),
                key("enter"),
                key("ctrl-b"),
                key("backspace"),
                key("tab")
            ]
        );
        assert_eq!(decoder.feed("é한".as_bytes()), vec![Input::Text("é한".into())]);
    }

    #[test]
    fn sequences_and_modifiers() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.feed(b"\x1b[A\x1bOB\x1b[1;5C\x1b[5~\x1b[3;2~\x1b[Z\x1bb\x1bB"),
            vec![
                key("up"),
                key("down"),
                key("ctrl-right"),
                key("pageup"),
                key("shift-delete"),
                key("shift-tab"),
                key("alt-b"),
                key("alt-shift-b"),
            ]
        );
    }

    #[test]
    fn cut_off_sequences_wait() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"a\x1b["), vec![Input::Text("a".into())]);
        assert!(decoder.waiting());
        assert_eq!(decoder.feed(b"1;3D"), vec![key("alt-left")]);
        // Half a character, then the rest.
        assert_eq!(decoder.feed(&"한".as_bytes()[..1]), vec![]);
        assert_eq!(decoder.feed(&"한".as_bytes()[1..]), vec![Input::Text("한".into())]);
        // An Escape alone is one once nothing follows.
        assert_eq!(decoder.feed(b"\x1b"), vec![]);
        assert_eq!(decoder.flush(), vec![key("escape")]);
        assert!(!decoder.waiting());
    }

    #[test]
    fn pastes_mouse_and_focus() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"\x1b[200~one\r\x1b[A"), vec![]);
        assert_eq!(
            decoder.feed(b"two\x1b[201~\x1b[<0;10;5M\x1b[<64;1;1M\x1b[<0;10;5m\x1b[I"),
            vec![
                Input::Paste("one\r\x1b[Atwo".into()),
                Input::Mouse(MouseEvent { code: 0, column: 9, line: 4, release: false }),
                Input::Mouse(MouseEvent { code: 64, column: 0, line: 0, release: false }),
                Input::Mouse(MouseEvent { code: 0, column: 9, line: 4, release: true }),
                Input::Focus(true),
            ]
        );
    }

    #[test]
    fn prefixes_parse() {
        let prefix = KeyPress::parse("ctrl-a").unwrap();
        assert_eq!(prefix, KeyPress::control("a"));
        assert_eq!(prefix.describe(), "ctrl-a");
        assert_eq!(KeyPress::parse("ctrl-A").unwrap(), prefix);
        assert_eq!(KeyPress::parse(""), None);
    }
}
