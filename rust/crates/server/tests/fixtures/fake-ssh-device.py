#!/usr/bin/env python3
"""A stand-in for `ssh` for the device-host tests. It never connects anywhere.

DIRECTORY (replaced by the test) holds what it answers and what it saw:
  reply.json         the start reply (nodePath, platforms, hubPort, daemonPort, token, entryPath, helpers, tools)
  commands.json      {"<command>": {"stdout", "stderr", "code"}} for host commands run over ssh
  fail-<mode>        a bootstrap in that mode fails with exit 255 while this file exists
  fail-forward       the next forward exits at once with an error, and the file is removed
  calls.jsonl        one line per run
  forwarders.log     "<pid> <local port>" per forward that is up
"""
import json
import os
import re
import shlex
import socket
import sys
import threading

DIRECTORY = "@@DIRECTORY@@"
argv = sys.argv[1:]


def path(name):
    return os.path.join(DIRECTORY, name)


def log(entry):
    with open(path("calls.jsonl"), "a", encoding="utf-8") as stream:
        stream.write(json.dumps(entry) + "\n")


if "-G" in argv:
    # Address resolution (`ssh -G <alias>`): a remote host.
    print("hostname 192.0.2.1")
    print("port 22")
    sys.exit(0)

if "-N" in argv:
    log({"kind": "forward", "argv": argv, "pid": os.getpid()})
    if os.path.exists(path("fail-forward")):
        os.remove(path("fail-forward"))
        sys.stderr.write("Port already bound")
        sys.exit(255)
    specs = [argv[index + 1] for index, value in enumerate(argv) if value == "-L"]

    def pump(source, sink):
        try:
            while True:
                chunk = source.recv(65536)
                if not chunk:
                    break
                sink.sendall(chunk)
        except OSError:
            pass
        finally:
            for side in (source, sink):
                try:
                    side.close()
                except OSError:
                    pass

    def serve(listener, remote_port):
        while True:
            client, _ = listener.accept()
            try:
                upstream = socket.create_connection(("127.0.0.1", remote_port))
            except OSError:
                client.close()
                continue
            threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
            threading.Thread(target=pump, args=(upstream, client), daemon=True).start()

    for spec in specs:
        _, local_port, _, remote_port = spec.split(":")
        listener = socket.socket()
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", int(local_port)))
        # Recorded before it answers, so a test that sees the forward answer finds it listed.
        with open(path("forwarders.log"), "a", encoding="utf-8") as stream:
            stream.write(f"{os.getpid()} {local_port}\n")
        listener.listen()
        threading.Thread(target=serve, args=(listener, int(remote_port)), daemon=True).start()
    threading.Event().wait()

command = shlex.split(argv[-1])
tail = command[0].rsplit("exec ", 1)[1]
words = shlex.split(tail)
if words == ["node"]:
    script = sys.stdin.read()
    mode = re.search(r'const mode = "([^"]+)"', script).group(1)
    owner = re.search(r'const owner = "([^"]+)"', script).group(1)
    log({"kind": "bootstrap", "mode": mode, "owner": owner, "argv": argv, "pid": os.getpid()})
    if os.path.exists(path(f"fail-{mode}")):
        sys.stderr.write(f"fixture failure in {mode}")
        sys.exit(255)
    with open(path("reply.json"), encoding="utf-8") as stream:
        reply = json.load(stream)
    if mode == "probe":
        print(json.dumps({key: reply[key] for key in ("nodePath", "platforms", "tools") if key in reply}))
    elif mode in ("start", "agent-start"):
        if mode == "start":
            for key in ("daemonPort", "token", "entryPath"):
                reply.pop(key, None)
        print(json.dumps(reply))
    sys.exit(0)

log({"kind": "command", "words": words, "stdin": sys.stdin.read(), "pid": os.getpid()})
with open(path("commands.json"), encoding="utf-8") as stream:
    answers = json.load(stream)
answer = answers.get(words[0], {})
sys.stdout.write(answer.get("stdout", ""))
sys.stderr.write(answer.get("stderr", ""))
sys.exit(answer.get("code", 0))
