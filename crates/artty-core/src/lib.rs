//! Terminal sessions for artty.
//!
//! A [`Session`] owns a shell running on a PTY and the terminal state it drives.
//! Rendering code never touches the terminal directly: it asks for a [`Frame`],
//! a renderer-independent snapshot of the visible screen.

mod config;
pub mod config_file;
pub mod control;
mod frame;
pub mod integration;
pub mod layout;
pub mod logging;
pub mod pane;
mod process;
pub mod protocol;
pub mod remote;
mod session;
mod shell;
mod state;
mod theme;

pub use artty_vt as vt;
pub use config::{Colors, Config, CursorStyle, KeyAction, ResumeCommand, ServerConfig};
pub use frame::{
    Frame, FrameCursor, FrameImage, FrameLine, FrameRect, FrameRun, FrameUnderline, UnderlineStyle,
};
pub use process::{ProcessInfo, ProcessSample, process_tree, processes};
pub use session::{Notice, Session, SessionHandover, SessionId, SessionOptions, TerminalSize};
pub use shell::{CommandRecord, ShellPhase};
pub use state::{
    SavedAgent, SavedLayout, SavedPane, SavedState, SavedTab, SavedWindow, SavedWindowState,
    SavedWorkspace,
};
pub use theme::Theme;
