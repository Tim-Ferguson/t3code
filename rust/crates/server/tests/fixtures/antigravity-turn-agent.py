#!/usr/bin/env python3
"""A scripted stand-in for the Antigravity ACP agent, run by antigravity-fake-cli.py (ANTIGRAVITY_FIXTURE_AGENT).
Never the real agent, never Google: its "Google sign-in" is a loopback server of its own.

ANTIGRAVITY_FIXTURE_SCRIPT names {"prompts": [[step, ...], ...], "sign_in": "loopback" | "refused" | null, "install_only": bool}:
the Nth `session/prompt` plays the Nth list. Steps:
  {"update": {...}}                 a `session/update` for the session
  {"request": {"method","params"}}  a request to the client, waited on before the next step
  {"end": "end_turn"}               answers the prompt
  {"hang": true}                    keeps the prompt open until the process is stopped
`authenticate` with "sign_in": "loopback" prints the Google authorization URL the way the agent does, listens on a
loopback port for the redirect, and answers once it has had the code; "refused" prints the URL and waits for nothing
(a launch that cannot sign in). Further script keys: "session_id" (the id session/new answers, default agy-turn-session), "models" (the model options),
"current_model", and "conversation_files" (session/new writes the conversation files the real agent keeps in its profile,
$GEMINI_HOME/antigravity-acp/conversations/<id>.meta and .db, for the cwd it was given). A step {"write_file": name} writes
that file into the session's cwd.
ANTIGRAVITY_FIXTURE_LOG receives every message the client sends, with this pid.
"""
import http.server
import json
import os
import sys
import threading
import urllib.parse

script = json.load(open(os.environ["ANTIGRAVITY_FIXTURE_SCRIPT"])) if os.environ.get("ANTIGRAVITY_FIXTURE_SCRIPT") else {}
log_path = os.environ.get("ANTIGRAVITY_FIXTURE_LOG")
SESSION = script.get("session_id", "agy-turn-session")
MODELS = script.get("models", [{"value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)"}, {"value": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)"}])
MODES = [{"value": "default", "name": "Default"}, {"value": "auto_edit", "name": "Auto edit"}, {"value": "yolo", "name": "YOLO"}]
state = {"model": script.get("current_model", "gemini-3.8-flash-low"), "mode": "default"}
session_cwd = None
prompts_seen = sum(1 for line in open(log_path) if json.loads(line)["in"].get("method") == "session/prompt") if log_path and os.path.exists(log_path) else 0
prompt_request = None
queue = []
waiting = None
next_id = 100


def emit(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def record(message):
    if log_path:
        with open(log_path, "a") as log:
            log.write(json.dumps({"pid": os.getpid(), "in": message}) + "\n")


def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})


def options():
    return [
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": state["model"], "options": MODELS},
        {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": state["mode"], "options": MODES},
    ]


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
        elif "write_file" in step:
            with open(os.path.join(session_cwd, step["write_file"]), "w") as handle:
                handle.write("written by the agent")
        elif "end" in step:
            reply(prompt_request, {"stopReason": step["end"]})
            prompt_request = None
        elif "hang" in step:
            return


def sign_in(request):
    """The loopback redirect of Google's flow: print the URL, wait for the one request that carries the code."""
    kind = script.get("sign_in")
    state_value = "fixture-state-" + str(os.getpid())
    done = threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            record({"callback": {key: value for key, value in query.items()}, "path": self.path.split("?")[0]})
            ok = query.get("state") == [state_value] and query.get("code") == ["fixture-code"]
            self.send_response(200 if ok else 400)
            self.end_headers()
            self.wfile.write(b"signed in" if ok else b"refused")
            if ok:
                done.set()

        def log_message(self, *args):
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    redirect = urllib.parse.quote("http://127.0.0.1:%d/" % port, safe="")
    line = "Open the following link to authenticate the ACP server: https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=fixture&redirect_uri=%s&state=%s\n" % (redirect, state_value)
    if os.environ.get("ANTIGRAVITY_FIXTURE_URL_STREAM") == "stderr":
        sys.stderr.write(line)
        sys.stderr.flush()
    else:
        sys.stdout.write(line)
        sys.stdout.flush()
    if kind == "refused":
        threading.Event().wait()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    done.wait()
    server.shutdown()
    reply(request, {})


for line in sys.stdin:
    message = json.loads(line)
    record(message)
    if "method" not in message:
        if message.get("id") == waiting:
            waiting = None
            run()
        continue
    method = message["method"]
    if method == "initialize":
        reply(message, {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "promptCapabilities": {"image": True, "audio": True, "embeddedContext": True}, "sessionCapabilities": {"list": {}, "resume": {}}, "auth": {} if script.get("no_logout") else {"logout": {}}},
                        "authMethods": [{"id": "oauth-personal", "name": "Log in with Google"}], "agentInfo": {"name": "antigravity-acp", "title": "Google Antigravity", "version": script.get("version", "1.3.0")}})
    elif method == "authenticate":
        if script.get("sign_in"):
            sign_in(message)
        elif script.get("credentials_rejected"):
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "bad credentials sk-secret"}})
        else:
            reply(message, {})
    elif method == "session/new":
        if script.get("needs_sign_in"):
            emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": "Authentication required"}})
        else:
            session_cwd = message["params"].get("cwd")
            if script.get("conversation_files"):
                directory = os.path.join(os.environ["GEMINI_HOME"], "antigravity-acp", "conversations")
                os.makedirs(directory, exist_ok=True)
                with open(os.path.join(directory, SESSION + ".meta"), "w") as handle:
                    json.dump({"cwd": session_cwd}, handle)
                with open(os.path.join(directory, SESSION + ".db"), "w") as handle:
                    handle.write("db")
            reply(message, {"sessionId": SESSION, "configOptions": options()})
    elif method in ("session/resume", "session/load"):
        reply(message, {"configOptions": options()})
    elif method == "session/set_config_option":
        state[message["params"]["configId"]] = message["params"]["value"]
        reply(message, {"configOptions": options()})
    elif method == "session/set_mode":
        state["mode"] = message["params"]["modeId"]
        reply(message, {})
    elif method == "session/prompt":
        prompt_request = message
        queue = list(script["prompts"][prompts_seen])
        prompts_seen += 1
        run()
    elif method == "session/cancel":
        if prompt_request is not None:
            queue = []
            waiting = None
            reply(prompt_request, {"stopReason": "cancelled"})
            prompt_request = None
    elif method == "logout":
        reply(message, {})
    elif "id" in message:
        emit({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unsupported"}})
