#!/usr/bin/env python3
"""Test-only stand-in for the one-shot runs of `codex exec` and `claude -p`.

It records exactly how it was started (arguments, working directory, the two config-home variables
and stdin, plus the schema file `codex exec` is given) and answers as scripted. It never runs a tool,
reads agent state or talks to the network.

  T3_TEXTGEN_DIR   directory for the calls; call N writes NNN.capture.json and reads NNN.plan.json,
                   or plan.json when there is no plan for that call.
  T3_FAKE_DELEGATE "codex" or "claude": any invocation that is not a one-shot run (health checks,
                   sessions) is handed to the sibling fake provider, so one executable can stand for
                   an enabled provider in a registry.

A plan is {"answer": {...}} (what a real CLI would print or write for a structured answer), or any of
"stdout", "outputFile" (what `codex exec` writes to --output-last-message), "stderr", "exit", and
"hang" (never finish). "stdout"/"outputFile" win over "answer".
"""
import json
import os
import sys
import time

argv = sys.argv[1:]
one_shot = argv[:1] == ["exec"] or argv[:1] == ["-p"]
if not one_shot:
    target = {"codex": "codex-provider.py", "claude": "claude-provider.py"}[os.environ["T3_FAKE_DELEGATE"]]
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), target)
    os.execv(sys.executable, [sys.executable, path] + argv)

directory = os.environ["T3_TEXTGEN_DIR"]
number = 1
while True:
    try:
        descriptor = os.open(os.path.join(directory, "%03d.capture.json" % number), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        break
    except FileExistsError:
        number += 1


def read_flag(flag):
    return argv[argv.index(flag) + 1] if flag in argv else None


stdin = sys.stdin.read()
schema_path = read_flag("--output-schema")
output_path = read_flag("--output-last-message")
capture = {
    "argv": argv,
    "cwd": os.path.realpath(os.getcwd()),
    "env": {"CLAUDE_CONFIG_DIR": os.environ.get("CLAUDE_CONFIG_DIR"), "CODEX_HOME": os.environ.get("CODEX_HOME")},
    "stdin": stdin,
    "schema": open(schema_path).read() if schema_path else None,
}
with os.fdopen(descriptor, "w") as handle:
    json.dump(capture, handle)

plan = {}
for name in ("%03d.plan.json" % number, "plan.json"):
    path = os.path.join(directory, name)
    if os.path.exists(path):
        plan = json.load(open(path))
        break
if plan.get("hang"):
    time.sleep(3600)
answer = plan.get("answer")
stdout = plan.get("stdout")
output_file = plan.get("outputFile")
if answer is not None:
    if argv[:1] == ["exec"] and output_file is None:
        output_file = json.dumps(answer)
    if argv[:1] == ["-p"] and stdout is None:
        stdout = json.dumps({"type": "result", "structured_output": answer})
if output_path and output_file is not None:
    with open(output_path, "w") as handle:
        handle.write(output_file)
if plan.get("stderr"):
    sys.stderr.write(plan["stderr"])
sys.stdout.write(stdout or "")
sys.stdout.flush()
sys.exit(int(plan.get("exit", 0)))
