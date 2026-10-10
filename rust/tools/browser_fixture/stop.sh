#!/bin/sh
# Stops the server (and the fake GitHub API) that start.sh started under --scratch, by the PIDs it recorded, after
# checking that each PID still belongs to the process that was started: the server's executable path and working
# directory, the fake API's script path. Never by name or pattern. The scratch directory is left for inspection.
set -u
scratch=""
while [ $# -gt 0 ]; do
  case "$1" in
    --scratch) scratch="$2"; shift 2 ;;
    *) echo "usage: stop.sh --scratch DIR" >&2; exit 2 ;;
  esac
done
[ -n "$scratch" ] && [ -f "$scratch/server.pid" ] && [ -f "$scratch/meta.txt" ] || { echo "stop.sh: $scratch has no server.pid/meta.txt from start.sh" >&2; exit 2; }
meta() { sed -n "s/^$1=//p" "$scratch/meta.txt" | head -n 1; }
here=$(cd "$(dirname "$0")" && pwd)

# stop_checked PID EXPECTED_COMMAND_PREFIX_OR_SUBSTRING LABEL [EXPECTED_CWD]
stop_checked() {
  pid="$1"; expect="$2"; label="$3"; cwd="${4:-}"
  case "$pid" in ''|*[!0-9]*) echo "$label: no usable pid"; return 0 ;; esac
  if ! kill -0 "$pid" 2>/dev/null; then echo "$label: pid $pid is not running"; return 0; fi
  command=$(ps -p "$pid" -o command= 2>/dev/null)
  case "$command" in
    *"$expect"*) ;;
    *) echo "$label: pid $pid is running '$command', not '$expect'; leaving it alone" >&2; return 1 ;;
  esac
  if [ -n "$cwd" ]; then
    actual=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
    if [ "$actual" != "$cwd" ]; then echo "$label: pid $pid has working directory '$actual', not '$cwd'; leaving it alone" >&2; return 1; fi
  fi
  kill "$pid"
  n=0
  while kill -0 "$pid" 2>/dev/null && [ $n -lt 100 ]; do sleep 0.1; n=$((n+1)); done
  if kill -0 "$pid" 2>/dev/null; then echo "$label: pid $pid did not exit within 10s (still running)" >&2; return 1; fi
  echo "$label: pid $pid stopped"
}

status=0
stop_checked "$(cat "$scratch/server.pid")" "$(meta server)" "server" "$(meta cwd)" || status=1
api=$(meta githubApiPid)
[ -n "$api" ] && { stop_checked "$api" "fake_github_api.py --port $(meta githubApiPort)" "fake github api" || status=1; }
exit $status
