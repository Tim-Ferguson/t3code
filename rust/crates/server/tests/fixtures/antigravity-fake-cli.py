#!/usr/bin/env python3
"""A stand-in for Google's `agy_acp_server`. Never the real agent, never the network.

It is started as the ACP agent (by the wrapper `agy_acp_server.par` the tests put beside a `localharness_external`).
With ANTIGRAVITY_FIXTURE_TRANSCRIPT it plays that recording with acp-replay-agent.py; with ANTIGRAVITY_FIXTURE_AGENT
it runs that script as the agent (antigravity-turn-agent.py). Any other launch is refused, so a test that forgot to
say what the agent does fails instead of hanging. The arguments and the credentials-bearing environment the agent
was launched with are appended to ANTIGRAVITY_FIXTURE_LAUNCH_LOG when it is set.
"""
import json
import os
import sys

here = os.path.dirname(os.path.abspath(__file__))
log = os.environ.get("ANTIGRAVITY_FIXTURE_LAUNCH_LOG")
if log:
    watched = ["GEMINI_HOME", "GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS", "AGY_ACP_FORCE_FILE_STORAGE", "BROWSER", "ANTIGRAVITY_HARNESS_PATH", "TMPDIR", "PYTHONUNBUFFERED", "FIXTURE_LEAKED_CREDENTIAL"]
    with open(log, "a") as handle:
        handle.write(json.dumps({"pid": os.getpid(), "args": sys.argv[1:], "cwd": os.getcwd(), "env": {name: os.environ.get(name) for name in watched}}) + "\n")
agent = os.environ.get("ANTIGRAVITY_FIXTURE_AGENT")
if agent:
    os.execvp("python3", ["python3", agent] + sys.argv[1:])
if os.environ.get("ANTIGRAVITY_FIXTURE_TRANSCRIPT"):
    os.execvp("python3", ["python3", os.path.join(here, "acp-replay-agent.py"), os.environ["ANTIGRAVITY_FIXTURE_TRANSCRIPT"], os.environ["ANTIGRAVITY_FIXTURE_STATUS"], os.environ["ANTIGRAVITY_FIXTURE_WORKSPACE"]])
sys.stderr.write("antigravity-fake-cli: nothing to play\n")
sys.exit(2)
