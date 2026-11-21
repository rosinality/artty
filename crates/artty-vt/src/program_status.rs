//! The Program Status Protocol (OSC 7501, revision 0.4): programs say what
//! they're doing, `ESC ] 7501 ; state=working:app=claude-code:msg=<base64>
//! ST`, and the terminal keeps it, a record per `id` (none: the root). A
//! program asks first whether the terminal knows it (`OSC 7501 ; ? ST`): a
//! terminal that does answers `OSC 7501 ; ? ST` (Claude Code then reports
//! here, and leaves its window title plain).
//!
//! Only state travels, never markup: free text (`msg`, `title`) is base64
//! of UTF-8 without control characters, and a report that breaks a limit is
//! dropped whole.

use base64::Engine;

/// A whole sequence's limit, `OSC 7501 ;` and `ST` included.
const MOST_BYTES: usize = 4096;
const MOST_KEY: usize = 16;
const MOST_MSG: (usize, usize) = (2732, 2048);
const MOST_TITLE: (usize, usize) = (256, 192);
const MOST_APP: usize = 32;
const MOST_ID: usize = 128;
const MOST_SEGMENT: usize = 32;
const MOST_DEPTH: usize = 8;
/// Records a terminal keeps (the spec's floor is 64, its cap 256).
pub const MOST_RECORDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

/// What a blocked program waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedOn {
    Permission,
    Question,
    Auth,
}

/// A program's status, as a report sets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramState,
    pub kind: Option<BlockedOn>,
    /// 0–100 (working or blocked); none: not known how far.
    pub progress: Option<u8>,
    /// Which program (`claude-code`, `cargo`): an identifier, not a label.
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

/// A report: a record's new status, or its clearing (with those under it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    /// Its record: none is the root.
    pub id: Option<String>,
    /// None: `state=clear`.
    pub status: Option<ProgramStatus>,
}

/// What the body of `OSC 7501 ; BODY ST` is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sequence {
    /// The program asks whether the terminal knows the protocol.
    Query,
    Report(StatusReport),
}

/// The body of an OSC 7501 (after `7501;`), if it's a query or a report
/// to apply: malformed ones are dropped as the spec says.
pub fn parse(body: &[u8]) -> Option<Sequence> {
    // `OSC 7501 ;` (7 bytes) and `ESC \` (2) around it.
    if body.len() + 9 > MOST_BYTES {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    if text.trim() == "?" {
        return Some(Sequence::Query);
    }
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in text.split(':') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || key.len() > MOST_KEY || !key.bytes().all(|b| b.is_ascii_lowercase()) {
            continue;
        }
        let allowed = |b: u8| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b);
        if !value.bytes().all(allowed) {
            // (A bad id isn't skipped: the report would land on another
            // record.)
            if key == "id" {
                return None;
            }
            continue;
        }
        // The last of a key wins.
        pairs.retain(|(k, _)| *k != key);
        pairs.push((key, value));
    }
    let get = |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|&(_, v)| v);
    let state = match get("state")? {
        "idle" => Some(ProgramState::Idle),
        "working" => Some(ProgramState::Working),
        "done" => Some(ProgramState::Done),
        "blocked" => Some(ProgramState::Blocked),
        "error" => Some(ProgramState::Error),
        "clear" => None,
        _ => return None,
    };
    let id = match get("id") {
        None => None,
        Some(id) => Some(valid_id(id)?.to_owned()),
    };
    let msg = text_of(get("msg"), MOST_MSG)?;
    let title = text_of(get("title"), MOST_TITLE)?;
    let Some(state) = state else {
        return Some(Sequence::Report(StatusReport { id, status: None }));
    };
    let kind = match (state, get("kind")) {
        (ProgramState::Blocked, Some("permission")) => Some(BlockedOn::Permission),
        (ProgramState::Blocked, Some("question")) => Some(BlockedOn::Question),
        (ProgramState::Blocked, Some("auth")) => Some(BlockedOn::Auth),
        _ => None,
    };
    let progress = match state {
        ProgramState::Working | ProgramState::Blocked => get("progress")
            .filter(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|&p| p <= 100),
        _ => None,
    };
    let app = get("app")
        .filter(|a| !a.is_empty() && a.len() <= MOST_APP && a.bytes().all(id_byte))
        .map(str::to_owned);
    let status = ProgramStatus { state, kind, progress, app, title, msg };
    Some(Sequence::Report(StatusReport { id, status: Some(status) }))
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.+-".contains(&b)
}

/// `id` if it's a record's path: `segment(/segment)*`.
fn valid_id(id: &str) -> Option<&str> {
    let segments: Vec<&str> = id.split('/').collect();
    let fits = id.len() <= MOST_ID
        && segments.len() <= MOST_DEPTH
        && segments
            .iter()
            .all(|s| !s.is_empty() && s.len() <= MOST_SEGMENT && s.bytes().all(id_byte));
    fits.then_some(id)
}

/// Free text: base64 of UTF-8 without control characters. `Some(None)`:
/// absent (or empty); `None`: the report is to be dropped.
fn text_of(value: Option<&str>, (encoded, decoded): (usize, usize)) -> Option<Option<String>> {
    let Some(value) = value.filter(|v| !v.is_empty()) else { return Some(None) };
    if value.len() > encoded {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(value.trim_end_matches('='))
        .ok()?;
    if bytes.len() > decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(Some(text).filter(|t| !t.is_empty()))
}

/// A terminal's records, least recently reported first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusRecords {
    records: Vec<(Option<String>, ProgramStatus)>,
}

impl StatusRecords {
    /// Apply a report: its record replaced (whole), or cleared with those
    /// under it (no id: all).
    pub fn apply(&mut self, report: StatusReport) {
        let StatusReport { id, status } = report;
        match status {
            None => match &id {
                None => self.records.clear(),
                Some(id) => {
                    self.records.retain(|(r, _)| !r.as_deref().is_some_and(|r| under(r, id)))
                },
            },
            Some(mut status) => {
                // An app not named comes from the nearest record above.
                if status.app.is_none() {
                    status.app = self.inherited_app(id.as_deref());
                }
                self.records.retain(|(r, _)| *r != id);
                if self.records.len() >= MOST_RECORDS {
                    self.records.remove(0);
                }
                self.records.push((id, status));
            },
        }
    }

    fn inherited_app(&self, id: Option<&str>) -> Option<String> {
        let mut at = id?;
        loop {
            let parent = at.rsplit_once('/').map(|(p, _)| p);
            let found = self.records.iter().find(|(r, _)| r.as_deref() == parent);
            if let Some(app) = found.and_then(|(_, s)| s.app.clone()) {
                return Some(app);
            }
            match parent {
                Some(p) => at = p,
                None => return None,
            }
        }
    }

    /// The program ended, or the shell's prompt is back: what was at work
    /// or waiting is over (idle goes too; done and error stay).
    pub fn program_ended(&mut self) {
        self.records.retain(|(_, s)| matches!(s.state, ProgramState::Done | ProgramState::Error));
    }

    /// The root record: what the program in the foreground says it's doing.
    pub fn root(&self) -> Option<&ProgramStatus> {
        self.get(None)
    }

    pub fn get(&self, id: Option<&str>) -> Option<&ProgramStatus> {
        self.records.iter().find(|(r, _)| r.as_deref() == id).map(|(_, s)| s)
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Whether record `id` is `of` or beneath it.
fn under(id: &str, of: &str) -> bool {
    id == of || id.strip_prefix(of).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(body: &str) -> StatusReport {
        match parse(body.as_bytes()) {
            Some(Sequence::Report(report)) => report,
            other => panic!("{body}: {other:?}"),
        }
    }

    #[test]
    fn reports_as_the_spec_has_them() {
        assert_eq!(parse(b"?"), Some(Sequence::Query));
        assert_eq!(parse(b" ? "), Some(Sequence::Query));
        let blocked = report(
            "state=blocked:kind=permission:app=terraform:msg=\
             QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/",
        );
        let status = blocked.status.unwrap();
        assert_eq!(status.state, ProgramState::Blocked);
        assert_eq!(status.kind, Some(BlockedOn::Permission));
        assert_eq!(status.app.as_deref(), Some("terraform"));
        assert_eq!(status.msg.as_deref(), Some("Apply 3 to add, 1 to change, 0 to destroy?"));
        // Unpadded base64; kind only when blocked; progress only at work.
        let working = report("state=working:id=us-east:title=VVMgRWFzdA:progress=40:kind=auth");
        assert_eq!(working.id.as_deref(), Some("us-east"));
        let status = working.status.unwrap();
        assert_eq!(
            (status.title.as_deref(), status.progress, status.kind),
            (Some("US East"), Some(40), None)
        );
        assert_eq!(report("state=done:progress=40").status.unwrap().progress, None);
        assert_eq!(
            report("state=clear:id=a/b"),
            StatusReport { id: Some("a/b".into()), status: None }
        );
        // Claude Code's.
        assert_eq!(
            report("state=idle:app=claude-code").status.unwrap().app.as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn malformed_ones_dropped() {
        // No state, or one not known: dropped.
        assert_eq!(parse(b"app=x"), None);
        assert_eq!(parse(b"state=sleeping"), None);
        // A bad pair is skipped; a bad id drops the report.
        assert!(report("state=idle:Bad=1:app=a b").status.unwrap().app.is_none());
        assert_eq!(parse(b"state=idle:id=a b"), None);
        assert_eq!(parse(b"state=idle:id="), None);
        assert_eq!(parse(b"state=idle:id=a//b"), None);
        // Text that isn't base64, or holds a control character: dropped.
        assert_eq!(parse(b"state=idle:msg=Q"), None);
        // (A byte outside the value set only skips the pair.)
        assert!(report("state=idle:msg=!!!").status.unwrap().msg.is_none());
        assert_eq!(parse(b"state=idle:msg=G1szMW0="), None); // ESC[31m
        // Too long.
        let long = format!("state=idle:msg={}", "QUFB".repeat(700));
        assert_eq!(parse(long.as_bytes()), None);
        // The last of a key wins; an empty msg is none.
        let status = report("state=working:state=done:msg=").status.unwrap();
        assert_eq!((status.state, status.msg), (ProgramState::Done, None));
    }

    #[test]
    fn records_kept_and_cleared() {
        let mut records = StatusRecords::default();
        records.apply(report("state=working:app=deploy"));
        records.apply(report("state=working:id=us"));
        records.apply(report("state=blocked:id=us/db"));
        records.apply(report("state=working:id=usa"));
        // Inherited from above.
        assert_eq!(records.get(Some("us/db")).unwrap().app.as_deref(), Some("deploy"));
        // Clearing `us` takes `us/db`, not `usa`.
        records.apply(report("state=clear:id=us"));
        assert!(records.get(Some("us")).is_none() && records.get(Some("us/db")).is_none());
        assert!(records.get(Some("usa")).is_some());
        // A report replaces its record whole.
        records.apply(report("state=done"));
        assert_eq!(records.root().unwrap().app, None);
        // The program gone: only done and error stay.
        records.program_ended();
        assert!(records.get(Some("usa")).is_none());
        assert_eq!(records.root().unwrap().state, ProgramState::Done);
        records.apply(report("state=clear"));
        assert!(records.is_empty());
        // At most so many: the least recently reported goes.
        for n in 0..=MOST_RECORDS {
            records.apply(report(&format!("state=idle:id=r{n}")));
        }
        assert!(records.get(Some("r0")).is_none() && records.get(Some("r1")).is_some());
    }
}
