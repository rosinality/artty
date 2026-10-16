//! Terminal sessions for artty.
//!
//! A [`Session`] owns a shell running on a PTY and the terminal state it drives.
//! Rendering code never touches the terminal directly: it asks for a [`Frame`],
//! a renderer-independent snapshot of the visible screen.

mod frame;
mod session;
mod theme;

pub use artty_vt as vt;
pub use frame::{Frame, FrameCursor, FrameLine, FrameRect, FrameRun};
pub use session::{Notice, Session, SessionId, SessionOptions, TerminalSize};
pub use theme::Theme;
