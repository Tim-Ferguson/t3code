#!/usr/bin/env python3
"""Isolated agent CLI/daemon protocol fixture; only captures test-owned PIDs."""
import json, os, signal, socket, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

def milestone(event):
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as peer:
        peer.sendto(json.dumps({"event": event, "pid": os.getpid()}).encode(), os.environ["FIXTURE_MILESTONES"])

if sys.argv[1] == "serve":
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.end_headers(); self.wfile.write(b"healthy")
        def log_message(self, *args): pass
    server = HTTPServer(("127.0.0.1", 0), Handler)
    with open(sys.argv[2], "w") as file:
        json.dump({"httpPort": server.server_address[1], "token": "isolated token", "pid": os.getpid(), "version": "0.21.12"}, file)
    milestone("server")
    server.serve_forever()
elif sys.argv[1:3] == ["devices", "--json"]:
    with open(os.environ["FIXTURE_LOG"], "a") as file:
        file.write(json.dumps({"args": sys.argv[1:], "env": dict(os.environ)}) + "\n")
    milestone("bootstrap")
    if os.path.exists(os.environ["FIXTURE_HOLD_START"]): signal.pause()
    with open(os.path.join(os.environ["AGENT_DEVICE_STATE_DIR"], "daemon.json"), "w") as file:
        json.dump({"httpPort": 12345, "token": "isolated token"}, file)
elif sys.argv[1:4] == ["daemon", "stop", "--state-dir"]:
    with open(os.environ["FIXTURE_LOG"], "a") as file:
        file.write(json.dumps({"args": sys.argv[1:], "env": dict(os.environ)}) + "\n")
    gate = os.environ.get("FIXTURE_STOP_GATE")
    if gate:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
            try: os.unlink(gate)
            except FileNotFoundError: pass
            peer.bind(gate); peer.listen(1); milestone("stop")
            connection, _ = peer.accept(); connection.close()
        os.unlink(gate)
    else: milestone("stop")
    path = os.path.join(sys.argv[4], "daemon.json")
    try:
        with open(path) as file: daemon = json.load(file)
        if daemon.get("pid"): os.kill(daemon["pid"], signal.SIGTERM)
        os.unlink(path)
    except FileNotFoundError: pass
