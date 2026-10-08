"""Isolated device protocol fixture; never launches a real simulator."""
import base64
import hashlib
import http.server
import json
import os
import signal
import socket
import sys
from urllib.parse import parse_qs, urlparse

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
        if self.path.startswith("/api/devices/ws") and self.headers.get("Upgrade", "").lower() == "websocket":
            self.milestone()
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.wfile.write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").encode())
            self.wfile.write(b"\x82\x09\x00fixture\xff")
            self.wfile.flush()
            try:
                while True:
                    head = self.rfile.read(2)
                    if len(head) != 2:
                        break
                    length = head[1] & 127
                    if length == 126:
                        length = int.from_bytes(self.rfile.read(2), "big")
                    elif length == 127:
                        length = int.from_bytes(self.rfile.read(8), "big")
                    mask = self.rfile.read(4) if head[1] & 128 else None
                    payload = self.rfile.read(length)
                    if mask:
                        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
                    opcode = head[0] & 15
                    if opcode == 8:
                        break
                    if opcode in (1, 2):
                        self.wfile.write(bytes([128 | opcode, len(payload)]) + payload)
                        self.wfile.flush()
            finally:
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
                    channel.sendto(json.dumps({"path": self.path + "#closed", "pid": os.getpid()}).encode(), os.environ["FIXTURE_MILESTONES"])
            return
        if self.path.startswith("/vendor/serve-sim/helper/proxy/stream.avcc"):
            self.milestone()
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.end_headers()
            self.wfile.write(b"\x00stream-frame\xff")
            self.wfile.flush()
            self.connection.recv(1)
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
                channel.sendto(json.dumps({"path": self.path + "#closed", "pid": os.getpid()}).encode(), os.environ["FIXTURE_MILESTONES"])
            return
        if self.path.startswith("/vendor/serve-sim/helper/proxy/health"):
            self.milestone()
            encoding = parse_qs(urlparse(self.path).query).get("encoding", [None])[0]
            if encoding:
                with open(os.environ["FIXTURE_COMPRESSED_DATA"], encoding="utf-8") as file:
                    record = next(json.loads(line) for line in file if json.loads(line)["encoding"] == encoding)
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Encoding", encoding)
                self.end_headers()
                self.wfile.write(base64.b64decode(record["compressed"]))
            else:
                self.answer({"path": self.path, "headers": dict(self.headers)})
            return
        if not self.milestone():
            return
        if self.path == "/readyz":
            self.answer({"ready": True})
        elif self.path == "/api/devices":
            android = profile().get("androidId")
            self.answer({"simulators": [{"id": "fixture-ios", "name": "Fixture iPhone", "version": "18.0", "platform": "ios", "booted": booted, "physical": False}], "emulators": [{"id": android, "name": "Fixture Pixel", "version": "35", "platform": "android", "booted": True, "physical": False}] if android else [], "errors": []})
        else:
            self.send_error(404)

    def do_POST(self):
        global booted
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            raw = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    break
                raw += self.rfile.read(size)
                self.rfile.read(2)
        else:
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = json.loads(raw) if raw else {}
        if not self.milestone():
            return
        if self.path.startswith("/vendor/serve-sim/api/screenshot?") or self.path.startswith("/vendor/serve-emu/api/screenshot?"):
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.end_headers()
            self.wfile.write(b"\x89PNG\r\n\x1a\n")
            self.wfile.flush()
            if profile().get("bodyHeld") == self.path:
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
                    channel.sendto(json.dumps({"path": self.path, "pid": os.getpid(), "held": True, "body": True}).encode(), os.environ["FIXTURE_MILESTONES"])
                signal.pause()
            self.wfile.write(b"fixture screenshot")
        elif self.path == "/vendor/serve-emu/api/stream-settings":
            self.answer({"body": body, "headers": dict(self.headers)})
        elif self.path == "/api/devices/boot":
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
