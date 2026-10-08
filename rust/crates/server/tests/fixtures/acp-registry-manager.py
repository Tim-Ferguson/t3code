"""Isolated managed package installer; never calls a real npm or uv registry."""
import json
import os
import pathlib
import signal
import socket
import sys

arguments = sys.argv[1:]
npm = pathlib.Path(sys.argv[0]).name == "npm"
root = pathlib.Path(os.environ["npm_config_prefix"] if npm else os.environ["UV_TOOL_DIR"])
bin_directory = root / "bin" if npm else pathlib.Path(os.environ["UV_TOOL_BIN_DIR"])
executable = bin_directory / "devin"
with open(os.environ["REGISTRY_MANAGER_LOG"], "a") as log:
    log.write(json.dumps({"args": arguments, "root": str(root), "bin": str(bin_directory)}) + "\n")

if npm and arguments == ["root", "--global"]:
    print(root / "lib/node_modules")
elif npm and arguments == ["prefix", "--global"]:
    print(root)
elif not npm and arguments == ["tool", "list"]:
    if executable.exists():
        print("fixture-acp v1.2.3\n- devin")
elif not npm and arguments == ["tool", "dir", "--bin"]:
    print(bin_directory)
elif arguments == (["install", "--global", "fixture-acp@1.2.3"] if npm else ["tool", "install", "--force", "fixture-acp==1.2.3"]):
    if os.environ.get("REGISTRY_MANAGER_HOLD"):
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as started:
            started.sendto(str(os.getpid()).encode(), os.environ["REGISTRY_COMMAND_STARTED"])
        while True:
            signal.pause()
    bin_directory.mkdir(parents=True, exist_ok=True)
    executable.write_bytes(pathlib.Path(os.environ["REGISTRY_PROVIDER_SOURCE"]).read_bytes())
    executable.chmod(0o755)
    if npm:
        package = root / "lib/node_modules/fixture-acp"
        package.mkdir(parents=True, exist_ok=True)
        (package / "package.json").write_text(json.dumps({"name": "fixture-acp", "version": "1.2.3", "bin": {"devin": "index.py"}}))
else:
    raise AssertionError(arguments)
