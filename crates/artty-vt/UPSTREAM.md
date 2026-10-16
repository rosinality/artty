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
