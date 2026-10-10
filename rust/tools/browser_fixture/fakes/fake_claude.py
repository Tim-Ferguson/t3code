#!/usr/bin/env python3
"""The `claude` of the browser fixture. Never the real CLI, never the network, never ~/.claude.

The repository's scripted stream-json CLI (tests/fixtures/claude-provider.py) plays one scenario per
process, chosen by an environment variable, which suits a test and not a long-lived server where one
provider instance serves many threads. This wrapper runs that same script unchanged, with one
substitution: the scenario is chosen per message, by a tag in the prompt.

  [subagent]    a subagent asks to run `ls` (a child thread with a pending approval)
  [children]    a subagent has a command open when the turn is stopped
  [background]  a background command that outlives the turn
  [wake]        a background command that finishes after the turn and wakes the agent
  [limit]       a usage-limit stop (resets about 90 minutes from now)
  [warning]     a rate-limit warning, then a normal reply
  [approval]    a Write waits for approval
  [question]    a question with choices waits for an answer
  [hang]        a command that never finishes (stop the run to end it)
  [steer]       a run that waits for a steering message
  [error]       a failed run (HTTP 429 from the API)
  [crash]       the process dies mid-run
  (no tag)      thinking, a Bash call, a todo list and a reply

Every launch is appended to $T3_FIXTURE_CALL_LOG. The upstream script's own log goes to
$CLAUDE_FIXTURE_LOG when the provider's environment names one.
"""
import json
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", "..", ".."))
UPSTREAM = os.environ.get("T3_FIXTURE_CLAUDE_PROVIDER") or os.path.join(ROOT, "crates", "server", "tests", "fixtures", "claude-provider.py")

TAGS = {
    "subagent": "subagent_approval", "children": "children_running", "background": "background_task", "wake": "background_wake",
    "limit": "limit", "warning": "warning", "approval": "approval", "question": "question", "hang": "hang", "steer": "steer",
    "error": "error", "crash": "crash",
}

path = os.environ.get("T3_FIXTURE_CALL_LOG")
if path:
    with open(path, "a") as handle:
        handle.write(json.dumps({"fake": "claude", "pid": os.getpid(), "at": time.time(), "argv": sys.argv[1:5]}) + "\n")

source = open(UPSTREAM).read()
needle = 'script = SCENARIOS[os.environ["CLAUDE_FIXTURE_SCENARIO"]]'
if needle not in source:
    sys.stderr.write("fake claude: the upstream script no longer selects its scenario with %r; update fakes/fake_claude.py\n" % needle)
    sys.exit(65)
namespace = {"__name__": "__main__", "__file__": UPSTREAM}


def dispatch(message):
    text = namespace["prompt_text"](message)
    found = re.findall(r"\[([a-z]+)\]", text)
    name = next((TAGS[tag] for tag in found if tag in TAGS), "turn")
    namespace["SCENARIOS"][name](message)


namespace["DISPATCH"] = dispatch
os.environ.setdefault("CLAUDE_FIXTURE_SCENARIO", "turn")
sys.argv[0] = UPSTREAM
exec(compile(source.replace(needle, "script = DISPATCH"), UPSTREAM, "exec"), namespace)
