// Execute the original history builder over counter/reset/identity windows.
import { writeFileSync } from "node:fs";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import * as DateTime from "../../packages/contracts/node_modules/effect/dist/DateTime.js";
import {
  buildResourceTelemetryHistory,
  normalizeResourceTelemetryHistoryInput,
} from "../../apps/server/src/resourceTelemetry/ResourceTelemetryHistory.ts";
const base = 1700000000000;
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
const health = {
  native: { status: "healthy", lastSampleAt: Option.none(), lastError: Option.none() },
  desktop: { status: "unavailable", lastSampleAt: Option.none(), lastError: Option.none() },
  sidecarVersion: Option.none(),
  sidecarPid: Option.some(900),
  restartCount: 0,
  collectionDurationMicros: 1,
  scannedProcessCount: 4,
  retainedProcessCount: 4,
  inaccessibleProcessCount: 0,
};
const process = (pid, ppid, start, counter, extra = {}) => ({
  pid,
  ppid,
  startTimeMs: start,
  runTimeMs: 1000,
  name: `process-${pid}`,
  command: `process-${pid}`,
  status: "Running",
  cpuPercent: 5,
  cpuTimeMs: counter,
  residentBytes: 1024 + counter,
  virtualBytes: 2048,
  ioReadBytes: counter * 10,
  ioWriteBytes: counter * 20,
  ioSemantics: "storage",
  ...extra,
});
const snapshot = (offset, counter, extra = {}) => ({
  version: 3,
  type: "snapshot",
  sequence: 1,
  sampledAtUnixMs: base + offset,
  collectionDurationMicros: 1,
  scannedProcessCount: 4,
  retainedProcessCount: 4,
  inaccessibleProcessCount: 0,
  processes: [
    process(100, 1, 10, 0),
    process(300, 100, 30, counter),
    process(900, 100, 90, counter),
    process(200, 0, 20, counter),
  ],
  ...extra,
});
const schedules = [
  [
    snapshot(-6000, 0),
    snapshot(-4000, 100),
    snapshot(-2000, 200),
    snapshot(0, 400),
    snapshot(1000, 1000),
  ],
  [snapshot(0, 400), snapshot(-2000, 200), snapshot(-4000, 100), snapshot(-6000, 0)],
  [snapshot(-60000, 0), snapshot(-2000, 200), snapshot(0, 100)],
  [
    snapshot(-4000, 100),
    snapshot(-2000, 150, { processes: [process(100, 1, 10, 0)] }),
    snapshot(0, 200),
  ],
  [
    snapshot(-4000, 100),
    snapshot(-2000, 100),
    snapshot(-2000, 200),
    snapshot(0, 50, { processes: [process(300, 100, 31, 50)] }),
  ],
];
const fixtures = [];
for (const windowMs of [0, 999, 1000, 2500, 3600000, 3600001])
  for (const bucketMs of [0, 999, 1000, 1999, 3600001])
    fixtures.push({
      kind: "normalize",
      input: { windowMs, bucketMs },
      expected: normalizeResourceTelemetryHistoryInput({ windowMs, bucketMs }),
    });
for (const windowMs of [1000, 2500, 5000])
  for (const bucketMs of [1000, 2000, 6000])
    for (const schedule of schedules)
      for (const externalProcesses of [
        undefined,
        [],
        [{ pid: 200, startTimeMs: 20 }],
        [{ pid: 200, startTimeMs: 2021 }],
      ]) {
        const snapshots = schedule.map((snapshot) => ({ ...snapshot, externalProcesses }));
        const input = {
          readAt: DateTime.makeUnsafe(base),
          windowMs,
          bucketMs,
          sampleIntervalMs: 1000,
          serverPid: 100,
          sidecarPid: Option.some(900),
          desktopSnapshot: Option.none(),
          snapshots,
          health,
        };
        fixtures.push({
          kind: "history",
          input: normalize({ ...input, sidecarPid: 900, desktopSnapshot: undefined }),
          expected: normalize(buildResourceTelemetryHistory(input)),
        });
      }
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-history.jsonl", import.meta.url),
  fixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
console.log(`${fixtures.length} original resource history witnesses`);
