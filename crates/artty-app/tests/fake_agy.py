"""A stand-in for Antigravity CLI (1.5.16) in a pane: its status reports
(OSC 7501, as app=antigravity) and its hooks' input, camelCase, through
artty's hooks (`artty status ... --hook --agent agy`).

It asks to be trusted (blocked), then on each line read: a turn, its
first model call to its Stop, the reply in its transcript; a line `bg`
ends the turn with a task left in the background. The first turn's
conversation gets a title in its list (in $HOME) while it runs, as
Antigravity CLI makes one up. Stays up till killed.
"""

import base64
import json
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import time

artty = sys.argv[1]
conversation = "e6435c3c-f9c0-4e70-ad66-7a3281033fa7"
transcript = os.path.join(tempfile.mkdtemp(), "transcript_full.jsonl")
steps = 0


def report(pairs, msg=None):
    if msg:
        pairs += ":msg=" + base64.b64encode(msg.encode()).decode()
    sys.stdout.write("\x1b]7501;" + pairs + ":app=antigravity\x07")
    sys.stdout.flush()


def step(kind, **fields):
    global steps
    with open(transcript, "a") as f:
        f.write(json.dumps(dict(step_index=steps, type=kind, **fields)) + "\n")
    steps += 1


def titled(title):
    dir = os.path.join(os.environ["HOME"], ".gemini", "antigravity-cli")
    os.makedirs(dir, exist_ok=True)
    db = sqlite3.connect(os.path.join(dir, "conversation_summaries.db"))
    db.execute(
        "CREATE TABLE IF NOT EXISTS conversation_summaries (conversation_id text PRIMARY KEY,"
        " title text NOT NULL DEFAULT '', workspace_uris text NOT NULL)"
    )
    db.execute("INSERT OR REPLACE INTO conversation_summaries VALUES (?, ?, '[]')", (conversation, title))
    db.commit()
    db.close()


def hook(state, event, prompt, **fields):
    payload = dict(
        hookEventName=event,
        conversationId=conversation,
        cwd=os.getcwd(),
        lastUserInput=prompt,
        transcriptPath=transcript,
        workspacePaths=[os.getcwd()],
        **fields,
    )
    subprocess.run(
        [artty, "status", state, "--hook", "--agent", "agy"], input=json.dumps(payload), text=True
    )


sys.stdout.write("\x1b]7501;?\x07")
sys.stdout.flush()
report("state=blocked:kind=permission", "Trust this folder?")
print("ready", flush=True)
first = True
for line in sys.stdin:
    # (Less the terminal's answer to the check, as agy reads it.)
    prompt = re.sub(r"\x1b\][^\x07]*\x07", "", line).strip()
    if first:
        hook("idle", "SessionStart", prompt)
        first = False
    step("USER_INPUT", content="<USER_REQUEST>\n" + prompt + "\n</USER_REQUEST>")
    hook("working", "PreInvocation", prompt, invocationNum=0, initialNumSteps=steps)
    report("state=working", "Thinking")
    if steps == 1:
        titled("Build Fix")
    step("PLANNER_RESPONSE", tool_calls=[dict(name="run_command")])
    hook("working", "PostToolUse", prompt, toolCall=dict(name="run_command", args={}))
    hook("working", "PreInvocation", prompt, invocationNum=1, initialNumSteps=steps)
    step("PLANNER_RESPONSE", content="Answered: " + prompt)
    background = prompt == "bg"
    hook("done", "Stop", prompt, fullyIdle=not background, terminationReason="NO_TOOL_CALL")
    report("state=working" if background else "state=done")
    print("turn over", flush=True)
time.sleep(600)
