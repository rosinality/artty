"""A stand-in for Claude Code's status reports (OSC 7501), in a pane.

Asks whether the terminal knows the protocol, then (argument 2):
  interrupt  a turn begins (its hooks say so), it reports working, then
             idle with no Stop: cut short, as Esc does
  blocked    reports blocked (a login, which hooks say nothing of), then
             working
and stays up till killed.
"""

import base64
import json
import os
import subprocess
import sys
import time

artty, case = sys.argv[1], sys.argv[2]


def report(pairs, msg=None):
    if msg:
        pairs += ":msg=" + base64.b64encode(msg.encode()).decode()
    sys.stdout.write("\x1b]7501;" + pairs + "\x07")
    sys.stdout.flush()


def hook(state, event):
    payload = json.dumps(dict(hook_event_name=event, session_id="fake-session", prompt="go"))
    subprocess.run([artty, "status", state, "--hook", "--agent", "claude"], input=payload, text=True)


sys.stdout.write("\x1b]7501;?\x07")
sys.stdout.flush()
report("state=idle:app=claude-code")
time.sleep(0.5)
if case == "interrupt":
    hook("working", "UserPromptSubmit")
    report("state=working:app=claude-code", "Writing note.txt")
    time.sleep(1)
    print("ready", flush=True)
    sys.stdin.readline()
    report("state=idle:app=claude-code")
elif case == "blocked":
    report("state=blocked:kind=auth:app=claude-code", "Login expired")
    print("ready", flush=True)
    sys.stdin.readline()
    report("state=working:app=claude-code", "Thinking")
time.sleep(600)
