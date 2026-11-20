"""A stand-in coding agent for tests/ask.rs: a TUI-ish prompt that takes
bracketed pastes, and reports its turns through `artty status --hook` with
the hook events Claude Code sends (UserPromptSubmit, Stop with
last_assistant_message). Its reply to a message is "echo: " and the message.

A message "tool: CMD" asks permission to run CMD as Claude Code does: its
PermissionRequest hook (`artty approve --hook`) runs while it shows its own
prompt. The hook's decision answers it; so does "1" (yes: the hook is left
running, and PostToolUse is reported) or "4" (no: the hook is ended, and the
turn stops with no Stop, as Claude Code's does) typed at the prompt.

Usage: fake_agent.py ARTTY [DELAY [TURNS]]  (DELAY: seconds a turn takes;
TURNS: quit after that many)
"""

import json
import os
import select
import signal
import subprocess
import sys
import termios
import time
import tty
import uuid

ARTTY = sys.argv[1]
DELAY = float(sys.argv[2]) if len(sys.argv) > 2 else 0.3
TURNS = int(sys.argv[3]) if len(sys.argv) > 3 else None


def hook(state, event):
    event = dict(event, session_id="fake-session")
    subprocess.run(
        [ARTTY, "status", state, "--hook", "--agent", "fake"],
        input=json.dumps(event).encode(),
        check=False,
    )


def out(text):
    os.write(1, text.encode())


def main():
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    tty.setraw(fd)
    try:
        out("\x1b[?2004h")
        hook("idle", {"hook_event_name": "SessionStart"})
        out("fake> ")
        pending = b""
        text = b""
        pasting = False
        turns = 0
        while True:
            chunk = os.read(fd, 4096)
            if not chunk:
                return
            pending += chunk
            while pending:
                if pending.startswith(b"\x1b[200~"):
                    pasting, pending = True, pending[6:]
                elif pending.startswith(b"\x1b[201~"):
                    pasting, pending = False, pending[6:]
                elif pending[:1] == b"\x1b" and len(pending) < 6:
                    break  # a marker cut short: wait for the rest
                elif pending[:1] == b"\r" and not pasting:
                    pending = pending[1:]
                    turn(text.decode())
                    text = b""
                    turns += 1
                    if TURNS is not None and turns >= TURNS:
                        return
                elif pending[:1] == b"\x03":
                    return
                else:
                    text, pending = text + pending[:1], pending[1:]
    finally:
        out("\x1b[?2004l")
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        hook("idle", {"hook_event_name": "SessionEnd"})


def permission(command):
    """Ask to run `command`: the reply, or None if turned down at the prompt."""
    tool_input = {"command": command}
    rule = {"toolName": "Bash", "ruleContent": command}
    request = {
        "hook_event_name": "PermissionRequest",
        "session_id": "fake-session",
        "tool_name": "Bash",
        "tool_input": tool_input,
        "permission_suggestions": [
            {"type": "addRules", "rules": [rule], "behavior": "allow", "destination": "session"}
        ],
    }
    always = False
    approve = subprocess.Popen(
        [ARTTY, "approve", "--hook", "--agent", "fake"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    approve.stdin.write(json.dumps(request).encode())
    approve.stdin.close()
    out("Allow " + command + "? 1 yes, 4 no\r\n")
    fd = sys.stdin.fileno()
    hook_out = approve.stdout
    while True:
        readable = [fd] + ([hook_out] if hook_out else [])
        ready, _, _ = select.select(readable, [], [])
        if hook_out in ready:
            decided = hook_out.read().decode().strip()
            hook_out = None
            if decided:
                decision = json.loads(decided)["hookSpecificOutput"]["decision"]
                if decision["behavior"] == "allow":
                    always = bool(decision.get("updatedPermissions"))
                    break
                return "denied: " + decision.get("message", "")
            # No decision: the prompt decides.
        if fd in ready:
            key = os.read(fd, 1)
            if key == b"1":
                break  # yes; the hook is left to run, as Claude Code does
            if key == b"4":
                approve.send_signal(signal.SIGTERM)
                approve.wait()
                return None
    used = {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": tool_input}
    hook("working", used)
    approve.wait()  # released by artty once it sees the tool used
    return ("ran (always): " if always else "ran: ") + command


def turn(prompt):
    prompt_id = str(uuid.uuid4())
    hook(
        "working",
        {"hook_event_name": "UserPromptSubmit", "prompt": prompt, "prompt_id": prompt_id},
    )
    out("\r\n" + prompt.replace("\n", "\r\n") + "\r\n")
    if prompt.startswith("tool: "):
        reply = permission(prompt[len("tool: "):])
        if reply is None:
            out("Interrupted\r\nfake> ")
            return
        out(reply + "\r\nfake> ")
        hook(
            "done",
            {"hook_event_name": "Stop", "prompt_id": prompt_id, "last_assistant_message": reply},
        )
        return
    time.sleep(DELAY)
    reply = "echo: " + prompt
    out(reply.replace("\n", "\r\n") + "\r\nfake> ")
    hook(
        "done",
        {"hook_event_name": "Stop", "prompt_id": prompt_id, "last_assistant_message": reply},
    )


main()
