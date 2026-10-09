"""A scripted ACP agent for turn-path tests. Usage: acp-turn-agent.py SCRIPT.json LOG.jsonl

The script is {"setup": {...session/new result extras}, "prompts": [[step, ...], ...]}; the Nth
`session/prompt` plays the Nth list. Steps: {"update": {...}} writes a session/update;
{"request": {"method", "params"}} asks the client and waits for its answer before the next step;
{"end": "end_turn"} answers the prompt; {"error": {...}} fails it; {"exit": true} dies. Every
request and every answer the client sends is appended to the log as {"in": message}.
"""
import json
import sys

script = json.load(open(sys.argv[1]))
log_path = sys.argv[2]
SESSION = "turn-session"
prompts_seen = 0
prompt_request = None
queue = []  # steps of the running prompt
waiting = None  # id of a client request the prompt is waiting on
next_id = 100


def emit(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def record(message):
    with open(log_path, "a") as log:
        log.write(json.dumps({"in": message}) + "\n")


def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})


def settle(reason):
    """Answers the running prompt; a v2 session also announces that it went idle."""
    global prompt_request
    reply(prompt_request, {"stopReason": reason})
    if script.get("protocol", 1) == 2:
        emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": reason}}})
    prompt_request = None


def run():
    global waiting, prompt_request, next_id
    while queue and waiting is None:
        step = queue.pop(0)
        if "update" in step:
            emit({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": step["update"]}})
        elif "request" in step:
            next_id += 1
            waiting = next_id
            params = dict(step["request"]["params"])
            params.setdefault("sessionId", SESSION)
            emit({"jsonrpc": "2.0", "id": next_id, "method": step["request"]["method"], "params": params})
        elif "end" in step:
            settle(step["end"])
        elif "error" in step:
            emit({"jsonrpc": "2.0", "id": prompt_request["id"], "error": step["error"]})
            prompt_request = None
        elif "exit" in step:
            sys.exit(0)


for line in sys.stdin:
    message = json.loads(line)
    if "method" not in message:
        record(message)
        if message.get("id") == waiting:
            waiting = None
            run()
        continue
    record(message)
    method, params = message["method"], message.get("params", {})
    if method == "initialize":
        if script.get("protocol", 1) == 2:
            reply(message, {"protocolVersion": 2, "capabilities": script.get("capabilities", {"session": {"prompt": {}}}), "authMethods": [], "info": {"name": "turn-agent", "version": "1"}})
        else:
            reply(message, {"protocolVersion": 1, "agentCapabilities": script.get("capabilities", {"loadSession": True, "promptCapabilities": {"image": False}}), "authMethods": [], "agentInfo": {"name": "turn-agent", "version": "1"}})
    elif method == "session/new":
        reply(message, {"sessionId": SESSION, "configOptions": [], **script.get("setup", {})})
    elif method in ("session/load", "session/resume"):
        if script.get("loadFails"):
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32002, "message": "session not found"}})
        else:
            reply(message, {"configOptions": []})
    elif method == "session/set_mode":
        reply(message, {})
    elif method == "session/set_config_option":
        options = script.get("setup", {}).get("configOptions", [])
        for option in options:
            if option["id"] == params["configId"]:
                option["currentValue"] = params["value"]
        reply(message, {"configOptions": options})
    elif method == "session/prompt":
        prompt_request = message
        queue = list(script["prompts"][prompts_seen])
        prompts_seen += 1
        run()
    elif method == "session/cancel":
        if prompt_request is not None:
            queue = []
            waiting = None
            settle("cancelled")
    elif "id" in message:
        emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unsupported"}})
