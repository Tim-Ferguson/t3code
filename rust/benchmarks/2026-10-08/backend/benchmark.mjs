// Compare real production desktop backends; no application source is modified.
// Node 24+, built-in modules only. See plan.json and README.md before execution.
import fs from "node:fs/promises";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import os from "node:os";
import crypto from "node:crypto";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const exec = promisify(execFile);
const settings = {
  providers: Object.fromEntries(
    ["codex", "claudeAgent", "cursor", "grok", "pi", "opencode", "antigravity"].map((name) => [
      name,
      { enabled: false },
    ]),
  ),
  providerInstances: Object.fromEntries(
    [
      ["codex", "codex"],
      ["claudeAgent", "claudeAgent"],
      ["cursor", "cursor"],
      ["grok", "grok"],
      ["pi", "pi"],
      ["opencode", "opencode"],
      ["antigravity", "antigravity"],
    ].map(([id, driver]) => [id, { driver, enabled: false }]),
  ),
  enableAgentBrowserAccess: false,
  enableAgentDeviceAccess: false,
};
function summary(values) {
  const sorted = values.filter(Number.isFinite).toSorted((a, b) => a - b);
  if (!sorted.length) return null;
  const middle = sorted.length / 2;
  return {
    n: sorted.length,
    median:
      sorted.length % 2 ? sorted[Math.floor(middle)] : (sorted[middle - 1] + sorted[middle]) / 2,
    p95: sorted[Math.max(0, Math.ceil(sorted.length * 0.95) - 1)],
    min: sorted[0],
    max: sorted.at(-1),
  };
}
function safeLog(value, secrets = []) {
  for (const secret of secrets.filter(Boolean)) value = value.replaceAll(secret, "[redacted]");
  return value
    .replace(/(https?:\/\/[^\s"'<>?]+)\?[^\s"'<>]+/g, "$1?[redacted]")
    .replace(
      /((?:credential|token|secret|authorization|pairingUrl)\s*[=:]\s*)[^\s,}]+/gi,
      "$1[redacted]",
    )
    .replace(/\b[A-Za-z0-9_-]{40,}\b/g, "[long-value-redacted]");
}
async function port() {
  const listener = net.createServer();
  await new Promise((resolve, reject) => {
    listener.once("error", reject);
    listener.listen(0, "127.0.0.1", resolve);
  });
  const value = listener.address().port;
  await new Promise((resolve) => listener.close(resolve));
  return value; // Inherent close/bind reservation race is reported, never hidden.
}
function transport(base) {
  const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
  let cookie = "";
  async function request(route, { method = "GET", body, timeout = 10000 } = {}) {
    const started = performance.now();
    const url = new URL(route, base);
    const encoded = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
    return new Promise((resolve, reject) => {
      const headers = { Accept: "application/json", "x-t3-orchestration-protocol": "2" };
      if (cookie) headers.Cookie = cookie;
      if (encoded) {
        headers["Content-Type"] = "application/json";
        headers["Content-Length"] = encoded.length;
      }
      const req = http.request(url, { agent, method, headers }, (res) => {
        const buffers = [];
        let bytes = 0;
        res.on("data", (chunk) => {
          bytes += chunk.length;
          if (bytes > 32 * 1024 * 1024) {
            res.destroy(new Error("Benchmark response exceeds32MiB"));
            return;
          }
          buffers.push(chunk);
        });
        res.on("error", reject);
        res.on("end", () => {
          const raw = Buffer.concat(buffers).toString("utf8");
          let json;
          try {
            json = raw ? JSON.parse(raw) : null;
          } catch {
            reject(new Error(`NonJSON response ${res.statusCode} on ${route}`));
            return;
          }
          if (res.headers["set-cookie"])
            cookie = res.headers["set-cookie"].map((value) => value.split(";")[0]).join("; ");
          resolve({
            status: res.statusCode,
            json,
            bytes,
            milliseconds: performance.now() - started,
          });
        });
      });
      req.setTimeout(timeout, () => req.destroy(new Error(`HTTP deadline on ${route}`)));
      req.on("error", reject);
      req.end(encoded);
    });
  }
  return { request, close: () => agent.destroy() };
}
async function rpcSocket(base, ticket) {
  const ws = new WebSocket(
    base.replace(/^http/, "ws") +
      `/ws?orchestrationProtocol=2&wsTicket=${encodeURIComponent(ticket)}`,
  );
  const pending = new Map();
  let next = 0;
  function fail(error) {
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(error);
    }
    pending.clear();
  }
  ws.addEventListener("message", (message) => {
    try {
      const decoded = JSON.parse(
        typeof message.data === "string"
          ? message.data
          : Buffer.from(message.data).toString("utf8"),
      );
      for (const frame of Array.isArray(decoded) ? decoded : [decoded]) {
        if (frame._tag === "Defect") {
          fail(new Error(`Socket defect ${JSON.stringify(frame.defect)}`));
          continue;
        }
        if (frame._tag !== "Exit") continue;
        const slot = pending.get(String(frame.requestId));
        if (!slot) continue;
        pending.delete(String(frame.requestId));
        clearTimeout(slot.timer);
        if (frame.exit?._tag !== "Success")
          slot.reject(new Error(`RPC ${slot.tag}: ${JSON.stringify(frame.exit)}`));
        else
          slot.resolve({
            value: frame.exit.value,
            bytes: Buffer.byteLength(JSON.stringify(frame)),
            milliseconds: performance.now() - slot.started,
          });
      }
    } catch (error) {
      fail(error);
    }
  });
  ws.addEventListener("close", () => fail(new Error("RPC socket closed")));
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error("WebSocket upgrade deadline"));
    }, 10000);
    ws.addEventListener(
      "open",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
    ws.addEventListener(
      "error",
      () => {
        clearTimeout(timer);
        reject(new Error("WebSocket upgrade failed"));
      },
      { once: true },
    );
  });
  return {
    request(tag, payload = {}) {
      const id = String(++next),
        started = performance.now();
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`RPC deadline ${tag}`));
        }, 10000);
        pending.set(id, { resolve, reject, timer, started, tag });
        ws.send(JSON.stringify({ _tag: "Request", id, tag, payload, headers: [] }));
      });
    },
    close() {
      ws.close();
      fail(new Error("Benchmark closed RPC socket"));
    },
  };
}
async function processTree(pid) {
  const { stdout } = await exec("/bin/ps", ["-axo", "pid=,ppid=,rss=,time=,comm="]);
  const rows = stdout.split("\n").flatMap((line) => {
    const found = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/);
    return found
      ? [
          {
            pid: Number(found[1]),
            ppid: Number(found[2]),
            rssKiB: Number(found[3]),
            cpu: found[4],
            command: path.basename(found[5]),
          },
        ]
      : [];
  });
  const owned = new Set([pid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows)
      if (owned.has(row.ppid) && !owned.has(row.pid)) {
        owned.add(row.pid);
        changed = true;
      }
  }
  const children = rows.filter((row) => owned.has(row.pid));
  return {
    sumRssKiB: children.reduce((sum, row) => sum + row.rssKiB, 0),
    rootRssKiB: children.find((row) => row.pid === pid)?.rssKiB ?? null,
    processes: children,
  };
}
async function timedRead(read, count) {
  for (let i = 0; i < 5; i++) await read();
  const values = [],
    byteLengths = [];
  for (let i = 0; i < count; i++) {
    const result = await read();
    values.push(result.milliseconds);
    byteLengths.push(result.bytes ?? Buffer.byteLength(JSON.stringify(result.value)));
  }
  return { samplesMs: values, summaryMs: summary(values), responseBytes: summary(byteLengths) };
}
function check(response, label) {
  if (response.status !== 200)
    throw new Error(`${label} HTTP${response.status}: ${JSON.stringify(response.json)}`);
  return response.json;
}
function disabled(config) {
  const rows = config.providers ?? [];
  if (!Array.isArray(rows)) throw new Error("Config providers is not an array");
  if (rows.length !== 7)
    throw new Error(`Expected7 disabled provider instances, got ${rows.length}`);
  if (rows.some((row) => row.status !== "disabled"))
    throw new Error("At least one provider is not disabled; benchmark not comparable");
  return rows.map((row) => ({
    id: row.instanceId ?? row.id,
    driver: row.driver,
    status: row.status,
  }));
}
async function runCase(plan, backend, round, phase, directory) {
  const root = path.join(directory, `${round}-${backend.name}`);
  const state = path.join(root, "t3"),
    work = path.join(root, "workspace"),
    userHome = path.join(root, "home");
  await Promise.all([
    fs.mkdir(path.join(state, "userdata"), { recursive: true }),
    fs.mkdir(work, { recursive: true }),
    fs.mkdir(userHome, { recursive: true }),
  ]);
  if (phase === "fresh")
    await fs.writeFile(path.join(state, "userdata", "settings.json"), JSON.stringify(settings));
  const listeningPort = await port(),
    base = `http://127.0.0.1:${listeningPort}`;
  const secret = crypto.randomBytes(32).toString("hex");
  const token = crypto
    .createHmac("sha256", secret)
    .update(`t3-desktop-bootstrap:${Math.floor(Date.now() / (12 * 60 * 60 * 1000))}`)
    .digest("hex");
  const envelope = {
    mode: "desktop",
    noBrowser: true,
    port: listeningPort,
    host: "127.0.0.1",
    t3Home: state,
    desktopBootstrapToken: token,
    desktopBootstrapSecret: secret,
    tailscaleServeEnabled: false,
    tailscaleServePort: 443,
    ...(plan.resourceMonitorPath ? { resourceMonitorPath: plan.resourceMonitorPath } : {}),
  };
  const inherited = Object.fromEntries(
    ["PATH", "LANG", "LC_ALL", "TZ", "SystemRoot", "HOME"]
      .filter((key) => process.env[key] !== undefined)
      .map((key) => [key, process.env[key]]),
  );
  // Explicit child XDG/T3 roots; never repurpose HOME or CODEX_HOME.
  const env = {
    ...inherited,
    T3CODE_HOME: state,
    T3CODE_BENCH_HOME: userHome,
    XDG_CONFIG_HOME: path.join(userHome, ".config"),
    XDG_CACHE_HOME: path.join(userHome, ".cache"),
    XDG_DATA_HOME: path.join(userHome, ".local/share"),
    TMPDIR: path.join(root, "tmp"),
    T3CODE_BOOTSTRAP_FD: "3",
    T3CODE_AUTO_BOOTSTRAP_PROJECT_FROM_CWD: "false",
    T3CODE_NO_BROWSER: "true",
    T3CODE_LOG_LEVEL: "Warn",
    RUST_LOG: "warn",
    ...backend.env,
  };
  await fs.mkdir(env.TMPDIR, { recursive: true });
  let logs = "",
    socket,
    client,
    closed = false,
    exitStatus;
  const launched = performance.now();
  const child = spawn(
    backend.command[0],
    [...backend.command.slice(1), ...(backend.args ?? ["serve"])],
    { cwd: work, env, stdio: ["ignore", "pipe", "pipe", "pipe"] },
  );
  const exit = new Promise((resolve) => {
    child.once("error", (error) => {
      closed = true;
      exitStatus = { error: String(error) };
      resolve(exitStatus);
    });
    child.once("exit", (code, signal) => {
      closed = true;
      exitStatus = { code, signal };
      resolve(exitStatus);
    });
  });
  for (const stream of [child.stdout, child.stderr])
    stream.on("data", (chunk) => {
      logs = (logs + chunk.toString()).slice(-128 * 1024);
    });
  child.stdio[3].end(JSON.stringify(envelope) + "\n");
  const record = {
    backend: backend.name,
    round,
    phase,
    excludedFromBenchmarks: !!plan.pilot,
    source: backend.source,
    command: backend.command,
    pid: child.pid,
    stateDirectory: state,
    launchMode: "desktop-bootstrap-no-renderer",
  };
  try {
    client = transport(base);
    let attempts = 0;
    while (true) {
      if (closed) throw new Error(`Child exited before readiness ${JSON.stringify(exitStatus)}`);
      if (performance.now() - launched > 60000) throw new Error("HTTP readiness exceeded60s");
      try {
        const response = await client.request("/api/auth/session", { timeout: 1000 });
        if (response.status === 200) {
          record.httpReadyMs = performance.now() - launched;
          break;
        }
      } catch {}
      attempts++;
      await delay(5);
    }
    record.readinessProbeAttempts = attempts;
    check(
      await client.request("/api/auth/browser-session", {
        method: "POST",
        body: { credential: token },
      }),
      "desktop auth",
    );
    const ticket = check(
      await client.request("/api/auth/websocket-ticket", { method: "POST", body: {} }),
      "WS ticket",
    ).ticket;
    socket = await rpcSocket(base, ticket);
    const config = await socket.request("server.getConfig", {});
    record.firstConfigReplyMs = performance.now() - launched;
    record.providers = disabled(config.value);
    record.firstConfigResponseBytes = config.bytes;
    const projectWorkspace = path.join(work, "projects", "0");
    await fs.mkdir(projectWorkspace, { recursive: true });
    const ready = await socket.request(
      "projects.mutate",
      phase === "fresh"
        ? {
            type: "project.create",
            commandId: "bench-ready-create",
            projectId: "bench-project-0",
            title: "Benchmark0",
            workspaceRoot: projectWorkspace,
          }
        : {
            type: "project.update",
            commandId: `bench-ready-reused-${round}`,
            projectId: "bench-project-0",
            title: "Benchmark0",
          },
    );
    if (ready.value?.id !== "bench-project-0")
      throw new Error("Ready mutation did not return the expected persisted project");
    record.usableCommandReadyMs = performance.now() - launched;
    record.firstWriteResponseBytes = ready.bytes;
    record.firstWriteMs = ready.milliseconds;
    check(await client.request("/api/orchestration/shell"), "shell");
    record.authenticatedShellReadyMs = performance.now() - launched;
    // Same1s idle observation in each backend; no application-specific wait.
    await delay(plan.idleMs ?? 1000);
    record.idleTree = await processTree(child.pid);
    if (record.idleTree.rootRssKiB === null)
      throw new Error("Backend root missing from idle RSS snapshot");
    if (
      plan.resourceMonitorPath &&
      !record.idleTree.processes.some(
        (row) => row.command === path.basename(plan.resourceMonitorPath),
      )
    )
      throw new Error("Expected shared resource monitor absent from idle process tree");
    record.httpShell = await timedRead(async () => {
      const result = await client.request("/api/orchestration/shell");
      check(result, "shell read");
      return result;
    }, plan.reads ?? 100);
    record.rpcConfig = await timedRead(
      () => socket.request("server.getConfig", {}),
      plan.reads ?? 100,
    );
    record.rpcProbe = await timedRead(() => socket.request("server.probe", {}), plan.reads ?? 100);
    if (phase === "fresh" && (plan.writes ?? 10) > 0) {
      const projectMs = [],
        threadMs = [];
      for (let i = 0; i < (plan.writes ?? 10); i++) {
        const projectId = `bench-project-${i}`;
        if (i > 0) {
          const projectWorkspace = path.join(work, "projects", String(i));
          await fs.mkdir(projectWorkspace, { recursive: true });
          const result = await socket.request("projects.mutate", {
            type: "project.create",
            commandId: `bench-create-project-${i}`,
            projectId,
            title: `Benchmark ${i}`,
            workspaceRoot: projectWorkspace,
          });
          projectMs.push(result.milliseconds);
        }
        for (let j = 0; j < 3; j++) {
          const threadId = `bench-thread-${i}-${j}`;
          const written = await socket.request("orchestration.dispatchCommand", {
            type: "thread.create",
            commandId: `bench-create-${threadId}`,
            threadId,
            projectId,
            title: `Benchmark ${i}/${j}`,
            modelSelection: { instanceId: "codex", model: "benchmark-disabled" },
            runtimeMode: "approval-required",
            interactionMode: "default",
            branch: null,
            worktreePath: null,
            createdBy: "user",
            creationSource: "web",
          });
          if (written.value.status === "rejected")
            throw new Error(`Thread write rejected ${JSON.stringify(written.value)}`);
          threadMs.push(written.milliseconds);
        }
      }
      record.projectWrites = { samplesMs: projectMs, summaryMs: summary(projectMs) };
      record.threadWrites = { samplesMs: threadMs, summaryMs: summary(threadMs) };
    }
    const seeded = check(await client.request("/api/orchestration/shell"), "persisted shell");
    record.persistedCounts = { projects: seeded.projects?.length, threads: seeded.threads?.length };
    record.dataset = {
      projects: (seeded.projects ?? [])
        .map((row) => ({
          id: row.id,
          title: row.title,
          workspaceRelative: path.relative(work, row.workspaceRoot),
        }))
        .toSorted((a, b) => a.id.localeCompare(b.id)),
      threads: (seeded.threads ?? [])
        .map((row) => ({
          id: row.id,
          projectId: row.projectId,
          title: row.title,
          modelSelection: row.modelSelection,
          runtimeMode: row.runtimeMode,
          interactionMode: row.interactionMode,
        }))
        .toSorted((a, b) => a.id.localeCompare(b.id)),
    };
    record.datasetSha256 = crypto
      .createHash("sha256")
      .update(JSON.stringify(record.dataset))
      .digest("hex");
    const expected = plan.writes ?? 10;
    if (
      expected > 0 &&
      (record.persistedCounts.projects !== expected ||
        record.persistedCounts.threads !== expected * 3)
    )
      throw new Error(`Persisted dataset mismatch ${JSON.stringify(record.persistedCounts)}`);
    if (expected > 0)
      record.rpcThreadProjection = await timedRead(
        () => socket.request("orchestration.getThreadProjection", { threadId: "bench-thread-0-0" }),
        plan.reads ?? 100,
      );
    record.seededHttpShell = await timedRead(async () => {
      const response = await client.request("/api/orchestration/shell");
      check(response, "seeded shell");
      return response;
    }, plan.reads ?? 100);
    record.finalTree = await processTree(child.pid);
    if (record.finalTree.rootRssKiB === null)
      throw new Error("Backend root missing from final RSS snapshot");
  } catch (error) {
    record.error = safeLog(String(error), [secret, token]);
    record.diagnosticLog = safeLog(logs, [secret, token]);
  } finally {
    socket?.close();
    client?.close();
    const beforeShutdown = await processTree(child.pid).catch(() => ({ processes: [] }));
    const stopping = performance.now();
    if (!closed) child.kill("SIGINT");
    const result = await Promise.race([
      exit,
      delay(10000, undefined, { ref: false }).then(() => null),
    ]);
    if (!result && !closed) {
      record.forcedShutdown = true;
      child.kill("SIGTERM");
      await Promise.race([exit, delay(3000, undefined, { ref: false })]);
      if (!closed) {
        child.kill("SIGKILL");
        await exit;
      }
    }
    record.shutdownMs = performance.now() - stopping;
    record.exit = exitStatus;
    record.diagnosticLog = safeLog(logs, [secret, token]);
    const ownedChildren = beforeShutdown.processes
      .filter((row) => row.pid !== child.pid)
      .map((row) => row.pid);
    const { stdout } = await exec("/bin/ps", ["-axo", "pid="]).catch(() => ({ stdout: "" }));
    const living = new Set(stdout.trim().split(/\s+/).map(Number));
    record.observedChildrenRemaining = ownedChildren.filter((pid) => living.has(pid));
    if (record.observedChildrenRemaining.length)
      record.error ??=
        "Observed child descendants remain after backend shutdown; no unowned PID was killed.";
    await fs.writeFile(
      path.join(directory, `${round}-${backend.name}-${phase}.json`),
      JSON.stringify(record, null, 2),
    );
  }
  return record;
}
async function main() {
  if (process.argv[2] === "--describe") {
    console.log(
      JSON.stringify(
        {
          schema: "t3-backend-benchmark-v1",
          settings,
          auth: "supported rotating desktop bootstrap over FD3; browser-session cookie and WS ticket",
          metrics: [
            "HTTP readiness",
            "authenticated command readiness",
            "HTTP/RPC reads",
            "persisted writes",
            "root+descendant RSS",
            "owned shutdown",
          ],
          launches: "none in describe mode",
        },
        null,
        2,
      ),
    );
    return;
  }
  if (!process.argv[2]) throw new Error("Usage: node benchmark.mjs plan.json | --describe");
  const plan = JSON.parse(await fs.readFile(process.argv[2], "utf8"));
  if (plan.pilot ? ![1, 2].includes(plan.backends?.length) : plan.backends?.length !== 2)
    throw new Error("Two production backends required, or1–2 explicit functional pilot backends");
  for (const backend of plan.backends) {
    if (!Array.isArray(backend.command) || !path.isAbsolute(backend.command[0]))
      throw new Error("Absolute executable command required");
  }
  const directory = await fs.mkdtemp(
    path.join(plan.outputRoot ?? "/private/tmp", "t3-backend-benchmark-run-"),
  );
  const startedAtUtc = new Date().toISOString();
  const artifactPaths = [
    ...new Set(
      [
        fileURLToPath(import.meta.url),
        path.join(path.dirname(fileURLToPath(import.meta.url)), "write-report.mjs"),
        plan.resourceMonitorPath,
        ...plan.backends.flatMap((backend) =>
          backend.command.filter((part) => path.isAbsolute(part)),
        ),
      ].filter(Boolean),
    ),
  ];
  const artifacts = await Promise.all(
    artifactPaths.map(async (artifact) => {
      const bytes = await fs.readFile(artifact);
      return {
        path: artifact,
        sizeBytes: bytes.length,
        sha256: crypto.createHash("sha256").update(bytes).digest("hex"),
      };
    }),
  );
  const metadata = {
    startedAtUtc,
    finishedAtUtc: null,
    sourceCommit: "fcd48c83a",
    functionalPilot: !!plan.pilot,
    host: {
      platform: os.platform(),
      arch: os.arch(),
      release: os.release(),
      node: process.version,
      cpu: os.cpus()[0]?.model,
      logicalCpuCount: os.cpus().length,
      totalMemoryBytes: os.totalmem(),
    },
    artifacts,
  };
  await fs.writeFile(path.join(directory, "metadata.json"), JSON.stringify(metadata, null, 2));
  for (const name of ["benchmark.mjs", "isolated-homedir.mjs", "write-report.mjs"])
    await fs.copyFile(
      path.join(path.dirname(fileURLToPath(import.meta.url)), name),
      path.join(directory, name),
    );
  console.log(
    JSON.stringify({ runDirectory: directory, functionalPilot: !!plan.pilot, startedAtUtc }),
  );
  await fs.writeFile(
    path.join(directory, "plan.json"),
    JSON.stringify(
      {
        ...plan,
        settings,
        host: {
          platform: os.platform(),
          arch: os.arch(),
          node: process.version,
          cpu: os.cpus()[0]?.model,
        },
        pollIntervalMs: 5,
        notes: [
          "Fresh state is not cold OS cache.",
          "Desktop backend only; no renderer.",
          "RSS descendant sum may double-count shared pages.",
          "Reads use persistent transports; requests sequential.",
        ],
      },
      null,
      2,
    ),
  );
  let seed = (plan.seed ?? 1729) >>> 0;
  function random() {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed / 2 ** 32;
  }
  const records = [];
  for (let round = 1; round <= (plan.rounds ?? 10); round++) {
    for (const phase of plan.pilot ? (plan.pilotPhases ?? ["fresh"]) : ["fresh", "reused"]) {
      const backends = random() < 0.5 ? [...plan.backends] : [...plan.backends].reverse();
      for (const backend of backends) {
        if (
          phase === "reused" &&
          records.some(
            (row) =>
              row.round === round &&
              row.backend === backend.name &&
              row.phase === "fresh" &&
              row.error,
          )
        )
          continue;
        const record = await runCase(plan, backend, round, phase, directory);
        records.push(record);
        console.log(
          JSON.stringify(
            plan.pilot
              ? {
                  backend: backend.name,
                  round,
                  phase,
                  functionalPilot: true,
                  persistedCounts: record.persistedCounts,
                  providers: record.providers,
                  error: record.error,
                }
              : {
                  backend: backend.name,
                  round,
                  phase,
                  httpReadyMs: record.httpReadyMs,
                  usableCommandReadyMs: record.usableCommandReadyMs,
                  error: record.error,
                },
          ),
        );
        await fs.writeFile(path.join(directory, "results.json"), JSON.stringify(records, null, 2));
      }
    }
  }
  const datasetHashes = new Set(
    records.filter((row) => !row.error).map((row) => row.datasetSha256),
  );
  const datasetMismatch = datasetHashes.size > 1;
  const aggregate = {
    directory,
    datasetSha256: [...datasetHashes][0],
    total: records.length,
    errors: records
      .filter((row) => row.error)
      .map(({ backend, round, phase, error }) => ({ backend, round, phase, error })),
    groups: [],
  };
  if (datasetMismatch)
    aggregate.errors.push({
      backend: "all",
      round: null,
      phase: "all",
      error: "Persisted normalized datasets differ; no metrics aggregated.",
    });
  for (const backend of plan.pilot || datasetMismatch ? [] : plan.backends)
    for (const phase of ["fresh", "reused"]) {
      const rows = records.filter(
        (row) => row.backend === backend.name && row.phase === phase && !row.error,
      );
      const group = { backend: backend.name, phase, successfulRounds: rows.length };
      group.firstConfigResponseBytes = summary(rows.map((row) => row.firstConfigResponseBytes));
      group.firstWriteResponseBytes = summary(rows.map((row) => row.firstWriteResponseBytes));
      for (const key of [
        "httpReadyMs",
        "firstConfigReplyMs",
        "usableCommandReadyMs",
        "authenticatedShellReadyMs",
        "shutdownMs",
      ])
        group[key] = summary(rows.map((row) => row[key]));
      group.finalRootRssKiB = summary(rows.map((row) => row.finalTree.rootRssKiB));
      group.finalSumRssKiB = summary(rows.map((row) => row.finalTree.sumRssKiB));
      group.idleRootRssKiB = summary(rows.map((row) => row.idleTree.rootRssKiB));
      group.idleSumRssKiB = summary(rows.map((row) => row.idleTree.sumRssKiB));
      for (const key of [
        "httpShell",
        "rpcConfig",
        "rpcProbe",
        "seededHttpShell",
        "rpcThreadProjection",
        "projectWrites",
        "threadWrites",
      ]) {
        group[key] = summary(rows.flatMap((row) => row[key]?.samplesMs ?? []));
        group[`${key}ResponseBytes`] = summary(
          rows.flatMap((row) => (row[key]?.responseBytes ? [row[key].responseBytes.median] : [])),
        );
        group[`${key}RoundMedians`] = summary(
          rows.flatMap((row) => (row[key] ? [row[key].summaryMs.median] : [])),
        );
      }
      aggregate.groups.push(group);
    }
  metadata.finishedAtUtc = new Date().toISOString();
  await fs.writeFile(path.join(directory, "metadata.json"), JSON.stringify(metadata, null, 2));
  await fs.writeFile(path.join(directory, "summary.json"), JSON.stringify(aggregate, null, 2));
  if (!plan.pilot)
    await exec(process.execPath, [path.join(directory, "write-report.mjs"), directory]);
  console.log(JSON.stringify(aggregate, null, 2));
  if (aggregate.errors.length) process.exitCode = 1;
}
main().catch((error) => {
  console.error(safeLog(String(error)));
  process.exitCode = 1;
});
