//! A terminal's state as plain data (artty), for handing a running pane to a
//! new server build: screens and scrollback, cursors, modes, colors set by
//! programs, titles, tab stops and keyboard modes. Not kept: the selection,
//! vi mode and damage (everything is redrawn).

use serde::{Deserialize, Serialize};

use super::color::COUNT;
use super::{Config, KeyboardModes, TabStops, Term, TermMode};
use crate::ansi::{CharsetIndex, CursorStyle, Rgb};
use crate::event::EventListener;
use crate::grid::{Dimensions, Grid, GridSnapshot};
use crate::index::Line;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TermSnapshot {
    grid: GridSnapshot,
    inactive_grid: GridSnapshot,
    active_charset: CharsetIndex,
    tabs: Vec<bool>,
    mode: u32,
    scroll_region: (i32, i32),
    colors: Vec<Option<Rgb>>,
    cursor_style: Option<CursorStyle>,
    title: Option<String>,
    title_stack: Vec<Option<String>>,
    keyboard_mode_stack: Vec<u8>,
    inactive_keyboard_mode_stack: Vec<u8>,
    is_focused: bool,
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

impl<T: EventListener> Term<T> {
    pub fn snapshot(&self) -> TermSnapshot {
        TermSnapshot {
            grid: self.grid.snapshot(),
            inactive_grid: self.inactive_grid.snapshot(),
            active_charset: self.active_charset,
            tabs: self.tabs.tabs.clone(),
            mode: self.mode.bits(),
            scroll_region: (self.scroll_region.start.0, self.scroll_region.end.0),
            colors: (0..COUNT).map(|i| self.colors[i]).collect(),
            cursor_style: self.cursor_style,
            title: self.title.clone(),
            title_stack: self.title_stack.clone(),
            keyboard_mode_stack: self.keyboard_mode_stack.iter().map(|m| m.bits()).collect(),
            inactive_keyboard_mode_stack: self.inactive_keyboard_mode_stack.iter().map(|m| m.bits()).collect(),
            is_focused: self.is_focused,
        }
    }

    /// A terminal in the state `snapshot` describes, under (possibly newer)
    /// `config`.
    pub fn from_snapshot(config: Config, snapshot: TermSnapshot, event_proxy: T) -> Self {
        let grid = Grid::from_snapshot(snapshot.grid);
        let inactive_grid = Grid::from_snapshot(snapshot.inactive_grid);
        let mut term = Term::new(config, &Size(grid.columns(), grid.screen_lines()), event_proxy);
        let lines = grid.screen_lines() as i32;
        term.grid = grid;
        term.inactive_grid = inactive_grid;
        term.active_charset = snapshot.active_charset;
        if snapshot.tabs.len() == term.grid.columns() {
            term.tabs = TabStops { tabs: snapshot.tabs };
        }
        term.mode = TermMode::from_bits_truncate(snapshot.mode);
        let (start, end) = snapshot.scroll_region;
        if 0 <= start && start < end && end <= lines {
            term.scroll_region = Line(start)..Line(end);
        }
        for (i, color) in snapshot.colors.into_iter().enumerate().take(COUNT) {
            term.colors[i] = color;
        }
        term.cursor_style = snapshot.cursor_style;
        term.title = snapshot.title;
        term.title_stack = snapshot.title_stack;
        term.keyboard_mode_stack = snapshot.keyboard_mode_stack.into_iter().map(KeyboardModes::from_bits_truncate).collect();
        term.inactive_keyboard_mode_stack =
            snapshot.inactive_keyboard_mode_stack.into_iter().map(KeyboardModes::from_bits_truncate).collect();
        term.is_focused = snapshot.is_focused;
        term.mark_fully_damaged();
        term
    }
}
