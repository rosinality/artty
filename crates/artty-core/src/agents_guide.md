# Working with other agents in artty

<!-- Written by artty (`artty guide` prints it). Edits here are overwritten. -->

You run in a pane of artty, a terminal; other coding agents (Claude Code,
Codex, Gemini CLI, Antigravity CLI) may run in its other panes, each in its own session. Your
own pane's id is in `$ARTTY_SESSION_ID`.

## Commands

- `artty agents` lists the agents on this machine: pane id, agent, state
  (working, waiting, done, idle), and workspace · tab title. `--json` for
  more (folder, unread replies, pending permission requests, its
  conversation's `session` id, and the `parent` pane that started it).
- `artty ask --tab N --timeout 100 "message"` gives agent N the message once
  its current turn ends, typed into its terminal as if the user typed it
  (it doesn't share your context), and prints its reply: its last message
  of that turn.
  - Not answered within the timeout: it prints the ask's id and exits with
    status 3. The ask goes on; `artty ask --result ID --timeout 100` waits
    for the reply again (`--timeout 0`: just say where it is). Your shell
    tool may stop a command sooner (Claude Code's Bash tool: after 2
    minutes, by default).
  - `--no-wait` prints the id at once; `artty ask --result ID` later.
  - `-` reads the message from stdin: `artty ask --tab N - < notes.md`.
  - A message starting with `/`, `!` or `#` is refused (the agent would take
    it as a command); `--raw` sends it anyway.
  - Nothing is typed twice: a failed ask says why, and isn't retried.
- `artty ask --list [--tab N]` lists asks not yet answered;
  `artty ask --cancel ID` takes back one not yet given to its agent.
- `artty wait ID... [--timeout S]` waits for the first of several asks to be
  over (`--all`: every one) and prints each one's answer or state; exit 3 if
  what it waited for didn't come in time.
- `artty events --follow [--tab N]` prints every agent's thread entries as
  they come (asks, replies, permission requests, files changed), a JSON
  object a line with its `pane` and `seq`; `artty events --after SEQ` prints
  those after one, and exits.
- `artty thread --tab N --lines 20` shows agent N's last 20 thread entries:
  what it was asked (and by whom), what it answered, its permission
  requests, the files it changed. `artty thread --search TEXT` searches
  every agent's thread (newest matches first; `--tab N` for one).
- `artty new --background --agent claude --cwd DIR --task "..."` starts an
  agent (codex, gemini, agy too) in a new tab with that task; it prints the new
  pane's id, and the task's ask id on stderr. With `--worktree BRANCH`, it
  runs in a git worktree of the repository, `REPO.worktrees/BRANCH` on that
  branch; the worktree is removed when its tab closes if it has no changes
  (the branch stays). How deep agents may start agents is the user's
  setting.
  Started with a task, Claude Code runs without its terminal UI (through
  its Agent Client Protocol adapter; the user's setting); asks and threads
  work the same.
- `artty new --background --agent claude --fork SESSION --task "..."` starts
  Claude Code on a copy of conversation SESSION (everything said in it so
  far), in its folder: the conversation itself isn't changed, and its agent
  isn't interrupted. The copy is a tab of its own (`artty close N` once
  you're done with it).
- `artty when --tab N [--failed] [--every] "note"` hands the command
  running in pane N to you once it finishes (`--failed`: only if it fails;
  `--every` time, not once): you're asked, as `artty ask` does, with what
  ran, how it ended, how long it took, its last lines, and the note. Start
  something long in another tab (a training run, a build), set this, and
  get on with other work. `--ask TAB` hands it to another agent, `--new
  [--agent NAME]` to a new one; `artty when --list`, `--cancel ID`. It
  needs that tab's shell integration (which says a command ended).
- `artty sessions` lists agents' past conversations on this machine (not
  those going on in panes): id, agent, when, folder, title;
  `--search TEXT` finds those mentioning TEXT. `artty sessions ID` shows
  what was said in one.

## Building on what other agents worked out

Another agent may already have worked out what you need: a method, a fix,
a decision. Look before working it out again (`artty thread --search`,
`artty sessions --search`), then go to where it was worked out:

- Its agent is running: ask it (`artty ask --tab N`).
- It's over, or its agent is busy with something else: read it
  (`artty sessions ID`); if what you need isn't written there, ask a copy
  of it (`--fork ID --task "..."` above), which answers with the whole
  conversation in mind. (A copy reads the whole conversation again: it
  costs more than an ask.)
- When you work out something others could use, say so in your reply. If
  you write it down (a note, a skill), say which conversation it came from
  (yours: the `session` of pane `$ARTTY_SESSION_ID` in
  `artty agents --json`), so a reader can ask there for more.

## What to know

- Asking your own pane is refused: it would wait for the turn you're in.
- Two agents each waiting on the other's answer both stall until their
  timeouts.
- Permission requests are the user's to answer: `artty approval` is refused
  from inside artty's panes.
- The user sees every ask, who it's from, and the files each agent changes,
  in artty (its sidebar and the thread panel beside each agent's terminal);
  two agents changing one file are pointed out.
