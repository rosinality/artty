//! Saved workspace state: the tabs to reopen on the next launch.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use artty_vt::ansi::{Color, NamedColor};
use artty_vt::grid::Dimensions;
use artty_vt::index::{Column, Line};
use artty_vt::term::Term;
use artty_vt::term::cell::{Cell, Flags};
use serde::{Deserialize, Serialize};

/// Version 4 groups each window's tabs into workspaces; version 3 saved
/// several windows; version 2 added split layouts; version 1 had one pane per
/// tab. All of them load.
const VERSION: u32 = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    pub version: u32,
    pub windows: Vec<SavedWindowState>,
    /// Workspaces in no window.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<SavedWorkspace>,
    // One-window layout of versions 1 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<SavedWindow>,
    #[serde(skip_serializing_if = "is_zero")]
    active: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tabs: Vec<SavedTab>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// One window: where it was, its workspaces, and the active one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindowState {
    pub window: Option<SavedWindow>,
    pub active: usize,
    pub workspaces: Vec<SavedWorkspace>,
    // Version 3: the window's tabs (and `active` among them), one workspace.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tabs: Vec<SavedTab>,
}

impl SavedWindowState {
    pub fn new(
        window: Option<SavedWindow>,
        active: usize,
        workspaces: Vec<SavedWorkspace>,
    ) -> Self {
        Self { window, active, workspaces, tabs: Vec::new() }
    }

    /// In the current form: a version 3 window becomes one workspace.
    fn upgraded(mut self) -> Self {
        if !self.tabs.is_empty() {
            let tabs = std::mem::take(&mut self.tabs);
            self.workspaces
                .insert(0, SavedWorkspace { active: self.active, tabs, ..Default::default() });
            self.active = 0;
        }
        self.workspaces.retain(|workspace| !workspace.tabs.is_empty());
        for workspace in &mut self.workspaces {
            workspace.active = workspace.active.min(workspace.tabs.len() - 1);
        }
        self.active = self.active.min(self.workspaces.len().saturating_sub(1));
        self
    }
}

/// A named group of tabs, shown as a section of a window's sidebar.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWorkspace {
    pub name: Option<String>,
    pub active: usize,
    pub tabs: Vec<SavedTab>,
    pub collapsed: bool,
    pub root: Option<PathBuf>,
    pub color: Option<String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "TabRepr")]
pub struct SavedTab {
    /// Name given by the user, if any.
    pub title: Option<String>,
    pub layout: SavedLayout,
    /// The focused pane, as an index into the layout's panes in reading order.
    pub focused: usize,
    pub zoomed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavedLayout {
    Pane(SavedPane),
    Split {
        /// Stacked (`true`) or side by side.
        vertical: bool,
        ratio: f32,
        first: Box<SavedLayout>,
        second: Box<SavedLayout>,
    },
}

/// How tabs were stored: version 2 has a layout, version 1 was a single pane.
#[derive(Deserialize)]
#[serde(untagged)]
enum TabRepr {
    Split {
        layout: SavedLayout,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        focused: usize,
        #[serde(default)]
        zoomed: bool,
    },
    Single(SavedPane),
}

impl From<TabRepr> for SavedTab {
    fn from(repr: TabRepr) -> Self {
        match repr {
            TabRepr::Split { layout, title, focused, zoomed } => {
                Self { title, layout, focused, zoomed }
            },
            TabRepr::Single(pane) => {
                Self { title: None, layout: SavedLayout::Pane(pane), focused: 0, zoomed: false }
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPane {
    /// Title given with `artty new --title`.
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Program the tab was opened with, if not the shell.
    pub command: Option<Vec<String>>,
    /// The coding agent running in the tab.
    pub agent: Option<SavedAgent>,
    /// Recent output, with SGR color codes.
    pub scrollback: String,
    /// In a server hand-over: the running pane this is (its id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    /// The key its agents' thread is kept under (see the server's store).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedAgent {
    /// Process name, e.g. `claude`.
    pub name: String,
    /// The agent's conversation id, when its hooks reported one.
    pub session: Option<String>,
}

impl SavedState {
    pub fn new(windows: Vec<SavedWindowState>, hidden: Vec<SavedWorkspace>) -> Self {
        Self { version: VERSION, windows, hidden, ..Default::default() }
    }

    /// The saved windows, in the current form whatever the file's version.
    pub fn into_windows(self) -> Vec<SavedWindowState> {
        let mut windows = self.windows;
        if !self.tabs.is_empty() {
            let workspace =
                SavedWorkspace { active: self.active, tabs: self.tabs, ..Default::default() };
            windows.insert(0, SavedWindowState::new(self.window, 0, vec![workspace]));
        }
        let mut windows: Vec<_> = windows.into_iter().map(SavedWindowState::upgraded).collect();
        windows.retain(|window| !window.workspaces.is_empty());
        windows
    }

    pub fn path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
            return Some(PathBuf::from(dir).join("artty").join("state.json"));
        }
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let dir = if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            home.join(".local").join("state")
        };
        Some(dir.join("artty").join("state.json"))
    }

    /// The saved state, if there is a readable one from this version.
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(Self::path()?).ok()?;
        let state: Self = serde_json::from_str(&text)
            .inspect_err(|err| log::warn!("ignoring saved state: {err}"))
            .ok()?;
        (1..=VERSION).contains(&state.version).then_some(state)
    }

    /// Write atomically, so a crash mid-write keeps the previous state.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path().ok_or_else(|| io::Error::other("no HOME"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("json.tmp");
        // Saved output can contain anything that was on screen: owner-only.
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?
        };
        io::Write::write_all(&mut file, &serde_json::to_vec(self)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    }
}

/// The last `lines` lines of output with SGR styling, ready to replay into a
/// new terminal. Soft-wrapped lines are joined so they rewrap at a new width.
pub(crate) fn styled_text<T>(term: &Term<T>, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let grid = term.grid();
    let columns = term.columns();
    let blank = |line: Line| (0..columns).all(|c| is_blank(&grid[line][Column(c)]));
    let Some(bottom) =
        (term.topmost_line().0..=term.bottommost_line().0).rev().map(Line).find(|&l| !blank(l))
    else {
        return String::new();
    };
    let top = Line((bottom.0 - lines as i32 + 1).max(term.topmost_line().0));

    let mut out = String::new();
    for line in (top.0..=bottom.0).map(Line) {
        let row = &grid[line];
        let end = (0..columns).rev().find(|&c| !is_blank(&row[Column(c)])).map_or(0, |c| c + 1);
        let mut style = Style::default();
        for column in 0..end {
            let cell = &row[Column(column)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let next = Style::of(cell);
            if next != style {
                next.write_sgr(&mut out);
                style = next;
            }
            // (A tab's cell, as a space: a tab would skip the cells after it.)
            out.push(if cell.c == '\t' { ' ' } else { cell.c });
            if let Some(extra) = cell.zerowidth() {
                out.extend(extra);
            }
        }
        if style != Style::default() {
            out.push_str("\x1b[0m");
        }
        let wrapped = row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        if !wrapped && line != bottom {
            out.push_str("\r\n");
        }
    }
    out
}

/// A cell that shows nothing: a default-background space.
fn is_blank(cell: &Cell) -> bool {
    matches!(cell.c, ' ' | '\t')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell.flags.contains(Flags::INVERSE)
}

const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Style {
    fn of(cell: &Cell) -> Self {
        Self { fg: cell.fg, bg: cell.bg, flags: cell.flags & STYLE_FLAGS }
    }

    /// A full SGR sequence for this style, starting from a reset.
    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, 1),
            (Flags::DIM, 2),
            (Flags::ITALIC, 3),
            (Flags::INVERSE, 7),
            (Flags::HIDDEN, 8),
            (Flags::STRIKEOUT, 9),
        ] {
            if self.flags.contains(flag) {
                let _ = write!(out, ";{code}");
            }
        }
        if self.flags.intersects(Flags::ALL_UNDERLINES) {
            out.push_str(";4");
        }
        write_color(out, self.fg, true);
        write_color(out, self.bg, false);
        out.push('m');
    }
}

fn write_color(out: &mut String, color: Color, foreground: bool) {
    let (base, bright, extended) = if foreground { (30, 90, 38) } else { (40, 100, 48) };
    match color {
        Color::Spec(rgb) => _ = write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => _ = write!(out, ";{extended};5;{index}"),
        Color::Named(name) => {
            let index = name as usize;
            let dim = NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize;
            match index {
                0..8 => _ = write!(out, ";{}", base + index),
                8..16 => _ = write!(out, ";{}", bright + index - 8),
                // Dim colors are their normal color; DIM is a flag.
                i if dim.contains(&i) => {
                    _ = write!(out, ";{}", base + i - NamedColor::DimBlack as usize)
                },
                // Default foreground/background, cursor color: nothing to set.
                _ => {},
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use artty_vt::ansi::{Processor, StdSyncHandler};
    use artty_vt::event::VoidListener;
    use artty_vt::term::Config;

    use super::*;
    use crate::TerminalSize;

    fn term(columns: u16, bytes: &[u8]) -> Term<VoidListener> {
        let size = TerminalSize { columns, lines: 4, cell_width: 8, cell_height: 16 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, bytes);
        term
    }

    #[test]
    fn styled_text_round_trips() {
        let original = term(
            20,
            "plain \x1b[1;31mbold red\x1b[0m\r\n\x1b[48;5;22m bg \x1b[0m 한글\r\n".as_bytes(),
        );
        let saved = styled_text(&original, 100);
        assert_eq!(saved, "plain \x1b[0;1;31mbold red\x1b[0m\r\n\x1b[0;48;5;22m bg \x1b[0m 한글");

        // Replaying the saved text reproduces the cells.
        let replayed = term(20, saved.as_bytes());
        for line in 0..2 {
            for column in 0..20 {
                let (a, b) = (
                    &original.grid()[Line(line)][Column(column)],
                    &replayed.grid()[Line(line)][Column(column)],
                );
                assert_eq!(
                    (a.c, a.fg, a.bg, a.flags),
                    (b.c, b.fg, b.bg, b.flags),
                    "line {line} column {column}"
                );
            }
        }
    }

    #[test]
    fn tabs_are_saved_as_the_spaces_they_cover() {
        // As Antigravity CLI lines up a table: tab stops, then a bar.
        let saved = styled_text(&term(20, b"ab\t|\tc"), 100);
        assert_eq!(saved, "ab      |       c");
        let replayed = term(20, saved.as_bytes());
        assert_eq!(replayed.grid()[Line(0)][Column(8)].c, '|');
        assert_eq!(replayed.grid()[Line(0)][Column(16)].c, 'c');
    }

    #[test]
    fn soft_wraps_are_joined_and_history_is_included() {
        let mut bytes = Vec::new();
        for i in 0..10 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"0123456789abcdefghij-wrapped");
        // Four rows: two lines and one line wrapped across two rows.
        let saved = styled_text(&term(20, &bytes), 4);
        assert_eq!(saved, "line 8\r\nline 9\r\n0123456789abcdefghij-wrapped");
    }

    #[test]
    fn state_file_format() {
        let pane = |cwd: &str| SavedPane { cwd: Some(cwd.into()), ..Default::default() };
        let agent = SavedPane {
            agent: Some(SavedAgent { name: "claude".into(), session: Some("abc".into()) }),
            ..pane("/tmp")
        };
        let tab = SavedTab {
            title: Some("work".into()),
            layout: SavedLayout::Split {
                vertical: false,
                ratio: 0.6,
                first: Box::new(SavedLayout::Pane(agent)),
                second: Box::new(SavedLayout::Pane(pane("/src"))),
            },
            focused: 1,
            zoomed: false,
        };
        let workspace = SavedWorkspace {
            name: Some("artty".into()),
            tabs: vec![tab],
            collapsed: true,
            root: Some("/src".into()),
            color: Some("teal".into()),
            env: [("A".to_owned(), "1".to_owned())].into(),
            ..Default::default()
        };
        let window = SavedWindowState::new(None, 1, vec![workspace.clone(), workspace]);
        let state = SavedState::new(vec![window.clone(), window], Vec::new());
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("\"tabs\":[]"), "no empty legacy fields: {json}");
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.into_windows(), state.windows);
    }

    #[test]
    fn version_2_files_load_as_one_window() {
        let old: SavedState = serde_json::from_str(
            r#"{"version":2,"active":1,"window":{"x":1,"y":2,"width":3,"height":4},
                "tabs":[{"layout":{"pane":{"cwd":"/a"}}},{"layout":{"pane":{"cwd":"/b"}}}]}"#,
        )
        .unwrap();
        let windows = old.into_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].workspaces.len(), 1);
        assert_eq!(windows[0].workspaces[0].active, 1);
        assert_eq!(windows[0].workspaces[0].tabs.len(), 2);
        assert_eq!(windows[0].window.map(|w| w.width), Some(3.));
    }

    #[test]
    fn version_3_windows_become_one_workspace_each() {
        let old: SavedState = serde_json::from_str(
            r#"{"version":3,"windows":[
                {"active":1,"tabs":[{"layout":{"pane":{"cwd":"/a"}}},{"layout":{"pane":{"cwd":"/b"}}}]},
                {"active":0,"tabs":[]}]}"#,
        )
        .unwrap();
        let windows = old.into_windows();
        assert_eq!(windows.len(), 1, "empty windows are dropped");
        assert_eq!(windows[0].active, 0);
        assert_eq!(windows[0].workspaces[0].name, None);
        assert_eq!(windows[0].workspaces[0].active, 1);
        assert_eq!(windows[0].workspaces[0].tabs.len(), 2);
    }

    #[test]
    fn version_1_files_load() {
        // One pane per tab; unknown fields are tolerated.
        let old: SavedState = serde_json::from_str(
            r#"{"version":1,"active":0,"tabs":[{"cwd":"/x","title":"t","future":1}]}"#,
        )
        .unwrap();
        let windows = old.into_windows();
        let tab = &windows[0].workspaces[0].tabs[0];
        let SavedLayout::Pane(pane) = &tab.layout else { panic!("expected a single pane") };
        assert_eq!(pane.cwd.as_deref(), Some(std::path::Path::new("/x")));
        assert_eq!(pane.title.as_deref(), Some("t"));
        assert_eq!(tab.focused, 0);
    }
}
