"""Isolated Devin-shaped ACP fixture; assertions check raw callback responses."""
import json
import sys

def emit(value):
    print(json.dumps(value), flush=True)

def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})

def call(identifier, method, params):
    emit({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
    response = json.loads(sys.stdin.readline())
    assert response["id"] == identifier and type(response["id"]) is type(identifier), response
    return response

session = "devin-session"
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    params = request.get("params", {})
    if method == "initialize":
        if len(sys.argv)>1:
            created=call("startup-cat","terminal/create",{"sessionId":session,"command":"/bin/cat","args":[]})
            terminal=created["result"]["terminalId"]
            emit({"jsonrpc":"2.0","method":"x/startup-terminal","params":{"terminalId":terminal}})
            if sys.argv[1]=="fail-initialize":
                emit({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"setup failed"}})
            continue
        assert params["clientCapabilities"]["terminal"] is True, params
        reply(request, {"protocolVersion": 2, "info": {"name": "fixture-devin", "version": "1"}, "capabilities": {"session": {"prompt": {}}}, "authMethods": []})
    elif method == "session/new":
        reply(request, {"sessionId": session})
    elif method == "session/prompt":
        denied = call("create-denied", "terminal/create", {"sessionId": session, "command": "printf unsafe"})
        assert "result" not in denied and "requires approval" in json.dumps(denied["error"]), denied
        permission = call(0, "session/request_permission", {"sessionId": session, "toolCall": {"toolCallId": "execute-approval", "kind": "execute", "title": "Execute a command", "status": "pending"}, "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}]})
        assert permission["result"] == {"outcome": {"outcome": "selected", "optionId": "once"}}, permission
        created = call("0", "terminal/create", {"sessionId": session, "command": "printf '%s' \"$T3_CALLBACK_ENV\" | tr a-z A-Z", "outputByteLimit": 64})
        terminal = created["result"]["terminalId"]
        handle = {"sessionId": session, "terminalId": terminal}
        waited = call("wait", "terminal/wait_for_exit", handle)
        assert waited["result"] == {"exitCode": 0, "signal": None}, waited
        output = call("output", "terminal/output", handle)
        assert output["result"] == {"output": "SESSION", "truncated": False, "exitStatus": {"exitCode": 0, "signal": None}}, output
        emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "tool_call_update", "toolCallId": "terminal-tool", "title": "Terminal", "kind": "execute", "status": "completed", "content": [{"type": "terminal", "terminalId": terminal}]}}})
        assert call("release", "terminal/release", handle)["result"] == {}
        rejected = call("released", "terminal/output", handle)
        assert "unknown terminal ID" in json.dumps(rejected["error"]), rejected
        reply(request, {"stopReason": "end_turn"})
        emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn"}}})
    else:
        raise AssertionError(request)
