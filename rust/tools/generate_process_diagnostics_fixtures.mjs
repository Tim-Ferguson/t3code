// Development-only execution of the unchanged original diagnostic services.
import { writeFileSync } from "node:fs";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as DateTime from "../../packages/contracts/node_modules/effect/dist/DateTime.js";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Telemetry from "../../apps/server/src/resourceTelemetry/ResourceTelemetry.ts";
import * as Diagnostics from "../../apps/server/src/diagnostics/ProcessDiagnostics.ts";
import * as Monitor from "../../apps/server/src/diagnostics/ProcessResourceMonitor.ts";
import * as Contracts from "../../packages/contracts/src/server.ts";
function normalize(value) {
  if (value === null || typeof value !== "object") return value;
  if (DateTime.isDateTime(value)) return DateTime.formatIso(value);
  if (Option.isOption(value))
    return Option.match(value, {
      onNone: () => ({ _tag: "None" }),
      onSome: (value) => ({ _tag: "Some", value: normalize(value) }),
    });
  if (Array.isArray(value)) return value.map(normalize);
  return Object.fromEntries(Object.entries(value).map(([key, value]) => [key, normalize(value)]));
}
const rows = [];
const stamp = "2026-05-05T10:00:00.000Z";
const health = { native: { lastError: Option.none() } };
const snapshot = (processes, error = false) => ({
  readAt: DateTime.makeUnsafe(stamp),
  processes,
  health: { native: { lastError: error ? Option.some("collector stalled") : Option.none() } },
});
const entry = (category = "server-child", fields = {}) => ({
  identity: { pid: 4242, startTimeMs: 2000 },
  ppid: 111,
  status: "Running",
  cpuPercent: 1.5,
  residentBytes: 2048,
  runTimeMs: 4000,
  command: "codex app-server",
  name: "agent",
  depth: 1,
  childPids: [4243],
  category,
  ...fields,
});
const oldPid = Object.getOwnPropertyDescriptor(process, "pid");
Object.defineProperty(process, "pid", { value: 111, configurable: true });
const originalKill = process.kill;
for (const processes of [
  [],
  ...[
    "server",
    "server-child",
    "provider-root",
    "terminal-root",
    "monitor",
    "electron-main",
    "unknown",
  ].map((category) => [entry(category)]),
  [entry("server-child", { command: "", name: "", status: "", depth: 0, runTimeMs: -10 })],
  ...[59999, 60000, 3599999, 3600000, 3601000].map((runTimeMs) => [
    entry("server-child", { runTimeMs }),
  ]),
])
  for (const failed of [false, true]) {
    const current = snapshot(processes, true);
    const service = await Effect.runPromise(
      Diagnostics.make().pipe(
        Effect.provideService(Telemetry.ResourceTelemetry, {
          latest: Effect.succeed(current),
          refresh: failed
            ? Effect.fail(new Error("collector unavailable"))
            : Effect.succeed(current),
        }),
      ),
    );
    const result = await Effect.runPromise(service.read);
    rows.push({
      op: "read",
      snapshot: Schema.encodeUnknownSync(Schema.Unknown)(
        JSON.parse(JSON.stringify({ ...current, readAt: stamp })),
      ),
      failed,
      serverPid: 111,
      result: Schema.encodeUnknownSync(Contracts.ServerProcessDiagnosticsResult)(result),
    });
  }
for (const category of [
  "server-child",
  "server",
  "monitor",
  "electron-main",
  "provider-root",
  "terminal-root",
  "unknown",
])
  for (const scenario of ["valid", "stale", "failed", "signal-failed", "self"]) {
    const current = snapshot([entry(category)]);
    const calls = [];
    process.kill = (pid, signal) => {
      calls.push({ pid, signal });
      if (scenario === "signal-failed") throw new Error("not permitted");
      return true;
    };
    const service = await Effect.runPromise(
      Diagnostics.make().pipe(
        Effect.provideService(Telemetry.ResourceTelemetry, {
          latest: Effect.succeed(current),
          refresh:
            scenario === "failed"
              ? Effect.fail(new Error("collector unavailable"))
              : Effect.succeed(current),
        }),
      ),
    );
    const input = {
      pid: scenario === "self" ? 111 : 4242,
      startTimeMs: scenario === "stale" ? 2001 : 2000,
      signal: category === "provider-root" ? "SIGKILL" : "SIGINT",
    };
    const result = await Effect.runPromise(service.signal(input));
    rows.push({
      op: "signal",
      snapshot: { ...current, readAt: stamp },
      failed: scenario === "failed",
      serverPid: 111,
      input,
      signalFailed: scenario === "signal-failed",
      calls,
      result: Schema.encodeUnknownSync(Contracts.ServerSignalProcessResult)(result),
    });
  }
process.kill = originalKill;
Object.defineProperty(process, "pid", oldPid);
for (const legacy of [true, false])
  for (const error of [true, false]) {
    const bucket = {
      startedAt: DateTime.makeUnsafe("2026-05-05T09:59:50.000Z"),
      endedAt: DateTime.makeUnsafe(stamp),
      avgCpuPercent: 15,
      maxCpuPercent: 25,
      maxRssBytes: 4096,
      maxProcessCount: 2,
    };
    const summaries = [
      "server",
      "server-child",
      "provider-root",
      "terminal-root",
      "monitor",
      "electron-main",
      "unknown",
    ].map((category, n) => ({
      identity: { pid: 111 + n, startTimeMs: 100 + n },
      ppid: 1,
      depth: n,
      name: category === "server-child" ? "" : "node",
      command: "",
      category,
      firstSeenAt: bucket.startedAt,
      lastSeenAt: bucket.endedAt,
      currentCpuPercent: n + 1,
      avgCpuPercent: 4,
      maxCpuPercent: 8,
      cpuTimeMs: 1500 + n,
      currentRssBytes: 2048,
      peakRssBytes: 4096,
      sampleCount: 2,
    }));
    const history = {
      readAt: DateTime.makeUnsafe(stamp),
      windowMs: 60000,
      bucketMs: 10000,
      sampleIntervalMs: 1000,
      retainedSampleCount: 2,
      buckets: [bucket],
      ...(legacy
        ? {
            legacyBackendBuckets: [
              { ...bucket, avgCpuPercent: 5, maxCpuPercent: 8, maxProcessCount: 1 },
            ],
          }
        : {}),
      topProcesses: summaries,
      health: { native: { lastError: error ? Option.some("collector stalled") : Option.none() } },
    };
    const service = await Effect.runPromise(
      Monitor.make().pipe(
        Effect.provideService(Telemetry.ResourceTelemetry, {
          readHistory: () => Effect.succeed(history),
        }),
      ),
    );
    const result = await Effect.runPromise(
      service.readHistory({ windowMs: 60000, bucketMs: 10000 }),
    );
    const input = {
      ...history,
      readAt: stamp,
      buckets: history.buckets.map((b) => ({
        ...b,
        startedAt: DateTime.formatIso(b.startedAt),
        endedAt: stamp,
      })),
      ...(legacy
        ? {
            legacyBackendBuckets: history.legacyBackendBuckets.map((b) => ({
              ...b,
              startedAt: DateTime.formatIso(b.startedAt),
              endedAt: stamp,
            })),
          }
        : {}),
      topProcesses: history.topProcesses.map((p) => ({
        ...p,
        firstSeenAt: DateTime.formatIso(p.firstSeenAt),
        lastSeenAt: stamp,
      })),
    };
    rows.push({
      op: "history",
      history: input,
      result: Schema.encodeUnknownSync(Contracts.ServerProcessResourceHistoryResult)(result),
    });
  }
writeFileSync(
  new URL("../crates/server/tests/fixtures/process-diagnostics.jsonl", import.meta.url),
  rows.map((x) => JSON.stringify(normalize(x))).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
