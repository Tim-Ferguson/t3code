// Root-operated resident pilot; lifecycle telemetry only, actual UI inspected by CUA.
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import readline from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import { captureDescendants, verifyDescendantsExit } from "./owned-process-lifecycle.mjs";
const [configPath, runtime, rawRoot] = process.argv.slice(2);
if (!configPath || !["original", "rust"].includes(runtime) || !rawRoot)
  throw Error("config.json original|rust /private/tmp/t3port-bench-pilot-... required");
const config = JSON.parse(fs.readFileSync(configPath, "utf8")),
  root = path.resolve(rawRoot);
if (!root.startsWith("/private/tmp/t3port-bench-") || fs.existsSync(root))
  throw Error("Fresh isolated pilot root required");
const executable = runtime === "rust" ? config.rustExecutable : config.originalExecutable;
if (!fs.existsSync(executable)) throw Error("Built artifact missing");
if (runtime === "rust") {
  const plist = path.resolve(path.dirname(executable), "../Info.plist");
  const bundle = spawnSync(
    "/usr/bin/plutil",
    ["-extract", "CFBundleIdentifier", "raw", "-o", "-", plist],
    { encoding: "utf8" },
  );
  if (bundle.status !== 0 || bundle.stdout.trim() !== config.rustBundleIdentifier)
    throw Error("Benchmark bundle ID mismatch");
}
fs.mkdirSync(root, { recursive: true });
const webkitStoreIdentifier = crypto.randomUUID().replaceAll("-", "");
const env = {
  ...process.env,
  T3_BENCH: "1",
  T3_BENCH_ROOT: root,
  T3_BENCH_WEBKIT_UUID: webkitStoreIdentifier,
  T3_SERVER_URL: "",
  T3_UI_DATA_DIR: path.join(root, "webview"),
  T3CODE_HOME: path.join(root, "t3-home"),
  T3CODE_DISABLE_AUTO_UPDATE: "true",
  XDG_CONFIG_HOME: path.join(root, "xdg-config"),
  XDG_CACHE_HOME: path.join(root, "xdg-cache"),
};
delete env.ELECTRON_RUN_AS_NODE;
delete env.VITE_DEV_SERVER_URL;
delete env.T3CODE_DESKTOP_DEV;
if (runtime === "original") env.T3_BENCH_ENTRY = config.originalEntry;
const child = spawn(executable, runtime === "original" ? [config.originalBootstrap] : [], {
  cwd: root,
  env,
  stdio: ["ignore", "pipe", "pipe"],
});
const metadata = {
  kind: "desktop-benchmark-pilot",
  runtime,
  pid: child.pid,
  executable,
  root,
  ...(runtime === "rust"
    ? { webkitStoreIdentifier, bundleIdentifier: config.rustBundleIdentifier }
    : {}),
  note: "No timed comparison; root verifies actual window via CUA",
};
fs.writeFileSync(path.join(root, "pilot.json"), JSON.stringify(metadata, null, 2) + "\n");
console.log(JSON.stringify(metadata));
readline.createInterface({ input: child.stdout }).on("line", (line) => {
  let row;
  try {
    row = JSON.parse(line);
  } catch {
    return;
  }
  if (row.kind === "desktop-benchmark-marker" && row.pid === child.pid)
    console.log(JSON.stringify(row));
});
const stderr = fs.createWriteStream(path.join(root, "stderr.log"));
child.stderr.pipe(stderr);
let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  const observed = captureDescendants(child.pid);
  for (const signal of ["SIGINT", "SIGTERM", "SIGKILL"]) {
    if (child.exitCode !== null || child.signalCode !== null) break;
    child.kill(signal);
    const deadline = Date.now() + 5000;
    while (child.exitCode === null && child.signalCode === null && Date.now() < deadline)
      await delay(50);
  }
  const cleanup = await verifyDescendantsExit(observed);
  fs.writeFileSync(path.join(root, "cleanup.json"), JSON.stringify(cleanup, null, 2) + "\n");
  console.log(
    JSON.stringify({ kind: "desktop-benchmark-pilot-stopped", pid: child.pid, ...cleanup }),
  );
  if (!cleanup.allObservedExited) process.exitCode = 1;
}
process.once("SIGINT", stop);
process.once("SIGTERM", stop);
child.once("error", (error) => {
  console.error(String(error));
  process.exitCode = 1;
});
child.once("exit", (code, signal) => {
  console.log(
    JSON.stringify({ kind: "desktop-benchmark-pilot-exit", pid: child.pid, code, signal }),
  );
});
