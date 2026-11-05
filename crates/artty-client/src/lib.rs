//! Reaching an artty server from any app — a phone's, a test's — without
//! the Mac app's machinery: ssh built in ([`ssh`]), or a command ([`Link`]);
//! the protocol, with each pane's screen kept ([`Client`]); and the bytes
//! for keys (`artty_core::keys`).

mod client;
mod link;
pub mod ssh;

pub use artty_core::keys::Key;
pub use client::{Client, Event, Screen};
pub use link::Link;
