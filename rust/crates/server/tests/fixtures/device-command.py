#!/usr/bin/python3
"""Typed device helper fixture; commands only mutate isolated fixture data."""
import fcntl
import json
import os
import sys

args = sys.argv[1:]
if args[:1] != ["simctl"]:
    sys.exit(0)
stdin = sys.stdin.read() if args[1:2] == ["push"] else None
with open(os.environ["FIXTURE_COMMAND_DATA"], "r+", encoding="utf-8") as file:
    fcntl.flock(file.fileno(), fcntl.LOCK_EX)
    file.seek(0)
    data = json.loads(file.read() or "{}")
    data.setdefault("calls", []).append({"args": args, **({"stdin": stdin} if stdin is not None else {})})
    state = data.setdefault("settings", {"appearance": "light", "content_size": "large", "increase_contrast": "enabled"})
    output = ""
    if args[1:2] == ["ui"]:
        if len(args) == 4:
            output = state.get(args[3], "")
        elif len(args) == 5:
            state[args[3]] = args[4]
    elif args[1:2] == ["spawn"] and args[-1:] == ["status"]:
        output = json.dumps({"reduce-motion": "on", "voiceover": "off", "color-filter": "grayscale"})
    file.seek(0)
    json.dump(data, file)
    file.truncate()
    if data.get("fail"):
        print("fixture command rejected", file=sys.stderr)
        sys.exit(7)
    print(output, end="")
