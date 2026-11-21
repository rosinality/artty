//! Terminal emulation core for artty, forked from `alacritty_terminal` (see UPSTREAM.md).

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]

pub mod ansi;
pub mod event;
pub mod event_loop;
pub mod grid;
pub mod index;
pub mod program_status;
pub mod selection;
pub mod sync;
pub mod term;
pub mod thread;
pub mod tty;
pub mod vi_mode;
pub mod vte;

pub use crate::grid::Grid;
pub use crate::term::Term;
