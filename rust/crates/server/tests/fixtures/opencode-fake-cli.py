#!/usr/bin/env python3
"""A fake `opencode`: `--version` and `serve`, answering over loopback HTTP with a server-sent event stream.

With OPENCODE_FIXTURE_TRANSCRIPT set it replays a recorded session exactly as the original's testkit does
(`OpenCodeAdapterV2.testkit.ts`, `OpenCode2AdapterV2.testkit.ts`): each request must match the next `expect_outbound`
entry (every key the recording names must be equal, `<any>` matches anything), is answered with the following
`sdk.response`, and `sdk.event` entries are delivered on the stream in order. A request waits until the events
recorded before it have been delivered. A successful `runtime_exit` ends the stream: the server has stopped, and a
later `event.subscribe` is a restart. The first mismatch fails the whole replay and every later request.

Before the first event subscription the server answers what a status check asks (the model, skill and command lists)
from the recording's own, without consuming it. OPENCODE_FIXTURE_STATUS names a file that always holds
{"cursor", "total", "failure"} so a test can say where a replay stopped. No real model, no network beyond loopback.
"""
import base64
import json
import os
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, unquote, urlparse

WILDCARDS = ("<any>", "<workspace>")
ID_KEYS = ("id", "messageID", "parentID", "inboxID", "before")


# The 1.x recordings were matched by the original's own comparison (`replayValueMatches`), where a wildcard matches a
# field the client did not send at all (a `session.create` recorded with a `title` placeholder sends none). 2.x
# recordings are matched strictly: a field they name must be sent.
LENIENT_WILDCARDS = False
ABSENT = object()


def matches(expected, actual):
    if isinstance(expected, str) and expected in WILDCARDS:
        return actual is not ABSENT or LENIENT_WILDCARDS
    if actual is ABSENT:
        return False
    if isinstance(expected, list):
        return isinstance(actual, list) and len(expected) == len(actual) and all(matches(e, a) for e, a in zip(expected, actual))
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(matches(value, actual.get(key, ABSENT)) for key, value in expected.items())
    if isinstance(expected, bool) or isinstance(actual, bool):
        return expected is actual
    return expected == actual


def materialize(value, ids):
    if isinstance(value, list):
        return [materialize(entry, ids) for entry in value]
    if isinstance(value, dict):
        return {key: (ids.get(entry, entry) if isinstance(entry, str) and key in ID_KEYS else materialize(entry, ids)) for key, entry in value.items()}
    return value


# ---- the original's schemas: every request the client sends is checked against the protocol it was generated from ----
SPEC = None
SPEC_FILE = "opencode2-openapi.json"


SPEC_LOCK = threading.Lock()


def spec():
    """The protocol the requests are checked against. Requests are served on threads of their own, and the first ones
    arrive together (a status check reads four lists at once), so the document is complete before anyone sees it."""
    global SPEC
    with SPEC_LOCK:
        if SPEC is None:
            path = os.path.join(os.path.dirname(os.path.abspath(__file__)), SPEC_FILE)
            with open(path) as handle:
                loaded = json.load(handle)
            for operation in loaded["operations"]:
                operation["pattern"] = re.compile("^" + re.sub(r"\{[^}]+\}", "[^/]+", operation["path"]) + "$")
            SPEC = loaded
        return SPEC


def violations(schema, value, where="body"):
    """JSON Schema (the subset OpenAPI 3.1 from Effect uses): the first violation, or None."""
    schemas = spec()["schemas"]
    if "$ref" in schema:
        return violations(schemas[schema["$ref"].split("/")[-1]], value, where)
    for key in ("anyOf", "oneOf"):
        if key in schema:
            attempts = [violations(option, value, where) for option in schema[key]]
            return None if any(attempt is None for attempt in attempts) else "%s matches none of %d alternatives (%s)" % (where, len(attempts), "; ".join(str(a) for a in attempts)[:300])
    if "allOf" in schema:
        for option in schema["allOf"]:
            found = violations(option, value, where)
            if found:
                return found
    if "const" in schema and value != schema["const"]:
        return "%s is not %r" % (where, schema["const"])
    if "enum" in schema and value not in schema["enum"]:
        return "%s is not one of %r" % (where, schema["enum"])
    kind = schema.get("type")
    kinds = kind if isinstance(kind, list) else [kind] if kind else []
    def is_kind(name):
        return {
            "string": isinstance(value, str), "boolean": isinstance(value, bool), "null": value is None,
            "integer": isinstance(value, int) and not isinstance(value, bool) or (isinstance(value, float) and value.is_integer()),
            "number": isinstance(value, (int, float)) and not isinstance(value, bool),
            "array": isinstance(value, list), "object": isinstance(value, dict),
        }[name]
    if kinds and not any(is_kind(name) for name in kinds):
        return "%s is %s, not %s" % (where, type(value).__name__, "/".join(kinds))
    if isinstance(value, str):
        if "pattern" in schema and not re.search(schema["pattern"], value):
            return "%s does not match %s" % (where, schema["pattern"])
        if "minLength" in schema and len(value) < schema["minLength"]:
            return "%s is shorter than %d" % (where, schema["minLength"])
    if isinstance(value, dict):
        for name in schema.get("required", []):
            if name not in value:
                return "%s lacks %s" % (where, name)
        properties = schema.get("properties", {})
        for name, entry in value.items():
            if name in properties:
                found = violations(properties[name], entry, where + "." + name)
                if found:
                    return found
            elif schema.get("additionalProperties") is False:
                return "%s has the unknown field %s" % (where, name)
            elif isinstance(schema.get("additionalProperties"), dict):
                found = violations(schema["additionalProperties"], entry, where + "." + name)
                if found:
                    return found
    if isinstance(value, list) and "items" in schema:
        for index, entry in enumerate(value):
            found = violations(schema["items"], entry, "%s[%d]" % (where, index))
            if found:
                return found
    return None


def check_request(method, path, body):
    """The message of the first way a request differs from the protocol, or None."""
    for operation in spec()["operations"]:
        if operation["method"] == method and operation["pattern"].match(path):
            if operation["body"] is None:
                return None if body is None else "%s %s takes no body" % (method, path)
            if body is None:
                return "%s %s needs a body" % (method, path) if operation["bodyRequired"] else None
            return violations(operation["body"], body)
    return "%s %s is no operation of the protocol" % (method, path)


def check_request_v1(method, path, query, body):
    """The 1.x request checked against the SDK's own request types: the operation, its query names and its body."""
    candidates = [operation for operation in spec()["operations"] if operation["method"] == method and operation["pattern"].match(path)]
    if not candidates:
        return "%s %s is no operation of the protocol" % (method, path)
    # `/session/status` is also a session id.
    operation = min(candidates, key=lambda candidate: candidate["path"].count("{"))
    allowed = {parameter["name"] for parameter in operation["parameters"] if parameter["in"] == "query"}
    for name in query:
        if name not in allowed:
            return "%s %s has the unknown query parameter %s" % (method, path, name)
    if operation["body"] is None:
        return None if body is None else "%s %s takes no body" % (method, path)
    if body is None:
        return "%s %s needs a body" % (method, path) if operation["bodyRequired"] else None
    return violations(operation["body"], body)


def check_response(method, path, data):
    """The first way a recorded answer differs from the protocol it is served as, or None."""
    candidates = [operation for operation in spec()["operations"] if operation["method"] == method and operation["pattern"].match(path)]
    if not candidates or data is None:
        return None
    operation = min(candidates, key=lambda candidate: candidate["path"].count("{"))
    # The 1.x subagent recordings abridge the answer to `session.update` (an id and the rules), which is not a session.
    if not operation.get("response") or (operation["id"] == "session.update" and SPEC_FILE.startswith("opencode1")):
        return None
    return violations(operation["response"], data, "response")


class Controller:
    def __init__(self, transcript, status_path):
        self.gate = (transcript or {}).get("gate")
        self.gated = False
        self.entries = transcript["entries"] if transcript else []
        self.scenario = transcript["scenario"] if transcript else "none"
        self.cv = threading.Condition()
        self.cursor = 0
        self.failure = None
        self.ids = {}
        self.exited = False
        self.subscribed = False
        # The recorded stream is one the first subscriber reads. The server sends every event to every subscriber, so a
        # later one (another thread's actor, while the first still reads) is given the events from where it joined.
        self.primary = False
        self.listeners = []
        self.status_path = status_path
        self.write_status()

    def write_status(self):
        if not self.status_path:
            return
        tmp = self.status_path + ".tmp"
        with open(tmp, "w") as out:
            # Everything but a closing successful exit has been used. The exit itself is taken by the stream's own thread
            # when it sees it, which can come a moment after the last request was answered.
            rest = self.entries[self.cursor:]
            settled = self.failure is None and (not rest or (len(rest) == 1 and rest[0]["type"] == "runtime_exit" and rest[0].get("status") == "success"))
            json.dump({"cursor": self.cursor, "total": len(self.entries), "failure": self.failure, "settled": settled}, out)
        os.replace(tmp, self.status_path)

    def entry(self):
        return self.entries[self.cursor] if self.cursor < len(self.entries) else None

    def advance(self):
        self.cursor += 1
        self.write_status()
        self.cv.notify_all()

    def fail(self, message):
        if self.failure is None:
            self.failure = message
            self.write_status()
        self.cv.notify_all()

    @property
    def finished(self):
        return self.cursor >= len(self.entries)

    def until_inbound_delivered(self):
        with self.cv:
            while True:
                if self.failure:
                    raise RuntimeError(self.failure)
                entry = self.entry()
                if entry is None or entry["type"] != "emit_inbound":
                    return
                self.cv.wait()

    def expect_outbound(self, actual):
        with self.cv:
            entry = self.entry()
            frame = entry["frame"] if entry and entry["type"] == "expect_outbound" else None
            ids = dict(self.ids)
            conflict = False
            if frame and frame.get("type") in ("session.prompt", "session.promptAsync", "session.command") and actual.get("type") == frame["type"]:
                key = "id" if frame["type"] == "session.prompt" else "messageID"
                recorded = (frame.get("input") or {}).get(key) if isinstance(frame.get("input"), dict) else None
                live = (actual.get("input") or {}).get(key) if isinstance(actual.get("input"), dict) else None
                if isinstance(recorded, str) and isinstance(live, str) and recorded not in WILDCARDS and recorded not in ids:
                    conflict = live in ids.values()
                    if not conflict:
                        ids[recorded] = live
            expected = materialize(frame, ids) if frame else entry
            if conflict or frame is None or not matches(expected, actual):
                message = "frame mismatch at cursor %d in %s\n expected: %s\n actual:   %s" % (self.cursor, self.scenario, json.dumps(expected)[:1500], json.dumps(actual)[:1500])
                self.fail(message)
                raise RuntimeError(message)
            self.ids = ids
            self.advance()

    def response(self, operation):
        with self.cv:
            while True:
                if self.failure:
                    raise RuntimeError(self.failure)
                entry = self.entry()
                if entry and entry["type"] == "emit_inbound" and entry["frame"]["type"] == "sdk.response" and entry["frame"]["operation"] == operation:
                    data = materialize(entry["frame"]["data"], self.ids)
                    self.advance()
                    return data
                if entry and entry["type"] == "runtime_exit":
                    message = "expected a response to %s at cursor %d in %s, found the server exit" % (operation, self.cursor, self.scenario)
                    self.fail(message)
                    raise RuntimeError(message)
                self.cv.wait()

    def next_listened(self, queue, alive):
        with self.cv:
            while True:
                if self.failure or not alive():
                    return None
                if queue:
                    return queue.pop(0)
                if self.exited and self.finished:
                    return None
                self.cv.wait(0.5)

    def next_event(self, alive):
        """The next recorded event, or None once the stream ended (a runtime exit) or the client went away."""
        gate = self.gate
        with self.cv:
            while True:
                if self.failure or not alive():
                    return None
                entry = self.entry()
                # A scenario can hold an event until it has done what happened meanwhile: the test writes to the gate's pipe.
                if gate and entry and entry.get("label") == gate["label"] and not self.gated:
                    self.gated = True
                    self.cv.release()
                    try:
                        with open(gate["fifo"]) as pipe:
                            pipe.read()
                    finally:
                        self.cv.acquire()
                    continue
                if entry and entry["type"] == "emit_inbound" and entry["frame"]["type"] == "sdk.event":
                    event = materialize(entry["frame"]["event"], self.ids)
                    self.advance()
                    for queue in self.listeners:
                        queue.append(event)
                    return event
                if entry and entry["type"] == "runtime_exit":
                    self.advance()
                    if entry.get("status") == "success":
                        self.exited = True
                        self.cv.notify_all()
                        return None
                    self.fail("the recording ends in a failed runtime exit")
                    return None
                self.cv.wait(0.5)


def operation_of(method, path, query, body):
    session = None
    parts = path.split("/")
    if len(parts) >= 4 and parts[1] == "api" and parts[2] == "session" and parts[3] not in ("active",):
        session = (parts[3], "/" + "/".join(parts[4:]) if len(parts) > 4 else "")
    if method == "GET" and path == "/api/event":
        return {"type": "event.subscribe"}
    if method == "GET" and path == "/api/model":
        return {"type": "model.list", "input": query}
    if method == "GET" and path == "/api/agent":
        return {"type": "agent.list", "input": query}
    if method == "POST" and path == "/api/session":
        return {"type": "session.create", "input": body}
    if method == "GET" and path == "/api/session/active":
        return {"type": "session.active"}
    if method == "GET" and path == "/api/command":
        return {"type": "command.list", "input": query}
    if method == "GET" and path == "/api/skill":
        return {"type": "skill.list", "input": query}
    if len(parts) == 5 and parts[:4] == ["", "api", "experimental", "mcp"] and method in ("PUT", "DELETE"):
        server = unquote(parts[4])
        return {"type": "mcp.add" if method == "PUT" else "mcp.remove", "input": {"server": server, **query, **(body or {})}}
    if len(parts) == 8 and parts[:3] == ["", "api", "experimental"] and parts[3] == "session" and parts[5:7] == ["instructions", "entries"] and method == "PUT":
        return {"type": "session.instructions.entry.put", "input": {"sessionID": parts[4], "key": unquote(parts[7]), **(body or {})}}
    if session:
        session_id, rest = session
        fields = {k: v for k, v in (body or {}).items() if v is not None}
        inp = {"sessionID": session_id, **query, **fields}
        table = {
            ("GET", ""): "session.get", ("DELETE", ""): "session.remove", ("PATCH", ""): "session.update",
            ("POST", "/prompt"): "session.prompt", ("POST", "/command"): "session.command", ("POST", "/compact"): "session.compact",
            ("POST", "/interrupt"): "session.interrupt", ("POST", "/model"): "session.switchModel", ("POST", "/agent"): "session.switchAgent",
            ("POST", "/move"): "session.move", ("POST", "/synthetic"): "session.synthetic", ("POST", "/fork"): "session.fork",
            ("POST", "/revert/stage"): "session.revert.stage", ("POST", "/revert/commit"): "session.revert.commit", ("DELETE", "/revert"): "session.revert.clear",
            ("GET", "/message"): "message.list", ("GET", "/permission"): "permission.list", ("GET", "/form"): "session.form.list",
        }
        if (method, rest) in table:
            return {"type": table[(method, rest)], "input": inp}
        sub = rest.split("/")
        if method == "DELETE" and len(sub) == 3 and sub[1] == "inbox":
            return {"type": "session.inbox.cancel", "input": {**inp, "inboxID": unquote(sub[2])}}
        if len(sub) >= 3 and sub[1] == "form":
            form = {**inp, "formID": sub[2]}
            if method == "POST" and len(sub) == 4 and sub[3] == "reply":
                return {"type": "session.form.reply", "input": form}
            if method == "DELETE" and len(sub) == 3:
                return {"type": "session.form.cancel", "input": form}
        if method == "POST" and len(sub) == 4 and sub[1] == "permission" and sub[3] == "reply":
            return {"type": "permission.reply", "input": {**inp, "requestID": sub[2]}}
    return {"type": "%s %s" % (method, path), "input": {**query, "body": body}}


# 1.x: the SDK's routes. A request names the SDK operation and its input (path, query and body fields together).
V1_ROUTES = [
    ("GET", "/session/status", "session.status"), ("POST", "/session/{sessionID}/command", "session.command"),
    ("POST", "/session/{sessionID}/summarize", "session.summarize"), ("POST", "/mcp/{name}/disconnect", "mcp.disconnect"),
    ("GET", "/provider", "provider.list"), ("GET", "/agent", "app.agents"), ("GET", "/skill", "app.skills"), ("GET", "/command", "command.list"),
    ("GET", "/session/{sessionID}", "session.get"), ("GET", "/session/{sessionID}/children", "session.children"),
    ("PATCH", "/session/{sessionID}", "session.update"), ("GET", "/session/{sessionID}/message", "session.messages"),
    ("POST", "/session/{sessionID}/prompt_async", "session.promptAsync"), ("POST", "/session/{sessionID}/abort", "session.abort"),
    ("POST", "/session/{sessionID}/revert", "session.revert"), ("POST", "/session/{sessionID}/unrevert", "session.unrevert"),
    ("POST", "/session/{sessionID}/fork", "session.fork"), ("POST", "/session", "session.create"),
    ("POST", "/permission/{requestID}/reply", "permission.reply"), ("POST", "/question/{requestID}/reply", "question.reply"),
    ("GET", "/mcp", "mcp.status"), ("POST", "/mcp", "mcp.add"), ("GET", "/event", "event.subscribe"),
]


def v1_operation(method, path, query, body, directory):
    for verb, template, name in V1_ROUTES:
        if verb != method:
            continue
        want, got = template.split("/"), path.split("/")
        if len(want) != len(got):
            continue
        params = {}
        for w, g in zip(want, got):
            if w.startswith("{"):
                params[w[1:-1]] = unquote(g)
            elif w != g:
                break
        else:
            if name == "event.subscribe":
                return {"type": name}
            inp = {**params}
            if directory and name != "session.create":
                inp["directory"] = directory
            inp.update({k: v for k, v in query.items() if k != "directory"})
            if isinstance(body, dict):
                inp.update(body)
            if name == "session.create" and directory:
                inp["directory"] = directory
            return {"type": name, "input": inp} if inp else {"type": name}
    return {"type": "%s %s" % (method, path), "input": {**query, "body": body}}


class Handler(BaseHTTPRequestHandler):
    server_version = "opencode-fake"

    def log_message(self, *args):
        pass

    def authorized(self):
        password = os.environ.get("OPENCODE_PASSWORD") or os.environ.get("OPENCODE_SERVER_PASSWORD")
        if not password:
            return True
        want = "Basic " + base64.b64encode(("opencode:" + password).encode()).decode()
        return self.headers.get("Authorization") == want

    def send_json(self, status, body):
        raw = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def handle_any(self):
        state = self.server.state
        url = urlparse(self.path)
        query = dict(parse_qsl(url.query, keep_blank_values=True))
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        body = json.loads(raw) if raw else None
        if not self.authorized():
            return self.send_json(401, {"_tag": "UnauthorizedError", "message": "unauthorized"})
        v2 = state["generation"] == "v2"
        # What identifies the server: the other generation's path answers its web UI as HTML.
        if url.path == "/api/info" or url.path == "/global/health":
            if (url.path == "/api/info") == v2:
                return self.send_json(200, {"version": state["version"], "pid": os.getpid()} if v2 else {"healthy": True, "version": state["version"]})
            raw = b"<html></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        controller = state["controller"]
        # The SDK client sends the directory as a header, and moves it into the query of a GET.
        directory = unquote(self.headers.get("x-opencode-directory") or "") or query.get("directory") or None
        if not v2 and state["validate"] and url.path != "/event":
            problem = check_request_v1(self.command, url.path, query, body)
            if problem:
                message = "schema violation: %s %s: %s" % (self.command, url.path, problem)
                controller.fail(message)
                return self.send_json(500, {"name": "SchemaViolation", "data": {"message": message[:400]}})
        if v2 and state["validate"]:
            problem = check_request(self.command, url.path, body)
            if problem:
                message = "schema violation: %s %s: %s" % (self.command, url.path, problem)
                controller.fail(message)
                return self.send_json(500, {"_tag": "SchemaViolation", "message": message[:400]})
        operation = operation_of(self.command, url.path, query, body) if v2 else v1_operation(self.command, url.path, query, body, directory)
        if os.environ.get("OPENCODE_FIXTURE_LOG"):
            with open(os.environ["OPENCODE_FIXTURE_LOG"], "a") as log:
                log.write(json.dumps({"op": operation["type"], "input": operation.get("input")}) + "\n")
        # Before the first subscription a status check is answered from the recording without consuming it.
        if not controller.subscribed and operation["type"] != "event.subscribe":
            canned = state["canned"].get(operation["type"])
            if canned is not None:
                return self.send_json(200, canned)
            if not controller.entries:
                return self.send_json(404, {"_tag": "NotFound", "message": "nothing is recorded"})
        try:
            if controller.exited and controller.finished:
                self.close_connection = True
                self.connection.shutdown(2)
                return
            if operation["type"] == "event.subscribe":
                with controller.cv:
                    extra = controller.primary
                    if extra:
                        queue = []
                        controller.listeners.append(queue)
                    else:
                        controller.primary = True
                        controller.exited = False
                        controller.subscribed = True
                if extra:
                    return self.listen(controller, queue)
                try:
                    return self.stream(controller, operation)
                finally:
                    with controller.cv:
                        controller.primary = False
            # With a second subscriber the discovery reads another actor makes are answered as the first's were.
            if controller.listeners:
                canned = state["canned"].get(operation["type"])
                if operation["type"] == "session.active":
                    # Another actor does not know the first one's sessions, so it asks the server whether one is busy.
                    canned = {"data": {}}
                entry = controller.entry()
                if canned is not None and not (entry and entry["type"] == "expect_outbound" and entry["frame"].get("type") == operation["type"]):
                    return self.send_json(200, canned)
            controller.until_inbound_delivered()
            controller.expect_outbound(operation)
            data = controller.response(operation["type"])
        except Exception as error:  # a mismatch: the replay is failed for good
            return self.send_json(500, {"_tag": "ReplayMismatch", "message": str(error)[:400]})
        if data == "<hang>":
            time.sleep(3600)
            return
        if state["validate"] and not (isinstance(data, dict) and set(data) == {"status", "body"}):
            problem = check_response(self.command, url.path, data)
            if problem:
                message = "schema violation in the answer to %s %s: %s" % (self.command, url.path, problem)
                controller.fail(message)
                return self.send_json(500, {"_tag": "SchemaViolation", "name": "SchemaViolation", "message": message[:400], "data": {"message": message[:400]}})
        if isinstance(data, dict) and set(data) == {"status", "body"}:
            return self.send_json(int(data["status"]), data["body"])
        return self.send_json(204 if data is None else 200, data)

    def stream(self, controller, operation):
        controller.expect_outbound(operation)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.flush()
        gone = threading.Event()

        def alive():
            return not gone.is_set()

        while True:
            event = controller.next_event(alive)
            if event is None:
                return
            try:
                self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                gone.set()
                return

    def listen(self, controller, queue):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.flush()
        gone = threading.Event()
        while True:
            event = controller.next_listened(queue, lambda: not gone.is_set())
            if event is None:
                return
            try:
                self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                gone.set()
                return

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = handle_any


def main():
    args = sys.argv[1:]
    transcript = None
    path = os.environ.get("OPENCODE_FIXTURE_TRANSCRIPT")
    if path:
        with open(path) as handle:
            transcript = json.load(handle)
    generation = os.environ.get("OPENCODE_FIXTURE_GENERATION") or ("v1" if transcript and transcript.get("protocol") == "opencode-sdk.sse" else "v2")
    version = os.environ.get("OPENCODE_FIXTURE_VERSION") or (transcript or {}).get("version") or ("1.14.39" if generation == "v1" else "2.0.18")
    if args[:1] == ["--version"]:
        print(version if generation == "v1" else "opencode v" + version)
        return
    if args[:1] != ["serve"]:
        print("unsupported", file=sys.stderr)
        sys.exit(2)
    if os.environ.get("OPENCODE_FIXTURE_STARTUP_FAIL"):
        # A server that cannot start says why on its way out, and the password it was given is in what it says.
        password = os.environ.get("OPENCODE_SERVER_PASSWORD") or os.environ.get("OPENCODE_PASSWORD") or ""
        print("starting", flush=True)
        print("fatal: cannot bind the port (password %s, Authorization: Bearer abcdef0123456789)" % password, file=sys.stderr, flush=True)
        sys.exit(1)
    host, port = "127.0.0.1", 0
    for arg in args[1:]:
        if arg.startswith("--hostname="):
            host = arg.split("=", 1)[1]
        if arg.startswith("--port="):
            port = int(arg.split("=", 1)[1])
    controller = Controller(transcript, os.environ.get("OPENCODE_FIXTURE_STATUS"))
    canned = {}
    for entry in controller.entries:
        if entry["type"] == "emit_inbound" and entry["frame"]["type"] == "sdk.response" and entry["frame"]["operation"] in ("model.list", "skill.list", "command.list", "agent.list", "provider.list"):
            canned.setdefault(entry["frame"]["operation"], entry["frame"]["data"])
    if generation == "v1":
        # What a 1.x status check reads, which its recordings (the SDK boundary of a turn) do not hold.
        models = {"gpt-5.4-mini": {"id": "gpt-5.4-mini", "name": "GPT 5.4 Mini", "variants": {"low": {}, "medium": {}, "high": {}}}, "gpt-5.4": {"id": "gpt-5.4", "name": "GPT 5.4", "variants": {}}}
        canned.setdefault("provider.list", {"all": [{"id": "openai", "name": "OpenAI", "source": "api", "env": [], "models": models}], "default": {"openai": "gpt-5.4-mini"}, "connected": ["openai"]})
        canned.setdefault("app.agents", [{"name": "build", "mode": "primary", "native": True}, {"name": "plan", "mode": "primary", "native": True}, {"name": "general", "mode": "subagent", "native": True}])
        canned.setdefault("app.skills", [])
        canned.setdefault("command.list", [{"name": "init", "description": "create or update AGENTS.md", "source": "command", "hints": ["$ARGUMENTS"]}])
    canned.update(json.loads(os.environ.get("OPENCODE_FIXTURE_CANNED") or "{}"))
    global SPEC_FILE, LENIENT_WILDCARDS
    if generation == "v1":
        SPEC_FILE = "opencode1-openapi.json"
        LENIENT_WILDCARDS = True
    server = ThreadingHTTPServer((host, port), Handler)
    server.daemon_threads = True
    server.state = {"controller": controller, "generation": generation, "version": version, "canned": canned, "validate": os.environ.get("OPENCODE_FIXTURE_NO_SCHEMA") is None}
    if os.environ.get("OPENCODE_FIXTURE_DEATHPIPE"):
        # A test reads this pipe to its end: that end is this process dying, however it dies.
        global deathpipe
        deathpipe = os.open(os.environ["OPENCODE_FIXTURE_DEATHPIPE"], os.O_WRONLY)
    if os.environ.get("OPENCODE_FIXTURE_PIDFILE"):
        with open(os.environ["OPENCODE_FIXTURE_PIDFILE"], "w") as pidfile:
            pidfile.write(str(os.getpid()))
    print("opencode server listening on http://%s:%d" % (host, server.server_address[1]), flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
