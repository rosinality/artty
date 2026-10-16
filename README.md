# artty

A GPU-accelerated macOS terminal for working with AI coding agents, written in
Rust with [GPUI](https://gpui.rs).

- A translucent window (blurred or clear) with a sidebar of tabs, each split
  into panes; as many windows as you like.
- The sidebar shows what each tab is doing: a running command and how long it
  has run, a coding agent working, waiting for you, or done, a failed exit code,
  and progress.
- Notifications when a tab needs you while you're elsewhere, and ⌘J to jump to it.
- An `artty` command that lets agents (or scripts) list tabs, read their
  output, run commands in other tabs and get the result, and report status.
- Sessions live in a background server, like tmux: closing a window or
  quitting the app (⌘Q) leaves them running, and reopening attaches again.
  `artty ps` / `artty top` show what runs where and what it costs.
- Tabs come back after a reboot or crash: directories, recent output, and
  the coding agents running in them, resumed in the same conversation.

## Build and run

```sh
./scripts/bundle.sh --install      # build target/release/artty.app and link
                                   # /Applications/artty.app to it (once)
./scripts/bundle.sh                # later builds update the installed app
artty restart                      # switch to the new build; tabs reopen

cargo run -p artty                 # or run from the tree
cargo run -p artty -- -e htop      # run a command in the first tab
cargo test --workspace
```

The app bundle gets the icon (`scripts/make-icon.py` redraws it) and native
notifications. A rebuild replaces the bundle atomically, so a running artty is
never modified underneath itself; it notices the new build and offers
"Restart to update". That restarts the app only — sessions keep running in
the server. When the server is from an older build too, the window offers to
restart it (artty → Restart Server), which reopens your tabs and resumes
agents, but running programs in them do restart. The server runs from its own
copy of the executable, so rebuilds never touch it. (A dev build run from
`target/` is killed by macOS when cargo rewrites it in place; its sessions
survive.) GPUI's Metal shaders compile at startup (`runtime_shaders`), so
the Xcode Metal toolchain isn't needed. `ARTTY_PROFILE=1` prints frame timings.

## Keys

| Keys | Action |
|---|---|
| ⌘N / ⌘T | New window / new tab (in the current directory) |
| ⌘, | Settings |
| ⌘D / ⌘⇧D | Split the pane right / down |
| ⌘W / ⌘⇧W | Close pane (the tab closes with its last pane) / close tab |
| ⌘⌥←↑→↓, ⌘[ / ⌘] | Move to the pane in that direction / previous, next pane |
| ⌘⇧↩ | Zoom the pane to fill the tab, and back |
| Drag a divider | Resize panes |
| ⌘1…⌘9, ⌘⇧[ / ⌘⇧] | Switch tabs |
| Double-click / drag a tab | Rename it / reorder it (also into another workspace) |
| ⌘⇧N | New workspace (name it, then Enter) |
| ⌘⇧O | Open a workspace: a hidden one, or one in another window |
| ⇧⌘A | Activity: every pane's processes, CPU and memory |
| ⌃⌘[ / ⌃⌘] | Previous / next workspace |
| Click / double-click / drag a workspace header | Fold it / rename it / reorder workspaces |
| Right-click (or ⌃-click) | In a pane: copy, paste, links, split, rename, clear, close. On a tab or workspace in the sidebar: rename, move to a new workspace, fold, close |
| ⌘J | Jump to the next tab that needs attention |
| ⌘C / ⌘V | Copy selection / paste |
| ⌘F, ⌘G / ⌘⇧G | Find (regex, all matches highlighted); next older / newer |
| ⌘= / ⌘- / ⌘0 | Font size |
| ⌘-click | Open a link (underlined while ⌘ is held) |
| Drag, double-, triple-click, ⇧-click | Select, select word, select line, extend |

Programs that use the mouse (vim, htop, tmux) get mouse events; hold ⇧ to select
text instead. Programs that ask for the kitty keyboard protocol (Claude Code,
fish, neovim) can tell Shift+Enter, Ctrl+I and Escape apart.

## Configuration

⌘, opens Settings for the common options (themes, font, cursor style, opacity and blur,
keyboard, bell, notifications, sessions). Everything lives in
`~/.config/artty/config.toml`, which Settings edits in place and every window
reloads when it changes; see [`config.example.toml`](config.example.toml).

## Agents

### Status in the sidebar

artty recognizes coding agents by their process name (`claude`, `codex`,
`gemini`, … — configurable; scripts run by node or python are named after the
script). Without help it can tell an agent is running and when it rings the
bell or sends a notification. For exact working / needs-input / done status,
let the agent report it through its hooks:

```sh
artty hooks status              # which agents have them
artty hooks install             # every installed agent, or name some: codex
artty hooks uninstall gemini    # remove artty's hooks (yours stay)
```

Settings › Agents shows the same per agent (installed, outdated, not
installed) with Install / Update / Uninstall buttons.

| Agent | Hooks file | Notes |
|---|---|---|
| Claude Code | `~/.claude/settings.json` | |
| Codex | `~/.codex/hooks.json` | approve them once with `/hooks` in Codex |
| Gemini CLI | `~/.gemini/settings.json` | |

The hooks call `artty status …` and do nothing outside artty. They also tell
artty the conversation id, which is what lets restored tabs resume the right
conversation. The files are backed up first; rerunning `install` upgrades
hooks from older versions.

## Sessions

The `artty server` process owns every terminal session; app windows attach
to it. A window's tabs are grouped into workspaces — named sections of the
sidebar, say one per project — which the server keeps as units of their own.
A section header shows what its tabs are doing (a waiting agent, a busy
command), even folded; a lone unnamed workspace is just a list of tabs.
A workspace's right-click menu moves it to a new window or another one, or
hides it: it keeps running out of sight, and "N more workspaces…" at the
bottom of the sidebar (or ⌘⇧O) brings it back.

Each workspace can have defaults of its own:

- **A root directory.** New tabs and splits start in the current pane's
  directory while that's inside the root, and in the root otherwise.
  `artty workspace new --cwd DIR` sets it; so does "Use This Directory as
  Root" in the header's menu.
- **A name.** Unnamed workspaces go by their git repository, or their
  directory: the root, or else the one their tab is in.
- **A color**, picked in the header's menu: a dot by its name and a tint on
  its selected tab.
- **Environment variables** for panes started in it. The app starts the server when needed, and the server exits once its
last session ends.

- Closing a window detaches it: its tabs keep running, and
  Shell → Reopen Closed Windows brings it back.
- ⌘Q quits the app and leaves everything running; the next launch reopens the
  windows. artty → Quit and End Sessions ends them instead.
- artty → Restart Server restarts it on the current build (see above).

```sh
artty ps                    # each pane: status, CPU, memory, process tree
artty ps --tab 3 --json
artty top                   # the same, live, busiest first
artty kill 3                # SIGTERM the program running in pane 3
artty kill 3 --signal INT   # or another signal; --pane closes the pane
artty workspace             # workspaces: id, window, tabs, status
artty workspace new blog --cwd ~/blog
artty workspace hide blog   # out of its window, still running
artty workspace open blog   # back into the frontmost window
artty workspace rename 3 notes
artty workspace set blog --root ~/blog --color teal --env NODE_ENV=development
artty workspace close blog  # end its tabs
artty server status         # pid, uptime, windows, panes, attached apps
artty server restart        # restart on this build; tabs and agents return
artty server stop           # end every session
```

**Activity** (⇧⌘A, the top of the sidebar, or `artty activity`) shows what
runs in every pane, live: workspaces, their panes and each pane's process
tree, with CPU and memory totals at every level. Click a column to sort,
type to filter, click a pane to go to it, and Stop a process (SIGTERM; ⌥
for SIGKILL; a pane's shell or agent asks first). It's a view, not a tab:
nothing runs in it, and it's in no workspace.

The server logs to `~/Library/Logs/artty/server.log`.

### Restoring tabs and resuming agents

The server saves your tabs every few seconds, and reopens them when it next
starts (after a reboot, a crash, or a restart): each tab's split layout, and each pane's directory, its last 1000 lines of output (with colors),
and the program it was opened with. A tab where an agent was running reopens
its shell and runs the agent's resume command — for Claude Code,
`claude --resume <id>` with the conversation id its hooks reported (or
`claude --continue` without hooks). Resuming opens the conversation; nothing
is sent until you type.

See `restore-sessions`, `restore-scrollback`, `resume-agents` and `[resume.*]` in
the config to change or turn this off.

### The `artty` command

Every pane gets `artty` on its `PATH`, plus `ARTTY_SOCKET` and
`ARTTY_SESSION_ID`, so an agent can drive other panes. Ids name panes; a tab
holds one or more.

```sh
artty list                                  # panes with id, tab, status, title
artty new --title server --background -- npm run dev
artty new --split right -- npm run dev      # a pane next to your own
artty new --workspace blog --cwd ~/blog     # a tab in workspace "blog" (made if needed)
artty run --tab 3 -- cargo test             # prints output, exits with its code
artty read --tab 3 --lines 50               # last 50 lines
artty read --tab 3 --last                   # last command's output
artty send --tab 3 "q"                      # type into a tab
artty notify "Build finished"
artty status waiting --message "Need a decision on the API"
```

Outside artty, `artty` talks to the running server. `artty help` lists
everything.

### Shell integration

`artty run`, `read --last`, command status, and "finished" notifications
use OSC 133 marks from the shell. fish 4 sends them natively; artty sets up zsh
and bash automatically (your startup files still load as usual). Elsewhere —
over ssh, say — `artty shell-integration zsh|bash` prints a script to source.

## Layout

| Crate | |
|---|---|
| `crates/artty-vt` | Terminal emulation: a fork of `alacritty_terminal` plus vte's escape-code layer, extended with shell-integration, notification, working-directory and progress escape codes. See [`UPSTREAM.md`](crates/artty-vt/UPSTREAM.md). |
| `crates/artty-core` | Sessions (PTY, process inspection, command tracking, search, links), config, the control protocol, and render snapshots. |
| `crates/artty-server` | The server that owns sessions, windows' tabs and layouts, saving and restoring them. |
| `crates/artty-app` | The GPUI app (a client of the server) and the `artty` command. |
