//! The protocol between the artty server, which owns every terminal session,
//! and the app, whose windows attach to them.
//!
//! A connection starts with one JSON line. `{"cmd":"attach",...}` makes it an
//! app connection that then exchanges length-prefixed postcard messages
//! ([`ClientMessage`] / [`ServerMessage`]); any other [`Request`] is a one-off
//! control request answered with one JSON line (the `artty` command).
//!
//! The messages are the core: both ends must read them alike, so
//! [`protocol_version`] guards them, and changing one is a breaking release
//! (every app, server and phone upgraded together). [`ServerMessage::Welcome`]
//! never changes at all: it's how builds tell each other apart.
//!
//! A feature the other end can do without (an older or newer build's) is an
//! extension instead: a [`ServerMessage::Extension`] or
//! [`ClientMessage::Extension`] of a kind (a name, see [`extension`]) whose
//! `data` is that kind's own postcard-encoded payload. A receiver skips
//! kinds it doesn't know. The app's [`Hello`] lists the kinds it reads; the
//! server answers its `Welcome` with [`ServerMessage::Extensions`], the
//! kinds it sends and reads. Neither sends a kind the other didn't list (nor
//! works one out for it).
//!
//! Adding a kind: its name in [`extension`], its payload type, a variant of
//! [`ServerExtension`] (or a client one) decoding it, the receivers handling
//! it, and a frozen sample in this file's tests (`extensions_are_frozen`).
//! Once released a kind's payload never changes, not even by a field added:
//! builds of every age read it. A change is a new kind (`status.2`), sent
//! beside the old one for as long as older builds matter. Payload types stay
//! out of [`protocol_version`], so adding a kind breaks nothing.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::control::{Request, Response, TabInfo};
// `Request`/`Response` travel as JSON; see `ClientMessage::Control`.
use crate::frame::{Frame, FrameCursor, FrameLine};
use crate::layout::{Axis, Node, PaneId};
use crate::vt::ansi::Rgb;
use crate::{SavedWindow, TerminalSize};

/// Bumped on incompatible changes that `protocol_version` can't see (new
/// core messages); app and server must agree on `protocol_version()`.
pub const PROTOCOL_VERSION: u32 = 23;

/// The protocol version app and server compare: `PROTOCOL_VERSION` mixed
/// with a fingerprint of how sample messages encode, every structure that
/// crosses the wire filled in. A field added, removed or reordered anywhere
/// changes it, so an app never takes a server's messages it can't read for
/// compatible (it hands the server over to its own build instead).
pub fn protocol_version() -> u32 {
    use crate::frame::{FrameImage, FrameRect, FrameRun, FrameUnderline, UnderlineStyle};
    use crate::layout::Node;
    let line = FrameLine {
        runs: vec![FrameRun {
            column: 1,
            cells: 2,
            text: "ab".into(),
            fg: Rgb { r: 3, g: 4, b: 5 },
            bold: true,
            italic: true,
            strikeout: true,
        }],
        backgrounds: vec![FrameRect { column: 6, width: 7, color: Rgb { r: 8, g: 9, b: 10 } }],
        selection: vec![FrameRect { column: 11, width: 12, color: Rgb { r: 13, g: 14, b: 15 } }],
        matches: vec![FrameRect { column: 16, width: 17, color: Rgb { r: 18, g: 19, b: 20 } }],
        images: vec![FrameImage {
            column: 21,
            cells: 22,
            image: 23,
            tile_column: 24,
            tile_row: 25,
        }],
        underlines: vec![FrameUnderline {
            column: 26,
            width: 27,
            style: UnderlineStyle::Dashed,
            color: Rgb { r: 28, g: 29, b: 30 },
        }],
    };
    let update = FrameUpdate {
        columns: 31,
        line_count: 32,
        shift: Some(LineShift { top: 47, count: 48, by: -49 }),
        lines: vec![(33, line)],
        cursor: Some(FrameCursor {
            line: 34,
            column: 35,
            shape: crate::vt::ansi::CursorShape::Beam,
            wide: true,
            blinking: true,
            color: Rgb { r: 36, g: 37, b: 38 },
        }),
        display_offset: 39,
        background: Rgb { r: 40, g: 41, b: 42 },
        foreground: Rgb { r: 43, g: 44, b: 45 },
        mode: 46,
    };
    let tab = TabLayout {
        title: Some("t".into()),
        root: Node::Pane(47),
        focused: 48,
        zoomed: true,
        broadcast: true,
    };
    let workspace = WorkspaceLayout {
        id: 49,
        name: Some("n".into()),
        title: "w".into(),
        color: Some("c".into()),
        root: Some("/r".into()),
        tabs: vec![tab],
        active: 50,
        collapsed: true,
        cpu_percent: 51.,
        memory_bytes: 52,
    };
    let info = TabInfo {
        id: 53,
        window: 54,
        workspace: 55,
        workspace_name: Some("s".into()),
        tab: 56,
        title: "i".into(),
        active: true,
        focused: true,
        cwd: Some("/d".into()),
        process: Some("p".into()),
        status: "x".into(),
        detail: Some("y".into()),
        needs_attention: true,
        shell_integration: true,
        kitty_keyboard: true,
    };
    let samples = [
        postcard::to_stdvec(&ServerMessage::Frame { pane: 57, update }),
        postcard::to_stdvec(&ServerMessage::Layout {
            window: 58,
            layout: WindowLayout { workspaces: vec![workspace], active: 59, hidden: 60 },
        }),
        postcard::to_stdvec(&ServerMessage::Panes {
            panes: vec![PaneState {
                info,
                icon: "z".into(),
                progress: Some(61),
                agent: Some("a".into()),
                ports: vec![3000],
            }],
        }),
        postcard::to_stdvec(&ServerMessage::Image(ImageData {
            id: 62,
            format: crate::vt::term::cell::ImageFormat::Rgba,
            data: vec![63],
            width: 64,
            height: 65,
            columns: 66.,
            rows: 67.,
        })),
        // The envelopes only: their payloads are frozen on their own.
        postcard::to_stdvec(&ServerMessage::Extensions { kinds: vec!["e".into()] }),
        postcard::to_stdvec(&ServerMessage::Extension { kind: "k".into(), data: vec![79] }),
        postcard::to_stdvec(&ClientMessage::Resize {
            pane: 68,
            size: TerminalSize { columns: 69, lines: 70, cell_width: 71, cell_height: 72 },
        }),
        postcard::to_stdvec(&ClientMessage::AttachWindow {
            window: Some(73),
            spec: PaneSpec {
                command: Some(vec!["c".into()]),
                cwd: Some("/w".into()),
                title: Some("t".into()),
            },
            bounds: Some(SavedWindow {
                x: 74.,
                y: 75.,
                width: 76.,
                height: 77.,
                maximized: true,
                fullscreen: true,
            }),
            share: true,
        }),
        postcard::to_stdvec(&ClientMessage::Appearance {
            theme: crate::Theme::default(),
            cursor: crate::CursorStyle::Bar,
        }),
        postcard::to_stdvec(&ClientMessage::CopyMode {
            pane: 78,
            op: CopyModeOp::Move(CopyMotion::PromptDown),
        }),
        postcard::to_stdvec(&ClientMessage::Extension { kind: "k".into(), data: vec![80] }),
    ];
    // FNV-1a: the same in every build.
    let mut hash: u32 = 0x811c_9dc5;
    let bytes =
        samples.iter().flat_map(|sample| sample.as_deref().unwrap_or_default().iter().copied());
    for byte in PROTOCOL_VERSION.to_le_bytes().into_iter().chain(bytes) {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    hash
}

/// Identifies a build: the package version and when the executable was
/// built (rebuilds don't change the version). The server runs from a copy
/// named `artty-server-<built>`, so it reports the build it was copied from.
pub fn build_id(executable: &std::path::Path) -> String {
    let name = executable.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let built = name.strip_prefix("artty-server-").map(str::to_owned).or_else(|| {
        let modified = std::fs::metadata(executable).ok()?.modified().ok()?;
        Some(modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos().to_string())
    });
    format!("{}+{}", env!("CARGO_PKG_VERSION"), built.unwrap_or_default())
}

pub type WindowId = u64;
pub type WorkspaceId = u64;

/// Colors a workspace can have; the app picks the shades.
pub const WORKSPACE_COLORS: [&str; 8] =
    ["red", "orange", "yellow", "green", "teal", "blue", "purple", "pink"];

/// Largest message accepted, against corrupt length prefixes.
const MAX_MESSAGE: usize = 64 << 20;

/// What a new pane runs; everything defaults to a shell in the directory of
/// the pane it's opened from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PaneSpec {
    pub command: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SelectionOp {
    /// Start at a viewport cell: kind (0 simple, 1 word, 2 line), line, column,
    /// and whether the point is in the right half of the cell.
    Start {
        kind: u8,
        line: usize,
        column: usize,
        right: bool,
    },
    Update {
        line: usize,
        column: usize,
        right: bool,
    },
    Clear,
}

/// Copy mode: the keyboard moves a cursor through the screen and history,
/// and selects from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyModeOp {
    Enter,
    Exit,
    Move(CopyMotion),
    /// Start a selection at the cursor (or end it, or change its kind):
    /// 0 characters, 1 lines, 2 a block.
    Select(u8),
}

/// Where copy mode's cursor goes, as in vi.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyMotion {
    Up,
    Down,
    Left,
    Right,
    /// `0`, `$`, `^`.
    LineStart,
    LineEnd,
    FirstOccupied,
    /// `H`, `M`, `L`.
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    /// `b`, `w`, `e`: words separated by punctuation too.
    WordLeft,
    WordRight,
    WordEnd,
    /// `B`, `W`, `E`: words separated by whitespace only.
    BigWordLeft,
    BigWordRight,
    BigWordEnd,
    /// `%`, `{`, `}`.
    Bracket,
    ParagraphUp,
    ParagraphDown,
    /// `gg`, `G`: the start of history, the end of the screen.
    Top,
    Bottom,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    /// `[[`, `]]`: the previous or next shell prompt.
    PromptUp,
    PromptDown,
}

/// App → server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Attach a window: `None` opens a new one with a tab from `spec`. With
    /// `share`, one another app shows is shown here too (else a new one is
    /// opened): its panes take the size of the app that last used it.
    AttachWindow {
        window: Option<WindowId>,
        spec: PaneSpec,
        bounds: Option<SavedWindow>,
        share: bool,
    },
    /// The window closed; its tabs keep running.
    DetachWindow {
        window: WindowId,
    },
    WindowBounds {
        window: WindowId,
        bounds: SavedWindow,
    },
    /// The window gained or lost focus (for notifications and focus reports).
    WindowFocused {
        window: WindowId,
        focused: bool,
    },

    /// A tab at the end of `workspace`, or of the window's active one.
    NewTab {
        window: WindowId,
        workspace: Option<WorkspaceId>,
        spec: PaneSpec,
    },
    Split {
        pane: PaneId,
        axis: Axis,
        spec: PaneSpec,
    },
    ClosePane {
        pane: PaneId,
    },
    /// Tabs are numbered from 0 within their workspace.
    CloseTab {
        workspace: WorkspaceId,
        tab: usize,
    },
    SelectTab {
        workspace: WorkspaceId,
        tab: usize,
    },
    FocusPane {
        pane: PaneId,
    },
    /// Move a tab, possibly into another workspace, to index `to` there.
    MoveTab {
        workspace: WorkspaceId,
        from: usize,
        to_workspace: WorkspaceId,
        to: usize,
    },
    RenameTab {
        workspace: WorkspaceId,
        tab: usize,
        title: Option<String>,
    },
    /// A workspace with one tab from `spec`, after the window's others.
    NewWorkspace {
        window: WindowId,
        name: Option<String>,
        spec: PaneSpec,
    },
    RenameWorkspace {
        workspace: WorkspaceId,
        name: Option<String>,
    },
    /// Close every tab in it.
    CloseWorkspace {
        workspace: WorkspaceId,
    },
    /// Fold a workspace's section in the sidebar.
    SetCollapsed {
        workspace: WorkspaceId,
        collapsed: bool,
    },
    /// Move a workspace to index `to` among `window`'s (possibly another
    /// window's).
    MoveWorkspace {
        workspace: WorkspaceId,
        window: WindowId,
        to: usize,
    },
    /// Move a tab into a new workspace right after its own.
    TabToNewWorkspace {
        workspace: WorkspaceId,
        tab: usize,
    },
    /// Show a workspace in `window`, or bring forward the window already
    /// showing it.
    OpenWorkspace {
        workspace: WorkspaceId,
        window: WindowId,
    },
    /// Take a workspace out of its window; it keeps running, hidden.
    HideWorkspace {
        workspace: WorkspaceId,
    },
    /// Move a workspace into a window of its own (the server asks the app to
    /// open it with [`AppCommand::OpenWindow`]).
    WorkspaceToNewWindow {
        workspace: WorkspaceId,
    },
    /// One of [`WORKSPACE_COLORS`], or none.
    SetWorkspaceColor {
        workspace: WorkspaceId,
        color: Option<String>,
    },
    /// Where its new tabs start (unless the current pane is inside it).
    SetWorkspaceRoot {
        workspace: WorkspaceId,
        root: Option<PathBuf>,
    },
    /// The app let go of these images; send them again if they're shown.
    ForgetImages {
        ids: Vec<u64>,
    },
    ToggleZoom {
        window: WindowId,
    },
    SetRatio {
        window: WindowId,
        path: Vec<bool>,
        ratio: f32,
    },

    Input {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    Resize {
        pane: PaneId,
        size: TerminalSize,
    },
    Scroll {
        pane: PaneId,
        lines: i32,
    },
    Selection {
        pane: PaneId,
        op: SelectionOp,
    },
    Search {
        id: u64,
        pane: PaneId,
        query: String,
        older: bool,
        restart: bool,
    },
    EndSearch {
        pane: PaneId,
    },
    ClearBuffer {
        pane: PaneId,
    },
    /// Scroll the previous (`older`) or next shell prompt to the top.
    JumpToPrompt {
        pane: PaneId,
        older: bool,
    },
    CopyMode {
        pane: PaneId,
        op: CopyModeOp,
    },
    /// The active tab's panes, evenly sized.
    EqualizePanes {
        window: WindowId,
    },
    /// Two panes of a tab trade places.
    SwapPanes {
        a: PaneId,
        b: PaneId,
    },
    /// A pane out of its tab, into a tab of its own after it.
    PaneToNewTab {
        pane: PaneId,
    },
    /// A pane into another tab, beside that tab's focused pane.
    MovePane {
        pane: PaneId,
        workspace: WorkspaceId,
        tab: usize,
    },
    /// Typing into every pane of the active tab at once, on or off.
    ToggleBroadcast {
        window: WindowId,
    },
    /// Reopen the tab closed last (into the window's active workspace if its
    /// own is gone).
    ReopenClosedTab {
        window: WindowId,
    },
    /// Stop telling this app about the folder its `ListDir` for `pane` watches.
    Unwatch {
        pane: PaneId,
    },
    /// Show a match from [`Query::SearchOutput`]: select it and scroll to it.
    Reveal {
        pane: PaneId,
        line: u64,
        column: usize,
        length: usize,
    },
    /// Carry on an agent's conversation ([`SessionHit`]) in a new tab of
    /// `window`, in its directory.
    ResumeSession {
        window: WindowId,
        agent: String,
        session: String,
        cwd: Option<PathBuf>,
    },
    /// Screen updates up to [`ServerMessage::Sync`] `seq` have arrived: the
    /// server sends more (it holds them back while two batches are unseen,
    /// so a slow link gets the newest screens rather than a backlog).
    Ack {
        seq: u64,
    },
    /// Are you there? Answered with [`ServerMessage::Pong`].
    Ping {
        id: u64,
    },
    /// The app's colors and cursor, for the server's panes in place of its
    /// own settings (another machine's server has its own config file).
    Appearance {
        theme: crate::Theme,
        cursor: crate::CursorStyle,
    },

    /// Questions with an answer in [`ServerMessage::Reply`].
    Query {
        id: u64,
        query: Query,
    },
    /// A control request (JSON of a [`Request`]), answered in
    /// [`ServerMessage::Control`]. JSON because `Request` is internally
    /// tagged, which postcard can't decode.
    Control {
        id: u64,
        request: String,
    },
    /// An extension (see the module docs); skipped if its kind is unknown.
    Extension {
        kind: String,
        data: Vec<u8>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Query {
    /// Windows with no app attached.
    DetachedWindows,
    /// Every window, shown or not.
    AllWindows,
    LinkSpan {
        pane: PaneId,
        line: usize,
        column: usize,
    },
    SelectionText {
        pane: PaneId,
    },
    Cwd {
        pane: PaneId,
    },
    /// The command in view, or the last one (see `Session::command_output`).
    CommandOutput {
        pane: PaneId,
    },
    /// Part of a file for the pane's machine (dropped on the pane): `data`
    /// goes on the end of upload `upload`'s file, named `name`. Answered
    /// with [`Reply::Upload`], the file's path once `last`.
    Upload {
        pane: PaneId,
        upload: u64,
        name: String,
        data: Vec<u8>,
        last: bool,
        to: UploadTo,
    },
    /// What's in a folder on the pane's machine: `path`, else the pane's
    /// current directory. Answered with [`Reply::Dir`]; without its entries
    /// if their fingerprint is still `known`. With `watch`, this app is then
    /// told when it changes ([`ServerMessage::DirChanged`]), or (without a
    /// path) when the pane's directory does, until
    /// [`ClientMessage::Unwatch`] or another `ListDir` for the pane.
    ListDir {
        pane: PaneId,
        path: Option<PathBuf>,
        known: Option<u64>,
        watch: bool,
    },
    /// Lines with `text` in every pane's output, history too (case
    /// ignored unless `text` has capitals), newest first: [`Reply::Found`].
    SearchOutput {
        text: String,
    },
    /// Coding agents' conversations on this machine mentioning `text`
    /// (Claude Code's, Codex's), newest first: [`Reply::Sessions`].
    SearchSessions {
        text: String,
    },
    /// Part of a file on the pane's machine, from `offset`: answered with
    /// [`Reply::File`].
    ReadFile {
        pane: PaneId,
        path: PathBuf,
        offset: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Reply {
    Windows(Vec<WindowSummary>),
    Link(Option<(String, usize, usize)>),
    Text(Option<String>),
    Path(Option<PathBuf>),
    /// Search: what matched, or the pattern's error.
    Search(Result<SearchFound, String>),
    /// A command line (if known) and its output.
    Output(Option<(Option<String>, String)>),
    /// Taken (`None`), or the whole file, where it went; or why not.
    Upload(Result<Option<Uploaded>, UploadError>),
    File(Result<FileChunk, String>),
    Dir(Result<Listing, String>),
    Found(Vec<OutputHit>),
    Sessions(Vec<SessionHit>),
}

/// A pane's search ([`ClientMessage::Search`]): whether a match is shown,
/// which of how many it is, and whether the pane has history beyond its
/// screen at all (a full-screen program's, as an agent's, isn't the
/// terminal's to search).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchFound {
    pub found: bool,
    /// 1-based, counted from the top of the history; 0 with no match.
    pub index: usize,
    pub total: usize,
    /// There are more than `total` (counting stopped).
    pub more: bool,
    pub history: bool,
}

/// A line of a pane's output with what was searched for
/// ([`Query::SearchOutput`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputHit {
    pub pane: PaneId,
    /// Its line, counted from the top of the pane's history (where it is
    /// for [`ClientMessage::Reveal`]).
    pub line: u64,
    pub column: usize,
    pub length: usize,
    pub text: String,
}

/// An agent's conversation mentioning what was searched for
/// ([`Query::SearchSessions`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHit {
    /// `claude` or `codex`.
    pub agent: String,
    pub session: String,
    /// Where it was had.
    pub cwd: Option<PathBuf>,
    /// Its first request, shortened.
    pub title: String,
    /// The text around the match.
    pub snippet: String,
    /// Last written, seconds since the epoch.
    pub modified: u64,
}

/// Where a file sent to a pane's machine goes ([`Query::Upload`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UploadTo {
    /// A folder of its own in the cache.
    Cache,
    /// Into `dir`, as its name; `existing` says what if that's taken.
    Folder { dir: PathBuf, existing: Existing },
    /// In place of `path` (a file edited here), if it's unchanged since
    /// `modified` (nanoseconds since the epoch; `None`: whatever it is now).
    File { path: PathBuf, modified: Option<u64> },
}

/// What to do when a file of the name is there.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Existing {
    /// Ask: the upload fails with [`UploadError::Exists`] (at once).
    Refuse,
    Replace,
    /// Keep both: `name (1)` beside it.
    Rename,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Uploaded {
    pub path: PathBuf,
    /// Its modification time there, nanoseconds since the epoch.
    pub modified: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UploadError {
    /// A file of the name is there ([`Existing::Refuse`]).
    Exists,
    /// The file changed there since it was fetched ([`UploadTo::File`]).
    Changed,
    Failed(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists => f.write_str("a file of that name is there"),
            Self::Changed => f.write_str("it changed there since"),
            Self::Failed(err) => f.write_str(err),
        }
    }
}

/// Part of a file ([`Query::ReadFile`]): the bytes from the offset asked
/// for, the file's length, and its modification time (nanoseconds since
/// the epoch).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileChunk {
    pub data: Vec<u8>,
    pub length: u64,
    pub modified: u64,
}

/// A folder listed ([`Query::ListDir`]): where (resolved), what's in it
/// (`None`: as `known`), and that's fingerprint; and whether it's watched
/// (it may not be: a network filesystem's changes from elsewhere aren't
/// seen, and watches are limited).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub path: PathBuf,
    pub fingerprint: u64,
    pub entries: Option<Vec<DirEntry>>,
    pub watched: bool,
}

/// A folder's entry ([`Query::ListDir`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// A folder (or a link to one).
    pub dir: bool,
    pub link: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowSummary {
    pub window: WindowId,
    pub tabs: usize,
    pub bounds: Option<SavedWindow>,
    /// How many apps show it.
    pub apps: usize,
    /// Its tabs' titles, for telling it apart.
    pub titles: Vec<String>,
}

/// One tab of a workspace, as the app draws it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabLayout {
    pub title: Option<String>,
    pub root: Node,
    pub focused: PaneId,
    pub zoomed: bool,
    /// Typing goes to every pane.
    pub broadcast: bool,
}

/// A named group of tabs: a section of the sidebar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceLayout {
    pub id: WorkspaceId,
    pub name: Option<String>,
    /// A name for it when it has none: its git repository or directory.
    pub title: String,
    pub color: Option<String>,
    pub root: Option<PathBuf>,
    pub tabs: Vec<TabLayout>,
    /// The tab shown when this workspace is the window's active one.
    pub active: usize,
    /// Its section is folded in the sidebar.
    pub collapsed: bool,
    /// What its panes use, as of the last second.
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

/// A window's workspaces; the active workspace's active tab fills it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowLayout {
    pub workspaces: Vec<WorkspaceLayout>,
    pub active: usize,
    /// Workspaces not on screen anywhere (hidden, or in closed windows).
    pub hidden: usize,
}

impl WindowLayout {
    pub fn active_workspace(&self) -> Option<&WorkspaceLayout> {
        self.workspaces.get(self.active)
    }

    pub fn active_tab(&self) -> Option<&TabLayout> {
        let workspace = self.active_workspace()?;
        workspace.tabs.get(workspace.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut TabLayout> {
        let workspace = self.workspaces.get_mut(self.active)?;
        workspace.tabs.get_mut(workspace.active)
    }

    /// Every tab, workspace by workspace, with where it is.
    pub fn tabs(&self) -> impl Iterator<Item = (WorkspaceId, usize, &TabLayout)> {
        self.workspaces
            .iter()
            .flat_map(|w| w.tabs.iter().enumerate().map(move |(i, tab)| (w.id, i, tab)))
    }
}

/// What the sidebar and input handling need to know about a pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneState {
    pub info: TabInfo,
    /// Icon to show: `busy`, `waiting`, `done`, `failed`, `attention` or empty.
    pub icon: String,
    pub progress: Option<u8>,
    pub agent: Option<String>,
    /// TCP ports its programs listen on (on Linux), for forwarding.
    pub ports: Vec<u16>,
}

impl PaneState {
    /// What a sidebar row shows beside a tab's title: the command or agent
    /// (not its time or state; the icon tells that), an exit code, progress.
    /// Not what the title already says: shells put the running command in it.
    pub fn short_detail(&self, title: &str) -> Option<String> {
        let detail = self.info.detail.as_deref()?;
        let what = detail.split_once(" · ").map_or(detail, |(what, _)| what);
        let what = (!title.starts_with(what)).then(|| what.to_owned());
        match (what, self.progress) {
            (Some(what), Some(percent)) => Some(format!("{what} · {percent}%")),
            (None, Some(percent)) => Some(format!("{percent}%")),
            (what, None) => what,
        }
    }

    /// How much a tab with this pane wants the user, for showing the most
    /// urgent of its panes: waiting, busy, failed, done, attention, nothing.
    pub fn urgency(&self) -> u8 {
        match self.icon.as_str() {
            "waiting" => 5,
            "busy" => 4,
            "failed" => 3,
            "done" => 2,
            "attention" => 1,
            _ => 0,
        }
    }
}

/// A screen update: everything but the lines, plus the lines that changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameUpdate {
    pub columns: usize,
    pub line_count: usize,
    /// Lines that moved (output scrolling, the view scrolled into history),
    /// applied before `lines`, so they needn't be sent again.
    pub shift: Option<LineShift>,
    pub lines: Vec<(u32, FrameLine)>,
    pub cursor: Option<FrameCursor>,
    pub display_offset: usize,
    pub background: Rgb,
    pub foreground: Rgb,
    /// Terminal mode bits (`TermMode`), for keys and mouse handling.
    pub mode: u32,
}

/// Lines `top..top + count` become those that were `by` lines below them
/// (above them, for a negative `by`): a scroll of the screen, or of a region
/// of it (a program's status lines staying put).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineShift {
    pub top: u32,
    pub count: u32,
    pub by: i32,
}

impl LineShift {
    /// Move `lines` as it says (the lines moved from are left as they were,
    /// or with others' contents: the update's own lines say what they are).
    fn apply<T>(self, lines: &mut [T]) {
        let (top, count) = (self.top as usize, self.count as usize);
        let by = self.by.unsigned_abs() as usize;
        if self.by > 0 && top + count + by <= lines.len() {
            lines[top..top + count + by].rotate_left(by);
        } else if self.by < 0 && by <= top && top + count <= lines.len() {
            lines[top - by..top + count].rotate_right(by);
        }
    }

    /// How `frame`'s lines moved from `previous`'s, if enough did to be
    /// worth saying. Each line that changed votes for where it was before
    /// (found by a fingerprint, then compared); the shift most lines agree
    /// on covers its first to its last line that it puts right.
    fn find(previous: &[FrameLine], lines: &[FrameLine]) -> Option<Self> {
        use std::collections::HashMap;
        let n = lines.len();
        if previous.len() != n || n < 3 {
            return None;
        }
        let blank = FrameLine::default();
        let mut was: HashMap<u64, Vec<usize>> = HashMap::new();
        for (j, line) in previous.iter().enumerate().filter(|(_, l)| **l != blank) {
            was.entry(fingerprint(line)).or_default().push(j);
        }
        let mut votes: HashMap<i32, usize> = HashMap::new();
        for (i, line) in lines.iter().enumerate() {
            if *line == blank || previous[i] == *line {
                continue;
            }
            // A line repeated all over the screen says little.
            match was.get(&fingerprint(line)) {
                Some(places) if places.len() <= 8 => {
                    for &j in places {
                        if previous[j] == *line {
                            *votes.entry(j as i32 - i as i32).or_default() += 1;
                        }
                    }
                },
                _ => {},
            }
        }
        let (&by, _) = votes
            .iter()
            .filter(|&(_, &count)| count >= 2)
            .max_by_key(|&(&by, &count)| (count, std::cmp::Reverse(by.unsigned_abs())))?;
        let right = |i: usize| {
            let j = i as i32 + by;
            (0..n as i32).contains(&j) && lines[i] != blank && previous[j as usize] == lines[i]
        };
        let first = (0..n).find(|&i| right(i))?;
        let last = (0..n).rev().find(|&i| right(i))?;
        Some(Self { top: first as u32, count: (last - first + 1) as u32, by })
    }
}

/// A line's text and where it is, for finding it again (compared after).
fn fingerprint(line: &FrameLine) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for run in &line.runs {
        (run.column, &run.text, run.fg.r, run.fg.g, run.fg.b).hash(&mut hasher);
    }
    line.backgrounds.len().hash(&mut hasher);
    hasher.finish()
}

impl FrameUpdate {
    /// The update turning `previous` into `frame`: lines that moved as a
    /// shift, then those still different.
    pub fn diff(previous: Option<&Frame>, frame: &Frame, mode: u32) -> Self {
        let mut before: Vec<Option<&FrameLine>> = match previous {
            Some(p) => p.lines.iter().map(Some).collect(),
            None => Vec::new(),
        };
        before.resize(frame.lines.len(), None);
        let changed = |before: &[Option<&FrameLine>]| {
            frame.lines.iter().zip(before).filter(|(line, was)| **was != Some(*line)).count()
        };
        // Only when it saves lines (a band can take in some that were right).
        let shift =
            previous.and_then(|p| LineShift::find(&p.lines, &frame.lines)).filter(|shift| {
                let mut shifted = before.clone();
                shift.apply(&mut shifted);
                changed(&shifted) < changed(&before)
            });
        if let Some(shift) = shift {
            shift.apply(&mut before);
        }
        let lines = frame
            .lines
            .iter()
            .enumerate()
            .filter(|(i, line)| before[*i] != Some(*line))
            .map(|(i, line)| (i as u32, line.clone()))
            .collect();
        Self {
            columns: frame.columns,
            line_count: frame.lines.len(),
            shift,
            lines,
            cursor: frame.cursor,
            display_offset: frame.display_offset,
            background: frame.background,
            foreground: frame.foreground,
            mode,
        }
    }

    /// Apply to the app's copy of the screen.
    pub fn apply(self, frame: &mut Frame) {
        frame.lines.resize_with(self.line_count, FrameLine::default);
        if let Some(shift) = self.shift {
            shift.apply(&mut frame.lines);
        }
        for (index, line) in self.lines {
            if let Some(slot) = frame.lines.get_mut(index as usize) {
                *slot = line;
            }
        }
        frame.columns = self.columns;
        frame.cursor = self.cursor;
        frame.display_offset = self.display_offset;
        frame.background = self.background;
        frame.foreground = self.foreground;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppCommand {
    /// Bring a window forward (`artty focus`).
    Activate {
        window: WindowId,
    },
    /// Open a view of a server window that has none.
    OpenWindow {
        window: WindowId,
    },
    /// Show the Activity view in a window (`artty activity`).
    ShowActivity {
        window: WindowId,
    },
    OpenSettings,
    /// Quit and reopen the app (sessions keep running).
    Restart,
    /// Open a web page, for a program on the server (`artty open`).
    OpenUrl {
        url: String,
    },
}

/// Server → app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// The first message, in every build alike (never changed).
    Welcome {
        version: u32,
        server_version: String,
    },
    /// A window is attached (also the reply to `AttachWindow`).
    Attached {
        window: WindowId,
        bounds: Option<SavedWindow>,
    },
    Layout {
        window: WindowId,
        layout: WindowLayout,
    },
    /// The window's last tab closed.
    WindowClosed {
        window: WindowId,
    },
    Frame {
        pane: PaneId,
        update: FrameUpdate,
    },
    Panes {
        panes: Vec<PaneState>,
    },
    Bell {
        pane: PaneId,
    },
    /// Show a desktop notification (the app can post native ones).
    Notify {
        title: String,
        body: String,
        /// The pane it's about, and its window, for showing it.
        window: Option<WindowId>,
        pane: Option<PaneId>,
    },
    /// Something for the app to do, from the `artty` command.
    App(AppCommand),
    /// A program copied this (OSC 52): onto the app's clipboard.
    Clipboard {
        text: String,
    },
    Reply {
        id: u64,
        reply: Reply,
    },
    /// JSON of a [`Response`].
    Control {
        id: u64,
        response: String,
    },
    /// The server is exiting: sessions ended (`artty server stop`), or it's
    /// restarting and a new server will reopen them.
    Stopping {
        restart: bool,
    },
    /// An image that frames show (by id), sent before the first of them.
    Image(ImageData),
    /// The end of a batch of screen updates, to acknowledge.
    Sync {
        seq: u64,
    },
    Pong {
        id: u64,
    },
    /// The folder watched for `pane` changed, or (watching the pane's
    /// directory) the pane moved to another: list it again.
    DirChanged {
        pane: PaneId,
    },
    /// Right after `Welcome`: the extension kinds this server sends and
    /// reads.
    Extensions {
        kinds: Vec<String>,
    },
    /// An extension (see the module docs); skipped if its kind is unknown.
    Extension {
        kind: String,
        data: Vec<u8>,
    },
}

/// Extension kinds, each with its payload (frozen once released; see the
/// module docs).
pub mod extension {
    /// Server → app: [`super::StatusUpdate`].
    pub const STATUS: &str = "status";
    /// Server → app: [`super::QueueUpdate`].
    pub const QUEUE: &str = "queue";
    /// App → server: [`super::Peek`].
    pub const PEEK: &str = "peek";
    /// Server → app, the connection itself: what the server sends after
    /// an `Extensions` listing this is one deflate stream (flushed as it
    /// sends), until the connection ends. Apps ask for it over a network,
    /// as not every sshd compresses (screens shrink 4-30x); see
    /// [`super::deflates_after`].
    pub const DEFLATE: &str = "deflate";
}

/// Whether, having read `message` from a server, the rest comes deflated
/// ([`extension::DEFLATE`]): then read it through [`inflating`].
pub fn deflates_after(message: &ServerMessage) -> bool {
    matches!(message, ServerMessage::Extensions { kinds } if kinds.iter().any(|k| k == extension::DEFLATE))
}

/// A server's messages after [`deflates_after`], from `reader`.
pub fn inflating<R: Read>(reader: R) -> Inflating<R> {
    Inflating {
        inner: reader,
        inflate: flate2::Decompress::new(false),
        input: vec![0; 32 * 1024],
        start: 0,
        end: 0,
    }
}

/// Reading a deflate stream that's flushed as it's sent, as a connection
/// is: what's decompressed already is given before waiting for more.
/// (flate2's own readers wait for more input first, so the end of a
/// message could sit until the next one came.)
pub struct Inflating<R> {
    inner: R,
    inflate: flate2::Decompress,
    /// Read and not yet decompressed: `input[start..end]`.
    input: Vec<u8>,
    start: usize,
    end: usize,
}

impl<R: Read> Read for Inflating<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let (read_before, written_before) = (self.inflate.total_in(), self.inflate.total_out());
            let status = self
                .inflate
                .decompress(&self.input[self.start..self.end], out, flate2::FlushDecompress::None)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            self.start += (self.inflate.total_in() - read_before) as usize;
            let written = (self.inflate.total_out() - written_before) as usize;
            if written > 0 || status == flate2::Status::StreamEnd {
                return Ok(written);
            }
            // It needs more than there is: keep what's left, read more.
            self.input.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
            if self.end == self.input.len() {
                self.input.resize(self.input.len() * 2, 0);
            }
            match self.inner.read(&mut self.input[self.end..])? {
                0 => return Ok(0),
                read => self.end += read,
            }
        }
    }
}

/// The stream an app reads, deflated from here on ([`extension::DEFLATE`]).
pub fn deflating<W: Write>(writer: W) -> flate2::write::DeflateEncoder<W> {
    flate2::write::DeflateEncoder::new(writer, flate2::Compression::default())
}

/// The kinds a server reads from apps (it lists them, with those it sends,
/// in its `Extensions`: an app sends only those).
pub const CLIENT_KINDS: &[&str] = &[extension::PEEK];

/// [`extension::PEEK`]: also send this pane's screens, though no window of
/// the app shows it (the Agents view showing an agent's terminal); `None`:
/// no longer. Its size stays as the windows that show it make it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peek {
    pub pane: Option<PaneId>,
}

impl Peek {
    pub fn message(&self) -> ClientMessage {
        let data = postcard::to_stdvec(self).unwrap_or_default();
        ClientMessage::Extension { kind: extension::PEEK.to_owned(), data }
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        postcard::from_bytes(data).ok()
    }
}

/// The server's extensions this build reads, decoded.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerExtension {
    Status(StatusUpdate),
    Queue(QueueUpdate),
}

impl ServerExtension {
    /// Every kind there is a variant for: what an app handling them all
    /// lists in its [`Hello`], and what a server sends.
    pub const KINDS: &[&str] = &[extension::STATUS, extension::QUEUE];

    /// `None` for a kind this build doesn't know, or can't read: skipped.
    pub fn decode(kind: &str, data: &[u8]) -> Option<Self> {
        let decoded = match kind {
            extension::STATUS => postcard::from_bytes(data).map(Self::Status),
            extension::QUEUE => postcard::from_bytes(data).map(Self::Queue),
            _ => {
                log::debug!("skipped an extension of unknown kind {kind:?}");
                return None;
            },
        };
        decoded.inspect_err(|err| log::debug!("skipped extension {kind:?}: {err}")).ok()
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Status(_) => extension::STATUS,
            Self::Queue(_) => extension::QUEUE,
        }
    }

    /// Wrapped for sending.
    pub fn message(&self) -> ServerMessage {
        let data = match self {
            Self::Status(update) => postcard::to_stdvec(update),
            Self::Queue(update) => postcard::to_stdvec(update),
        };
        ServerMessage::Extension { kind: self.kind().to_owned(), data: data.unwrap_or_default() }
    }
}

/// [`extension::QUEUE`]: what needs the user, from every agent on the
/// server, whole each time it changes. Frozen, with [`QueueItem`] and
/// [`QueueKind`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueUpdate {
    /// Permission requests (oldest first), agents waiting, then replies
    /// (newest first).
    pub items: Vec<QueueItem>,
}

/// One thing in the queue. Answered with control requests: `Decide` (a
/// permission request, by `id`; `always` picks one of `options`), or
/// `QueueDismiss` (a reply, by `id`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub kind: QueueKind,
    /// The permission request's or the reply's id; a waiting agent's pane.
    pub id: u64,
    pub pane: PaneId,
    /// `claude`, `codex`, ...
    pub agent: Option<String>,
    /// The pane's tab title, and its workspace's name.
    pub title: String,
    pub workspace: String,
    /// What it's about: the command or file asked for, the agent's question,
    /// the reply (its start, if long).
    pub text: String,
    /// More: the tool's whole input (JSON), or the prompt a reply answers.
    pub context: String,
    /// A permission request's "always" choices, as the agent offered them.
    pub options: Vec<String>,
    /// When, in milliseconds since the Unix epoch.
    pub since_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueKind {
    /// An agent asks permission to use a tool.
    Approval,
    /// An agent waits for you otherwise (a question, a prompt of its own).
    Waiting,
    /// An agent finished a turn; unread.
    Reply,
}

/// [`extension::STATUS`]: the window's status line changed (see
/// [`crate::status`]). Frozen, with [`StatusLine`] and [`StatusSegment`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub window: WindowId,
    pub line: StatusLine,
}

/// A window's status line, worked out by the server from the `[status]`
/// config: what goes on its left and on its right, in styled pieces.
/// Empty when there's none to show.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusLine {
    pub left: Vec<StatusSegment>,
    pub right: Vec<StatusSegment>,
}

impl StatusLine {
    pub fn is_empty(&self) -> bool {
        self.left.is_empty() && self.right.is_empty()
    }
}

/// Text in one style; colors `None` are the client's own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusSegment {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub dim: bool,
}

/// An image's pixels and how big it's shown.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageData {
    pub id: u64,
    pub format: crate::vt::term::cell::ImageFormat,
    /// The image file, or RGBA rows.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Size in cells.
    pub columns: f32,
    pub rows: f32,
}

/// Encode a control request for [`ClientMessage::Control`].
pub fn encode_request(request: &Request) -> String {
    serde_json::to_string(request).unwrap_or_default()
}

/// Decode a [`ServerMessage::Control`] response.
pub fn decode_response(response: &str) -> Response {
    serde_json::from_str(response)
        .unwrap_or_else(|err| Response::error(format!("bad response: {err}")))
}

/// Write one length-prefixed message.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    write_message_unflushed(writer, message)?;
    writer.flush()
}

/// [`write_message`] without flushing: for several at once, then a flush
/// (on a deflated stream, each flush costs bytes).
pub fn write_message_unflushed<T: Serialize>(
    writer: &mut impl Write,
    message: &T,
) -> io::Result<()> {
    let bytes = postcard::to_stdvec(message).map_err(io::Error::other)?;
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)
}

/// Read one length-prefixed message.
pub fn read_message<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    postcard::from_bytes(&bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// The JSON line that starts an app connection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Hello {
    Attach {
        version: u32,
        app_version: String,
        /// The extension kinds the app reads (none, from older apps).
        #[serde(default)]
        extensions: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FrameRun;

    fn line(text: &str) -> FrameLine {
        FrameLine {
            runs: vec![FrameRun {
                column: 0,
                cells: text.len(),
                text: text.into(),
                fg: Rgb { r: 1, g: 2, b: 3 },
                bold: false,
                italic: false,
                strikeout: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn diffs_send_changed_lines_only() {
        let before = Frame {
            columns: 10,
            lines: vec![line("a"), line("b"), line("c")],
            ..Default::default()
        };
        let mut after = before.clone();
        after.lines[1] = line("B");
        let update = FrameUpdate::diff(Some(&before), &after, 0);
        assert_eq!(update.lines.len(), 1);
        assert_eq!(update.lines[0].0, 1);

        let mut copy = before.clone();
        update.apply(&mut copy);
        assert_eq!(copy, after);

        // A first update carries everything.
        assert_eq!(FrameUpdate::diff(None, &after, 0).lines.len(), 3);
    }

    fn screen(texts: &[&str]) -> Frame {
        let lines = texts
            .iter()
            .map(|t| if t.is_empty() { FrameLine::default() } else { line(t) })
            .collect();
        Frame { columns: 10, lines, ..Default::default() }
    }

    /// `update` turns `before` into `after`; the lines it sends, by index.
    fn sent(before: &Frame, after: &Frame) -> (Option<LineShift>, Vec<u32>) {
        let update = FrameUpdate::diff(Some(before), after, 0);
        let result = (update.shift, update.lines.iter().map(|(i, _)| *i).collect());
        let mut copy = before.clone();
        update.apply(&mut copy);
        assert_eq!(&copy, after);
        result
    }

    #[test]
    fn scrolled_lines_move_instead_of_being_sent() {
        let before = screen(&["a", "b", "c", "d", "e", "f"]);
        // Output scrolling up a line: only the new one is sent.
        let (shift, lines) = sent(&before, &screen(&["b", "c", "d", "e", "f", "g"]));
        assert_eq!(shift, Some(LineShift { top: 0, count: 5, by: 1 }));
        assert_eq!(lines, [5]);
        // Three at once.
        let (shift, lines) = sent(&before, &screen(&["d", "e", "f", "x", "y", "z"]));
        assert_eq!(shift, Some(LineShift { top: 0, count: 3, by: 3 }));
        assert_eq!(lines, [3, 4, 5]);
        // The view scrolled back into history: lines move down.
        let (shift, lines) = sent(&before, &screen(&["z", "a", "b", "c", "d", "e"]));
        assert_eq!(shift, Some(LineShift { top: 1, count: 5, by: -1 }));
        assert_eq!(lines, [0]);
    }

    #[test]
    fn a_scroll_region_moves_between_lines_that_stay() {
        // A program's title and status lines stay; what's between scrolls.
        let before = screen(&["title", "a", "b", "c", "d", "e", "status"]);
        let after = screen(&["title", "c", "d", "e", "f", "g", "status"]);
        let (shift, lines) = sent(&before, &after);
        assert_eq!(shift, Some(LineShift { top: 1, count: 3, by: 2 }));
        assert_eq!(lines, [4, 5]);
    }

    #[test]
    fn no_shift_without_moved_lines() {
        let before = screen(&["a", "b", "c", "", "", ""]);
        // A line typed at: no shift.
        assert_eq!(sent(&before, &screen(&["a", "b", "cd", "", "", ""])), (None, vec![2]));
        // Blank lines matching blank lines say nothing.
        assert_eq!(sent(&screen(&["", "", "", ""]), &screen(&["", "x", "", ""])), (None, vec![1]));
        // A single moved line isn't worth a shift.
        assert_eq!(sent(&before, &screen(&["b", "x", "y", "", "", ""])).0, None);
    }

    #[test]
    fn any_change_round_trips() {
        // Random screens from few texts (so lines repeat), random scrolls of
        // random regions and random edits: applying the diff always gives
        // the new screen, and a shift never sends more than without one.
        let texts = ["", "a", "b", "c", "d", "e", "f"];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..5000 {
            let rows = 3 + next(10);
            let before: Vec<&str> = (0..rows).map(|_| texts[next(texts.len())]).collect();
            let mut after = before.clone();
            if next(2) == 0 {
                let top = next(rows);
                let bottom = top + 1 + next(rows - top);
                let by = 1 + next(bottom - top);
                if next(2) == 0 {
                    after[top..bottom].rotate_left(by % (bottom - top));
                } else {
                    after[top..bottom].rotate_right(by % (bottom - top));
                }
            }
            for _ in 0..next(3) {
                let i = next(rows);
                after[i] = texts[next(texts.len())];
            }
            let (before, after) = (screen(&before), screen(&after));
            let (_, lines) = sent(&before, &after);
            let unshifted = before.lines.iter().zip(&after.lines).filter(|(a, b)| a != b).count();
            assert!(lines.len() <= unshifted, "{before:?} -> {after:?}");
        }
    }

    #[test]
    fn layouts_and_pane_states_round_trip() {
        // Postcard isn't self-describing: a field serde skips when empty would
        // break decoding, so check the messages with optional fields.
        let info = TabInfo {
            id: 3,
            window: 1,
            workspace: 2,
            workspace_name: None,
            tab: 1,
            title: "t".into(),
            active: true,
            focused: true,
            cwd: None,
            process: None,
            status: "idle".into(),
            detail: None,
            needs_attention: false,
            shell_integration: true,
            kitty_keyboard: false,
        };
        let state =
            PaneState { info, icon: String::new(), progress: None, agent: None, ports: vec![] };
        let tab = TabLayout {
            title: None,
            root: Node::Pane(3),
            focused: 3,
            zoomed: false,
            broadcast: false,
        };
        let workspace = WorkspaceLayout {
            id: 2,
            name: None,
            title: "artty".into(),
            color: Some("blue".into()),
            root: None,
            tabs: vec![tab],
            active: 0,
            collapsed: true,
            cpu_percent: 1.5,
            memory_bytes: 1 << 20,
        };
        let layout = WindowLayout { workspaces: vec![workspace], active: 0, hidden: 2 };
        let mut buffer = Vec::new();
        write_message(&mut buffer, &ServerMessage::Panes { panes: vec![state.clone()] }).unwrap();
        write_message(&mut buffer, &ServerMessage::Layout { window: 1, layout: layout.clone() })
            .unwrap();
        let mut reader = &buffer[..];
        let ServerMessage::Panes { panes } = read_message(&mut reader).unwrap() else { panic!() };
        assert_eq!(panes, vec![state]);
        let ServerMessage::Layout { layout: back, .. } = read_message(&mut reader).unwrap() else {
            panic!()
        };
        assert_eq!(back, layout);
    }

    #[test]
    fn the_protocol_version_includes_the_message_layout() {
        // The same in every run (it's compared across processes), and not
        // just the hand-kept number.
        assert_eq!(protocol_version(), protocol_version());
        assert_ne!(protocol_version(), PROTOCOL_VERSION);
    }

    #[test]
    fn messages_round_trip() {
        let mut buffer = Vec::new();
        let message = ClientMessage::Input { pane: 7, bytes: b"ls\r".to_vec() };
        write_message(&mut buffer, &message).unwrap();
        let frame = Frame { columns: 4, lines: vec![line("hi")], ..Default::default() };
        write_message(
            &mut buffer,
            &ServerMessage::Frame { pane: 7, update: FrameUpdate::diff(None, &frame, 5) },
        )
        .unwrap();
        let mut reader = &buffer[..];
        assert!(matches!(
            read_message::<ClientMessage>(&mut reader).unwrap(),
            ClientMessage::Input { pane: 7, .. }
        ));
        let ServerMessage::Frame { update, .. } =
            read_message::<ServerMessage>(&mut reader).unwrap()
        else {
            panic!()
        };
        assert_eq!(update.mode, 5);
        assert_eq!(update.lines[0].1, line("hi"));
    }

    /// Extensions are read by builds of every age: their bytes never change.
    /// A new kind adds its own sample here (and an incompatible change is a
    /// new kind, with a new sample, never an edit of these).
    #[test]
    fn extensions_are_frozen() {
        let bytes = |message: &ServerMessage| postcard::to_stdvec(message).unwrap();
        let status = ServerExtension::Status(StatusUpdate {
            window: 300,
            line: StatusLine {
                left: vec![StatusSegment {
                    text: "l".into(),
                    fg: Some(Rgb { r: 1, g: 2, b: 3 }),
                    bg: None,
                    bold: true,
                    dim: false,
                }],
                right: vec![StatusSegment {
                    text: "r".into(),
                    fg: None,
                    bg: Some(Rgb { r: 4, g: 5, b: 6 }),
                    bold: false,
                    dim: true,
                }],
            },
        });
        let message = status.message();
        let ServerMessage::Extension { kind, data } = &message else { panic!() };
        assert_eq!(kind, "status");
        assert_eq!(data, &FROZEN_STATUS);
        assert_eq!(ServerExtension::decode(kind, data), Some(status));
        let queue = ServerExtension::Queue(QueueUpdate {
            items: vec![QueueItem {
                kind: QueueKind::Reply,
                id: 9,
                pane: 3,
                agent: Some("c".into()),
                title: "t".into(),
                workspace: "w".into(),
                text: "x".into(),
                context: "p".into(),
                options: vec!["o".into()],
                since_ms: 300,
            }],
        });
        let queued = queue.message();
        let ServerMessage::Extension { kind: queue_kind, data: queue_data } = &queued else {
            panic!()
        };
        assert_eq!(queue_kind, "queue");
        assert_eq!(queue_data, &FROZEN_QUEUE);
        assert_eq!(ServerExtension::decode(queue_kind, queue_data), Some(queue));
        // The envelopes, each way (core: changed only in a breaking release).
        assert_eq!(bytes(&message), FROZEN_ENVELOPE);
        let client = ClientMessage::Extension { kind: "k".into(), data: vec![7] };
        assert_eq!(postcard::to_stdvec(&client).unwrap(), FROZEN_CLIENT_ENVELOPE);
        assert_eq!(
            bytes(&ServerMessage::Extensions { kinds: vec!["status".into()] }),
            FROZEN_KINDS
        );
        // App → server.
        let peek = Peek { pane: Some(300) };
        let ClientMessage::Extension { kind, data } = peek.message() else { unreachable!() };
        assert_eq!((kind.as_str(), data.as_slice()), ("peek", &FROZEN_PEEK[..]));
        assert_eq!(Peek::decode(&data), Some(peek));
        assert_eq!(Peek::decode(&[0]), Some(Peek { pane: None }));
    }

    /// `peek`: pane 300.
    const FROZEN_PEEK: [u8; 3] = [1, 172, 2];

    /// `status`: window 300, `l` bold in 1,2,3 on the left, `r` dim on
    /// 4,5,6 on the right.
    /// `queue`: one reply (kind 2), id 9, pane 3, agent "c", title "t",
    /// workspace "w", text "x", context "p", options ["o"], 300 ms.
    const FROZEN_QUEUE: [u8; 20] =
        [1, 2, 9, 3, 1, 1, 99, 1, 116, 1, 119, 1, 120, 1, 112, 1, 1, 111, 172, 2];
    const FROZEN_STATUS: [u8; 22] =
        [172, 2, 1, 1, 108, 1, 1, 2, 3, 0, 1, 0, 1, 1, 114, 0, 1, 4, 5, 6, 0, 1];
    /// `ServerMessage::Extension` (variant 18): `status`, then its payload.
    const FROZEN_ENVELOPE: [u8; 31] = [
        18, 6, 115, 116, 97, 116, 117, 115, 22, 172, 2, 1, 1, 108, 1, 1, 2, 3, 0, 1, 0, 1, 1, 114,
        0, 1, 4, 5, 6, 0, 1,
    ];
    /// `ClientMessage::Extension` (variant 49): `k`, `[7]`.
    const FROZEN_CLIENT_ENVELOPE: [u8; 5] = [49, 1, 107, 1, 7];
    /// `ServerMessage::Extensions` (variant 17): `["status"]`.
    const FROZEN_KINDS: [u8; 9] = [17, 1, 6, 115, 116, 97, 116, 117, 115];

    #[test]
    fn unknown_and_unreadable_extensions_are_skipped() {
        assert_eq!(ServerExtension::decode("nothing.yet", &[1, 2, 3]), None);
        assert_eq!(ServerExtension::decode(extension::STATUS, &[0xff]), None);
    }

    #[test]
    fn hellos_list_extensions_and_older_ones_none() {
        let old = r#"{"cmd":"attach","version":1,"app_version":"0"}"#;
        let Hello::Attach { extensions, .. } = serde_json::from_str(old).unwrap();
        assert!(extensions.is_empty());
        let new = r#"{"cmd":"attach","version":1,"app_version":"0","extensions":["status"]}"#;
        let Hello::Attach { extensions, .. } = serde_json::from_str(new).unwrap();
        assert_eq!(extensions, ["status"]);
    }
}

#[cfg(test)]
mod deflate_tests {
    use std::io::Write;

    use super::*;

    /// What a connection gives: what was sent so far, and no more. Asked
    /// for more, it fails the test: a reader that has a message's bytes
    /// must give them without waiting for the next.
    struct Sent {
        flushes: std::collections::VecDeque<Vec<u8>>,
        chunk: usize,
    }

    impl Read for Sent {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some(front) = self.flushes.front_mut() else {
                panic!("waited for more, with a message's bytes in hand")
            };
            let n = front.len().min(out.len()).min(self.chunk);
            out[..n].copy_from_slice(&front[..n]);
            front.drain(..n);
            if front.is_empty() {
                self.flushes.pop_front();
            }
            Ok(n)
        }
    }

    #[test]
    fn each_message_arrives_without_waiting_for_the_next() {
        let messages = [
            ServerMessage::Pong { id: 1 },
            ServerMessage::Notify {
                title: "t".into(),
                body: "screen ".repeat(30_000),
                window: None,
                pane: None,
            },
            ServerMessage::Sync { seq: 2 },
            ServerMessage::Pong { id: 3 },
        ];
        for chunk in [1, 7, 64 * 1024] {
            // Each message flushed on its own, as the server does when none wait.
            let mut flushes = std::collections::VecDeque::new();
            let mut out = deflating(Vec::new());
            for message in &messages {
                write_message_unflushed(&mut out, message).unwrap();
                out.flush().unwrap();
                flushes.push_back(std::mem::take(out.get_mut()));
            }
            let mut reader = inflating(Sent { flushes, chunk });
            for message in &messages {
                let got: ServerMessage = read_message(&mut reader).unwrap();
                assert_eq!(format!("{got:?}"), format!("{message:?}"), "chunk {chunk}");
            }
        }
    }
}
