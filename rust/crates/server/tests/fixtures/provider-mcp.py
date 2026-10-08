"""Owned MCP integration fixture; credentials never enter logs or argv."""
import hashlib
import json
import os
import socket
import subprocess
import sys
import urllib.request

driver, scenario = sys.argv[1:3]
negotiated = driver == "acp-negotiated"
if negotiated:
    driver = "acp"
connection_id = None
callback_id = 0
turn_ordinal = 0
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

def callback(method, params):
    global callback_id
    callback_id += 1
    identifier = "callback-" + str(callback_id)
    emit({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
    response = json.loads(sys.stdin.readline())
    assert response["id"] == identifier and "error" not in response, response
    return response["result"]

def configure(params):
    global authorization, endpoint, connection_id
    if negotiated:
        servers = params["mcpServers"]
        if not servers:
            return False
        assert servers == [{"type": "acp", "name": "t3-code", "serverId": "t3-code"}], servers
        connection_id = callback("mcp/connect", {"serverId": "t3-code"})["connectionId"]
        initialized = callback("mcp/message", {"connectionId": connection_id, "method": "initialize", "params": {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "fixture-negotiated", "version": "1"}}})
        assert initialized["protocolVersion"] == version
        emit({"jsonrpc": "2.0", "method": "mcp/message", "params": {"connectionId": connection_id, "method": "notifications/initialized"}})
        if scenario == "hold-http":
            signal("configured")
        assert "tools" in callback("mcp/message", {"connectionId": connection_id, "method": "tools/list"})
        signal("configured")
        return True
    if os.environ.get("MCP_EXPECT_DEVICE") is not None and not negotiated:
        configured = bool(params["mcpServers"]) if driver == "acp" else True
        if configured:
            granted = os.environ["MCP_EXPECT_DEVICE"] == "true"
            assert (os.environ.get("MCP_SCOPED_DEVICE_MARKER") == "granted") == granted
            assert os.environ["MCP_PROVIDER_KEY"] == "preserved"
            if granted:
                assert os.environ["PATH"].startswith(os.environ["MCP_DEVICE_DIR"] + ":")
            assert "PATH_SEPARATOR" not in os.environ
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
                "capabilities": {"session": {"prompt": {}, "mcp": {"acp": {}} if negotiated else {}}}, "authMethods": []})
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
        turn_ordinal += 1
        if driver == "acp":
            assert len(params["prompt"]) == 2, params
            prompt = params["prompt"][0]["text"]
            assert ("<t3_code_instructions>" in prompt) == (turn_ordinal == 1 or (scenario == "prompt-retry" and turn_ordinal == 2))
            assert "Use MCP" in prompt
            assert "<runtime_info>" in params["prompt"][1]["text"]
            assert "acpRegistry harness, as fixture-model" in params["prompt"][1]["text"]
        else:
            context = params["additionalContext"]
            assert context["t3_code_orchestration"]["kind"] == "application"
            assert "Codex harness, as fixture-model with medium reasoning effort" in context["t3_code_runtime"]["value"]
            assert params["collaborationMode"]["mode"] == "default"
            assert "<collaboration_mode>" in params["collaborationMode"]["settings"]["developer_instructions"]
            granted = os.environ.get("MCP_EXPECT_DEVICE") == "true"
            assert ("t3_code_tools" in context) == granted
            if granted:
                assert "## T3 Code devices" in context["t3_code_tools"]["value"]
                assert "collaborative browser" not in context["t3_code_tools"]["value"]
        if negotiated:
            assert "tools" in callback("mcp/message", {"connectionId": connection_id, "method": "tools/list"})
        else:
            assert "tools" in call("tools/list", {}, 3)["result"]
        signal("prompt")
        if driver == "acp" and scenario == "prompt-retry" and turn_ordinal == 1:
            emit({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"Fixture transient prompt rejection"}})
            continue
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
