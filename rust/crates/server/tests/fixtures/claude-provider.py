#!/usr/bin/env python3
"""Deterministic stand-in for the `claude` CLI's stream-json mode.

Never contacts a network and never reads ~/.claude. Behaviour comes from the environment:
  CLAUDE_FIXTURE_SCENARIO    scripted scenario name (see SCENARIOS)
  CLAUDE_FIXTURE_TRANSCRIPT  path of a recorded Claude Agent SDK replay transcript (ndjson);
                             recorded inbound frames are emitted as the CLI would write them
  CLAUDE_FIXTURE_STATE       file holding the transcript cursor across process launches
  CLAUDE_FIXTURE_LOG         file receiving one JSON line per launch and per message received
  CLAUDE_FIXTURE_VERSION     output of `--version` (default "2.1.111 (Claude Code)")
  CLAUDE_FIXTURE_USAGE       JSON reply to the probe's `get_usage` request (default: no windows available)
  CLAUDE_FIXTURE_USAGE_FAIL  when set, `get_usage` is answered with an error
  CLAUDE_FIXTURE_SUBSCRIPTION  the account's subscription type (default "max")
  CLAUDE_FIXTURE_GATES       JSON object of recorded frame label to a fifo path: the frame with that label is
                             held until something is written to (and closed on) the fifo
"""
import json, os, sys, time

ARGS = sys.argv[1:]


def log(entry):
    path = os.environ.get("CLAUDE_FIXTURE_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def receive():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


if ARGS == ["--version"]:
    print(os.environ.get("CLAUDE_FIXTURE_VERSION", "2.1.111 (Claude Code)"))
    sys.exit(int(os.environ.get("CLAUDE_FIXTURE_VERSION_EXIT", "0")))

SESSION = "00000000-0000-4000-8000-000000000000"
for index, value in enumerate(ARGS):
    if value.startswith("--session-id=") or value.startswith("--resume="):
        SESSION = value.split("=", 1)[1]
    if value == "--model" and index + 1 < len(ARGS):
        MODEL = ARGS[index + 1]
flag = lambda name: ARGS[ARGS.index(name) + 1] if name in ARGS and ARGS.index(name) + 1 < len(ARGS) else None
log({"launch": ARGS, "cwd": os.getcwd(), "entrypoint": os.environ.get("CLAUDE_CODE_ENTRYPOINT"), "config": os.environ.get("CLAUDE_CONFIG_DIR"), "mcp": flag("--mcp-config")})
PROBE = "--no-session-persistence" in ARGS
USAGE = {"input_tokens": 3, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 50, "output_tokens": 7}
uid = lambda: "00000000-0000-4000-8000-%012d" % int(time.time() * 1000000 % 10**12)


def init_response():
    account = {"email": os.environ.get("CLAUDE_FIXTURE_EMAIL", "dev@example.test"), "subscriptionType": os.environ.get("CLAUDE_FIXTURE_SUBSCRIPTION", "max"), "tokenSource": "claude.ai"}
    return {"commands": [{"name": "review", "description": "Review a pull request", "argumentHint": "<pr>"}], "agents": [], "output_style": "default", "available_output_styles": ["default"], "models": [], "account": account}


def respond(request_id, response=None):
    send({"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": response or {}}})


def respond_error(request_id, message):
    send({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": message}})


def system_init():
    send({"type": "system", "subtype": "init", "session_id": SESSION, "uuid": uid(), "model": flag("--model"), "permissionMode": flag("--permission-mode"), "cwd": os.getcwd(), "tools": [], "mcp_servers": [], "agents": [], "apiKeySource": "none", "claude_code_version": "2.1.111", "slash_commands": [], "output_style": "default", "skills": [], "plugins": []})


def assistant(content, message_id, parent=None):
    send({"type": "assistant", "message": {"id": message_id, "type": "message", "role": "assistant", "model": flag("--model"), "content": content, "usage": USAGE, "stop_reason": None}, "parent_tool_use_id": parent, "session_id": SESSION, "uuid": uid()})


def user_result(tool_use_id, content, is_error=False, structured=None, parent=None):
    frame = {"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}]}, "parent_tool_use_id": parent, "session_id": SESSION, "uuid": uid()}
    if structured is not None:
        frame["tool_use_result"] = structured
    send(frame)


def result(text, subtype="success", is_error=False, **extra):
    frame = {"type": "result", "subtype": subtype, "is_error": is_error, "api_error_status": None, "duration_ms": 1, "num_turns": 1, "result": text, "stop_reason": "end_turn", "session_id": SESSION, "total_cost_usd": 0, "usage": USAGE, "uuid": uid(), "terminal_reason": "completed"}
    frame.update(extra)
    send(frame)


def stream(event):
    send({"type": "stream_event", "event": event, "parent_tool_use_id": None, "session_id": SESSION, "uuid": uid()})


def prompt_text(message):
    content = message["message"]["content"]
    if isinstance(content, str):
        return content
    return " ".join(block.get("text", "[" + block["type"] + "]") for block in content)


def can_use_tool(request_id, tool, tool_input, tool_use_id, **extra):
    request = {"subtype": "can_use_tool", "tool_name": tool, "input": tool_input, "tool_use_id": tool_use_id}
    request.update(extra)
    send({"type": "control_request", "request_id": request_id, "request": request})
    while True:
        message = receive()
        if message is None:
            sys.exit(0)
        log({"received": message})
        if message.get("type") == "control_response" and message["response"].get("request_id") == request_id:
            return message["response"]["response"]


def scenario_turn(message):
    text = prompt_text(message)
    system_init()
    stream({"type": "message_start", "message": {"id": "msg_think"}})
    stream({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})
    stream({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Considering "}})
    stream({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "the request."}})
    stream({"type": "content_block_stop", "index": 0})
    assistant([{"type": "thinking", "thinking": "Considering the request.", "signature": "sig"}], "msg_think")
    assistant([{"type": "text", "text": "Working on: " + text}], "msg_a")
    assistant([{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "echo hi", "description": "say hi"}}], "msg_b")
    user_result("toolu_1", "hi\n", structured={"stdout": "hi\n", "stderr": "", "interrupted": False})
    assistant([{"type": "tool_use", "id": "toolu_2", "name": "TodoWrite", "input": {"todos": [{"content": "Write it", "status": "completed", "activeForm": "Writing"}, {"content": "Test it", "status": "in_progress", "activeForm": "Testing"}]}}], "msg_c")
    user_result("toolu_2", "Todos have been modified")
    assistant([{"type": "text", "text": "done"}], "msg_d")
    result("done")


def scenario_approval(message):
    system_init()
    assistant([{"type": "tool_use", "id": "toolu_w", "name": "Write", "input": {"file_path": "/tmp/fixture.txt", "content": "x"}}], "msg_w")
    answer = can_use_tool("req-approval", "Write", {"file_path": "/tmp/fixture.txt", "content": "x"}, "toolu_w", title="Claude wants to write /tmp/fixture.txt", permission_suggestions=[{"type": "addRules", "rules": [{"toolName": "Write"}], "behavior": "allow", "destination": "localSettings"}])
    if answer.get("behavior") == "allow":
        user_result("toolu_w", "File created", structured={"type": "create", "filePath": "/tmp/fixture.txt"})
        assistant([{"type": "text", "text": "written"}], "msg_x")
        result("written")
    else:
        user_result("toolu_w", "User declined tool execution.", is_error=True)
        assistant([{"type": "text", "text": "declined: " + answer.get("message", "")}], "msg_x")
        result("declined")


def scenario_question(message):
    system_init()
    questions = [{"question": "Which database?", "header": "Database", "options": [{"label": "Postgres", "description": "Relational"}, {"label": "SQLite", "description": "Embedded"}], "multiSelect": False}]
    assistant([{"type": "tool_use", "id": "toolu_q", "name": "AskUserQuestion", "input": {"questions": questions}}], "msg_q")
    answer = can_use_tool("req-question", "AskUserQuestion", {"questions": questions}, "toolu_q")
    chosen = (answer.get("updatedInput") or {}).get("answers", {})
    user_result("toolu_q", json.dumps(chosen))
    assistant([{"type": "text", "text": "chosen: " + json.dumps(chosen, sort_keys=True)}], "msg_r")
    result("chosen")


def scenario_hang(message):
    system_init()
    assistant([{"type": "tool_use", "id": "toolu_h_" + uid(), "name": "Bash", "input": {"command": "sleep 600"}}], "msg_h_" + uid())
    scenario_hang_loop()


def scenario_hang_loop():
    while True:
        incoming = receive()
        if incoming is None:
            sys.exit(0)
        log({"received": incoming})
        if incoming.get("type") == "control_request" and incoming["request"].get("subtype") == "interrupt":
            respond(incoming["request_id"], {"still_queued": []})
            result("", subtype="error_during_execution", is_error=True, errors=["Request was interrupted by user"], terminal_reason="aborted_tools")


def scenario_steer(message):
    system_init()
    assistant([{"type": "text", "text": "started"}], "msg_s1")
    while True:
        incoming = receive()
        if incoming is None:
            sys.exit(0)
        log({"received": incoming})
        if incoming.get("type") == "user" and incoming.get("priority") == "now":
            assistant([{"type": "text", "text": "steered: " + prompt_text(incoming)}], "msg_s2")
            result("steered")
            return


def scenario_crash(message):
    system_init()
    sys.stderr.write("fixture crash\n")
    sys.stderr.flush()
    sys.exit(3)


def scenario_error(message):
    system_init()
    result("rate limited", is_error=True, api_error_status=429)


def scenario_limit(message):
    system_init()
    reset = int(time.time()) + 5400
    for _ in range(2):
        send({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "utilization": 1.0, "resetsAt": reset}, "uuid": uid(), "session_id": SESSION})
    frame = {"type": "assistant", "message": {"id": "msg_limit", "type": "message", "role": "assistant", "model": flag("--model"), "content": [{"type": "text", "text": "You've hit your limit"}], "usage": USAGE, "stop_reason": None}, "parent_tool_use_id": None, "session_id": SESSION, "uuid": uid(), "error": "rate_limit"}
    send(frame)
    result("You've hit your limit", is_error=True, terminal_reason="blocking_limit")


def scenario_warning(message):
    system_init()
    send({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed_warning", "rateLimitType": "seven_day", "utilization": 0.9, "resetsAt": int(time.time()) + 86400}, "uuid": uid(), "session_id": SESSION})
    assistant([{"type": "text", "text": "fine"}], "msg_ok")
    result("fine")


def task_frame(subtype, task_id, **fields):
    frame = {"type": "system", "subtype": subtype, "task_id": task_id, "uuid": uid(), "session_id": SESSION}
    frame.update(fields)
    send(frame)


def launch_agent(tool_use_id, prompt, **fields):
    assistant([{"type": "tool_use", "id": tool_use_id, "name": "Agent", "input": {"description": "Inspect", "prompt": prompt, "subagent_type": "general-purpose"}}], "msg_" + tool_use_id)
    task_frame("task_started", "task_" + tool_use_id, tool_use_id=tool_use_id, description="Inspect", task_type="local_agent", prompt=prompt, **fields)


def scenario_subagent_approval(message):
    """A subagent asks to run a command; the request is raised on the parent and answered there."""
    system_init()
    launch_agent("toolu_agent", "Run ls")
    child = {"command": "ls", "description": "list"}
    assistant([{"type": "tool_use", "id": "toolu_child", "name": "Bash", "input": child}], "msg_child", parent="toolu_agent")
    answer = can_use_tool("req-child", "Bash", child, "toolu_child", title="Claude wants to run ls")
    allowed = answer.get("behavior") == "allow"
    user_result("toolu_child", "a.txt\n" if allowed else "User declined tool execution.", is_error=not allowed, parent="toolu_agent")
    assistant([{"type": "text", "text": "listed" if allowed else "declined"}], "msg_child_text", parent="toolu_agent")
    task_frame("task_notification", "task_toolu_agent", tool_use_id="toolu_agent", status="completed", summary="Inspect", output_file="")
    user_result("toolu_agent", [{"type": "text", "text": "listed" if allowed else "declined"}], structured={"status": "completed", "content": [{"type": "text", "text": "listed" if allowed else "declined"}]})
    assistant([{"type": "text", "text": "finished"}], "msg_done")
    result("finished")


def scenario_children_running(message):
    """A subagent has a command open when the parent turn is stopped."""
    system_init()
    launch_agent("toolu_agent", "Sleep")
    assistant([{"type": "tool_use", "id": "toolu_child", "name": "Bash", "input": {"command": "sleep 600"}}], "msg_child", parent="toolu_agent")
    scenario_hang_loop()


def scenario_background_task(message):
    """A background command outlives the turn that started it; the CLI keeps running until it is closed."""
    system_init()
    assistant([{"type": "tool_use", "id": "toolu_bg", "name": "Bash", "input": {"command": "sleep 600", "run_in_background": True}}], "msg_bg")
    task_frame("task_started", "bg_1", tool_use_id="toolu_bg", description="sleep 600", task_type="local_bash", is_backgrounded=True)
    send({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "bg_1", "task_type": "local_bash", "description": "sleep 600"}], "uuid": uid(), "session_id": SESSION})
    user_result("toolu_bg", "started in the background")
    assistant([{"type": "text", "text": "started"}], "msg_started")
    result("started")


def scenario_background_wake(message):
    """The background command finishes after the turn ended; Claude wakes itself to report it."""
    scenario_background_task(message)
    send({"type": "system", "subtype": "background_tasks_changed", "tasks": [], "uuid": uid(), "session_id": SESSION})
    task_frame("task_notification", "bg_1", tool_use_id="toolu_bg", status="completed", summary="sleep 600 finished", output_file="")
    system_init()
    assistant([{"type": "text", "text": "the command finished"}], "msg_wake")
    result("the command finished")


SCENARIOS = {"subagent_approval": scenario_subagent_approval, "children_running": scenario_children_running, "background_task": scenario_background_task, "background_wake": scenario_background_wake, "limit": scenario_limit, "warning": scenario_warning, "turn": scenario_turn, "approval": scenario_approval, "question": scenario_question, "hang": scenario_hang, "steer": scenario_steer, "crash": scenario_crash, "error": scenario_error}


def serve_scenario():
    script = SCENARIOS[os.environ["CLAUDE_FIXTURE_SCENARIO"]]
    while True:
        message = receive()
        if message is None:
            log({"stdin": "closed"})
            return
        log({"received": message})
        if message.get("type") == "control_request":
            if message["request"].get("subtype") == "initialize":
                respond(message["request_id"], init_response())
            else:
                respond(message["request_id"])
            continue
        if message.get("type") == "user":
            script(message)


# --- recorded transcript replay -------------------------------------------------------------
def serve_transcript():
    global cursor
    path = os.environ["CLAUDE_FIXTURE_TRANSCRIPT"]
    state_path = os.environ["CLAUDE_FIXTURE_STATE"]
    global save
    def save():
        with open(state_path, "w") as handle:
            handle.write(str(cursor))
    entries = [json.loads(line) for line in open(path) if line.strip()]
    cursor = int(open(state_path).read()) if os.path.exists(state_path) else 1
    # A launch consumes the recorded `query.open`.
    if entries[cursor]["type"] == "expect_outbound" and entries[cursor]["frame"]["type"] == "query.open":
        cursor += 1
        save()
    else:
        sys.stderr.write("transcript has no query.open for this launch at %d\n" % cursor)
        sys.exit(4)
    store_subagent_sidecars(entries)
    while True:
        message = receive()
        if message is None:
            return
        log({"received": message})
        kind = message.get("type")
        if kind == "control_request" and message["request"].get("subtype") == "initialize":
            respond(message["request_id"], init_response())
            continue
        if kind == "user":
            entry = entries[cursor]
            expected = entry["frame"]["message"]["message"]["content"] if entry["type"] == "expect_outbound" and entry["frame"]["type"] == "prompt.offer" else None
            if expected is None:
                sys.stderr.write("unexpected prompt at transcript cursor %d\n" % cursor)
                sys.exit(5)
            offered = entry["frame"]["message"].get("uuid")
            if offered and message.get("uuid"):
                PROMPT_UUIDS[offered] = message["uuid"]
            cursor += 1
            save()
            pump(entries)
            continue
        if kind == "control_request" and message["request"].get("subtype") == "interrupt":
            entry = entries[cursor] if cursor < len(entries) else None
            if entry and entry["type"] == "expect_outbound" and entry["frame"]["type"] == "query.interrupt":
                cursor += 1
                save()
                respond(message["request_id"], {"still_queued": []})
                pump(entries)
            else:
                respond(message["request_id"], {"still_queued": []})
            continue
        if kind == "control_request":
            respond(message["request_id"])
            continue


def store_subagent_sidecars(entries):
    """What the CLI keeps from an earlier process: a subagent's launching call, beside its transcript."""
    config = os.environ.get("CLAUDE_CONFIG_DIR")
    for index, entry in enumerate(entries):
        if entry["type"] != "expect_outbound" or entry["frame"]["type"] != "subagent.lookup":
            continue
        found = next((later["frame"] for later in entries[index + 1:] if later["type"] == "emit_inbound" and later["frame"]["type"] == "subagent.found"), None)
        if not config or not found or not found.get("toolUseId"):
            continue
        directory = os.path.join(config, "projects", "-fixture", SESSION, "subagents")
        os.makedirs(directory, exist_ok=True)
        with open(os.path.join(directory, "agent-%s.meta.json" % entry["frame"]["agentId"]), "w") as handle:
            json.dump({"toolUseId": found["toolUseId"]}, handle)


def pump(entries):
    """Emit recorded inbound frames until the next expected outbound frame (or the end)."""
    global cursor
    while cursor < len(entries):
        entry = entries[cursor]
        if entry["type"] == "runtime_exit":
            cursor += 1
            save()
            sys.exit(0 if entry.get("status") == "success" else 1)
        if entry["type"] == "expect_outbound":
            frame = entry["frame"]
            if frame["type"] in ("prompt.offer", "query.interrupt"):
                return
            if frame["type"] == "permission.response":
                return
            if frame["type"] == "subagent.lookup":
                # The adapter reads the launch call from session storage; the sidecar is written at launch.
                cursor += 1
                save()
                continue
            sys.stderr.write("unsupported outbound frame %s\n" % frame["type"])
            sys.exit(6)
        frame = entry["frame"]
        cursor += 1
        save()
        if frame["type"] == "permission.request":
            options = frame["options"]
            request = {"subtype": "can_use_tool", "tool_name": frame["toolName"], "input": frame["input"], "tool_use_id": options["toolUseID"]}
            for source, target in (("suggestions", "permission_suggestions"), ("blockedPath", "blocked_path"), ("decisionReason", "decision_reason"), ("title", "title"), ("displayName", "display_name"), ("description", "description"), ("agentID", "agent_id")):
                if options.get(source) is not None:
                    request[target] = options[source]
            request_id = "req-" + options["toolUseID"]
            send({"type": "control_request", "request_id": request_id, "request": request})
            answer = None
            while answer is None:
                message = receive()
                if message is None:
                    sys.exit(0)
                log({"received": message})
                if message.get("type") == "control_response" and message["response"].get("request_id") == request_id:
                    answer = message["response"]["response"]
            expected = entries[cursor]["frame"]["result"]
            cursor += 1
            save()
            if answer.get("behavior") != expected.get("behavior"):
                sys.stderr.write("permission answer %s differs from recorded %s\n" % (answer, expected))
                sys.exit(7)
            continue
        if frame["type"] in ("session.forked", "subagent.found"):
            continue
        # A frame the test holds back (a wake turn's result, the end of a roster) waits for its release.
        hold = GATES.get(entry.get("label"))
        if hold:
            with open(hold) as released:
                released.read()
        send(with_prompt_uuids(frame))


PROMPT_UUIDS = {}


def with_prompt_uuids(frame):
    """A recorded frame names the prompt uuid of the recording; the adapter offered its own."""
    if not PROMPT_UUIDS or not isinstance(frame, dict):
        return frame
    frame = dict(frame)
    for key in ("command_uuid", "user_message_uuid"):
        if isinstance(frame.get(key), str):
            frame[key] = PROMPT_UUIDS.get(frame[key], frame[key])
    if isinstance(frame.get("user_message_uuids"), list):
        frame["user_message_uuids"] = [PROMPT_UUIDS.get(uuid, uuid) for uuid in frame["user_message_uuids"]]
    return frame


GATES = json.loads(os.environ.get("CLAUDE_FIXTURE_GATES", "{}"))
cursor = 0
if PROBE:
    while True:
        message = receive()
        if message is None:
            sys.exit(0)
        log({"received": message})
        if message.get("type") == "control_request":
            subtype = message["request"].get("subtype")
            if subtype == "get_usage":
                if os.environ.get("CLAUDE_FIXTURE_USAGE_FAIL"):
                    respond_error(message["request_id"], "usage unavailable")
                else:
                    respond(message["request_id"], json.loads(os.environ.get("CLAUDE_FIXTURE_USAGE", '{"rate_limits_available": false}')))
            else:
                respond(message["request_id"], init_response())
elif "CLAUDE_FIXTURE_TRANSCRIPT" in os.environ:
    serve_transcript()
else:
    serve_scenario()
