#!/usr/bin/env python3
"""The `gh` of the browser fixture. Never the real CLI, never the network.

The server reads GitHub through its HTTP API and uses `gh` only to find out who is signed in and to
borrow a token. This answers exactly those calls for the fixture host github.t3code-test.invalid and
refuses everything else (exit 97, with the arguments named), so a call the server starts making later
shows up instead of silently returning nothing. Every call is appended to $T3_FIXTURE_CALL_LOG.
"""
import json
import os
import sys
import time

HOST = "github.t3code-test.invalid"
LOGIN = "carol"
argv = sys.argv[1:]

path = os.environ.get("T3_FIXTURE_CALL_LOG")
if path:
    with open(path, "a") as handle:
        handle.write(json.dumps({"fake": "gh", "pid": os.getpid(), "at": time.time(), "args": argv}) + "\n")

if argv == ["--version"] or argv[:1] == ["version"]:
    print("gh version 2.60.0 (browser fixture)")
elif argv[:2] == ["auth", "status"]:
    if "--json" in argv:
        print(json.dumps({"hosts": {HOST: [{"state": "success", "active": True, "host": HOST, "login": LOGIN, "tokenSource": "keyring", "scopes": "repo, read:org", "gitProtocol": "https"}]}}))
    else:
        print("%s\n  ✓ Logged in to %s account %s (keyring)\n  - Active account: true" % (HOST, HOST, LOGIN))
elif argv[:2] == ["auth", "token"]:
    hostname = argv[argv.index("--hostname") + 1] if "--hostname" in argv else HOST
    if hostname != HOST:
        sys.stderr.write("no oauth token found for %s\n" % hostname)
        sys.exit(1)
    print("fixture-token-not-a-secret")
else:
    sys.stderr.write("fake gh (browser fixture): no handler for: gh %s\n" % " ".join(argv))
    sys.exit(97)
