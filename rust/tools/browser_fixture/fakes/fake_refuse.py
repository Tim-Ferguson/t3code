#!/usr/bin/env python3
"""Stands in for a program the browser fixture must never run for real (macOS `open`, which would start the
developer's editor or the Finder). It records the call and refuses."""
import json
import os
import sys
import time

path = os.environ.get("T3_FIXTURE_CALL_LOG")
if path:
    with open(path, "a") as handle:
        handle.write(json.dumps({"fake": "open", "refused": True, "pid": os.getpid(), "at": time.time(), "args": sys.argv[1:]}) + "\n")
sys.stderr.write("the browser fixture does not open editors or the file manager (args: %s)\n" % " ".join(sys.argv[1:]))
sys.exit(1)
