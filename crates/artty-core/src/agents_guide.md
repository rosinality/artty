# Working with other agents in artty

<!-- Written by artty (`artty guide` prints it). Edits here are overwritten. -->

You run in a pane of artty, a terminal; other coding agents (Claude Code,
Codex, Gemini CLI) may run in its other panes, each in its own session. You
can ask one of them something and get its answer. Your own pane's id is in
`$ARTTY_SESSION_ID`.

## Commands

- `artty agents` lists the agents on this machine: pane id, agent, state
  (working, waiting, done, idle), and workspace · tab title. `--json` for
  more (folder, unread replies, pending permission requests).
- `artty ask --tab N --timeout 100 "message"` gives agent N your message
  once its current turn ends (it's typed into its terminal, as if the user
  typed it) and prints its reply: its last message of the turn.
  - Not answered within the timeout: it prints the ask's id and exits with
    status 3. The ask goes on; `artty ask --result ID --timeout 100` waits
    for the reply again (`--timeout 0`: just say where it is).
  - `--no-wait` prints the id at once, for something long: carry on, and
    check back with `artty ask --result ID`.
  - `-` reads the message from stdin, for a long one:
    `artty ask --tab N - < notes.md`.
  - A message starting with `/`, `!` or `#` would be taken as a command, and
    is refused (`--raw` sends it anyway: only when the user asks for that).
- `artty ask --list [--tab N]` lists asks not yet answered;
  `artty ask --cancel ID` takes back one not yet given to its agent.
- `artty thread --tab N --lines 20` shows agent N's last 20 thread entries:
  what it was asked (by whom), what it answered, its permission requests.

## How to ask

- Say who you are and what you need, and make the message stand on its own:
  the other agent doesn't share your context or your conversation. Name
  files, branches and commands rather than "the bug" or "my change".
- Ask for what you need back ("reply with the failing test's name"), so its
  answer is useful to you as it comes.
- Keep `--timeout` within your shell tool's own limit (Claude Code's Bash
  tool stops a command after 2 minutes, by default). A busy agent answers
  only once its current work is done: for anything that may take long, use
  `--no-wait` and check back.
- One ask at a time per agent; check `artty ask --list` before asking again.
  Nothing is typed twice: a failed ask says why, and isn't retried for you.

## Rules

- Never ask your own pane (`$ARTTY_SESSION_ID`): it's refused, as it would
  wait for the very turn you're in.
- Don't wait on an agent that may be waiting on you (two agents asking each
  other with a timeout both stall): use `--no-wait` when in doubt.
- Don't type into other agents' terminals with `artty send`: `artty ask`
  waits for their turn, and gets their answer.
- Don't start long chains of agents asking agents. When the user should
  decide something, ask the user instead.
- Permission requests (yours or others') are answered by the user, not by
  agents: `artty approval` is refused from inside artty's panes.
- The user sees every ask in the agents' threads (artty's Agents view), as
  from your tab.
