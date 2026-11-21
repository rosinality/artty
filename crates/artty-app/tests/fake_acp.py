"""A stand-in Agent Client Protocol adapter for the tests (`artty acp`):
JSON-RPC a line at a time on stdin and stdout, as the real ones.

What a prompt does, by its text:
  edit PATH  asks permission to edit PATH; allowed, writes it ("edited"),
             else "not allowed"
  slow       says "." every 0.2s until cancelled (stopReason cancelled)
  bg         answers "started", and a second later goes on by itself
             ("background done"), as after a background task
  bgslow     answers "started", then goes on by itself until cancelled
  busy       runs a tool ("cargo test --workspace") until cancelled
  hook       runs the agent's hooks as Claude Code would (artty status
             --hook: under artty acp, they keep quiet), answers "echo: hook"
  which      answers "session: ID", the conversation it was given
  else       answers "echo: TEXT"
A loaded conversation is replayed first, as the real ones do; a forked one
(session/fork) isn't: it's a new conversation, "fork-of-ID".
"""

import json
import os
import subprocess
import sys
import threading
import time

lock = threading.Lock()
answers = {}
cancelled = threading.Event()
next_id = [1000]
SESSION = "fake-acp-session"


def send(msg):
    with lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()


def update(session, body):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": body}})


def say(session, text, message):
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}, "messageId": message})


def ask(method, params):
    """A request to the client; its answer."""
    with lock:
        next_id[0] += 1
        rid = next_id[0]
        answers[rid] = [threading.Event(), None]
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    answers[rid][0].wait()
    return answers.pop(rid)[1]


def prompt(rid, session, text):
    cancelled.clear()
    if text.startswith("edit "):
        path = text[5:].strip()
        call = {"toolCallId": "call-1", "title": f"Edit {os.path.basename(path)}", "kind": "edit",
                "rawInput": {"file_path": path, "old_string": "", "new_string": "edited\n"},
                "_meta": {"claudeCode": {"toolName": "Edit"}}}
        update(session, dict(call, sessionUpdate="tool_call", status="pending"))
        options = [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                   {"optionId": "always", "name": "Always", "kind": "allow_always"},
                   {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]
        answer = ask("session/request_permission", {"sessionId": session, "toolCall": call, "options": options})
        outcome = (answer or {}).get("result", {}).get("outcome", {})
        if outcome.get("outcome") == "cancelled" or cancelled.is_set():
            send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "cancelled"}})
            return
        if outcome.get("optionId") in ("allow", "always"):
            open(path, "w").write("edited\n")
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed"})
            say(session, "edited", "m-edit")
        else:
            update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "failed"})
            say(session, "not allowed", "m-edit")
    elif text == "slow":
        while not cancelled.wait(0.2):
            say(session, ".", "m-slow")
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "cancelled"}})
        return
    elif text == "busy":
        call = {"toolCallId": "call-2", "_meta": {"claudeCode": {"toolName": "Bash"}}}
        update(session, dict(call, sessionUpdate="tool_call", title="Terminal", kind="execute", status="pending"))
        update(session, dict(call, sessionUpdate="tool_call_update", title="cargo test --workspace",
                             rawInput={"command": "cargo test --workspace"}))
        cancelled.wait()
        update(session, dict(call, sessionUpdate="tool_call_update", status="failed"))
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "cancelled"}})
        return
    elif text == "bgslow":
        say(session, "started", "m-bg")

        def on():
            time.sleep(0.5)
            while not cancelled.wait(0.2):
                say(session, ".", "m-bgslow")
        update(session, {"sessionUpdate": "usage_update", "used": 1, "size": 2, "cost": {"amount": 0, "currency": "USD"}})
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        cancelled.clear()
        threading.Thread(target=on, daemon=True).start()
        return
    elif text == "hook":
        event = json.dumps({"hook_event_name": "Stop", "last_assistant_message": "from the hook"})
        subprocess.run([os.environ["ARTTY_BIN"], "status", "done", "--hook", "--agent", "claude"],
                       input=event, text=True)
        say(session, "echo: hook", "m-hook")
    elif text == "which":
        say(session, "session: " + session, "m-which")
    elif text == "bg":
        say(session, "started", "m-bg")

        def later():
            time.sleep(1)
            say(session, "background ", "m-bg2")
            say(session, "done", "m-bg2")
            update(session, {"sessionUpdate": "usage_update", "used": 1, "size": 2,
                             "cost": {"amount": 0, "currency": "USD"}})
        threading.Thread(target=later, daemon=True).start()
    else:
        say(session, "echo: ", "m-" + str(rid))
        say(session, text, "m-" + str(rid))
    update(session, {"sessionUpdate": "usage_update", "used": 1, "size": 2, "cost": {"amount": 0, "currency": "USD"}})
    send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})


for line in sys.stdin:
    msg = json.loads(line)
    method, rid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method is None:
        with lock:
            if rid in answers:
                answers[rid][1] = msg
                answers[rid][0].set()
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
        # (As the real one: the account's status, not for showing.)
        send({"jsonrpc": "2.0", "method": "auth/status_update", "params": {"state": "authenticated"}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": SESSION}})
    elif method == "session/fork":
        send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "fork-of-" + params["sessionId"]}})
    elif method == "session/load":
        session = params["sessionId"]
        update(session, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "an old message"}})
        say(session, "an old reply", "m-old")
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
    elif method == "session/prompt":
        text = "".join(p.get("text", "") for p in params.get("prompt", []))
        threading.Thread(target=prompt, args=(rid, params["sessionId"], text), daemon=True).start()
    elif method == "session/cancel":
        cancelled.set()
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
