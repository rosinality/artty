# Working with other agents in artty

<!-- Written by artty (`artty guide` prints it). Edits here are overwritten. -->

You run in a pane of artty, a terminal; other coding agents (Claude Code,
Codex, Gemini CLI) may run in its other panes, each in its own session. Your
own pane's id is in `$ARTTY_SESSION_ID`.

## Commands

- `artty agents` lists the agents on this machine: pane id, agent, state
  (working, waiting, done, idle), and workspace · tab title. `--json` for
  more (folder, unread replies, pending permission requests).
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
  requests, the files it changed.
- `artty new --background --agent claude --cwd DIR --task "..."` starts an
  agent (codex, gemini too) in a new tab with that task; it prints the new
  pane's id, and the task's ask id on stderr. With `--worktree BRANCH`, it
  runs in a git worktree of the repository, `REPO.worktrees/BRANCH` on that
  branch; the worktree is removed when its tab closes if it has no changes
  (the branch stays). How deep agents may start agents is the user's
  setting.

## What to know

- Asking your own pane is refused: it would wait for the turn you're in.
- Two agents each waiting on the other's answer both stall until their
  timeouts.
- Permission requests are the user's to answer: `artty approval` is refused
  from inside artty's panes.
- The user sees every ask, who it's from, and the files each agent changes,
  in artty's Agents view; two agents changing one file are pointed out.
