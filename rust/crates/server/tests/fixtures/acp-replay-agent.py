#!/usr/bin/env python3
"""Plays a recorded ACP transcript (tests/fixtures/acp-replay.json rows) against the client.

Mirrors apps/server/scripts/acp-replay-agent.ts: the client's frames are compared with the
`expect_outbound` entries (`<any>` and `<workspace>` placeholders), `emit_inbound` entries are
written as soon as the cursor reaches them, and the cursor is written to a status file so the
test can prove the whole transcript was consumed. Usage: acp-replay-agent.py TRANSCRIPT STATUS WORKSPACE
"""
import json
import sys

transcript = json.load(open(sys.argv[1]))
status_path = sys.argv[2]
workspace = sys.argv[3]
entries = [entry for entry in transcript["entries"] if entry["type"] != "transcript_start"]
cursor = 0
stopped = False
next_agent_id = 1
pending_client = {}
pending_agent = {}


def write_status(failure=None):
    body = {"scenario": transcript["scenario"], "cursor": cursor, "total": len(entries)}
    if failure is not None:
        body["failure"] = failure
    with open(status_path, "w") as handle:
        json.dump(body, handle)


def expand(value):
    return value.replace("<workspace>", workspace)


def matches(expected, actual):
    if expected == "<any>":
        return True
    if isinstance(expected, str) and isinstance(actual, str):
        text = expand(expected)
        if "<any>" not in text:
            return text == actual
        offset = 0
        for part in text.split("<any>"):
            at = actual.find(part, offset)
            if at == -1:
                return False
            offset = at + len(part)
        return True
    if isinstance(expected, list):
        return isinstance(actual, list) and len(expected) == len(actual) and all(matches(e, a) for e, a in zip(expected, actual))
    if isinstance(expected, dict):
        if not isinstance(actual, dict) or sorted(expected) != sorted(actual):
            return False
        return all(matches(expected[key], actual[key]) for key in expected)
    return expected == actual and type(expected) is type(actual)


def fail(detail, actual=None):
    global stopped
    if stopped:
        return
    stopped = True
    failure = {"detail": detail, "cursor": cursor, "expected": entries[cursor] if cursor < len(entries) else None}
    if actual is not None:
        failure["actual"] = actual
    write_status(failure)
    sys.stderr.write("ACP replay mismatch: " + json.dumps(failure) + "\n")
    sys.stderr.flush()
    sys.exit(1)


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def materialize(value):
    if isinstance(value, str):
        return expand(value)
    if isinstance(value, list):
        return [materialize(item) for item in value]
    if isinstance(value, dict):
        return {key: materialize(item) for key, item in value.items()}
    return value


def emit(recorded):
    global next_agent_id
    frame = materialize(recorded)
    kind = frame["kind"]
    if kind == "notification":
        message = {"jsonrpc": "2.0", "method": frame["method"]}
        if "params" in frame:
            message["params"] = frame["params"]
        send(message)
    elif kind == "request":
        ident = next_agent_id
        next_agent_id += 1
        pending_agent[str(ident)] = frame["method"]
        message = {"jsonrpc": "2.0", "id": ident, "method": frame["method"], "headers": []}
        if "params" in frame:
            message["params"] = frame["params"]
        send(message)
    elif kind == "response":
        ident = pending_client.pop(frame["method"], None)
        if ident is None:
            fail("No pending client request for " + frame["method"], frame)
        message = {"jsonrpc": "2.0", "id": ident}
        if "result" in frame:
            message["result"] = frame["result"]
        if "error" in frame:
            message["error"] = frame["error"]
        send(message)


def flush():
    global cursor
    while not stopped:
        if cursor >= len(entries):
            return
        entry = entries[cursor]
        if entry["type"] == "expect_outbound":
            return
        if entry["type"] == "runtime_exit":
            if entry.get("status") not in ("success", "cancelled"):
                fail("Recorded runtime exit was " + str(entry.get("status")), entry.get("error"))
            cursor += 1
            write_status()
            continue
        emit(entry["frame"])
        cursor += 1
        write_status()


def logical(message):
    if isinstance(message.get("method"), str):
        notification = message.get("id") in (None, "")
        frame = {"kind": "notification" if notification else "request", "method": message["method"]}
        if "params" in message:
            frame["params"] = message["params"]
        return frame
    ident = message.get("id")
    if ident is None:
        return None
    method = pending_agent.get(str(ident))
    if method is None:
        return None
    frame = {"kind": "response", "method": method}
    for key in ("result", "error"):
        if key in message:
            frame[key] = message[key]
    return frame


def handle(message):
    global cursor
    actual = logical(message)
    if actual is None:
        fail("Could not identify outbound ACP frame", message)
    entry = entries[cursor] if cursor < len(entries) else None
    if entry is None or entry["type"] != "expect_outbound" or not matches(entry["frame"], actual):
        if actual["kind"] == "request" and message.get("id") is not None:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": "ACP replay frame mismatch"}})
        fail("Unexpected outbound ACP frame", actual)
    if actual["kind"] == "request" and message.get("id") is not None:
        pending_client[actual["method"]] = message["id"]
    elif actual["kind"] == "response" and message.get("id") is not None:
        pending_agent.pop(str(message["id"]), None)
    cursor += 1
    write_status()
    flush()


write_status()
flush()
for line in sys.stdin:
    if stopped or not line.strip():
        continue
    try:
        handle(json.loads(line))
    except SystemExit:
        raise
    except Exception as cause:  # noqa: BLE001
        fail("Failed to decode outbound ACP JSON-RPC", str(cause))
if not stopped and cursor != len(entries):
    fail("ACP replay input closed before transcript completion")
