# artty

A GPU-accelerated macOS terminal for working with AI coding agents, written in
Rust with [GPUI](https://gpui.rs).

- A translucent, blurred window with a sidebar of tabs, each split into panes.
- The sidebar shows what each tab is doing: a running command and how long it
  has run, a coding agent working, waiting for you, or done, a failed exit code,
  and progress.
- Notifications when a tab needs you while you're elsewhere, and ⌘J to jump to it.
- An `artty` command that lets agents (or scripts) list tabs, read their
  output, run commands in other tabs and get the result, and report status.
- Tabs come back after a restart or crash: directories, recent output, and
  the coding agents running in them, resumed in the same conversation.

## Build and run

```sh
cargo run -p artty                 # the app
cargo run -p artty -- -e htop      # run a command in the first tab
cargo test --workspace
```

GPUI's Metal shaders compile at startup (`runtime_shaders`), so the Xcode Metal
toolchain isn't needed for development.

## Keys

| Keys | Action |
|---|---|
| ⌘T | New tab (in the current directory) |
| ⌘D / ⌘⇧D | Split the pane right / down |
| ⌘W / ⌘⇧W | Close pane (the tab closes with its last pane) / close tab |
| ⌘⌥←↑→↓, ⌘[ / ⌘] | Move to the pane in that direction / previous, next pane |
| ⌘⇧↩ | Zoom the pane to fill the tab, and back |
| Drag a divider | Resize panes |
| ⌘1…⌘9, ⌘⇧[ / ⌘⇧] | Switch tabs |
| ⌘J | Jump to the next tab that needs attention |
| ⌘C / ⌘V | Copy selection / paste |
| ⌘F, ⌘G / ⌘⇧G | Find (regex); next older / newer match |
| ⌘= / ⌘- / ⌘0 | Font size |
| ⌘-click | Open a link |
| Drag, double-, triple-click, ⇧-click | Select, select word, select line, extend |

Programs that use the mouse (vim, htop, tmux) get mouse events; hold ⇧ to select
text instead.

## Configuration

`~/.config/artty/config.toml`, reloaded when saved. See
[`config.example.toml`](config.example.toml) for every setting: shell, fonts,
opacity and blur, colors, Option as Meta, cursor blink, agent names, and
notifications.

## Agents

### Status in the sidebar

artty recognizes coding agents by their process name (`claude`, `codex`,
`gemini`, … — configurable). Without help it can tell an agent is running and
when it rings the bell or sends a notification. For exact
working / needs-input / done status, let the agent report it. For Claude Code:

```sh
artty hooks            # print the hooks
artty hooks install    # add them to ~/.claude/settings.json (backs it up first)
```

The hooks call `artty status …` and do nothing outside artty. They also tell
artty the conversation id, which is what lets restored tabs resume the right
conversation. Rerunning `artty hooks install` upgrades hooks from older
versions.

### Restoring tabs and resuming agents

artty saves your tabs every few seconds and on quit, and reopens them on the
next launch: each tab's split layout, and each pane's directory, its last 1000 lines of output (with colors),
and the program it was opened with. A tab where an agent was running reopens
its shell and runs the agent's resume command — for Claude Code,
`claude --resume <id>` with the conversation id its hooks reported (or
`claude --continue` without hooks). Resuming opens the conversation; nothing
is sent until you type.

Only the first artty instance saves and restores; later ones start fresh. See
`restore-sessions`, `restore-scrollback`, `resume-agents` and `[resume.*]` in
the config to change or turn this off.

### The `artty` command

Every pane gets `artty` on its `PATH`, plus `ARTTY_SOCKET` and
`ARTTY_SESSION_ID`, so an agent can drive other panes. Ids name panes; a tab
holds one or more.

```sh
artty list                                  # panes with id, tab, status, title
artty new --title server --background -- npm run dev
artty new --split right -- npm run dev      # a pane next to your own
artty run --tab 3 -- cargo test             # prints output, exits with its code
artty read --tab 3 --lines 50               # last 50 lines
artty read --tab 3 --last                   # last command's output
artty send --tab 3 "q"                      # type into a tab
artty notify "Build finished"
artty status waiting --message "Need a decision on the API"
```

Outside artty, `artty` finds the most recently started instance. `artty help`
lists everything.

### Shell integration

`artty run`, `read --last`, command status, and "finished" notifications
use OSC 133 marks from the shell. fish 4 sends them natively. zsh and bash
need a prompt hook that emits them (automatic setup isn't built yet). Without
them, artty falls back to watching the foreground process.

## Layout

| Crate | |
|---|---|
| `crates/artty-vt` | Terminal emulation: a fork of `alacritty_terminal` plus vte's escape-code layer, extended with shell-integration, notification, working-directory and progress escape codes. See [`UPSTREAM.md`](crates/artty-vt/UPSTREAM.md). |
| `crates/artty-core` | Sessions (PTY, process inspection, command tracking, search, links), config, the control protocol, and render snapshots. |
| `crates/artty-app` | The GPUI app and the `artty` command. |
