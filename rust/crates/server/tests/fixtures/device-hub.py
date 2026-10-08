"""Isolated device protocol fixture; never launches a real simulator."""
import http.server
import json
import os
import signal
import socket
import sys

args = sys.argv[1:]
port = int(args[args.index("--port") + 1])
booted = False


def profile():
    with open(os.environ["FIXTURE_PROFILE"], encoding="utf-8") as stream:
        return json.load(stream)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, value):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(value).encode())

    def milestone(self):
        settings = profile()
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
            channel.sendto(json.dumps({"path": self.path, "pid": os.getpid(), "held": settings.get("held") == self.path}).encode(), os.environ["FIXTURE_MILESTONES"])
        if settings.get("held") == self.path:
            signal.pause()
        if settings.get("fail") == self.path:
            self.send_error(503)
            return False
        return True

    def do_GET(self):
        if not self.milestone():
            return
        if self.path == "/readyz":
            self.answer({"ready": True})
        elif self.path == "/api/devices":
            self.answer({"simulators": [{"id": "fixture-ios", "name": "Fixture iPhone", "version": "18.0", "platform": "ios", "booted": booted, "physical": False}], "emulators": [], "errors": []})
        else:
            self.send_error(404)

    def do_POST(self):
        global booted
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if not self.milestone():
            return
        if self.path == "/api/devices/boot":
            booted = True
            self.answer({"ok": True, "id": body["id"]})
        elif self.path.endswith("/shutdown"):
            booted = False
            self.answer({"ok": True})
        elif self.path.endswith("/start"):
            self.answer({"ok": True})
        else:
            self.send_error(404)


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
