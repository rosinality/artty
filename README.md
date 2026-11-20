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

The app bundle gets the icon (`scripts/make-icon.sh` renders it from
`assets/icon.svg`) and native notifications. A rebuild replaces the bundle
atomically, so a running artty is never modified underneath itself; it notices the new build and offers
"Restart to update". That restarts the app only — sessions keep running in
the server. When the server is from an older build too, the window offers to
move it to the new build (artty → Restart Server): the server hands every
session over to the new build in place, so shells, agents and whatever runs
in them carry on, screens and scrollback included. (A server from before
hand-overs existed restarts instead: tabs reopen and agents resume, but
running programs restart.) The server runs from its own
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
| Drag a divider, ⌃⌘←↑→↓ | Resize panes (move the divider that way); ⌃⌘= makes them even |
| ⌃⌘⇧←↑→↓ | Swap the pane with the one in that direction |
| Drag a pane's line in the sidebar | Move the pane into another tab ("Move Pane to New Tab" in the Window menu takes it out) |
| ⌘⌥I | Type into every pane of the tab at once, and back |
| ⌘⇧T | Reopen the last closed tab (directories, recent output, agents) |
| ⌘1…⌘9, ⌘⇧[ / ⌘⇧] | Switch tabs (hold ⌘ to see their numbers) |
| Double-click / drag a tab | Rename it / reorder it (also into another workspace) |
| ⌘⇧N | New workspace (name it, then Enter) |
| ⌘⇧O | Open a workspace: a hidden one, or one in another window |
| ⇧⌘A | Activity: every pane's processes, CPU and memory |
| ⇧⌘K | Agents: every agent on every server, and the chosen one's thread (↑↓ choose, Enter shows its terminal) |
| ⇧⌘E | Files: the focused pane's folder, on its machine |
| ⌘⇧F | Search everything: what every pane on every server printed (history too), and Claude Code's and Codex's conversations on those machines (✳ Claude Code, ◇ Codex; matches highlighted); a line goes to its pane, selected, a conversation carries on in a new tab |
| ⌘P / ⌘⇧P | Go to any pane on any server (fuzzy: title, workspace, folder, program, agent) / run any command (`>` in ⌘P) |
| ⌘B | Fold the sidebar away, and back (folded, the titlebar says how many tabs need you; a click goes there) |
| Settings > Workspaces in the sidebar | List (every workspace with its tabs), Accordion (only the current one open; the others a line each, with what needs you there; a click goes to one), or Switcher (only the current one's tabs, under a button listing the others by server, with what needs you in each) |
| ⌃⌘[ / ⌃⌘] | Previous / next workspace |
| Click a workspace header's arrow / the header / double-click / drag | Fold it / go to it / rename it (a click elsewhere saves the name) / reorder workspaces |
| Click a server header's dot / right-click the header | Fold its workspaces away / connect or disconnect, reconnect from scratch |
| Right-click (or ⌃-click) | In a pane: copy, paste, links, split, rename, clear, close. On a tab or workspace in the sidebar: rename, move to a new workspace, fold, close |
| ⌘J | Jump to the next tab that needs attention |
| ⌘C / ⌘V | Copy selection / paste (several lines into a program that would run them one by one asks first) |
| ⌘↑ / ⌘↓ | Scroll to the previous / next shell prompt |
| ⌘⌥C | Copy the output of the command in view (or the last one) |
| ⌘⇧C | Copy mode: move with vi keys (hjkl, w b e, 0 $, gg G, ⌃U ⌃D, [ ] for prompts), select with v / V / ⌃V, copy with y, leave with esc |
| Drop files on a pane | Type their paths, quoted |
| ⌘F, ⌘G / ⌘⇧G | Find (regex, all matches highlighted, "3 of 17"); next older / newer. A full-screen agent (Claude Code) keeps its history itself, out of the terminal's reach: Find offers to search its conversations (⌘⇧F) instead |
| ⌘= / ⌘- / ⌘0 | Font size |
| ⌘-click | Open a link (underlined while ⌘ is held) |
| Drag, double-, triple-click, ⇧-click | Select, select word, select line, extend |

Prompt jumps and copying a command's output need shell integration (fish has
it built in; artty sets up zsh and bash). Secure Keyboard Entry, in the artty
menu or Settings, keeps other apps from reading what you type while artty is
in front.

Programs that use the mouse (vim, htop, tmux) get mouse events; hold ⇧ to select
text instead. Programs that ask for the kitty keyboard protocol (Claude Code,
fish, neovim) can tell Shift+Enter, Ctrl+I and Escape apart.

## Configuration

⌘, opens Settings for the common options, in pages: Appearance (themes,
font, cursor style, opacity and blur), Behavior, Keys (your bindings; record
a new one — several keys make a sequence — and choose an action or text to
type), Sessions, Machines and Agents. A theme can come from another
terminal: Import… (or drop the file on Settings) reads iTerm2's
`.itermcolors`, Ghostty's, kitty's, Alacritty's, WezTerm's and Windows
Terminal's; with Ghostty installed, its few hundred themes are listed, to
try with a click (light ones marked), and its
config's theme and key bindings come over with a click each (Ghostty's
actions as artty's, where it has them). Everything lives in
`~/.config/artty/config.toml`, which Settings edits in place and every window
reloads when it changes; see [`config.example.toml`](config.example.toml).
Its `[keys]` table rebinds keys: `"cmd-shift-t" = "new-tab"`, `"cmd-t" =
"none"` to take a default away, or `"shift-enter" = { send = "..." }` to
type text; the example lists every action. Keys can come in sequences —
`"cmd-e 1" = { send = "è" }`, or as Ghostty writes it, `"cmd+e>1" =
"text:è"` — which wait for their next key as long as it takes
(`key-sequence-timeout` sets a limit), the keys so far shown at the bottom
of the window meanwhile.

### A status line

`[status]` gives every window a status line, which the server works out and
every app shows the same: at the bottom of the Mac's sidebar, under the tab
bar on a phone, and on `artty attach`'s status line (after the tabs, and on
the right). Templates as tmux's, with the focused pane's values and your
commands' output:

```toml
[status]
left = "{cwd}"
right = "{git.branch}{git.dirty} #[fg=yellow]⚑{agents.waiting} #[default]{cmd:gpu} {time:%H:%M}"

[status.commands.gpu]
run = "nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader"
every = "5s"      # 10s if not said
timeout = "2s"    # 2s if not said; a failing or slow command shows a dim ⚠
```

The values: `host`, `user`, `cwd`, `git.branch`, `git.dirty` (`*`),
`command`, `exit` (when not 0), `duration`, `agent`, `agents.waiting`,
`progress`, `ports`, `time:FORMAT` (strftime's `%H %M %S %d %e %m %Y %y %a %b
%I %p`) and `cmd:NAME`. `#[fg=COLOR]`, `#[bg=COLOR]`, `#[bold]`, `#[dim]` and
`#[default]` style what follows. A word whose values are all empty is left
out with its spaces, so `⚑{agents.waiting}` shows only when an agent waits.
Commands run in the background, in the focused pane's directory (with
`ARTTY_CWD` and `ARTTY_PANE`), on the server's machine; their output's first
line is kept between runs. Without `[status]` there's no status line. Plain
ssh machines (without artty) have none: it's their server's config that says.

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
command), even folded. There's always at least one workspace; the + by
WORKSPACES adds another. A tab split into panes lists them, a line each
(`list-panes = false` shows only the focused one).
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
- artty → Restart Server moves it to the current build without ending
  anything (see above).

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
artty server restart        # move to this build; programs keep running
artty server stop           # end every session
```

**Activity** (⇧⌘A, the top of the sidebar, or `artty activity`) shows what
runs in every pane, live: workspaces, their panes and each pane's process
tree, with CPU and memory totals at every level and a graph of the last
minute (the server samples once a second, so the graphs are full when you
open it), plus what the server itself uses. Hovering a workspace header in
the sidebar shows its totals too. Click a column to sort,
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
artty ask --tab 4 "Review the diff in api/"  # the agent's reply, once its turn ends
artty ask --tab 4 --no-wait - < task.md     # prints the ask's id; carries on
artty ask --result 7 --timeout 60           # wait for ask 7's reply
artty notify "Build finished"
artty status waiting --message "Need a decision on the API"
```

`artty ask` gives an agent a message as if you typed it, but only once it's
between turns (by its hooks' events), nobody has typed in its pane for a few
seconds, and never into a shell: whether the agent is still in the
foreground is checked again just before the paste and before the Enter. The
reply is what its `Stop` hook reports as its last message (an interrupted
turn's reply is what it got to). A message the agent doesn't take fails
rather than being typed again; one queued behind a turn waits for it
(`--cancel ID` takes it back). An agent just started is waited for, up to
30 seconds, until its hooks report (Claude Code runs none while it asks
whether to trust a folder). It needs the agent's hooks (`artty hooks
install`). Asks are kept across a hand-over to a new build (Restart Server
with this build, `artty server restart`), and an `artty ask` waiting for a
reply gets it. A server that stops and starts again (a reboot, a crash,
the app's Restart Server from an older build) keeps its queued asks for the
reopened tab's resumed agent; an agent stopped mid-turn can't answer, and
its ask fails (it isn't sent again). Finished asks are kept for an hour.

### Permission requests

With its hooks installed, Claude Code's permission prompts come to artty
too: its `PermissionRequest` hook (`artty approve`) hands artty the
request while Claude Code shows its prompt as usual, so either answers it.

```sh
artty approvals                       # pending first: id, pane, state, command,
                                      # and the agent's "always" options
artty approval allow 1795129609022
artty approval allow 1795129609022 --always 0   # with the agent's option 0
artty approval deny 1795129609022 --message "Use the staging database"
```

"Always" offers only what the agent itself suggests with the request
(Claude Code's `permission_suggestions`, such as allowing a folder for the
session), passed back as it is: never more than its own prompt would.

### Agents and their threads

The server keeps each agent's thread: what it was given (by
`artty ask`, or typed in its terminal), what it said at the end of each
turn, its permission requests and how they went, and its sessions starting,
resuming, compacting and ending. The Agents view (⇧⌘K, or Agents in the
sidebar) lists every agent on every connected server with what it's doing,
shows the chosen one's thread (a pending permission request is answered
there too), and what's known about it. Threads are kept on disk (an
`agents.db` beside the saved tabs, readable by you only; the last 1000
entries of each): a tab reopened after a restart takes its thread back. A
thread goes when its pane is closed. Unread replies (the queue's) don't
outlast a restart yet.

Below the thread, write to the agent: ⏎ sends it as an `artty ask` (given
to the agent once its turn ends; until then it shows as queued), ⇧⏎ starts
a new line, Esc clears. Begin with `@tab` — the agent's tab title, spaces
left out — to send it to another agent instead, or several (`@api @web
…`); typing `@` lists them, and ⇥ completes. ↑↓ choose an agent while
nothing's written.

```sh
artty agents                          # pane, agent, state, workspace · tab
artty thread --tab 4 --lines 20       # its last 20 entries
```

### The queue

What needs you, from every agent on the server: permission requests,
agents waiting for you otherwise, and replies you haven't read (each
finished turn's last message, with the prompt it answers). A reply is read
once you look at its pane, or when dismissed. Apps are sent it as it
changes; the command shows it too:

```sh
artty queue                           # id, pane, kind, what it's about
artty queue --dismiss 1795132310737   # mark a reply read
```

An agent mustn't approve its own requests: these (and `artty send` into a
pane asking permission) are refused from programs running in artty's panes,
apps attached from one included. Answer from a terminal outside artty (or
over ssh), or at the agent's prompt. (A program that left its parent,
daemonized, can't be told apart.) artty knows a request answered at the
prompt: allowed (the tool's use is reported, once the tool has run), or
dismissed (Claude Code ends the hook), and an `artty ask` whose turn
stopped there fails as interrupted. One nobody answers in artty for 10
minutes is left to the prompt; an agent that can't show one (`claude -p`)
then denies it, as it would have at once. Codex's permission requests are
shown, not answered.

Outside artty, `artty` talks to the running server. `artty help` lists
everything.

### Shell integration

`artty run`, `read --last`, command status, and "finished" notifications
use OSC 133 marks from the shell. fish 4 sends them natively; artty sets up zsh
and bash automatically (your startup files still load as usual). Elsewhere —
over ssh, say — `artty shell-integration zsh|bash` prints a script to source.

## Other machines

artty can show other machines' servers beside this Mac's, in the same
window: each server's workspaces go under its name in the sidebar, and its
tabs work as local ones do — splits, agents' status, notifications, images,
the `artty` command inside them. Add one in Settings (Other machines), with
`artty remote add devbox --ssh me@devbox` (both check the connection first),
or in the config:

```toml
[[servers]]
name = "devbox"
ssh = "me@devbox"
```

The connection is ssh running `artty server --stdio` there (ssh uses your
config, keys and agent). Everything artty does there — the connection,
forwards, installs — shares one ssh connection per machine (OpenSSH's
ControlMaster, its socket in `~/.ssh/artty`; it lasts ten minutes past its
last use), so connecting again is quick, and a machine that wants a password, a
passphrase, a code, or a yes to its key asks once, in the window: ssh hands
artty its questions (as `SSH_ASKPASS`), and a sheet asks them — typing
there is secure keyboard entry, and a password or passphrase can be
remembered in the Keychain (tried once; asked again if it's refused).
(After the connection is lost — a network gone — the shared connection is
closed too, so such a machine asks again.) `artty remote test` and
`install` ask on the terminal instead. On Linux, artty is the server
and the `artty` command (the app is macOS only), and this app installs it:
when artty isn't there, or runs another build, the server's header says so
— click it (or run `artty remote install NAME`) and the app copies its build
for that machine there by ssh (into `~/.local/bin`; Linux on x86_64 or
aarch64, with glibc 2.28 or later — any distribution from 2018 on), moves a
running server onto it (programs keep running), and connects. The app
carries those builds when `scripts/bundle.sh` makes it with `brew install
zig`, `cargo install cargo-zigbuild` and `rustup target add
x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu`. From the source,
`./scripts/remote-install.sh DESTINATION` does the same (with
`--build-there`, it builds there with that machine's cargo instead).
Like this Mac's, that server keeps its sessions running on its own, and it
sends screens rather than a stream of output, so a dropped connection —
sleep, a new network — loses nothing: artty reconnects with backoff (a
click on the server's header tries at once) and shows the same tabs again.
Tabs and panes stay on their server (they can't be
dragged to another); Activity and the workspace picker (⌘⇧O) show every
server's.

Panes on another machine still reach this Mac: files dropped on one are sent
through the connection (into `~/.cache/artty/uploads` there, kept a week)
and their paths typed; a `file://` link to a file there (⌘-click, e.g. from
`ls --hyperlink`) is fetched and opened here, marked as downloaded; what a
program copies (OSC 52) lands on this Mac's clipboard; and web pages it
opens — `artty open URL`, which panes there have as `$BROWSER` — open in
this Mac's browser (web pages only). There, artty keeps its socket and
scripts in `/tmp/artty-UID`, made private (other users can't take its
place, as anyone can make files in `/tmp`).

Files (⌘⇧E): a drawer over the terminal's right edge lists the focused
pane's current directory — on whichever machine the pane is — and follows
it as the shell moves (open a folder in it to stay there; **Pane** goes back).
Double-click a file to open it here; another machine's opens to edit: it's
fetched into a folder of this Mac's and opened in its app, and each save
goes back, replacing the file there if that hasn't changed since (else you're
asked). **Download** puts one in ~/Downloads (**Reveal**, for this Mac's),
**Copy Path** copies where it is; drop files on the drawer to put them in
the folder it shows — one whose name is there asks whether to replace it,
keep both, or skip. The server watches the folder shown (inotify; kqueue on
macOS) and tells the app when it changes, or when the pane's shell moves to
another, so the drawer keeps up within a moment; it also looks again every
30 seconds, for changes a watch can't see (a network filesystem's, from
other machines), and every two where watching isn't to be had. ⟳ looks at
once. It lies over the
terminal instead of narrowing it, so nothing running there resizes.

Ports: when a program in a pane listens on a TCP port (a dev server, say),
the tab shows it (`:3000`; on Linux, where the server finds them). Click it
to open it in the browser — another machine's is forwarded here first, by
ssh, on the same port when it's free (`3000 → 3001` when it isn't); × stops
forwarding. Forwards end when the app quits (or loses the connection). `auto-forward-ports = true` forwards
each as it starts listening. Activity shows them by process, and `artty ps`
lists them.

Slow and flaky links: the server holds back screen updates while two
batches are unacknowledged, then sends the newest screens — so a flood of
output never queues up ahead of what you type (over a 100 ms link, a key
still comes back in 100 ms while `yes` runs), and ssh compresses what goes
through. The app pings each server every 5 s: 15 s of silence, or no answer
within 4 s after the Mac wakes or its network changes, and it reconnects
(ssh alone can take minutes to notice a dead network). What you type shows
at once, before the echo comes back, as mosh does (`local-echo`; checked
against what the server shows, and never shown where nothing echoes, such as
a password prompt). Activity shows each remote's round trip and traffic
over the last minute, how long it's been connected, and what last went
wrong. Panes there use this Mac's theme and cursor settings.

## Images

Programs can show images in the terminal, with kitty's graphics protocol
(`kitten icat`, yazi, timg, image.nvim, ...), iTerm2's inline images
(`imgcat`, and the many tools that support it) or sixel (img2sixel, chafa,
lsix, matplotlib backends): an image takes up the cells it covers, scrolls
with the text, and goes when cleared or overwritten. Cell sizes are
reported in screen pixels, so programs that size images to the window send
them sharp on Retina displays.

Inside tmux, images work through kitty's Unicode placeholders
(`kitten icat --unicode-placeholder`, and tools built on it): the image is passed through to
artty once and tmux keeps only special characters standing for it, so it
moves with tmux's panes, windows and scrollback. tmux needs
`set -g allow-passthrough on`.

## Performance

`./scripts/bench.sh [io|fire|mem|all] [kitty alacritty ghostty]` measures
artty, and any of those terminals that are installed, at the same grid:
`cat` of an 11 MB text file, `cat` of 25 MB of colorful full-screen frames
(every cell's background changing, like a fire animation), and memory at
idle. artty runs isolated from your own server and settings. The method
follows moktavizen/terminal-benchmark, as tty7's harness does.

On an Apple Silicon Mac (2026-10), against kitty 0.49:

| | artty | kitty |
|---|---|---|
| 11 MB text | 80 ms | 79 ms |
| 25 MB of colorful frames | 188 ms | 169 ms |
| Memory at idle | 147 MB (app + server) | 159 MB |

Plain text is bound by the macOS PTY for both. Animations draw at 120 fps
(the server's cap), about 1 ms per frame; a pane with 10,000 lines of
scrollback costs the server about 6 MB.

## Layout

| Crate | |
|---|---|
| `crates/artty-vt` | Terminal emulation: a fork of `alacritty_terminal` plus vte's escape-code layer, extended with shell-integration, notification, working-directory and progress escape codes. See [`UPSTREAM.md`](crates/artty-vt/UPSTREAM.md). |
| `crates/artty-core` | Sessions (PTY, process inspection, command tracking, search, links), config, the control protocol, and render snapshots. |
| `crates/artty-server` | The server that owns sessions, windows' tabs and layouts, saving and restoring them. |
| `crates/artty-app` | The GPUI app (a client of the server) and the `artty` command. |

## License

artty is licensed under the [Apache License 2.0](LICENSE). It builds on:

- [alacritty_terminal](https://github.com/alacritty/alacritty) and
  [vte](https://github.com/alacritty/vte) (Apache-2.0, The Alacritty
  Project): forked as `crates/artty-vt`; the changes are listed in its
  [UPSTREAM.md](crates/artty-vt/UPSTREAM.md).
- [GPUI](https://github.com/zed-industries/zed) 0.2.2 (Apache-2.0, Zed
  Industries): vendored in `vendor/gpui` with one change, described in its
  [ARTTY.md](vendor/gpui/ARTTY.md).
