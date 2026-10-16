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
