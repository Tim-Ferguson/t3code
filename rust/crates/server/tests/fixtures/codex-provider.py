#!/usr/bin/env python3
"""Deterministic test-only Codex app-server. Never runs tools or reads agent state.

Use binaryPath pointing to this executable in isolated native settings. Prompts
containing [approval], [question], or [hold] exercise the corresponding runtime
branches; otherwise one tool and an assistant response complete immediately.
"""
import json
import os
import sys
import socket

threads = {}
turns = {}
requests = {}
serial = 0


def send(message):
    print(json.dumps(message), flush=True)


def notify(method, params):
    send({"method": method, "params": params})


def finish(turn, status="completed"):
    turn["status"] = status
    notify("turn/completed", {"threadId": turn["threadId"], "turn": {
        "id": turn["id"], "status": status, "items": [], "error": None,
        "startedAt": 1800000000, "completedAt": 1800000001,
    }})


def assistant(turn, text):
    item = {"id": turn["id"] + "-assistant", "type": "agentMessage", "text": "", "phase": "final_answer"}
    params = {"threadId": turn["threadId"], "turnId": turn["id"], "item": item}
    notify("item/started", params)
    if "[crash]" in text:
        os._exit(9)
    notify("item/agentMessage/delta", {"threadId": turn["threadId"], "turnId": turn["id"], "itemId": item["id"], "delta": text})
    item["text"] = text
    notify("item/completed", params)


def start_turn(turn):
    thread_id, turn_id, text = turn["threadId"], turn["id"], turn["text"]
    notify("turn/started", {"threadId": thread_id, "turn": {
        "id": turn_id, "status": "inProgress", "items": [], "error": None,
        "startedAt": 1800000000, "completedAt": None,
    }})
    tool = {"id": turn_id + "-tool", "type": "commandExecution", "command": "printf 'fixture tool output'", "cwd": os.getcwd(), "status": "inProgress", "commandActions": [], "aggregatedOutput": None, "exitCode": None, "durationMs": None, "processId": None}
    params = {"threadId": thread_id, "turnId": turn_id, "item": tool}
    notify("item/started", params)
    if "[approval]" in text:
        request_id = 1001 if os.environ.get("FIXTURE_NUMERIC_REQUEST_IDS") else turn_id + "-approval"
        requests[request_id] = (turn, "approval", tool)
        send({"id": request_id, "method": "item/commandExecution/requestApproval", "params": {"threadId": thread_id, "turnId": turn_id, "itemId": tool["id"], "approvalId": request_id, "reason": "Fixture approval", "command": tool["command"], "cwd": os.getcwd(), "availableDecisions": ["accept", "decline", "cancel"]}})
        return
    complete_tool(turn, tool)
    if "[question]" in text:
        request_id = 1002 if os.environ.get("FIXTURE_NUMERIC_REQUEST_IDS") else turn_id + "-question"
        requests[request_id] = (turn, "question", None)
        send({"id": request_id, "method": "item/tool/requestUserInput", "params": {"threadId": thread_id, "turnId": turn_id, "itemId": tool["id"], "questions": [{"id": "choice", "header": "Fixture", "question": "Choose a fixture response", "isOther": False, "isSecret": False, "options": [{"label": "One", "description": "First response"}, {"label": "Two", "description": "Second response"}]}]}})
    else:
        assistant(turn, "Fixture response: " + text)
        if "[hold]" not in text:
            finish(turn)


def complete_tool(turn, tool):
    notify("item/commandExecution/outputDelta", {"threadId": turn["threadId"], "turnId": turn["id"], "itemId": tool["id"], "delta": "fixture tool output\n"})
    tool.update({"status": "completed", "aggregatedOutput": "fixture tool output\n", "exitCode": 0, "durationMs": 1})
    notify("item/completed", {"threadId": turn["threadId"], "turnId": turn["id"], "item": tool})


for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    method = message.get("method")
    params = message.get("params") or {}
    if method is None:
        pending = requests.pop(request_id, None)
        if pending:
            turn, kind, tool = pending
            result = message.get("result") or {}
            decision = result.get("decision")
            if kind == "question":
                assert set(result.get("answers", {})) <= {"choice"}
                for answer in result.get("answers", {}).values():
                    assert isinstance(answer, dict) and set(answer) == {"answers"}
                    assert isinstance(answer["answers"], list) and all(isinstance(item, str) for item in answer["answers"])
            elif kind == "approval":
                assert decision in ("accept", "acceptForSession", "decline", "cancel")
            if kind == "approval":
                if decision in ("accept", "acceptForSession"):
                    complete_tool(turn, tool)
                else:
                    tool["status"] = "declined"
                    notify("item/completed", {"threadId": turn["threadId"], "turnId": turn["id"], "item": tool})
            assistant(turn, "Fixture " + kind + " answered: " + json.dumps(result, sort_keys=True))
            finish(turn, "interrupted" if decision == "cancel" else "completed")
        continue
    if method == "initialized":
        continue
    result = None
    after_response = None
    if method == "initialize":
        result = {"userAgent": "codex/0.156.1 fixture", "platformFamily": "unix", "platformOs": sys.platform}
    elif method == "account/read":
        result = {"account": None, "requiresOpenaiAuth": False}
    elif method == "model/list":
        result = {"data": [{"id": "fixture-model", "model": "fixture-model", "displayName": "Fixture Model", "description": "Deterministic native tests", "hidden": False, "isDefault": True, "defaultReasoningEffort": "low", "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "Fixture"}], "inputModalities": ["text"], "supportsPersonality": False}], "nextCursor": None}
    elif method == "skills/list":
        result = {"data": [{"cwd": cwd, "skills": [], "errors": []} for cwd in params.get("cwds", [])]}
    elif method == "account/rateLimits/read":
        result = {"rateLimits": {"limitId": "fixture", "limitName": "Fixture", "primary": None, "secondary": None, "credits": None, "planType": "unknown"}, "rateLimitsByLimitId": None, "rateLimitResetCredits": None}
    elif method in ("thread/start", "thread/resume"):
        serial += 1
        native_id = params.get("threadId") or "fixture-thread-" + str(serial)
        thread = {"id": native_id, "preview": "", "ephemeral": False, "modelProvider": "openai", "createdAt": 1800000000, "updatedAt": 1800000000, "status": {"type": "idle"}, "cwd": params.get("cwd", os.getcwd()), "path": None, "turns": [], "gitInfo": None, "source": "appServer", "cliVersion": "0.156.1", "agentNickname": None, "agentRole": None, "memoryMode": "enabled"}
        threads[native_id] = thread
        result = {"thread": thread, "model": params.get("model", "fixture-model"), "modelProvider": "openai", "cwd": thread["cwd"], "approvalPolicy": params.get("approvalPolicy", "on-request"), "sandbox": {"type": "workspaceWrite"}, "reasoningEffort": "low"}
    elif method == "thread/read":
        result = {"thread": threads[params["threadId"]]}
    elif method == "turn/start":
        serial += 1
        native_id = "fixture-turn-" + str(serial)
        text = "\n".join(item.get("text", "") for item in params.get("input", []) if item.get("type") == "text")
        turn = {"id": native_id, "threadId": params["threadId"], "text": text, "status": "inProgress"}
        turns[native_id] = turn
        if "[hang-start]" in text:
            signal = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            signal.sendto(json.dumps({"text": text, "pid": os.getpid()}).encode(), os.environ["FIXTURE_REQUEST_SIGNAL"])
            signal.close()
            continue
        result = {"turn": {"id": native_id, "items": [], "status": "inProgress", "error": None, "startedAt": 1800000000, "completedAt": None}}
        after_response = lambda: start_turn(turn)
    elif method == "turn/interrupt":
        result = {}
        turn = turns[params["turnId"]]
        after_response = lambda: finish(turn, "interrupted")
    else:
        send({"id": request_id, "error": {"code": -32601, "message": "Fixture method not found: " + method}})
        continue
    send({"id": request_id, "result": result})
    if after_response:
        after_response()
