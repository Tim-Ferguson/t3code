"""Isolated ACP fixture. Every request is validated against the actual wire shape."""
import json
import os
import socket
import sys

generation = int(sys.argv[1]) if len(sys.argv) > 1 else 2
scenario = sys.argv[2] if len(sys.argv) > 2 else "normal"
session_id = "native-session"
pending = None
authenticated = False
setup_attempts = 0

def emit(value):
    print(json.dumps(value), flush=True)

def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})

def update(value):
    emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": value}})

def configuration():
    return [{"type": "select", "configId" if generation == 2 else "id": "model", "category": "model", "name": "Model", "currentValue": "fixture-model", "options": [{"value": "fixture-model", "name": "Fixture model"}]}]

def complete(reason):
    global pending
    assert pending is not None
    reply(pending, {"stopReason": reason})
    if generation == 2:
        update({"sessionUpdate": "state_update", "state": "idle", "stopReason": reason})
    pending = None
    if scenario == "exit-after-prompt":
        sys.exit(0)

for line in sys.stdin:
    request = json.loads(line)
    assert request.get("jsonrpc") == "2.0", request
    if "method" not in request:
        assert request["id"] == 0 and type(request["id"]) is int, request
        assert request["result"] == {"outcome": {"outcome": "selected", "optionId": "once"}}, request
        if scenario == "tools":
            update({"sessionUpdate": "tool_call_update", "toolCallId": "command", "status": "completed", "rawInput": {}, "rawOutput": {"stdout": "z" * 9001, "exit_code": 0}})
        if scenario == "mcp-tools":
            update({"sessionUpdate": "tool_call_update", "toolCallId": "mcp-tagged", "status": "completed", "rawInput": {}, "rawOutput": {"result": {"structuredContent": {"completed": True}}}})
            update({"sessionUpdate": "tool_call_update", "toolCallId": "mcp-title", "status": "completed", "rawOutput": {"error": {"message": "Denied"}, "result": {"structuredContent": {"attempt": 1}}}})
        if scenario == "plans":
            update({"sessionUpdate": "plan_update", "plan": {"type": "items", "planId": "todo plan", "entries": [{"content": " Finish ", "status": "completed", "priority": "medium"}]}})
            update({"sessionUpdate": "plan_removed", "planId": "draft"})
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "approved"}})
        complete("end_turn")
        continue
    method = request["method"]
    if "FIXTURE_REQUEST_LOG" in os.environ:
        with open(os.environ["FIXTURE_REQUEST_LOG"], "a") as log:
            log.write(method + "\n")
    if "FIXTURE_ALL_REQUEST_SIGNAL" in os.environ:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as signal:
            signal.sendto(json.dumps({"pid": os.getpid(), "method": method}).encode(), os.environ["FIXTURE_ALL_REQUEST_SIGNAL"])
    if scenario.startswith("auth-"):
        with open(sys.argv[3], "a") as log:
            log.write(method + "\n")
    params = request.get("params", {})
    if method == "initialize":
        assert params["protocolVersion"] == 2, params
        if scenario == "hold-runtime-initialize":
            marker = sys.argv[3]
            if os.path.exists(marker):
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as signal:
                    signal.sendto(json.dumps({"pid": os.getpid()}).encode(), os.environ["FIXTURE_REQUEST_SIGNAL"])
                continue
            with open(marker, "w") as record:
                record.write("discovered")
        if scenario == "hold-initialize":
            emit({"jsonrpc": "2.0", "method": "x/initialize-held", "params": {}})
            continue
        if scenario == "fail-initialize":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "fixture initialization failure"}})
            continue
        if scenario.startswith("auth-"):
            kind = "terminal" if scenario == "auth-terminal" else "agent"
            methods = [] if scenario == "auth-none" else [{"methodId" if generation == 2 else "id": "agent", "name": "Agent", "type": kind}]
            if kind == "terminal" and methods:
                methods[0].update({"args": [], "env": [] if generation == 2 else {}})
            if generation == 2:
                reply(request, {"protocolVersion": 2, "info": {"name": "fixture", "version": "1"}, "capabilities": {"session": {"prompt": {}, "mcp": {}}}, "authMethods": methods})
            else:
                reply(request, {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}, "authMethods": methods})
            continue
        if generation == 2:
            reply(request, {"protocolVersion": 2, "info": {"name": "fixture", "version": "1"}, "capabilities": {"session": {"prompt": {}, "mcp": {}}}, "authMethods": []})
        else:
            reply(request, {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}, "agentInfo": {"name": "fixture", "version": "1"}, "authMethods": []})
    elif method in ("authenticate", "auth/login"):
        assert scenario.startswith("auth-") and params["methodId"] == "agent", request
        assert setup_attempts == 1, "Normal sessions must attempt setup before authentication"
        authenticated = True
        reply(request, {})
    elif method == "session/new":
        assert params["cwd"] and params["mcpServers"] == [], params
        setup_attempts += 1
        if scenario.startswith("auth-") and scenario != "auth-ready" and (not authenticated or scenario == "auth-twice"):
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602 if scenario == "auth-other-error" else -32000, "message": "Authentication required"}})
            continue
        reply(request, {"sessionId": session_id, "configOptions": configuration()})
    elif method in ("session/load", "session/resume"):
        assert params["sessionId"] == session_id and params["mcpServers"] == [], params
        if scenario == "fail-load":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "fixture load failure"}})
            continue
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "replayed"}, "_meta": {"fixture": True}})
        reply(request, {"configOptions": configuration()})
    elif method in ("session/set_config_option", "session/set_model"):
        assert params["sessionId"] == session_id, params
        reply(request, {"configOptions": configuration()} if method.endswith("config_option") else {})
    elif method == "session/prompt":
        assert params["sessionId"] == session_id and params["prompt"][0]["type"] == "text", params
        pending = request
        text = params["prompt"][0]["text"]
        if text == "hold":
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "waiting"}})
        else:
            if scenario == "thoughts":
                update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "AQ=="}})
                update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": ""}})
                update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": " think "}})
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello "}})
            if scenario == "mcp-tools":
                update({"sessionUpdate": "tool_call_update", "toolCallId": "mcp-tagged", "kind": "execute", "title": "mcp.t3-code.delegate_task", "status": "in_progress", "rawInput": {"server": "t3-code", "tool": "delegate_task", "arguments": {"task": "fixture"}}, "_meta": {"is_mcp_tool_call": True}, "rawOutput": {"result": {"structuredContent": {"pending": True}}}})
                update({"sessionUpdate": "tool_call_update", "toolCallId": "mcp-title", "kind": "other", "title": "t3-code_orchestrator_capabilities", "status": "pending", "rawInput": {"retained": True}})
                update({"sessionUpdate": "tool_call_update", "toolCallId": "mcp-weather", "kind": "other", "title": "Checking forecast", "status": "completed", "_meta": {"serverId": "weather", "toolName": "mcp::weather::get_weather"}, "rawInput": {"arguments": {"city": "Phoenix"}}, "rawOutput": {"result": {"content": [{"type": "text", "text": "sunny"}], "_meta": {"source": {"name": "Weather Service", "logoUrl": "https://example.test/weather.png"}}}}})
            if scenario == "tools":
                update({"sessionUpdate": "tool_call_update" if generation == 2 else "tool_call", "toolCallId": "command", "kind": "execute", "title": "Terminal", "status": "pending", "rawInput": {"command": [" pwd ", " -P "]}})
                update({"sessionUpdate": "tool_call_update", "toolCallId": "command", "status": "in_progress", "rawOutput": {"stdout": "before", "exit_code": 0}})
                for tool_id, raw_input in [("read-scalar", "scalar"), ("read-array", ["array"])]:
                    update({"sessionUpdate": "tool_call_update" if generation == 2 else "tool_call", "toolCallId": tool_id, "kind": "read", "title": "Read", "status": "completed", "rawInput": raw_input, "locations": [{"path": "doc.txt"}], "rawOutput": "file body"})
            if scenario == "tools":
                update({"sessionUpdate": "tool_call_update" if generation == 2 else "tool_call", "toolCallId": "monitor", "kind": "other", "title": "Monitor", "status": "completed", "rawInput": {"variant": " Monitor ", "command": " monitor cmd "}, "rawOutput": {"type": "Bash", "stdout": "monitor done", "exit_code": 0}})
                update({"sessionUpdate": "tool_call_update" if generation == 2 else "tool_call", "toolCallId": "web-search", "kind": "search", "title": "Web search:", "status": "completed", "rawInput": {"variant": "WebSearch", "backend": True}, "rawOutput": {"action": {"type": "search", "query": " ACP ", "sources": [{"url": " https://example.test "}, {"url": "https://example.test"}]}}})
                if generation == 2:
                    update({"sessionUpdate": "tool_call_update", "toolCallId": "structured-file", "kind": "other", "title": "Edit", "status": "completed", "content": [{"type": "diff", "changes": [{"path": " src/file.rs ", "operation": "move", "oldPath": " src/old.rs "}], "patch": {"format": "git_patch", "text": "diff patch"}}]})
            if scenario == "plans":
                update({"sessionUpdate": "plan_update", "plan": {"type": "items", "planId": "todo plan", "entries": [{"content": " Finish ", "status": "in_progress", "priority": "medium"}]}})
                update({"sessionUpdate": "plan_update", "plan": {"type": "markdown", "planId": "draft", "content": "Draft plan"}})
            emit({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission", "params": {"sessionId": session_id, "toolCall": {"toolCallId": "command", "kind": "execute", "title": "Run pwd", "status": "pending"}, "options": [{"optionId": "once", "name": "Allow once", "kind": "allow_once"}, {"optionId": "always", "name": "Allow always", "kind": "allow_always"}, {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
    elif method == "session/cancel":
        assert "id" not in request and params["sessionId"] == session_id, request
        complete("cancelled")
    else:
        raise AssertionError(request)
