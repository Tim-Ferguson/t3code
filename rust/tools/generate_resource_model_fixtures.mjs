// Development-only oracle: execute the original process model, no sidecars.
import { writeFileSync } from "node:fs";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import * as DateTime from "../../packages/contracts/node_modules/effect/dist/DateTime.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import { ResourceTelemetryProcess } from "../../packages/contracts/src/resourceTelemetry.ts";
import {
  mergeProcesses,
  emptyTelemetryCounters,
} from "../../apps/server/src/resourceTelemetry/Model.ts";
const base = 1700000000000;
function normalize(value) {
  if (value === null || typeof value !== "object") return value;
  if (DateTime.isDateTime(value)) return DateTime.formatIso(value);
  if (value instanceof Map)
    return Object.fromEntries([...value].map(([key, v]) => [key, normalize(v)]));
  if (value instanceof Set) return [...value];
  if (Array.isArray(value)) return value.map(normalize);
  return Object.fromEntries(Object.entries(value).map(([key, v]) => [key, normalize(v)]));
}
const process = (pid, ppid, start, extra = {}) => ({
  pid,
  ppid,
  startTimeMs: start,
  runTimeMs: 1000,
  name: `process-${pid}`,
  command: `process-${pid}`,
  status: "Running",
  cpuPercent: 5,
  cpuTimeMs: 0,
  residentBytes: 1024,
  virtualBytes: 2048,
  ioReadBytes: 0,
  ioWriteBytes: 0,
  ioSemantics: "storage",
  ...extra,
});
const snapshot = (time, processes) => ({
  version: 3,
  type: "snapshot",
  sequence: 1,
  sampledAtUnixMs: time,
  collectionDurationMicros: 1,
  scannedProcessCount: processes.length,
  retainedProcessCount: processes.length,
  inaccessibleProcessCount: 0,
  processes,
});
const metric = (pid, start, type, extra = {}) => ({
  pid,
  creationTimeMs: start,
  type,
  cpuPercent: 17.125,
  idleWakeupsPerSecond: 2,
  workingSetBytes: 2048,
  peakWorkingSetBytes: 4096,
  ...extra,
});
const desktop = (time, metrics) => ({
  version: 1,
  type: "desktopTelemetry",
  sequence: 1,
  sampledAtUnixMs: time,
  electronPid: metrics[0]?.pid ?? 200,
  power: {
    source: "electron-main",
    idle: "false",
    idleSeconds: 0,
    locked: "false",
    suspended: false,
    onBattery: "false",
    lowPowerMode: "unknown",
    thermalState: "nominal",
    stale: false,
    updatedAt: DateTime.makeUnsafe(time),
  },
  speedLimitPercent: null,
  electronProcesses: metrics,
});
const fixtures = [];
function record(input) {
  const source = {
    ...input,
    sidecarPid: Option.fromUndefinedOr(input.sidecarPid),
    nativeSnapshot: Option.fromUndefinedOr(input.nativeSnapshot),
    desktopSnapshot: Option.fromUndefinedOr(input.desktopSnapshot),
  };
  const result = mergeProcesses(source);
  const wireAcceptance = result.processes.map((value) => {
    try {
      Schema.encodeUnknownSync(Schema.toCodecJson(ResourceTelemetryProcess))(value);
      return true;
    } catch {
      return false;
    }
  });
  fixtures.push({ input: normalize(input), expected: normalize(result), wireAcceptance });
  return result;
}
const clean = () => ({
  serverPid: 100,
  fallbackSampledAtMs: base,
  previous: new Map(),
  counters: emptyTelemetryCounters(),
  electronRootPids: new Set(),
  electronRootStartTimes: new Map(),
  updatePrevious: true,
});
const trees = [
  [],
  [process(100, 1, 10)],
  [
    process(300, 200, 30),
    process(900, 100, 90),
    process(100, 1, 10),
    process(200, 100, 20),
    process(301, 200, 31),
  ],
  [process(100, 300, 10), process(200, 100, 20), process(300, 200, 30)],
  [
    process(200, 0, 20, { command: "electron --type=renderer" }),
    process(201, 200, 21, { command: "electron --type=gpu-process" }),
    process(100, 1, 10),
  ],
  [process(100, 1, 10), process(100, 1, 11), process(200, 100, 20)],
];
for (const tree of trees)
  for (const sidecarPid of [undefined, 900, 100])
    for (const updatePrevious of [true, false])
      record({ ...clean(), sidecarPid, updatePrevious, nativeSnapshot: snapshot(base, tree) });
for (const elapsed of [0, 1, 999, 1000, 30000, 30001, -1])
  for (const reset of [false, true])
    for (const updatePrevious of [true, false]) {
      const previous = record({
        ...clean(),
        sidecarPid: 900,
        nativeSnapshot: snapshot(base, [
          process(100, 1, 10, { cpuTimeMs: 100, ioReadBytes: 100, ioWriteBytes: 100 }),
          process(200, 100, 20),
          process(900, 100, 90),
        ]),
      });
      record({
        ...clean(),
        sidecarPid: 900,
        previous: previous.previous,
        counters: previous.counters,
        updatePrevious,
        nativeSnapshot: snapshot(base + elapsed, [
          process(100, 1, 10, {
            cpuTimeMs: reset ? 99 : 150,
            ioReadBytes: reset ? 99 : 200,
            ioWriteBytes: reset ? 99 : 300,
          }),
          process(200, 100, 21),
          process(900, 100, 90),
        ]),
      });
    }
for (const skew of [-2001, -2000, -1, 0, 2000, 2001])
  for (const type of ["Browser", "Tab", "GPU", "Utility"])
    for (const roots of [false, true]) {
      record({
        ...clean(),
        nativeSnapshot: snapshot(base, [
          process(100, 1, 10),
          process(200, 0, 20000),
          process(201, 200, 30, { command: "electron --type=renderer" }),
          process(202, 201, 40, { command: "electron --type=gpu-process" }),
        ]),
        desktopSnapshot: desktop(base + 100, [
          metric(200, 20000 + skew, type, { name: "", serviceName: "service" }),
        ]),
        electronRootPids: new Set(roots ? [200] : []),
        electronRootStartTimes: new Map(roots ? [[200, 20000 + skew]] : []),
      });
    }
for (const cumulativeCpuSeconds of [undefined, 0, 1.0004, 1.0005])
  for (const updatePrevious of [true, false]) {
    const previous = record({
      ...clean(),
      desktopSnapshot: desktop(base, [metric(200, 20, "Browser", { cumulativeCpuSeconds })]),
    });
    record({
      ...clean(),
      previous: previous.previous,
      counters: previous.counters,
      updatePrevious,
      desktopSnapshot: desktop(base + 1, [
        metric(200, 20, "Browser", {
          cumulativeCpuSeconds:
            cumulativeCpuSeconds === undefined ? undefined : cumulativeCpuSeconds + 0.125,
        }),
      ]),
    });
  }
const inherited = record({
  ...clean(),
  desktopSnapshot: desktop(base, [metric(200, 20, "Browser")]),
});
for (const start of [20, 21, 2020, 2021])
  record({
    ...clean(),
    nativeSnapshot: snapshot(base + 1000, [process(200, 0, start), process(201, 200, 30)]),
    previous: inherited.previous,
    counters: inherited.counters,
    electronRootPids: new Set([200]),
  });
record(clean());
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-model.jsonl", import.meta.url),
  fixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
console.log(
  `${fixtures.length} source process model witnesses, ${fixtures.filter((value) => value.wireAcceptance.includes(false)).length} with a source-rejected integer wire counter`,
);
