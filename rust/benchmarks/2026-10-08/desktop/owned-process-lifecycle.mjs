// Process table telemetry only. Never inspects windows/DOM/AX or signals helpers.
import { spawnSync } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
function processTable() {
  const ps = spawnSync("/bin/ps", ["-axo", "pid=,ppid=,lstart="], {
    encoding: "utf8",
    env: { ...process.env, LC_ALL: "C" },
  });
  if (ps.status !== 0) throw Error("Cannot verify benchmark process descendants");
  return ps.stdout.split("\n").flatMap((line) => {
    const m = /^\s*(\d+)\s+(\d+)\s+(.+?)\s*$/.exec(line);
    return m ? [{ pid: Number(m[1]), parentPid: Number(m[2]), started: m[3] }] : [];
  });
}
export function captureDescendants(parentPid) {
  const table = processTable(),
    pids = new Set([parentPid]),
    captured = [];
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of table)
      if (!pids.has(row.pid) && pids.has(row.parentPid)) {
        pids.add(row.pid);
        captured.push(row);
        changed = true;
      }
  }
  return captured;
}
export async function verifyDescendantsExit(captured, timeoutMs = 3000) {
  const started = Date.now();
  let remaining = [];
  do {
    const table = new Map(processTable().map((row) => [row.pid, row]));
    remaining = captured.filter((row) => table.get(row.pid)?.started === row.started);
    if (!remaining.length) break;
    await delay(100);
  } while (Date.now() - started < timeoutMs);
  return {
    captured,
    remaining,
    allObservedExited: remaining.length === 0,
    scope:
      "Observed descendants only; no shared/OS-owned XPC processes are queried by name or signaled",
  };
}
