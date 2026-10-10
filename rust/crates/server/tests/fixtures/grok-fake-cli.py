#!/usr/bin/env python3
"""A stand-in for the `grok` executable. Never the real CLI, never the network.

`--version`, `models` and `inspect --json` answer like the real ones. `agent stdio` with no other flag is T3's
provider probe: it answers `initialize` only. Any other `agent` invocation is a session: it plays the
recording named by GROK_FIXTURE_TRANSCRIPT with acp-replay-agent.py, or, with GROK_FIXTURE_AGENT set, runs that
script as the agent. The arguments it was launched with are appended to GROK_FIXTURE_ARGS_LOG when set.
"""
import json
import os
import sys

args = sys.argv[1:]
log = os.environ.get("GROK_FIXTURE_ARGS_LOG")
if log:
    with open(log, "a") as handle:
        handle.write(json.dumps(args) + "\n")
here = os.path.dirname(os.path.abspath(__file__))
version_file = os.environ.get("GROK_FIXTURE_VERSION_FILE")
if args == ["--version"]:
    if version_file and os.path.exists(version_file):
        print(open(version_file).read().strip())
    else:
        print(os.environ.get("GROK_FIXTURE_VERSION", "grok 1.0.41"))
elif args == ["update"]:
    # `grok update` installs the release GROK_FIXTURE_UPDATE_TO under GROK_HOME; the version file stands for it.
    if version_file:
        with open(version_file, "w") as handle:
            handle.write("grok " + os.environ.get("GROK_FIXTURE_UPDATE_TO", "1.0.50"))
    print("Updated")
elif args == ["models"]:
    if os.environ.get("GROK_FIXTURE_LOGGED_OUT"):
        print("Not authenticated. Run `grok login`.")
    else:
        print("You are logged in with grok.com.\nDefault model: grok-4.7\nAvailable models:\n  * grok-4.7 (default)\n  - grok-4.6")
elif args[:2] == ["inspect", "--json"]:
    # The second skill is the directory the CLI was asked about, so a workspace scan shows its own.
    print(json.dumps({"skills": [{"name": "review", "description": "Review a change", "source": {"type": "user", "path": "/skills/review/SKILL.md"}, "userInvocable": True}, {"name": "here", "description": os.getcwd(), "source": {"type": "project", "path": "/skills/here/SKILL.md"}, "userInvocable": True}]}))
elif args == ["agent", "stdio"]:
    if os.environ.get("GROK_FIXTURE_PROBE_FAILS"):
        sys.exit(3)
    for line in sys.stdin:
        message = json.loads(line)
        if message.get("method") == "initialize":
            result = {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
                "authMethods": [{"id": "cached_token", "name": "cached_token"}],
                "_meta": {
                    "modelState": {
                        "currentModelId": "grok-4.7",
                        "availableModels": [
                            {"modelId": "grok-4.7", "name": "Grok 4.7", "_meta": {"supportsReasoningEffort": True, "reasoningEffort": "high", "reasoningEfforts": [{"id": "high", "value": "high", "label": "High", "default": True}, {"id": "low", "value": "low", "label": "Low"}]}},
                            {"modelId": "grok-4.6", "name": "Grok 4.6"},
                        ],
                    },
                    "availableCommands": [{"name": "compact", "description": "Compress conversation history", "input": {"hint": "what to keep"}}, {"name": "always-approve", "description": "Toggle"}, {"name": "context", "description": "Context"}, {"name": "init", "description": "Create AGENTS.md"}],
                },
            }
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}) + "\n")
            sys.stdout.flush()
elif args and "agent" in args:
    agent = os.environ.get("GROK_FIXTURE_AGENT")
    if agent:
        os.execvp("python3", ["python3", agent] + sys.argv[1:])
    os.execvp("python3", ["python3", os.path.join(here, "acp-replay-agent.py"), os.environ["GROK_FIXTURE_TRANSCRIPT"], os.environ["GROK_FIXTURE_STATUS"], os.environ["GROK_FIXTURE_WORKSPACE"]])
else:
    sys.stderr.write("unsupported: " + " ".join(args) + "\n")
    sys.exit(2)
