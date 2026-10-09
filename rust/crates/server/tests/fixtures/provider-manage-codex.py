#!/usr/bin/env python3
"""Test-only Codex CLI for provider management: reports the version in the file `version` beside the
real path of this script, so an update (a fake package manager rewriting that file) is observable.
`--version` prints what the installer checks; `app-server` answers just the health-probe requests."""
import json
import os
import sys

here = os.path.dirname(os.path.realpath(__file__))
try:
    version = open(os.path.join(here, "version")).read().strip()
except OSError:
    version = "0.0.0"

if "--version" in sys.argv[1:]:
    print("codex-cli " + version)
    sys.exit(0)

for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if "id" not in message:
        continue
    method, params = message.get("method"), message.get("params") or {}
    if method == "initialize":
        result = {"userAgent": "codex/" + version + " fixture", "platformFamily": "unix", "platformOs": sys.platform}
    elif method == "account/read":
        result = {"account": None, "requiresOpenaiAuth": False}
    elif method == "model/list":
        result = {"data": [{"id": "m", "model": "m", "displayName": "Model", "description": "", "hidden": False, "isDefault": True, "defaultReasoningEffort": "low", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}], "inputModalities": ["text"], "supportsPersonality": False}], "nextCursor": None}
    elif method == "skills/list":
        result = {"data": [{"cwd": cwd, "skills": [], "errors": []} for cwd in params.get("cwds", [])]}
    else:
        result = {}
    print(json.dumps({"id": message["id"], "result": result}), flush=True)
