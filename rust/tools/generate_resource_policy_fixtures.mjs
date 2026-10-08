// Pure source policy; no sidecar, telemetry collection, or provider startup.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/server/src/resourceTelemetry/NativeTelemetryClient.ts", import.meta.url),
  "utf8",
);
const interval = source.slice(
  source.indexOf("function isThermallyConstrained("),
  source.indexOf("export function commitCollectionControlUpdate"),
);
const recovery = source
  .slice(
    source.indexOf("export function retainRecentNativeTelemetryFailures"),
    source.indexOf(
      "/** @public Service construction",
      source.indexOf("export function retainRecentNativeTelemetryFailures"),
    ),
  )
  .replace(/function errorMessage\([\s\S]*?\n}\n/, "");
const policy = new Function(
  stripTypeScriptTypes(
    "const SAMPLE_INTERVAL_MS=1000,UNKNOWN_BACKGROUND_SAMPLE_INTERVAL_MS=5000,BATTERY_SAMPLE_INTERVAL_MS=5000,CONSTRAINED_SAMPLE_INTERVAL_MS=15000,FAILURE_WINDOW_MS=60000;" +
      interval +
      recovery,
    { mode: "strip" },
  ).replaceAll("export function ", "function ") +
    "\nreturn {resolveNativeSampleIntervalMs,retainRecentNativeTelemetryFailures,canRequestNativeTelemetryRetry,canCommandNativeTelemetrySidecar};",
)();
const cases = [];
const base = {
  source: "electron-main",
  idle: "false",
  idleSeconds: 0,
  locked: "false",
  suspended: false,
  onBattery: "false",
  lowPowerMode: "false",
  thermalState: "nominal",
  stale: false,
  updatedAt: "2026-10-08T13:00:00.000Z",
};
for (const live of [0, 1, 2])
  for (const source of ["unknown", "electron-main", "node-linux"])
    for (const stale of [false, true])
      for (const suspended of [false, true])
        for (const locked of ["true", "false", "unknown"])
          for (const lowPowerMode of ["true", "false", "unknown"])
            for (const onBattery of ["true", "false", "unknown"])
              for (const thermalState of ["unknown", "nominal", "fair", "serious", "critical"]) {
                const power = {
                  ...base,
                  source,
                  stale,
                  suspended,
                  locked,
                  lowPowerMode,
                  onBattery,
                  thermalState,
                };
                cases.push({
                  kind: "interval",
                  power,
                  live,
                  expected: policy.resolveNativeSampleIntervalMs(power, live),
                });
              }
for (const status of ["starting", "healthy", "degraded", "unavailable", "stopped"])
  for (const handle of [false, true]) {
    cases.push({
      kind: "retry",
      status,
      handle,
      expected: policy.canRequestNativeTelemetryRetry(status, handle),
    });
    cases.push({
      kind: "command",
      status,
      handle,
      expected: policy.canCommandNativeTelemetrySidecar(status, handle),
    });
  }
for (const now of [-1, 0, 60000, 60001, 90000, 90001])
  for (const failures of [[], [0, 30000, 60000], [now + 1, now, now - 60000, now - 60001]])
    cases.push({
      kind: "failures",
      now,
      failures,
      expected: policy.retainRecentNativeTelemetryFailures(failures, now),
    });
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-policy.jsonl", import.meta.url),
  cases.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${cases.length} source collection policy witnesses`);
