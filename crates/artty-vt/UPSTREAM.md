# Upstream

`artty-vt` is a fork of two Apache-2.0 sources. Record every upstream sync here
so fixes can be cherry-picked.

| Source | Upstream | Commit |
|---|---|---|
| `src/` (except `ansi.rs`), `tests/` | [alacritty/alacritty](https://github.com/alacritty/alacritty) `alacritty_terminal/`, tag `alacritty_terminal_v0.26.0` | `94e7c8874e526b1e67b349d9ba30ddf81669119e` |
| `src/ansi.rs` | [alacritty/vte](https://github.com/alacritty/vte) `src/ansi.rs`, tag `v0.15.0` | see tag |

The core `vte` parser is still used as a pinned dependency (`vte = "=0.15.0"`);
only its `ansi` layer is vendored, so OSC sequences it ignores can be handled here.

## Local changes

- `ansi.rs`: made std-only (removed the `no_std` variants) and switched to the
  external `vte` parser types.
- `term/mod.rs`: the secondary device attributes reply reports artty's version,
  so the test asserting an Alacritty-era version number was removed.
- `tty/unix.rs`: children get `ARTTY_SESSION_ID` instead of `ALACRITTY_WINDOW_ID`.
- `tty/unix.rs`: the login shell from the account database wins over `$SHELL`.
- `lib.rs`: dropped `cfg_attr(clippy, deny(warnings))` so newer clippy lints in upstream code are warnings, not errors.
- Shell integration and agent support: `ansi.rs` parses OSC 133 (marks), OSC 7
  (working directory), OSC 9 (notification, `9;4` progress), OSC 777 `notify`
  and single-part OSC 99; `Term` forwards them as new `Event` variants.
  `Grid::lines_scrolled` counts lines scrolled into history so marks have
  stable line numbers. Tests: `tests/artty.rs`.
- `term/mod.rs`: emoji after a zero-width joiner, and skin-tone modifiers, join the previous cell (grapheme clusters take one emoji's width, as in Ghostty and kitty). Test: `tests/artty.rs`.
- `ansi.rs`: `CursorShape` is serializable (screen snapshots sent from the artty server).
- `term/mod.rs`: fixes, each with a test in `tests/artty.rs`:
  - `push_keyboard_mode` removed the oldest entry from `title_stack` instead
    of `keyboard_mode_stack`; once that was empty it panicked (about 20KB of
    `CSI > 1 u`). Also fixed in [tty7's fork](https://github.com/l0ng-ai/tty7).
  - VS16 (U+FE0F) widens a one-cell base with an emoji presentation (❤️, ⚠️)
    to two cells, as in kitty and Ghostty.
  - `Term::resize` and `Term::new` enforce `MIN_COLUMNS`/`MIN_SCREEN_LINES`
    themselves (upstream leaves it to its window code): reflowing a wide
    character into one column looped forever, allocating a row each time.
  - `Term::reset_keyboard_modes` clears both screens' kitty keyboard stacks
    and flags; an OSC 133 prompt mark calls it, so a program killed without
    popping its modes can't garble the shell (idea from tty7).
- `event_loop.rs`: a panic while parsing output resets the terminal and says
  so on screen, instead of ending the reader thread (which froze the pane).
- `tests/artty.rs`: `random_input_never_panics` feeds seeded random bytes and
  escape sequences, in uneven chunks with resizes, and checks nothing panics
  or runs away (it found the one-column reflow loop).
- Server hand-over (a new artty server build takes over running sessions in
  place):
  - `term/snapshot.rs`, `grid/snapshot.rs`: `Term::snapshot`/`from_snapshot`
    carry a terminal's state as data (both screens and scrollback, trimmed of
    trailing blanks; cursors, modes, colors, titles, tab stops, keyboard
    modes). Cursor and charset types derive serde for it. Test:
    `tests/artty.rs`.
  - `tty/unix.rs`: `Pty::adopt` takes over a running child's PTY by
    descriptor and pid (waited for with `waitpid`; the hand-over `exec`s in
    place, so it's still our child). `Pty::child()` became `Pty::pid()`.
  - `event_loop.rs`: `Msg::Freeze` stops the reader with everything read
    parsed, until resumed or the process is replaced; a failed PTY read now
    reports `Event::Exit` instead of ending the reader silently.
- Images (artty), `term/image.rs`: iTerm2 inline images (`OSC 1337;File=`,
  and `MultipartFile`/`FilePart`/`FileEnd`), sized from `width`/`height`
  (cells, `px`, `%`, `auto`) and the cell size (`Term::set_cell_size`). An
  image lives in the cells it covers: `CellExtra` holds an `ImageTile` (an
  `Arc<Image>` and which cell of it), so scrolling, clearing, overwriting and
  the scrollback limit handle images like text, and the data goes with the
  last cell showing it. `Term::place_image` puts one at the cursor.
  Snapshots carry each image once (tiles serialize as ids). Tests:
  `tests/artty.rs`, `term::image`.
- `src/vte/`: vte's parser (`src/lib.rs`, `src/params.rs`, v0.15.0) is
  vendored, std build only (no 1KB OSC cap, no const generic), so that APC
  strings (`ESC _ ... ESC \`) are collected and passed to the new
  `Perform::apc_dispatch` instead of dropped (SOS and PM still are). The
  `vte` dependency is gone.
- `term/kitty.rs`: kitty's graphics protocol on top of the image cells:
  transmit (direct base64 in chunks, or a file; PNG, RGBA, RGB; zlib), show
  at the cursor (now or by id; size in cells; `C=1`), delete (on screen, by
  id, at the cursor; upper case frees), query, and replies honoring `q`.
  Images kept for later display are capped (oldest go first); a reset
  forgets them. Test: `tests/artty.rs`.
