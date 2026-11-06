//! Reaching an artty server from any app — a phone's, a test's, a terminal's
//! — without the Mac app's machinery: ssh built in ([`ssh`], with the `ssh`
//! feature), or a command or a socket ([`Link`]); the protocol, with each
//! pane's screen kept ([`Client`]); the bytes for keys (`artty_core::keys`);
//! and a window shown in a terminal ([`attach`]).

pub mod attach;
mod client;
mod link;
#[cfg(feature = "ssh")]
pub mod plain;
#[cfg(feature = "ssh")]
mod plain_files;
#[cfg(feature = "ssh")]
pub mod ssh;
pub mod ssh_config;

pub use artty_core::keys::Key;
pub use artty_core::mouse::{Action as MouseAction, Button as MouseButton};
pub use client::{Client, Event, Screen, SelectionKind, StartError};
pub use link::Link;
