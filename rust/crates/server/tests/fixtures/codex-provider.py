#!/usr/bin/env python3
"""Deterministic test-only Codex app-server. Never runs tools or reads agent state.

Use binaryPath pointing to this executable in isolated native settings. Prompts
containing [approval], [question], or [hold] exercise the corresponding runtime
branches; otherwise one tool and an assistant response complete immediately.
"""
import json
import os
import sys
import socket
import threading

threads = {}
turns = {}
requests = {}
background = {}
serial = 0


def send(message):
    print(json.dumps(message), flush=True)


def notify(method, params):
    send({"method": method, "params": params})


def finish(turn, status="completed"):
    turn["status"] = status
    notify("turn/completed", {"threadId": turn["threadId"], "turn": {
        "id": turn["id"], "status": status, "items": [], "error": None,
        "startedAt": 1800000000, "completedAt": 1800000001,
    }})


def assistant(turn, text):
    item = {"id": turn["id"] + "-assistant", "type": "agentMessage", "text": "", "phase": "final_answer"}
    params = {"threadId": turn["threadId"], "turnId": turn["id"], "item": item}
    notify("item/started", params)
    if "[crash]" in text:
        os._exit(9)
    notify("item/agentMessage/delta", {"threadId": turn["threadId"], "turnId": turn["id"], "itemId": item["id"], "delta": text})
    item["text"] = text
    notify("item/completed", params)


SCRIPTED = ("[think]", "[todo]", "[plan]", "[usage]", "[retry]", "[usage-limit]", "[mcp-tool]", "[mcp-app]", "[file]", "[permissions]", "[permissions-read]", "[elicit]", "[exec-legacy]", "[patch-legacy]", "[withdraw]")
# The MCP App the fake `widget_server` serves, and the answers it gives the app-server's MCP requests.
MCP_APP_URI = "ui://widget/app.html"
MCP_APP_TOOLS = {
    "render_widget": {"name": "render_widget", "description": "Renders the widget", "inputSchema": {"type": "object"}, "annotations": {"title": "Render widget", "readOnlyHint": True}, "_meta": {"ui": {"resourceUri": MCP_APP_URI, "visibility": ["model", "app"]}}},
    "model_only": {"name": "model_only", "description": "Hidden from apps", "inputSchema": {"type": "object"}, "_meta": {"ui": {"visibility": ["model"]}}},
}
NEW_REQUEST_KINDS = ("file", "permissions", "elicit", "exec-legacy", "patch-legacy")


def record_response(kind, result):
    path = os.environ.get("FIXTURE_RESPONSE_RECORD")
    if path:
        with open(path, "a", encoding="utf-8") as record:
            record.write(json.dumps({"response": kind, "result": result}) + "\n")


def usage_update(turn, total, last):
    breakdown = lambda counts: {"totalTokens": counts[0] + counts[2], "inputTokens": counts[0], "cachedInputTokens": counts[1], "cacheWriteInputTokens": 0, "outputTokens": counts[2], "reasoningOutputTokens": counts[3]}
    notify("thread/tokenUsage/updated", {"threadId": turn["threadId"], "turnId": turn["id"], "tokenUsage": {"total": breakdown(total), "last": breakdown(last), "modelContextWindow": 200000}})


def file_item(turn, status, changes=True):
    return {"id": turn["id"] + "-file", "type": "fileChange", "status": status, "changes": [{"path": "generated.txt", "kind": {"type": "add"}, "diff": "fixture\n"}] if changes else []}


def ask(turn, kind, method, params, item=None):
    request_id = turn["id"] + "-" + kind
    requests[request_id] = (turn, kind, item)
    send({"id": request_id, "method": method, "params": dict(params, threadId=turn["threadId"]) if "conversationId" not in params else params})
    return request_id


def scripted(turn):
    """Tagged prompts that exercise the app-server surface the plain script does not: reasoning,
    plans, usage, errors and every approval kind. Approval requests honour the turn's
    approvalPolicy the way Codex does: under `never` nothing is asked and the work just happens."""
    thread_id, turn_id, text = turn["threadId"], turn["id"], turn["text"]
    base = {"threadId": thread_id, "turnId": turn_id}
    if "[think]" in text:
        item = {"id": turn_id + "-reasoning", "type": "reasoning", "summary": [], "content": []}
        notify("item/started", dict(base, item=item))
        for index, part in enumerate(["**Reading the task**", "**Planning the change**"]):
            notify("item/reasoning/summaryPartAdded", dict(base, itemId=item["id"], summaryIndex=index))
            notify("item/reasoning/summaryTextDelta", dict(base, itemId=item["id"], summaryIndex=index, delta=part[:6]))
            notify("item/reasoning/summaryTextDelta", dict(base, itemId=item["id"], summaryIndex=index, delta=part[6:]))
        item["summary"] = ["**Reading the task**", "**Planning the change**"]
        notify("item/completed", dict(base, item=item))
    if "[todo]" in text:
        notify("turn/plan/updated", dict(base, explanation="Fixture steps", plan=[{"step": "Read", "status": "inProgress"}, {"step": "Write", "status": "pending"}]))
        notify("turn/plan/updated", dict(base, explanation="Fixture steps", plan=[{"step": "Read", "status": "completed"}, {"step": "Write", "status": "completed"}]))
    if "[plan]" in text:
        item = {"id": turn_id + "-plan", "type": "plan", "text": ""}
        notify("item/started", dict(base, item=item))
        for delta in ["1. Do the ", "work.\n"]:
            notify("item/plan/delta", dict(base, itemId=item["id"], delta=delta))
        if "[plan-open]" not in text:
            item["text"] = "1. Do the work.\n"
            notify("item/completed", dict(base, item=item))
    if "[usage]" in text:
        usage_update(turn, (100, 40, 10, 2), (100, 40, 10, 2))
        usage_update(turn, (260, 90, 30, 5), (160, 50, 20, 3))
    if "[retry]" in text:
        notify("error", dict(base, willRetry=True, error={"message": "Reconnecting... 1/5", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": None}}, "additionalDetails": "stream disconnected"}))
        notify("error", dict(base, willRetry=True, error={"message": "Reconnecting... 2/5", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": None}}, "additionalDetails": "stream disconnected"}))
    if "[usage-limit]" in text:
        notify("account/rateLimits/updated", {"rateLimits": {"limitId": "codex", "planType": "plus", "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": int(os.environ.get("FIXTURE_USAGE_RESET_AT", "1800003600"))}, "secondary": None}})
        error = {"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded", "additionalDetails": None}
        notify("error", dict(base, willRetry=False, error=error))
        notify("turn/completed", {"threadId": thread_id, "turn": {"id": turn_id, "status": "failed", "items": [], "error": error, "startedAt": 1800000000, "completedAt": 1800000001}})
        return
    if "[mcp-tool]" in text:
        mcp = {"type": "mcpToolCall", "id": turn_id + "-mcp", "server": "github_app", "tool": "create_issue", "status": "inProgress", "arguments": {"title": "Fixture"}, "result": None, "error": None, "durationMs": None}
        notify("item/started", dict(base, item=mcp))
        notify("item/completed", dict(base, item=dict(mcp, status="completed", result={"content": [{"type": "text", "text": "created"}], "structuredContent": None})))
        dynamic = {"type": "dynamicToolCall", "id": turn_id + "-dynamic", "namespace": "fixture", "tool": "lookup", "status": "inProgress", "arguments": {"q": 1}, "contentItems": None, "success": None, "durationMs": None}
        notify("item/started", dict(base, item=dynamic))
        notify("item/completed", dict(base, item=dict(dynamic, status="completed", contentItems=[{"type": "inputText", "text": "found"}], success=True)))
    if "[mcp-app]" in text:
        # A tool whose result names a `ui://` document: the port reads it through `mcpServer/resource/read`.
        app = {"type": "mcpToolCall", "id": turn_id + "-app", "server": "widget_server", "tool": "render_widget", "status": "inProgress", "arguments": {"name": "fixture"}, "result": None, "error": None, "durationMs": None, "mcpAppUi": {"resourceUri": MCP_APP_URI}}
        notify("item/started", dict(base, item=app))
        notify("item/completed", dict(base, item=dict(app, status="completed", result={"content": [{"type": "text", "text": "rendered"}], "structuredContent": {"widget": "fixture"}})))
    never = turn.get("policy") == "never"
    if any(tag in text for tag in ("[file]", "[withdraw]")):
        item = file_item(turn, "inProgress", changes=False)
        notify("item/started", dict(base, item=item))
        if never:
            notify("item/completed", dict(base, item=file_item(turn, "completed")))
        else:
            request_id = ask(turn, "file", "item/fileChange/requestApproval", dict(base, itemId=item["id"], reason="Write the fixture file", grantRoot=None), item)
            if "[withdraw]" in text:
                # Codex decides without the user (a stop, a policy change): the request is withdrawn.
                requests.pop(request_id)
                notify("serverRequest/resolved", {"threadId": thread_id, "requestId": request_id})
                notify("item/completed", dict(base, item=file_item(turn, "failed", changes=False)))
            else:
                return
    elif "[permissions]" in text or "[permissions-read]" in text:
        if not never:
            access = {"read": ["/tmp/fixture-input"]} if "[permissions-read]" in text else {"write": ["/tmp/fixture-output"]}
            ask(turn, "permissions", "item/permissions/requestApproval", dict(base, itemId=turn_id + "-permissions", reason="Reach outside the workspace", permissions={"fileSystem": access, "network": None}))
            return
    elif "[elicit]" in text:
        schema = {"type": "object", "properties": {"decision": {"type": "string", "oneOf": [{"const": "allow_once", "title": "Once"}, {"const": "allow_session", "title": "This session"}]}}, "required": ["decision"]}
        ask(turn, "elicit", "mcpServer/elicitation/request", {"threadId": thread_id, "turnId": turn_id, "serverName": "fixture-mcp", "mode": "form", "message": "Allow ChatGPT to use Fixture?", "_meta": {"persist": ["session"]}, "requestedSchema": schema})
        return
    elif "[exec-legacy]" in text:
        ask(turn, "exec-legacy", "execCommandApproval", {"conversationId": thread_id, "callId": turn_id + "-call", "command": ["ls", "-la"], "cwd": os.getcwd(), "reason": None, "parsedCmd": []})
        return
    elif "[patch-legacy]" in text:
        ask(turn, "patch-legacy", "applyPatchApproval", {"conversationId": thread_id, "callId": turn_id + "-call", "fileChanges": {"b.txt": {"type": "add", "content": "x"}, "a.txt": {"type": "delete", "content": "y"}}, "reason": None, "grantRoot": None})
        return
    if "[hold]" in text:
        return
    assistant(turn, "Fixture scripted: " + text)
    finish(turn)


def compaction(turn):
    base = {"threadId": turn["threadId"], "turnId": turn["id"]}
    notify("turn/started", {"threadId": turn["threadId"], "turn": {"id": turn["id"], "status": "inProgress", "items": [], "error": None, "startedAt": 1800000000, "completedAt": None}})
    item = {"id": turn["id"] + "-compaction", "type": "contextCompaction"}
    notify("item/started", dict(base, item=item))
    notify("item/completed", dict(base, item=item))
    finish(turn)


def scripted_response(turn, kind, item, result):
    record_response(kind, result)
    decision = result.get("decision")
    granted = (
        (kind == "file" and decision in ("accept", "acceptForSession"))
        or (kind == "permissions" and bool(result.get("permissions")))
        or (kind == "elicit" and result.get("action") == "accept")
        or (kind in ("exec-legacy", "patch-legacy") and decision in ("approved", "approved_for_session"))
    )
    stopped = decision in ("cancel", "abort") or result.get("action") == "cancel"
    if item is not None:
        done = file_item(turn, "completed") if granted else file_item(turn, "declined", changes=False)
        notify("item/completed", {"threadId": turn["threadId"], "turnId": turn["id"], "item": done})
    assistant(turn, "Fixture " + kind + (" granted" if granted else " refused"))
    finish(turn, "interrupted" if stopped else "completed")


def start_turn(turn):
    thread_id, turn_id, text = turn["threadId"], turn["id"], turn["text"]
    notify("turn/started", {"threadId": thread_id, "turn": {
        "id": turn_id, "status": "inProgress", "items": [], "error": None,
        "startedAt": 1800000000, "completedAt": None,
    }})
    if "[finish-background]" in text:
        # A command an earlier turn left running ends now, reported against that earlier turn.
        for old_turn, old_tool in list(background.values()):
            if old_tool["type"] == "dynamicToolCall":
                old_tool.update({"status": "completed", "success": True, "contentItems": [{"type": "inputText", "text": "watching"}], "durationMs": 5})
            else:
                old_tool.update({"status": "completed", "aggregatedOutput": "ready\ndone\n", "exitCode": 0, "durationMs": 5})
            notify("item/completed", {"threadId": old_turn["threadId"], "turnId": old_turn["id"], "item": old_tool})
        background.clear()
    if "[persistent-tool]" in text:
        # A tool that is meant to outlive the turn that started it.
        tool = {"id": turn_id + "-watch", "type": "dynamicToolCall", "namespace": None, "tool": "watch", "arguments": {"persistent": True}, "status": "inProgress", "contentItems": None, "success": None, "durationMs": None}
        notify("item/started", {"threadId": thread_id, "turnId": turn_id, "item": tool})
        background[turn_id + "-watch"] = (turn, tool)
        assistant(turn, "Fixture response: " + text)
        finish(turn)
        return
    if "[background]" in text:
        tool = {"id": turn_id + "-dev", "type": "commandExecution", "command": "bun run dev", "cwd": os.getcwd(), "status": "inProgress", "commandActions": [], "aggregatedOutput": None, "exitCode": None, "durationMs": None, "processId": turn_id + "-process"}
        notify("item/started", {"threadId": thread_id, "turnId": turn_id, "item": tool})
        notify("item/commandExecution/outputDelta", {"threadId": thread_id, "turnId": turn_id, "itemId": tool["id"], "delta": "ready\n"})
        background[tool["processId"]] = (turn, tool)
        assistant(turn, "Fixture response: " + text)
        finish(turn)
        return
    if any(tag in text for tag in SCRIPTED):
        scripted(turn)
        return
    tool = {"id": turn_id + "-tool", "type": "commandExecution", "command": "printf 'fixture tool output'", "cwd": os.getcwd(), "status": "inProgress", "commandActions": [], "aggregatedOutput": None, "exitCode": None, "durationMs": None, "processId": None}
    params = {"threadId": thread_id, "turnId": turn_id, "item": tool}
    notify("item/started", params)
    if "[approval]" in text:
        request_id = 1001 if os.environ.get("FIXTURE_NUMERIC_REQUEST_IDS") else turn_id + "-approval"
        requests[request_id] = (turn, "approval", tool)
        send({"id": request_id, "method": "item/commandExecution/requestApproval", "params": {"threadId": thread_id, "turnId": turn_id, "itemId": tool["id"], "approvalId": request_id, "reason": "Fixture approval", "command": tool["command"], "cwd": os.getcwd(), "availableDecisions": ["accept", "decline", "cancel"]}})
        return
    complete_tool(turn, tool)
    if "[question]" in text:
        request_id = 1002 if os.environ.get("FIXTURE_NUMERIC_REQUEST_IDS") else turn_id + "-question"
        requests[request_id] = (turn, "question", None)
        send({"id": request_id, "method": "item/tool/requestUserInput", "params": {"threadId": thread_id, "turnId": turn_id, "itemId": tool["id"], "questions": [{"id": "choice", "header": "Fixture", "question": "Choose a fixture response", "isOther": False, "isSecret": False, "options": [{"label": "One", "description": "First response"}, {"label": "Two", "description": "Second response"}]}]}})
    else:
        assistant(turn, "Fixture response: " + text)
        if "[hold]" not in text:
            finish(turn)


def complete_tool(turn, tool):
    notify("item/commandExecution/outputDelta", {"threadId": turn["threadId"], "turnId": turn["id"], "itemId": tool["id"], "delta": "fixture tool output\n"})
    tool.update({"status": "completed", "aggregatedOutput": "fixture tool output\n", "exitCode": 0, "durationMs": 1})
    notify("item/completed", {"threadId": turn["threadId"], "turnId": turn["id"], "item": tool})



class Replay:
    """Serves a recorded Codex app-server transcript (CODEX_FIXTURE_TRANSCRIPT, the original's
    `emit_inbound`/`expect_outbound` ndjson) once the client opens a thread. Account, model and
    skill discovery keep using the scripted handlers below, so one executable serves both the
    health probe and the session. Frames the client sends are compared with the recording after
    the same normalisation the original's replay applies; a difference is written to
    CODEX_FIXTURE_LOG and ends the process, which fails the run under test.

    Deliberate differences from the original's replay, because the Rust client is not the
    original client: request ids are not compared (the recorded response ids are rewritten to the
    ids the client used), the MCP Apps `extensions` capability of `initialize` is not compared,
    empty `text_elements` on turn/start and turn/steer text input (the schema default), and turn/start runtime policy fields (approvalPolicy, approvalsReviewer, sandboxPolicy) are
    not compared because the recordings used a test-only policy override the port has no
    equivalent of; the mapping from runtime mode to those fields is covered by unit tests."""

    THREAD_METHODS = ("thread/start", "thread/resume", "thread/fork")

    def __init__(self, path):
        lines = [json.loads(line) for line in open(path) if line.strip()]
        self.entries = [entry for entry in lines if entry.get("type") != "transcript_start"]
        # `metadata.recorderThreadConfigKeys` are thread config keys of the recorder, not of the
        # client; the original's replay ignores them when it compares thread requests.
        header = next((entry for entry in lines if entry.get("type") == "transcript_start"), {})
        self.ignored_config = set((header.get("metadata") or {}).get("recorderThreadConfigKeys") or [])
        # The subagent threads the recording names. The original's replay answers the metadata
        # reads of these in process (`withCodexReplayChildMetadata`), so they are not recorded
        # frames: `thread/read` without turns and `thread/resume` that excludes them, answered
        # with a thread that names no model.
        self.child_threads = set()
        for entry in self.entries:
            item = ((entry.get("frame") or {}).get("params") or {}).get("item") if entry.get("type") == "emit_inbound" else None
            if isinstance(item, dict):
                if item.get("type") == "subAgentActivity" and isinstance(item.get("agentThreadId"), str):
                    self.child_threads.add(item["agentThreadId"])
                if item.get("type") == "collabAgentToolCall":
                    self.child_threads.update(thread for thread in item.get("receiverThreadIds") or [] if isinstance(thread, str))
        # The recording is one stream, but the port serves each thread from a process of its own. The
        # stream is cut into segments, a client frame and what the server sent in answer, and a
        # process serves the segments of the native thread it opened (see `activate`). A recording of
        # one thread is one owner and is replayed whole.
        self.segments = []
        for entry in self.entries:
            if entry["type"] == "expect_outbound" or not self.segments:
                self.segments.append({"method": (entry.get("frame") or {}).get("method"), "frame": entry.get("frame"), "entries": [], "owner": None})
            self.segments[-1]["entries"].append(entry)
        for segment in self.segments:
            params = (segment["frame"] or {}).get("params") or {}
            if segment["method"] in self.THREAD_METHODS:
                for entry in segment["entries"]:
                    thread = ((entry.get("frame") or {}).get("result") or {}).get("thread") if entry["type"] == "emit_inbound" else None
                    if isinstance(thread, dict) and isinstance(thread.get("id"), str):
                        segment["owner"] = thread["id"]
                        break
                else:
                    segment["owner"] = params.get("threadId")
        self.owners = {segment["owner"] for segment in self.segments if segment["owner"]}
        for segment in self.segments:
            if segment["method"] not in self.THREAD_METHODS:
                owner = ((segment["frame"] or {}).get("params") or {}).get("threadId")
                segment["owner"] = owner if owner in self.owners else None
        self.cursor = 0
        self.active = False
        self.ids = {}
        self.initialize = None
        self.workspace = os.environ.get("CODEX_FIXTURE_WORKSPACE")
        # CODEX_FIXTURE_GATE_LABEL holds the recording back at the frame with that label until
        # something is written to the named pipe CODEX_FIXTURE_GATE_FIFO (the original's
        # `release_replay_gate`). The release blocks on the pipe, so nothing here polls.
        self.gate = os.environ.get("CODEX_FIXTURE_GATE_LABEL")
        self.lock = threading.RLock()
        fifo = os.environ.get("CODEX_FIXTURE_GATE_FIFO")
        if self.gate and fifo:
            threading.Thread(target=self.wait_for_release, args=(fifo,), daemon=True).start()
        else:
            self.gate = None

    def wait_for_release(self, fifo):
        with open(fifo) as pipe:
            pipe.read()
        with self.lock:
            self.gate = None
            self.drain()

    def record(self, entry):
        path = os.environ.get("CODEX_FIXTURE_LOG")
        if path:
            with open(path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(entry) + "\n")

    def fail(self, reason, expected, actual):
        self.record({"mismatch": reason, "cursor": self.cursor, "expected": expected, "actual": actual})
        sys.stderr.write("codex replay mismatch at %d: %s\n" % (self.cursor, reason))
        sys.stderr.flush()
        os._exit(7)

    def workspace_value(self, value):
        if isinstance(value, str):
            return self.workspace if value == "<workspace>" and self.workspace else value
        if isinstance(value, list):
            return [self.workspace_value(item) for item in value]
        if isinstance(value, dict):
            return {key: self.workspace_value(item) for key, item in value.items()}
        return value

    def normalize(self, frame, client):
        frame = json.loads(json.dumps(self.workspace_value(frame)))
        method, params = frame.get("method"), frame.get("params")
        if client and method and "id" in frame:
            del frame["id"]
        if method == "initialize" and isinstance(params, dict):
            if isinstance(params.get("clientInfo"), dict):
                params["clientInfo"]["version"] = "<ignored>"
            if isinstance(params.get("capabilities"), dict):
                params["capabilities"].pop("extensions", None)
        if method == "turn/start" and isinstance(params, dict):
            for key in ("approvalPolicy", "approvalsReviewer", "sandboxPolicy"):
                params.pop(key, None)
            mode = params.get("collaborationMode")
            if isinstance(mode, dict) and isinstance(mode.get("settings"), dict):
                mode["settings"]["developer_instructions"] = "<ignored>"
            params.pop("additionalContext", None)
            # The Rust client names the schema default (empty text_elements) explicitly.
            for part in params.get("input") or []:
                if isinstance(part, dict) and part.get("text_elements") == []:
                    del part["text_elements"]
        if method == "turn/steer" and isinstance(params, dict):
            for part in params.get("input") or []:
                if isinstance(part, dict) and part.get("text_elements") == []:
                    del part["text_elements"]
        if method in self.THREAD_METHODS and isinstance(params, dict):
            params.pop("cwd", None)
            params.pop("model", None)
            if isinstance(params.get("config"), dict):
                params["config"].pop("mcp_servers", None)
                for key in self.ignored_config:
                    params["config"].pop(key, None)
        return json.dumps(frame, sort_keys=True)

    def emit(self, entry):
        frame = dict(entry["frame"])
        if "method" not in frame and frame.get("id") in self.ids:
            frame["id"] = self.ids[frame["id"]]
        send(frame)

    def drain(self):
        while self.cursor < len(self.entries):
            entry = self.entries[self.cursor]
            if entry["type"] == "emit_inbound" and self.gate is not None and entry.get("label") == self.gate:
                return
            if entry["type"] == "emit_inbound":
                self.emit(entry)
                self.cursor += 1
            elif entry["type"] == "runtime_exit":
                self.cursor += 1
                if entry.get("status") == "success":
                    # The original ends the app-server's output here; stay readable until the
                    # client closes its end so late client frames do not break the pipe.
                    self.finished = True
                return
            else:
                return

    def claimed(self):
        path = os.environ.get("CODEX_FIXTURE_CLAIMS")
        if path and os.path.exists(path):
            return {int(line) for line in open(path) if line.strip()}
        return set(getattr(self, "local_claims", set()))

    def claim(self, index):
        path = os.environ.get("CODEX_FIXTURE_CLAIMS")
        if path:
            with open(path, "a", encoding="utf-8") as handle:
                handle.write(str(index) + "\n")
        self.local_claims = set(getattr(self, "local_claims", set())) | {index}

    def activate(self, message):
        """Serves from the first recorded request of this method that no process has taken: the
        segments of that thread from there on. With several threads in the recording the process
        keeps to its own native thread and the others' frames are never sent by it."""
        taken = self.claimed()
        method = message.get("method")
        first = next((index for index, segment in enumerate(self.segments) if segment["method"] == method and index not in taken), None)
        if first is None:
            first = next((index for index, segment in enumerate(self.segments) if segment["method"] == method), None)
        if first is None:
            self.fail("no recorded " + str(method), None, message)
        self.claim(first)
        owner = self.segments[first]["owner"]
        mine = [segment for index, segment in enumerate(self.segments) if index >= first and (len(self.owners) <= 1 or owner is None or segment["owner"] in (owner, None))]
        self.entries = [entry for segment in mine for entry in segment["entries"]]
        self.cursor = 0
        self.active = True
        first = next((entry for entry in self.entries if entry["type"] == "expect_outbound" and entry["frame"].get("method") == "initialize"), None)
        if first is not None and self.initialize is not None and self.normalize(first["frame"], True) != self.normalize(self.initialize, True):
            self.fail("initialize differs", first["frame"], self.initialize)

    def child_metadata(self, message):
        """The in-process answer for a subagent thread's metadata read, or None."""
        params = message.get("params") or {}
        method = message.get("method")
        if params.get("threadId") not in self.child_threads or "id" not in message:
            return None
        if (method == "thread/read" and params.get("includeTurns") is False) or (method == "thread/resume" and params.get("excludeTurns") is True):
            return {"id": message["id"], "result": {"thread": {"id": params["threadId"]}, "model": None}}
        return None

    def handle(self, message):
        with self.lock:
            answer = self.child_metadata(message)
            if answer is not None:
                send(answer)
                return
            self.handle_locked(message)

    def handle_locked(self, message):
        if not self.active:
            self.activate(message)
        self.drain()
        if self.cursor >= len(self.entries) or self.entries[self.cursor]["type"] != "expect_outbound":
            self.fail("unexpected client frame", self.entries[self.cursor] if self.cursor < len(self.entries) else None, message)
        entry = self.entries[self.cursor]
        if self.normalize(entry["frame"], "method" in message) != self.normalize(message, "method" in message):
            self.fail("client frame differs from the recording", entry["frame"], message)
        if message.get("method") and "id" in message:
            self.ids[entry["frame"]["id"]] = message["id"]
        self.cursor += 1
        self.drain()


REPLAY = Replay(os.environ["CODEX_FIXTURE_TRANSCRIPT"]) if os.environ.get("CODEX_FIXTURE_TRANSCRIPT") else None

if os.environ.get("FIXTURE_LAUNCH_RECORD"):
    with open(os.environ["FIXTURE_LAUNCH_RECORD"], "a", encoding="utf-8") as record:
        record.write(json.dumps({"argv": sys.argv[1:], "accessToken": os.environ.get("ACCESS_TOKEN"), "codexHome": os.environ.get("CODEX_HOME"), "openaiKey": os.environ.get("OPENAI_API_KEY")}) + "\n")
for line in sys.stdin:
    message = json.loads(line)
    if REPLAY is not None:
        if message.get("method") == "initialize":
            REPLAY.initialize = message
        if REPLAY.active and message.get("method") == "thread/inject_items" and "id" in message:
            # History the port injects into a native thread is not in a recording made before
            # injection existed: it is answered, and written down for the test to look at.
            path = os.environ.get("CODEX_FIXTURE_INJECT_LOG")
            if path:
                with open(path, "a", encoding="utf-8") as handle:
                    handle.write(json.dumps(message.get("params")) + "\n")
            send({"id": message["id"], "result": {}})
            continue
        if REPLAY.active or message.get("method") in Replay.THREAD_METHODS:
            REPLAY.handle(message)
            continue
    request_id = message.get("id")
    method = message.get("method")
    params = message.get("params") or {}
    if os.environ.get("FIXTURE_RPC_RECORD") and method:
        with open(os.environ["FIXTURE_RPC_RECORD"], "a", encoding="utf-8") as record:
            record.write(json.dumps({"method": method, "params": params}) + "\n")
    if method is None:
        pending = requests.pop(request_id, None)
        if pending:
            turn, kind, tool = pending
            result = message.get("result") or {}
            decision = result.get("decision")
            if kind in NEW_REQUEST_KINDS:
                scripted_response(turn, kind, tool, result)
                continue
            if kind == "question":
                assert set(result.get("answers", {})) <= {"choice"}
                for answer in result.get("answers", {}).values():
                    assert isinstance(answer, dict) and set(answer) == {"answers"}
                    assert isinstance(answer["answers"], list) and all(isinstance(item, str) for item in answer["answers"])
            elif kind == "approval":
                assert decision in ("accept", "acceptForSession", "decline", "cancel")
            if kind == "approval":
                if decision in ("accept", "acceptForSession"):
                    complete_tool(turn, tool)
                else:
                    tool["status"] = "declined"
                    notify("item/completed", {"threadId": turn["threadId"], "turnId": turn["id"], "item": tool})
            assistant(turn, "Fixture " + kind + " answered: " + json.dumps(result, sort_keys=True))
            finish(turn, "interrupted" if decision == "cancel" else "completed")
        continue
    if method == "initialized":
        continue
    result = None
    after_response = None
    if method == "initialize":
        result = {"userAgent": "codex/0.156.1 fixture", "platformFamily": "unix", "platformOs": sys.platform}
    elif method == "account/read":
        result = {"account": None, "requiresOpenaiAuth": False}
    elif method == "model/list":
        result = {"data": [{"id": "fixture-model", "model": "fixture-model", "displayName": "Fixture Model", "description": "Deterministic native tests", "hidden": False, "isDefault": True, "defaultReasoningEffort": "low", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "Fixture"}], "inputModalities": ["text"], "supportsPersonality": False}], "nextCursor": None}
        if os.environ.get("FIXTURE_SECOND_MODEL") == "1":
            alternative = dict(result["data"][0])
            alternative.update({"id": "fixture-model-alt", "model": "fixture-model-alt", "displayName": "Fixture Alternate", "isDefault": False})
            result["data"].append(alternative)
    elif method == "skills/list":
        result = {"data": [{"cwd": cwd, "skills": [], "errors": []} for cwd in params.get("cwds", [])]}
    elif method == "account/rateLimits/read" and os.environ.get("FIXTURE_RATE_LIMITS"):
        result = json.loads(os.environ["FIXTURE_RATE_LIMITS"])
    elif method == "account/rateLimits/read":
        result = {"rateLimits": {"limitId": "fixture", "limitName": "Fixture", "primary": None, "secondary": None, "credits": None, "planType": "unknown"}, "rateLimitsByLimitId": None, "rateLimitResetCredits": None}
    elif method == "thread/inject_items" and not os.environ.get("FIXTURE_NO_INJECT"):
        # History goes into the thread; the request is recorded with the others for the test to read.
        result = {}
    elif method in ("thread/start", "thread/resume", "thread/fork"):
        serial += 1
        # A fork is a new native thread cut from the one it names.
        native_id = ("fixture-fork-" + str(os.getpid()) + "-" + str(serial)) if method == "thread/fork" else (params.get("threadId") or "fixture-thread-" + str(serial))
        thread = {"id": native_id, "preview": "", "ephemeral": False, "modelProvider": "openai", "createdAt": 1800000000, "updatedAt": 1800000000, "status": {"type": "idle"}, "cwd": params.get("cwd", os.getcwd()), "path": None, "turns": [], "gitInfo": None, "source": "appServer", "cliVersion": "0.156.1", "agentNickname": None, "agentRole": None, "memoryMode": "enabled", "historyMode": "legacy" if os.environ.get("FIXTURE_LEGACY_HISTORY") else "paginated"}
        threads[native_id] = thread
        result = {"thread": thread, "model": params.get("model", "fixture-model"), "modelProvider": "openai", "cwd": thread["cwd"], "approvalPolicy": params.get("approvalPolicy", "on-request"), "sandbox": {"type": "workspaceWrite"}, "reasoningEffort": "low"}
    elif method == "thread/read":
        # A thread this process has not loaded reads as notLoaded, as after a restart.
        result = {"thread": threads.get(params["threadId"]) or {"id": params["threadId"], "status": {"type": "notLoaded"}, "updatedAt": 1800000000, "historyMode": "legacy" if os.environ.get("FIXTURE_LEGACY_HISTORY") else "paginated"}}
    elif method == "thread/turns/list":
        # Newest first, as Codex pages paginated history. Reverted turns are gone.
        finished = [turn for turn in turns.values() if turn["threadId"] == params["threadId"] and turn["status"] != "inProgress" and not turn.get("reverted")]
        result = {"data": [{"id": turn["id"], "status": turn["status"], "items": []} for turn in reversed(finished)][: params.get("limit", 100)], "nextCursor": None}
    elif method == "thread/revert":
        # Everything from beforeTurnId onward is dropped from the conversation.
        ordered = [turn for turn in turns.values() if turn["threadId"] == params["threadId"]]
        index = next(position for position, turn in enumerate(ordered) if turn["id"] == params["beforeTurnId"])
        for turn in ordered[index:]:
            turn["reverted"] = True
        threads[params["threadId"]]["updatedAt"] = 1800000500
        result = {"thread": threads[params["threadId"]]}
    elif method == "turn/start":
        serial += 1
        native_id = "fixture-turn-" + str(serial)
        text = "\n".join(item.get("text", "") for item in params.get("input", []) if item.get("type") == "text")
        turn = {"id": native_id, "threadId": params["threadId"], "text": text, "status": "inProgress", "policy": params.get("approvalPolicy")}
        turns[native_id] = turn
        if "[hang-start]" in text:
            signal = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            signal.sendto(json.dumps({"text": text, "pid": os.getpid()}).encode(), os.environ["FIXTURE_REQUEST_SIGNAL"])
            signal.close()
            continue
        result = {"turn": {"id": native_id, "items": [], "status": "inProgress", "error": None, "startedAt": 1800000000, "completedAt": None}}
        after_response = lambda: start_turn(turn)
    elif method == "mcpServerStatus/list":
        server = params.get("serverName")
        result = {"data": [{"name": server, "tools": MCP_APP_TOOLS if server == "widget_server" else {}, "resources": [], "resourceTemplates": [], "authStatus": "unsupported"}], "nextCursor": None}
    elif method == "mcpServer/tool/call":
        if params.get("server") != "widget_server" or params.get("tool") not in MCP_APP_TOOLS:
            send({"id": request_id, "error": {"code": -32000, "message": "Fixture MCP tool not found: " + str(params.get("tool"))}})
            continue
        result = {"content": [{"type": "text", "text": "called " + params["tool"] + " " + json.dumps(params.get("arguments"), sort_keys=True)}], "structuredContent": {"echo": params.get("arguments")}, "isError": False, "_meta": {"fixture": True}}
    elif method == "mcpServer/resource/read":
        if params.get("server") != "widget_server" or params.get("uri") != MCP_APP_URI:
            send({"id": request_id, "error": {"code": -32000, "message": "Fixture MCP resource not found: " + str(params.get("uri"))}})
            continue
        result = {"contents": [{"uri": MCP_APP_URI, "mimeType": "text/html;profile=mcp-app", "text": "<!doctype html><html><body>Fixture widget</body></html>", "_meta": {"ui": {"csp": {"connectDomains": ["https://api.example.com"]}, "prefersBorder": True}}}]}
    elif method == "feedback/upload":
        serial += 1
        result = {"threadId": "fixture-feedback-" + str(serial)}
    elif method == "thread/compact/start":
        # Codex compacts in a turn of its own, announced by turn/started rather than a response.
        serial += 1
        native_id = "fixture-turn-" + str(serial)
        turn = {"id": native_id, "threadId": params["threadId"], "text": "[compact]", "status": "inProgress"}
        turns[native_id] = turn
        result = {}
        after_response = lambda: compaction(turn)
    elif method == "thread/backgroundTerminals/terminate":
        # Stopping a terminal ends its command, which the server shows as stopped by its own Stop.
        background.pop(params["processId"], None)
        result = {"terminated": True}
    elif method == "turn/steer":
        # The steering message joins the running turn, which then finishes.
        result = {"turnId": params["expectedTurnId"]}
        turn = turns[params["expectedTurnId"]]
        after_response = lambda: finish(turn)
    elif method == "turn/interrupt":
        result = {}
        turn = turns[params["turnId"]]
        after_response = lambda: finish(turn, "interrupted")
    else:
        send({"id": request_id, "error": {"code": -32601, "message": "Fixture method not found: " + method}})
        continue
    send({"id": request_id, "result": result})
    if after_response:
        after_response()
