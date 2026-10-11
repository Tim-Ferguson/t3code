#!/usr/bin/env python3
"""Deterministic stand-in for the `pi` coding agent. It never contacts a network and never reads ~/.pi.

Three modes, chosen from what it is launched with:
  pi --version                       prints PI_FIXTURE_VERSION (default 1.0.0); exits PI_FIXTURE_VERSION_EXIT
  pi --mode rpc ... (PI_FIXTURE_SOCKET)
                                     a relay: every record the server writes is handed to the replay controller on
                                     the Unix socket (the test), and every record the controller answers is written
                                     to stdout. The controller plays a recorded session (`tests/pi_support`).
  pi --mode rpc ... (otherwise)      a small scripted Pi: sessions kept in PI_FIXTURE_SESSIONS, behaviour chosen by
                                     the prompt text (see `Script`). `--no-session` is the provider's discovery
                                     launch and answers from PI_FIXTURE_MODELS and PI_FIXTURE_COMMANDS.
Environment:
  PI_FIXTURE_LOG        file receiving one JSON line per launch and per record received
  PI_FIXTURE_SESSIONS   directory of session files (scripted mode)
  PI_FIXTURE_MODELS     JSON list of models `get_available_models` returns (default: one reasoning model)
  PI_FIXTURE_COMMANDS   JSON list of commands `get_commands` returns
  PI_FIXTURE_BLOCK      when set, `get_state` is never answered (a Pi that hangs on startup)
  PI_FIXTURE_VERSION_FILE / PI_FIXTURE_UPDATE_TO
                        the version `--version` prints is read from this file, and `pi update --self` writes the other value to it
  PI_FIXTURE_REPLY      when set, every prompt is answered with this text (a one-shot text generation reads it back)
  PI_FIXTURE_DISCOVERY_FAILS  when set, the discovery launch (`--no-session`) exits at once
"""
import json, os, signal, socket, sys, threading, time, uuid

ARGS = sys.argv[1:]
LOCK = threading.Lock()


def log(entry):
    path = os.environ.get("PI_FIXTURE_LOG")
    if path:
        with LOCK, open(path, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


def emit(record):
    with LOCK:
        sys.stdout.write(json.dumps(record) + "\n")
        sys.stdout.flush()


def current_version():
    path = os.environ.get("PI_FIXTURE_VERSION_FILE")
    if path and os.path.exists(path):
        return open(path).read().strip()
    return os.environ.get("PI_FIXTURE_VERSION", "1.0.0")


if ARGS == ["--version"]:
    print(current_version())
    sys.exit(int(os.environ.get("PI_FIXTURE_VERSION_EXIT", "0")))
if ARGS == ["update", "--self"]:
    log({"update": ARGS})
    if os.environ.get("PI_FIXTURE_UPDATE_TO") and os.environ.get("PI_FIXTURE_VERSION_FILE"):
        open(os.environ["PI_FIXTURE_VERSION_FILE"], "w").write(os.environ["PI_FIXTURE_UPDATE_TO"])
    sys.exit(0)

log({"launch": ARGS, "cwd": os.getcwd(), "home": os.environ.get("HOME"), "mcp": os.environ.get("T3_MCP_URL"), "token": os.environ.get("T3_MCP_BEARER_TOKEN"), "mode": os.environ.get("T3_PI_RUNTIME_MODE")})


if "--no-session" in ARGS and os.environ.get("PI_FIXTURE_DISCOVERY_FAILS"):
    sys.exit(1)


def relay():
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(os.environ["PI_FIXTURE_SOCKET"])
    connection.sendall((json.dumps({"type": "process_start", "args": ARGS}) + "\n").encode())

    def forward_input():
        for line in sys.stdin:
            line = line.rstrip("\n")
            if line:
                connection.sendall((line + "\n").encode())
        try:
            connection.shutdown(socket.SHUT_WR)
        except OSError:
            pass

    threading.Thread(target=forward_input, daemon=True).start()
    reader = connection.makefile("r", encoding="utf-8", newline="\n")
    for line in reader:
        with LOCK:
            sys.stdout.write(line)
            sys.stdout.flush()
    sys.exit(0)


MODEL = {"type": "chat", "id": "fake-model", "name": "Fake Model", "api": "openai-completions", "provider": "fake", "reasoning": True, "contextWindow": 100000, "thinkingLevelMap": {"off": "none", "minimal": None, "low": None}}


def mcp_tool_count(url, token):
    """What Pi's T3 extension does with the credential: initialize an MCP session and list the tools."""
    import urllib.request

    session = [None]

    def post(method, params=None, notification=False):
        body = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            body["params"] = params
        if not notification:
            body["id"] = 1
        headers = {"accept": "application/json, text/event-stream", "authorization": "Bearer " + token, "content-type": "application/json", "mcp-protocol-version": "2025-06-18"}
        if session[0]:
            headers["mcp-session-id"] = session[0]
        with urllib.request.urlopen(urllib.request.Request(url, json.dumps(body).encode(), headers, method="POST"), timeout=20) as response:
            session[0] = response.headers.get("mcp-session-id") or session[0]
            text = response.read().decode()
            kind = response.headers.get("content-type", "")
        if not text:
            return None
        if "text/event-stream" in kind:
            for line in text.split("\n"):
                if line.startswith("data:") and line[5:].strip():
                    return json.loads(line[5:].strip())
            return None
        return json.loads(text)

    try:
        post("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t3-pi-mcp", "version": "1.0.0"}})
        post("notifications/initialized", {}, notification=True)
        return len(post("tools/list", {})["result"]["tools"])
    except Exception as error:
        return -1 if os.environ.get("PI_FIXTURE_DEBUG") is None else (_ for _ in ()).throw(error)


class Script:
    """The scripted Pi. One instance serves one process."""

    def __init__(self):
        self.sessions = os.environ.get("PI_FIXTURE_SESSIONS") or os.getcwd()
        os.makedirs(self.sessions, exist_ok=True)
        self.models = json.loads(os.environ["PI_FIXTURE_MODELS"]) if "PI_FIXTURE_MODELS" in os.environ else [MODEL]
        self.commands = json.loads(os.environ.get("PI_FIXTURE_COMMANDS", "[]"))
        self.file = None
        self.entries = []
        self.name = None
        self.model = {"provider": self.models[0].get("provider"), "id": self.models[0].get("id")} if self.models else None  # Pi's configured default
        self.thinking = "high"
        self.streaming = False
        self.aborted = threading.Event()
        self.answers = {}
        self.answer_arrived = threading.Condition()
        self.counter = 0
        self.settled_after = None
        if "--no-session" not in ARGS:
            self.open_new()

    # -- session files: one JSON entry per line, a message tree flattened to its active branch --
    def open_new(self):
        self.counter = 0
        while True:
            self.counter += 1
            path = os.path.join(self.sessions, "session-%d.jsonl" % self.counter)
            if not os.path.exists(path):
                break
        self.file, self.entries = path, []
        self.save()

    def open_existing(self, path):
        self.file = path
        self.entries = [json.loads(line) for line in open(path)] if os.path.exists(path) else []

    def save(self):
        if self.file is None:  # --no-session: nothing is written
            return
        with open(self.file, "w") as handle:
            for entry in self.entries:
                handle.write(json.dumps(entry) + "\n")

    def entry_id(self):
        return uuid.uuid4().hex[:8]

    def leaf(self):
        return self.entries[-1]["id"] if self.entries else None

    def state(self):
        data = {"thinkingLevel": self.thinking, "isStreaming": self.streaming, "isCompacting": False, "pendingMessageCount": 0, "sessionFile": self.file, "sessionId": "00000000-0000-4000-8000-0000000000%02d" % self.counter}
        if self.model:
            data["model"] = next((m for m in self.models if m.get("provider") == self.model["provider"] and m.get("id") == self.model["id"]), self.models[0])
        return data

    def respond(self, request, data=None, success=True, error=None):
        record = {"type": "response", "command": request["type"], "success": success}
        if "id" in request:
            record["id"] = request["id"]
        if data is not None:
            record["data"] = data
        if error is not None:
            record["error"] = error
        emit(record)

    def handle(self, request):
        kind = request["type"]
        log({"received": request})
        if kind == "get_state":
            if os.environ.get("PI_FIXTURE_BLOCK"):
                return
            return self.respond(request, self.state())
        if kind == "get_available_models":
            return self.respond(request, {"models": self.models})
        if kind == "get_commands":
            return self.respond(request, {"commands": self.commands})
        if kind == "get_entries":
            since = request.get("since")
            entries = self.entries
            if since is not None:
                ids = [entry["id"] for entry in entries]
                entries = entries[ids.index(since) + 1:] if since in ids else entries
            return self.respond(request, {"entries": entries, "leafId": self.leaf()})
        if kind == "get_session_stats":
            return self.respond(request, {"sessionFile": self.file, "contextUsage": {"tokens": 1000 + 10 * len(self.entries), "contextWindow": 100000}, "tokens": {"input": 900, "output": 100, "cacheRead": 0}})
        if kind == "get_messages":
            return self.respond(request, {"messages": [entry["message"] for entry in self.entries if entry["type"] == "message"]})
        if kind == "get_last_assistant_text":
            replies = [entry["message"]["content"][0]["text"] for entry in self.entries if entry["message"]["role"] == "assistant"]
            return self.respond(request, {"text": replies[-1] if replies else None})
        if kind == "set_model":
            self.model = {"provider": request["provider"], "id": request["modelId"]}
            return self.respond(request, next((m for m in self.models if m.get("id") == request["modelId"]), self.models[0]))
        if kind == "set_thinking_level":
            self.thinking = request["level"]
            return self.respond(request)
        if kind == "set_session_name":
            self.name = request["name"]
            return self.respond(request)
        if kind == "switch_session":
            self.open_existing(request["sessionPath"])
            return self.respond(request, {"cancelled": False})
        if kind == "new_session":
            self.open_new()
            return self.respond(request, {"cancelled": False})
        if kind == "fork":
            ids = [entry["id"] for entry in self.entries]
            at = ids.index(request["entryId"])
            text = self.entries[at]["message"]["content"][0]["text"]
            kept = self.entries[:at]
            self.open_new()
            self.entries = kept
            self.save()
            return self.respond(request, {"text": text, "cancelled": False})
        if kind == "abort":
            self.aborted.set()
            return self.respond(request)
        if kind == "extension_ui_response":
            with self.answer_arrived:
                self.answers[request["id"]] = request
                self.answer_arrived.notify_all()
            return
        if kind in ("prompt", "compact"):
            threading.Thread(target=self.run, args=(request,), daemon=True).start()
            return
        self.respond(request, success=False, error="unsupported command " + kind)

    # -- one prompt --
    def say(self, role, text):
        entry = {"type": "message", "id": self.entry_id(), "parentId": self.leaf(), "message": {"role": role, "content": [{"type": "text", "text": text}]}}
        self.entries.append(entry)
        self.save()

    def dialog(self, method, **fields):
        request_id = "dialog-" + uuid.uuid4().hex[:12]
        emit(dict({"type": "extension_ui_request", "id": request_id, "method": method}, **fields))
        with self.answer_arrived:
            while request_id not in self.answers:
                self.answer_arrived.wait()
            return self.answers[request_id]

    def reply(self, text):
        emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
        emit({"type": "message_update", "usage": {"totalTokens": 0}, "assistantMessageEvent": {"type": "thinking_start", "contentIndex": 0}})
        emit({"type": "message_update", "usage": {"totalTokens": 0}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "Thinking about it."}})
        emit({"type": "message_update", "usage": {"totalTokens": 0}, "assistantMessageEvent": {"type": "text_start", "contentIndex": 1}})
        emit({"type": "message_update", "usage": {"totalTokens": 0}, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 1, "delta": text}})
        emit({"type": "message_update", "usage": {"totalTokens": 1500, "input": 1400, "output": 100, "cacheRead": 0}, "assistantMessageEvent": {"type": "thinking_end", "contentIndex": 0, "content": "Thinking about it."}})
        emit({"type": "message_update", "usage": {"totalTokens": 1500, "input": 1400, "output": 100, "cacheRead": 0}, "assistantMessageEvent": {"type": "text_end", "contentIndex": 1, "content": text}})
        message = {"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"}
        emit({"type": "message_end", "message": message})
        self.say("assistant", text)

    def tool(self, call_id, name, args, output, error=False, details=None):
        emit({"type": "tool_execution_start", "toolCallId": call_id, "toolName": name, "args": args})
        emit({"type": "tool_execution_end", "toolCallId": call_id, "toolName": name, "result": {"content": [{"type": "text", "text": output}], "details": details or {}}, "isError": error})

    def subagent(self, call_id):
        """The result shape of Pi's example subagent extension: one task running, then finished."""
        args = {"agent": "worker", "task": "survey the repository"}
        running = {"details": {"results": [{"agent": "worker", "task": "survey the repository", "step": 0, "messages": [{"role": "assistant", "content": [{"type": "text", "text": "looking"}]}]}]}, "content": []}
        done = {"details": {"results": [{"agent": "worker", "task": "survey the repository", "step": 0, "exitCode": 0, "stopReason": "stop", "model": "fake-model", "messages": [{"role": "assistant", "content": [{"type": "text", "text": "looking"}]}, {"role": "assistant", "content": [{"type": "text", "text": "found three files"}]}]}]}, "content": [{"type": "text", "text": "worker finished"}]}
        emit({"type": "tool_execution_start", "toolCallId": call_id, "toolName": "subagent", "args": args})
        emit({"type": "tool_execution_update", "toolCallId": call_id, "toolName": "subagent", "args": args, "partialResult": running})
        emit({"type": "tool_execution_end", "toolCallId": call_id, "toolName": "subagent", "result": done, "isError": False})

    def run(self, request):
        text = request.get("message", "")
        steering = request.get("streamingBehavior") == "steer"
        if self.streaming and not steering:
            return self.respond(request, success=False, error="Agent is already processing.")
        self.respond(request, {"disposition": "queued" if steering else "started"})
        if steering:
            return
        self.streaming, self.aborted = True, threading.Event()
        emit({"type": "agent_start"})
        emit({"type": "turn_start"})
        self.say("user", text)
        emit({"type": "message_start", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
        emit({"type": "message_end", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
        if request["type"] == "compact":
            emit({"type": "compaction_start", "reason": "manual"})
            emit({"type": "compaction_end", "reason": "manual", "aborted": False, "willRetry": False, "result": {"summary": "Summary of the conversation.", "tokensBefore": 5000, "estimatedTokensAfter": 800}})
            emit({"type": "response", "command": "compact", "success": True, "data": {}})
        elif os.environ.get("PI_FIXTURE_REPLY") is not None:
            self.reply(os.environ["PI_FIXTURE_REPLY"])
        elif text.startswith("approve edit"):
            answer = self.dialog("confirm", title="Allow edit?", message="edit notes.txt")
            if answer.get("confirmed"):
                self.tool("call-edit", "edit", {"path": "notes.txt", "oldText": "a", "newText": "b"}, "Edited notes.txt", details={"patch": "--- a/notes.txt\n+++ b/notes.txt\n-a\n+b"})
                self.reply("The file changed.")
            else:
                self.reply("The edit was declined.")
        elif text.startswith("approve"):
            answer = self.dialog("confirm", title="Allow bash?", message="echo approved")
            if answer.get("confirmed"):
                self.tool("call-approve", "bash", {"command": "echo approved"}, "approved\n", details={"exitCode": 0})
                self.reply("The command ran.")
            else:
                self.reply("The command was declined.")
        elif text.startswith("retry gives up"):
            emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
            emit({"type": "message_end", "message": {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "overloaded"}})
            emit({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 2, "delayMs": 50, "errorMessage": "overloaded"})
            emit({"type": "auto_retry_start", "attempt": 2, "maxAttempts": 2, "delayMs": 100, "errorMessage": "overloaded"})
            emit({"type": "auto_retry_end", "success": False, "attempt": 2, "finalError": "gave up after 2 attempts"})
        elif text.startswith("retry recovers"):
            emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
            emit({"type": "message_end", "message": {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "overloaded"}})
            emit({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 50, "errorMessage": "overloaded"})
            emit({"type": "auto_retry_end", "success": True, "attempt": 1})
            self.reply("Recovered.")
        elif text.startswith("notices"):
            emit({"type": "extension_ui_request", "id": "n1", "method": "notify", "message": "heads up", "notifyType": "warning"})
            emit({"type": "extension_ui_request", "id": "n2", "method": "setStatus", "statusKey": "x", "statusText": "ignored"})
            emit({"type": "extension_error", "extensionPath": "/home/u/.pi/ext/guard.ts", "event": "tool_call", "error": "boom"})
            self.reply("Done.")
        elif text.startswith("delegate"):
            self.subagent("call-sub")
            self.reply("The worker is done.")
        elif text.startswith("ask"):
            answer = self.dialog("select", title="Pick one", options=["One", "Two"])
            self.reply("You chose " + str(answer.get("value", "nothing")))
        elif text.startswith("hang"):
            emit({"type": "tool_execution_start", "toolCallId": "call-hang", "toolName": "bash", "args": {"command": "sleep 60"}})
            self.aborted.wait()
            emit({"type": "tool_execution_end", "toolCallId": "call-hang", "toolName": "bash", "result": {"content": [{"type": "text", "text": "Command aborted"}], "details": {}}, "isError": True})
        elif text.startswith("remember"):
            earlier = len([entry for entry in self.entries if entry["message"]["role"] == "user"]) - 1
            self.reply("I remember %d earlier messages." % earlier)
        elif text.startswith("mcp"):
            self.reply("MCP tools: %d" % mcp_tool_count(os.environ.get("T3_MCP_URL"), os.environ.get("T3_MCP_BEARER_TOKEN")))
        elif text.startswith("crash"):
            os._exit(3)
        else:
            self.reply("Echo: " + text)
        emit({"type": "turn_end", "message": {"role": "assistant", "content": []}, "toolResults": []})
        emit({"type": "agent_end", "messages": [], "willRetry": False})
        self.streaming = False
        emit({"type": "agent_settled"})


def serve():
    script = Script()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        script.handle(json.loads(line))


signal.signal(signal.SIGPIPE, signal.SIG_DFL)
if "PI_FIXTURE_SOCKET" in os.environ and "--no-session" not in ARGS:
    relay()
else:
    serve()
