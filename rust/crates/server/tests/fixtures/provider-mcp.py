"""Owned MCP integration fixture; credentials never enter logs or argv."""
import hashlib
import json
import os
import socket
import subprocess
import sys
import urllib.request

driver, scenario = sys.argv[1:3]
authorization = None
endpoint = None
http_session = None
version = "2025-06-18"

def emit(value):
    print(json.dumps(value), flush=True)

def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})

def signal(method):
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
        channel.sendto(json.dumps({"pid": os.getpid(), "method": method,
            "authorization": authorization,
            "hash": hashlib.sha256((authorization or "").encode()).hexdigest()}).encode(), os.environ["MCP_SIGNAL"])

def call(method, params, identifier):
    global http_session
    headers = {"Authorization": authorization, "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": version}
    if http_session:
        headers["Mcp-Session-Id"] = http_session
    body = {"jsonrpc": "2.0", "method": method, "params": params}
    if identifier is not None:
        body["id"] = identifier
    request = urllib.request.Request(endpoint, json.dumps(body).encode(), headers)
    with urllib.request.urlopen(request) as response:
        http_session = response.headers.get("Mcp-Session-Id", http_session)
        raw = response.read()
        return json.loads(raw) if raw else None

def configure(params):
    global authorization, endpoint
    if driver == "acp":
        servers = params["mcpServers"]
        if not servers:
            return False  # disposable discovery has no grant
        server = servers[0]
        assert server["name"] == "t3-code" and server["args"] == ["acp-mcp-bridge"]
        environment = {entry["name"]: entry["value"] for entry in server["env"]}
        endpoint = environment["T3_ACP_MCP_ENDPOINT"]
        authorization = environment["T3_ACP_MCP_AUTHORIZATION"]
        # Execute the advertised Rust stdio server, not a fixture replacement.
        bridge = subprocess.Popen([server["command"]] + server["args"],
            env={**os.environ, **environment}, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            def stdio(method, params, identifier):
                message = {"jsonrpc": "2.0", "method": method, "params": params}
                if identifier is not None:
                    message["id"] = identifier
                bridge.stdin.write(json.dumps(message) + "\n")
                bridge.stdin.flush()
                return json.loads(bridge.stdout.readline()) if identifier is not None else None
            initialized = stdio("initialize", {"protocolVersion": version,
                "capabilities": {}, "clientInfo": {"name": "fixture-stdio", "version": "1"}}, 10)
            assert initialized["result"]["protocolVersion"] == version
            stdio("notifications/initialized", {}, None)
            assert "tools" in stdio("tools/list", {}, 11)["result"]
            bridge.stdin.close()
            assert bridge.wait(timeout=5) == 0, "Rust stdio bridge failed"
        finally:
            if bridge.poll() is None:
                bridge.kill()
            bridge.wait()
    else:
        server = params["config"]["mcp_servers"]["t3-code"]
        endpoint = server["url"]
        authorization = server["http_headers"]["Authorization"]
    result = call("initialize", {"protocolVersion": version, "capabilities": {},
        "clientInfo": {"name": "fixture", "version": "1"}}, 1)
    assert result["result"]["protocolVersion"] == version
    call("notifications/initialized", {}, None)
    assert "tools" in call("tools/list", {}, 2)["result"]
    signal("configured")
    return True

for line in sys.stdin:
    request = json.loads(line)
    method, params = request.get("method"), request.get("params", {})
    if method == "initialize":
        if driver == "acp":
            reply(request, {"protocolVersion": 2, "info": {"name": "fixture", "version": "1"},
                "capabilities": {"session": {"prompt": {}, "mcp": {}}}, "authMethods": []})
        else:
            reply(request, {"userAgent": "fixture/1"})
    elif method == "initialized":
        pass
    elif method == "account/read":
        reply(request, {"account": {"type": "apiKey"}, "requiresOpenaiAuth": False})
    elif method == "model/list":
        reply(request, {"data": [{"id": "fixture-model", "model": "fixture-model",
            "displayName": "Fixture", "isDefault": True, "supportedReasoningEfforts": []}], "nextCursor": None})
    elif method == "skills/list":
        reply(request, {"data": []})
    elif method in ("session/new", "session/load", "thread/start", "thread/resume"):
        configured = configure(params)
        if configured and scenario == "hold-startup":
            continue
        if configured and scenario == "fail-startup":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "Fixture failed after MCP setup"}})
            continue
        if driver == "acp":
            reply(request, {"sessionId": "native-session", "configOptions": [{"type": "select",
                "configId": "model", "category": "model", "name": "Model", "currentValue": "fixture-model",
                "options": [{"value": "fixture-model", "name": "Fixture"}]}]})
            emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "native-session",
                "update": {"sessionUpdate": "available_commands_update", "availableCommands": []}}})
        else:
            reply(request, {"thread": {"id": "native-thread"}})
    elif method in ("session/set_config_option", "session/set_model"):
        reply(request, {})
    elif method in ("session/prompt", "turn/start"):
        assert "tools" in call("tools/list", {}, 3)["result"]
        signal("prompt")
        if driver == "acp":
            emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "native-session",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "MCP OK"}}}})
            reply(request, {"stopReason": "end_turn"})
            emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "native-session",
                "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn"}}})
        else:
            reply(request, {"turn": {"id": "native-turn"}})
            emit({"method": "turn/completed", "params": {"threadId": "native-thread",
                "turn": {"id": "native-turn", "status": "completed", "error": None}}})
    else:
        raise AssertionError("Unexpected fixture method: " + str(method))
