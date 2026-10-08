"""Strict isolated ACP lifecycle fixture; callbacks use exact correlated JSON-RPC IDs."""
import json
import os
import socket
import sys

scenario = sys.argv[1]
marker = sys.argv[2]
session_id = "coordinator-session"
initialization = None

def emit(value):
    print(json.dumps(value), flush=True)

def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})

def update(value, target=session_id):
    emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": target, "update": value}})

def options(model="a", enabled=False):
    values = [
        {"id" if scenario in ("state", "config-only") else "configId": "model", "name": "Model", "type": "select", "category": "model", "currentValue": model,
         "options": [{"value": item, "name": item.upper()} for item in ["a", "b", "early"]]},
        {"id" if scenario in ("state", "config-only") else "configId": "enabled", "name": "Enabled", "type": "boolean", "currentValue": enabled},
    ]
    if scenario == "config-only":
        values.append({"id": "mode", "name": "Mode", "type": "select", "category": "mode", "currentValue": "normal",
                       "options": [{"value": "normal", "name": "Normal"}, {"value": "alt", "name": "Alternate"}]})
    return values

def initialized(request):
    if scenario in ("state", "config-only"):
        reply(request, {"protocolVersion": 1, "agentInfo": {"name": "coordinator fixture", "version": "informational-agent-version"},
                        "agentCapabilities": {"loadSession": True}, "authMethods": []})
    else:
        reply(request, {"protocolVersion": 2, "info": {"name": "coordinator fixture", "version": "informational-agent-version"},
                        "capabilities": {"session": {"prompt": {}, "mcp": {}}, "elicitation": {}}, "authMethods": []})

model = "a"
enabled = False
for line in sys.stdin:
    request = json.loads(line)
    assert request["jsonrpc"] == "2.0", request
    if "method" not in request:
        assert request["id"] == "auth:0", request
        assert request["result"] == {"action": "accept"}, request
        initialized(initialization)
        initialization = None
        continue
    method = request["method"]
    params = request.get("params", {})
    if method == "initialize":
        first = not os.path.exists(marker)
        with open(marker + ".log", "a") as file:
            file.write(str(os.getpid()) + "\n")
        if first:
            with open(marker, "w") as file:
                file.write(str(os.getpid()))
        if scenario == "priority" and first:
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as signal:
                signal.sendto(json.dumps({"pid": os.getpid()}).encode(), os.environ["T3_SIGNAL_SOCKET"])
            continue
        if scenario == "state" and not first:
            assert params["capabilities"]["elicitation"] == {"url": {}}, params
            initialization = request
            emit({"jsonrpc": "2.0", "id": "auth:0", "method": "elicitation/create",
                  "params": {"sessionId": session_id, "mode": "url", "message": " Sign in ",
                             "url": "HTTPS://ACCOUNTS.EXAMPLE.COM:443/login", "elicitationId": "login-0"}})
        else:
            initialized(request)
    elif method == "session/new":
        assert params["mcpServers"] == [], params
        if scenario == "state":
            update({"sessionUpdate": "config_option_update", "configOptions": options("b", True)}, "child-session")
            update({"sessionUpdate": "current_mode_update", "currentModeId": "normal"}, "child-session")
            update({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "foreign", "description": ""}]}, "child-session")
            update({"sessionUpdate": "config_option_update", "configOptions": options("b", True)})
            model = "early"
            update({"sessionUpdate": "config_option_update", "configOptions": options(model, enabled)})
            update({"sessionUpdate": "current_mode_update", "currentModeId": "alt"})
        setup = {"sessionId": session_id, "configOptions": options()}
        if scenario != "config-only":
            setup["modes"] = {"currentModeId": "normal", "availableModes": [
                {"id": "normal", "name": "Normal"}, {"id": "alt", "name": "Alternate"}]}
        reply(request, setup)
        if scenario == "state":
            update({"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "help", "description": " Help ", "input": {"hint": " topic "}},
                {"name": "HELP", "description": "duplicate"},
                {"name": "$space/skill", "description": " Skill "}]})
    elif method == "session/set_config_option":
        assert params["sessionId"] == session_id, params
        if params["configId"] == "model":
            model = params["value"]
        else:
            assert params["configId"] == "enabled", params
            enabled = params["value"]
        reply(request, {"configOptions": options(model, enabled)})
    elif method == "x/mode":
        update({"sessionUpdate": "current_mode_update", "currentModeId": params["mode"]})
        reply(request, {})
    else:
        raise AssertionError(request)
