import fs from "node:fs/promises";
import path from "node:path";
const directory = process.argv[2];
if (!directory) throw new Error("Usage: node write-report.mjs <run directory>");
const summary = JSON.parse(await fs.readFile(path.join(directory, "summary.json"), "utf8"));
const metadata = JSON.parse(await fs.readFile(path.join(directory, "metadata.json"), "utf8"));
const rows = JSON.parse(await fs.readFile(path.join(directory, "results.json"), "utf8"));
const metrics = [
  ["httpReadyMs", "HTTP readiness", "ms"],
  ["usableCommandReadyMs", "Authenticated accepted project command", "ms"],
  ["firstConfigReplyMs", "First authenticated config reply", "ms"],
  ["authenticatedShellReadyMs", "Authenticated shell readiness", "ms"],
  ["idleRootRssKiB", "Idle backend root RSS", "KiB"],
  ["idleSumRssKiB", "Idle backend + descendants sumRSS", "KiB"],
  ["finalRootRssKiB", "Post-work backend root RSS", "KiB"],
  ["finalSumRssKiB", "Post-work backend + descendants sumRSS", "KiB"],
  ["seededHttpShell", "Seeded HTTP shell read", "ms"],
  ["rpcThreadProjection", "Seeded thread projection RPC", "ms"],
  ["rpcConfig", "Config RPC", "ms"],
  ["rpcProbe", "No-op probe RPC", "ms"],
  ["projectWrites", "Persisted project create", "ms"],
  ["threadWrites", "Persisted thread create", "ms"],
  ["shutdownMs", "Owned backend shutdown", "ms"],
];
const number = (value) => (Number.isFinite(value) ? value.toFixed(3) : "—");
const lines = [
  "# Production backend benchmark",
  "",
  `UTC: ${metadata.startedAtUtc} → ${metadata.finishedAtUtc}`,
  "",
  `Source: committed ${metadata.sourceCommit}; ${summary.total} launches, ${summary.errors.length} errors.`,
  "",
  "Both use isolated application state, all seven providers disabled, and the same preserved native resource monitor. Fresh/reused describes application state, not cold OS caches. No renderer is included.",
  "",
  `Normalized seeded dataset SHA256: \`${summary.datasetSha256 ?? "unavailable"}\`. Each successful case verifies ten projects and thirty threads.`,
  "",
  "Startup/RSS/shutdown percentiles cover ten launches per backend and state phase. Read percentiles pool 1,000 sequential warmed requests per backend/phase; project/thread writes pool fresh-state operations. p95 uses the nearest-rank method (with ten startup samples, p95 is the maximum). Raw samples and per-round medians are retained.",
  "",
];
const csv = ["phase,metric,unit,backend,n,median,p95,min,max"];
for (const phase of ["fresh", "reused"]) {
  lines.push(
    `## ${phase} state`,
    "",
    "| Metric | Unit | Original median / p95 | Rust median / p95 | Original ÷ Rust median |",
    "|---|---|---:|---:|---:|",
  );
  const original = summary.groups.find(
    (row) => row.phase === phase && row.backend === "original-production",
  );
  const rust = summary.groups.find((row) => row.phase === phase && row.backend === "rust-release");
  for (const [key, label, unit] of metrics) {
    const a = original?.[key],
      b = rust?.[key];
    if (!a && !b) continue;
    lines.push(
      `| ${label} | ${unit} | ${number(a?.median)} / ${number(a?.p95)} | ${number(b?.median)} / ${number(b?.p95)} | ${number(a?.median / b?.median)} |`,
    );
    for (const group of [original, rust]) {
      const value = group?.[key];
      if (value)
        csv.push(
          [
            phase,
            key,
            unit,
            group.backend,
            value.n,
            value.median,
            value.p95,
            value.min,
            value.max,
          ].join(","),
        );
    }
  }
  lines.push(
    "",
    "Response sizes (median serialized bytes):",
    "",
    "| Reply | Original | Rust |",
    "|---|---:|---:|",
  );
  for (const [key, label] of [
    ["firstConfigResponseBytes", "First config"],
    ["rpcConfigResponseBytes", "Config RPC"],
    ["seededHttpShellResponseBytes", "Seeded HTTP shell"],
    ["rpcThreadProjectionResponseBytes", "Thread projection RPC"],
    ["rpcProbeResponseBytes", "No-op probe RPC"],
  ])
    lines.push(
      `| ${label} | ${number(original?.[key]?.median)} | ${number(rust?.[key]?.median)} |`,
    );
  lines.push("");
}
lines.push(
  "## Interpretation",
  "",
  "Readiness uses HTTP probes approximately every 5 ms plus request/scheduling latency. Original Node startup includes a small pre-import homedir isolation shim. RSS is the backend process plus observed descendants; sumRSS can double-count shared pages and excludes desktop renderer processes outside the backend ancestry. Root and child process snapshots are retained before and after the request workload.",
  "",
  "The original has more complete runtime/service coverage than the current Rust port. These results compare the measured common endpoints and datasets; they do not isolate language cost or establish equivalent full application performance. Config and other reply sizes are reported because payload and service coverage differ.",
  "",
  `Shutdown checks: ${rows.filter((row) => row.observedChildrenRemaining?.length).length} cases with observed surviving descendants; ${rows.filter((row) => row.forcedShutdown).length} forced shutdowns.`,
  "",
  "Artifacts: metadata.json (UTC/host/SHA256), plan.json, results.json, per-case JSON, summary.json, measurements.csv, and harness/preload/report snapshots.",
);
if (summary.errors.length)
  lines.push(
    "",
    "## Errors",
    "",
    ...summary.errors.map(
      (row) => `- ${row.backend}, round ${row.round}, ${row.phase}: ${row.error}`,
    ),
  );
await fs.writeFile(path.join(directory, "report.md"), lines.join("\n") + "\n");
await fs.writeFile(path.join(directory, "measurements.csv"), csv.join("\n") + "\n");
console.log(path.join(directory, "report.md"));
