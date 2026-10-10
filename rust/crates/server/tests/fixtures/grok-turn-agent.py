#!/usr/bin/env python3
"""A scripted stand-in for `grok agent stdio`, run by grok-fake-cli.py (GROK_FIXTURE_AGENT). Never the real CLI.

GROK_FIXTURE_SCRIPT names {"prompts": [[step, ...], ...]}: the Nth `session/prompt` plays the Nth list. Steps:
  {"update": {...}}                 a `session/update` for the session
  {"ext": {"method", "params"}}     a notification of Grok's own extension
  {"request": {"method","params"}}  a request to the client, waited on before the next step
  {"detach": "name"}                starts a tool that outlives its parent (same process group, no terminal) and
                                    waits until it is ready; it records its exit by writing "exited" to the fifo
                                    GROK_FIXTURE_DETACH_DIR/<name>.fifo, and its pid to <name>.pid
  {"detach": "name", "stubborn": true}  the same, but it ignores the request to end and holds the fifo open
                                    instead: only its death closes it
  {"end": "end_turn"}               answers the prompt
  {"hang": true}                    keeps the prompt open until the process is stopped
GROK_FIXTURE_LOG receives every message the client sends, with this process's pid.
With GROK_FIXTURE_DETACH_DIR set, this process writes its own pid to agent-<pid>.pid there when it starts, and
a detached tool leaves <name>.pid, which it removes itself when it is told to end: the test's cleanup guard
signals exactly the pids still listed (see PidGuard in grok_turn_flow.rs).
"""
import json
import os
import signal
import subprocess
import sys

script = json.load(open(os.environ["GROK_FIXTURE_SCRIPT"]))
log_path = os.environ["GROK_FIXTURE_LOG"]
detach_dir = os.environ.get("GROK_FIXTURE_DETACH_DIR")
SESSION = "grok-turn-session"
MODELS = {"currentModelId": "grok-4.7", "availableModels": [{"modelId": "grok-4.7", "name": "Grok 4.7"}]}
if detach_dir:
    with open(os.path.join(detach_dir, "agent-%d.pid" % os.getpid()), "w") as handle:
        handle.write(str(os.getpid()))
# A process that replaces a stopped one carries on with the next prompt of the script.
prompts_seen = sum(1 for line in open(log_path) if json.loads(line)["in"].get("method") == "session/prompt") if os.path.exists(log_path) else 0
prompt_request = None
queue = []
waiting = None
next_id = 100

# `sleep` and the shell below are the tool; ignoring SIGHUP is what a detached tool does.
STUBBORN = 'trap "" TERM; exec {hold}>"$2"; echo ready >&{fd}; while :; do sleep 0.1; done'
DETACHED = 'trap \'rm -f "$3"; echo exited > "$2"; exit 0\' TERM; echo ready >&{fd}; while :; do sleep 0.1; done'


def emit(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def record(message):
    with open(log_path, "a") as log:
        log.write(json.dumps({"pid": os.getpid(), "in": message}) + "\n")


def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})


def detach(name, stubborn=False):
    ready_read, ready_write = os.pipe()
    fifo = os.path.join(detach_dir, name + ".fifo")
    if not os.path.exists(fifo):
        os.mkfifo(fifo)
    child = subprocess.Popen(
        ["/bin/sh", "-c", (STUBBORN if stubborn else DETACHED).format(fd=ready_write, hold=ready_write + 10), "sh", name, fifo, os.path.join(detach_dir, name + ".pid")],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        pass_fds=[ready_write],
        preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN),
    )
    os.close(ready_write)
    # The tool has installed its handler once it says so.
    os.read(ready_read, 5)
    os.close(ready_read)
    with open(os.path.join(detach_dir, name + ".pid"), "w") as handle:
        handle.write(str(child.pid))


def run():
    global waiting, prompt_request, next_id
    while queue and waiting is None:
        step = queue.pop(0)
        if "update" in step:
            emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": step.get("session", SESSION), "update": step["update"]}})
        elif "ext" in step:
            params = dict(step["ext"]["params"])
            params.setdefault("sessionId", SESSION)
            emit({"jsonrpc": "2.0", "method": step["ext"]["method"], "params": params})
        elif "request" in step:
            next_id += 1
            waiting = next_id
            params = dict(step["request"]["params"])
            params.setdefault("sessionId", SESSION)
            emit({"jsonrpc": "2.0", "id": next_id, "method": step["request"]["method"], "params": params})
        elif "detach" in step:
            detach(step["detach"], step.get("stubborn", False))
        elif "end" in step:
            reply(prompt_request, {"stopReason": step["end"]})
            prompt_request = None
        elif "hang" in step:
            return


for line in sys.stdin:
    message = json.loads(line)
    record(message)
    if "method" not in message:
        if message.get("id") == waiting:
            waiting = None
            run()
        continue
    method = message["method"]
    if method == "initialize":
        reply(message, {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "promptCapabilities": {"image": False}, "sessionCapabilities": {"close": {}}}, "authMethods": [{"id": "cached_token", "name": "cached_token"}]})
    elif method == "session/new":
        reply(message, {"sessionId": SESSION, "models": MODELS})
    elif method in ("session/load", "session/resume"):
        reply(message, {"models": MODELS})
    elif method in ("session/set_model", "session/set_mode", "session/set_config_option"):
        reply(message, {})
    elif method == "session/prompt":
        prompt_request = message
        queue = list(script["prompts"][prompts_seen])
        prompts_seen += 1
        run()
    elif method == "session/cancel":
        if prompt_request is not None:
            queue = []
            waiting = None
            reply(prompt_request, {"stopReason": "cancelled"})
            prompt_request = None
    elif method == "session/close":
        reply(message, {})
    elif "id" in message:
        emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unsupported"}})
