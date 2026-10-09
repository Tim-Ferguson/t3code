#!/usr/bin/env python3
"""Test-only ACP v2 agent for session, provider and logout management. Never touches a real credential.

argv[1] is a JSON-lines log of every request (method, params, client name); MANAGE_SCENARIO selects a
failure: `delete-error`, `list-error`, `auth-error`, `no-delete`, `no-providers`, `no-session`, `no-auth`, `hold`."""
import json, os, sys

log = sys.argv[1]
scenario = os.environ.get("MANAGE_SCENARIO", "normal")
client = ""

def emit(value): print(json.dumps(value), flush=True)
def reply(request, result): emit({"jsonrpc": "2.0", "id": request["id"], "result": result})
def fail(request, code, message): emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}})

for line in sys.stdin:
    request = json.loads(line)
    if "method" not in request:
        continue
    method, params = request["method"], request.get("params", {})
    with open(log, "a") as file:
        file.write(json.dumps({"method": method, "params": params, "client": client, "scenario": scenario}) + "\n")
    if method == "initialize":
        client = params.get("clientInfo", {}).get("name", "")
        session = {} if scenario == "no-delete" else {"delete": {}}
        capabilities = {} if scenario == "no-session" else {"session": session}
        if scenario != "no-providers":
            capabilities["providers"] = {}
        reply(request, {"protocolVersion": 2, "info": {"name": "manager fixture", "version": "0"}, "capabilities": capabilities,
                        "authMethods": [] if scenario == "no-auth" else [{"methodId": "agent", "name": "Browser login", "type": "agent"}]})
    elif method == "session/new":
        configuration = {"type": "select", "configId": "model", "category": "model", "name": "Model", "currentValue": "m", "options": [{"value": "m", "name": "Model"}]}
        reply(request, {"sessionId": "disposable", "configOptions": [configuration]})
        emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "disposable", "update": {"sessionUpdate": "available_commands_update", "availableCommands": []}}})
    elif method == "session/list":
        if scenario == "hold":
            continue
        if scenario == "list-error":
            fail(request, -32000, "Authentication required: token sk-secret-value")
        elif params.get("cursor") == "page2":
            reply(request, {"sessions": [{"sessionId": "s-2", "cwd": params["cwd"]}]})
        else:
            reply(request, {"sessions": [
                {"sessionId": "s-1", "cwd": params["cwd"], "title": "  First session  ", "updatedAt": "2026-01-01T00:00:00Z", "additionalDirectories": ["/extra", " padded "]},
                {"sessionId": "", "cwd": params["cwd"]},
                {"sessionId": "s-bad-cwd", "cwd": " padded "},
            ], "nextCursor": "page2"})
    elif method == "session/delete":
        if scenario == "delete-error":
            fail(request, -32603, "internal: deleting /home/user/.secret failed")
        else:
            reply(request, {})
    elif method == "providers/list":
        reply(request, {"providers": [
            {"providerId": "openai", "supported": ["openai-compat", "anthropic"], "required": False, "current": {"apiType": "openai-compat", "baseUrl": "https://api.example.test/v1"}},
            {"providerId": "core", "supported": ["native"], "required": True, "current": None},
            {"providerId": " bad ", "supported": [], "required": False},
        ]})
    elif method in ("providers/set", "providers/disable"):
        reply(request, {})
    elif method == "auth/logout":
        if scenario == "auth-error":
            fail(request, -32000, "Fixture logout refused")
        else:
            reply(request, {})
    else:
        fail(request, -32601, "Method not found: " + method)
