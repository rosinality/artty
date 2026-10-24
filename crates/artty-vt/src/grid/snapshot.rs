//! A grid's contents as plain data (artty): what a server hands its
//! successor so a terminal carries on exactly where it was.

use serde::{Deserialize, Serialize};

use super::{Cursor, Dimensions, Grid, Row};
use crate::index::{Column, Line};
use crate::term::cell::Cell;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridSnapshot {
    columns: usize,
    lines: usize,
    max_scroll_limit: usize,
    display_offset: usize,
    lines_scrolled: u64,
    /// From the bottom of the screen up through history, without trailing
    /// blank cells (most of a row usually is).
    rows: Vec<Vec<Cell>>,
    cursor: Cursor<Cell>,
    saved_cursor: Cursor<Cell>,
}

impl GridSnapshot {
    /// Every cell kept.
    pub(crate) fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.rows.iter().flatten()
    }
}

impl Grid<Cell> {
    pub fn snapshot(&self) -> GridSnapshot {
        let blank = Cell::default();
        let rows = (self.topmost_line().0..self.screen_lines() as i32)
            .rev()
            .map(|line| {
                let row = &self[Line(line)];
                let mut cells: Vec<Cell> =
                    (0..self.columns).map(|column| row[Column(column)].clone()).collect();
                while cells.last() == Some(&blank) {
                    cells.pop();
                }
                cells
            })
            .collect();
        GridSnapshot {
            columns: self.columns,
            lines: self.lines,
            max_scroll_limit: self.max_scroll_limit,
            display_offset: self.display_offset,
            lines_scrolled: self.lines_scrolled,
            rows,
            cursor: self.cursor.clone(),
            saved_cursor: self.saved_cursor.clone(),
        }
    }

    pub fn from_snapshot(snapshot: GridSnapshot) -> Self {
        let GridSnapshot {
            columns,
            lines,
            max_scroll_limit,
            display_offset,
            lines_scrolled,
            rows,
            cursor,
            saved_cursor,
        } = snapshot;
        let mut grid = Grid::new(lines, columns, max_scroll_limit);
        let mut rows: Vec<Row<Cell>> = rows
            .into_iter()
            .map(|mut cells| {
                cells.truncate(columns);
                let occupied = cells.len();
                cells.resize_with(columns, Cell::default);
                Row::from_vec(cells, occupied)
            })
            .collect();
        // At least a screen's worth, whatever the snapshot held.
        rows.resize_with(rows.len().max(lines), || Row::new(columns));
        grid.raw.replace_inner(rows);
        grid.display_offset = display_offset.min(grid.history_size());
        grid.lines_scrolled = lines_scrolled;
        grid.cursor = cursor;
        grid.saved_cursor = saved_cursor;
        grid
    }
}
