"""Owned ACP sign-in fixture: never touches a real agent credential store."""
import json, os, socket, sys

log, version = sys.argv[1], int(sys.argv[2])
marker = log + ".credentials"
scenario = os.environ.get("AUTH_SCENARIO", "normal")

def record(method):
    with open(log, "a") as file:
        file.write(json.dumps({"pid": os.getpid(), "method": method}) + "\n")

def milestone(method):
    if "AUTH_SOCKET" in os.environ:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as signal:
            signal.sendto(json.dumps({"pid": os.getpid(), "method": method}).encode(), os.environ["AUTH_SOCKET"])

if "--login" in sys.argv:
    record("terminal-login")
    milestone("terminal-login")
    print("Enter fixture code:", flush=True)
    if sys.stdin.readline().strip() == "fixture-code":
        with open(marker,"w") as file: file.write("agent-owned")
        print("Accepted",flush=True)
        sys.exit(0)
    sys.exit(9)

def emit(value): print(json.dumps(value), flush=True)
def reply(request,result): emit({"jsonrpc":"2.0","id":request["id"],"result":result})

login = None
for line in sys.stdin:
    request = json.loads(line)
    assert request["jsonrpc"] == "2.0", request
    if "method" not in request:
        assert request["id"] == "auth:0", request
        assert request["result"]["action"] in ("accept","decline"), request
        if request["result"]["action"] == "accept":
            with open(marker,"w") as file: file.write("agent-owned")
            reply(login,{})
        else:
            emit({"jsonrpc":"2.0","id":login["id"],"error":{"code":-32000,"message":"Fixture login declined"}})
        login=None
        continue
    method, params = request["method"], request.get("params",{})
    record(method)
    if method == "initialize":
        caps = params["clientCapabilities"]
        assert caps["auth"] == {"terminal":True}, caps
        assert caps["terminal"] is False, caps
        assert caps["fs"] == {"readTextFile":False,"writeTextFile":False}, caps
        assert params["protocolVersion"] == 2, params
        assert params["capabilities"]["auth"] == {"terminal":{}}, params
        milestone(method)
        if scenario == "held-initialize": continue
        methods=[{"id":"agent","name":"Browser login","description":"Fixture agent"},
                 {"id":"credentials","name":"Configured token","description":None,"type":"env_var","vars":[{"name":"FIXTURE_TOKEN"}]},
                 {"id":"terminal","name":"Terminal login","description":None,"type":"terminal","args":["--login"],"env":{} if version==1 else []}]
        methods[0]["type"]="agent"
        if version==1:
            reply(request,{"protocolVersion":1,"agentInfo":{"name":"auth fixture","version":"0"},"agentCapabilities":{"auth":{"logout":{}}},"authMethods":methods})
        else:
            for advertised in methods:
                advertised["methodId"] = advertised.pop("id")
            reply(request,{"protocolVersion":2,"info":{"name":"auth fixture","version":"0"},"capabilities":{"session":{"prompt":{},"mcp":{}},"auth":{"logout":{}},"elicitation":{}},"authMethods":methods})
    elif method in ("authenticate","auth/login"):
        assert params["methodId"] == "agent", params
        milestone(method)
        if scenario == "held-login": continue
        login=request
        emit({"jsonrpc":"2.0","id":"auth:0","method":"elicitation/create","params":{"requestId":request["id"],"mode":"url","url":"https://example.test/login","elicitationId":"fixture-consent","message":"Fixture login"}})
    elif method=="session/new":
        assert os.path.exists(marker) or os.environ.get("FIXTURE_TOKEN")=="configured-only", "verification without credentials"
        reply(request,{"sessionId":"disposable-auth-session"})
    elif method in ("logout","auth/logout"):
        if os.path.exists(marker): os.remove(marker)
        reply(request,{})
    else: raise AssertionError(request)
