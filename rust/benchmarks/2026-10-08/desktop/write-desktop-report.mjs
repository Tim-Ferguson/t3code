// Postprocess recorded lifecycle samples only; never launches/reads application UI.
import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
const [warmPath, freshPath, outputBase] = process.argv.slice(2);
if (!warmPath || !freshPath || !outputBase?.startsWith("/private/tmp/t3port-bench-"))
  throw Error("warm/results.json fresh/results.json /private/tmp/t3port-bench-report-... required");
const inputHash = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
const load = (file, mode) => {
  const r = JSON.parse(fs.readFileSync(file, "utf8"));
  if (r.kind !== "instrumented-native-requested-visible" || r.profileMode !== mode || r.error)
    throw Error(`Incomplete or incorrect input ${file}`);
  if (!r.host || !r.startedUtc || !r.finishedUtc) throw Error(`Missing host/UTC metadata ${file}`);
  if (
    r.milestones?.original !== "electron-window-shown" ||
    r.milestones?.rust !== "rust-native-set-visible-completed"
  )
    throw Error("Different lifecycle milestone");
  const samples = r.samples.filter((s) => s.measured === true);
  if (samples.length !== 20) throw Error("Require20 measuredsamples excludingwarmups");
  const warmups = r.warmups ?? [];
  if (
    mode === "warm" &&
    (warmups.length !== 2 ||
      !["original", "rust"].every((runtime) =>
        warmups.some(
          (row) =>
            row.runtime === runtime &&
            row.round === 0 &&
            row.measured === false &&
            row.cleanup?.allObservedExited,
        ),
      ))
  )
    throw Error("Missing successful untimed warmup pair");
  if (mode === "fresh-profile" && warmups.length) throw Error("Fresh profile unexpectedly warmed");
  const statistics = {};
  for (const runtime of ["original", "rust"]) {
    const rows = samples.filter((s) => s.runtime === runtime);
    if (
      rows.length !== 10 ||
      new Set(rows.map((s) => s.round)).size !== 10 ||
      rows.some(
        (s) =>
          !Number.isInteger(s.round) ||
          s.round < 1 ||
          s.round > 10 ||
          !Number.isFinite(s.requestedVisibleMs) ||
          s.requestedVisibleMs <= 0 ||
          !s.cleanup?.allObservedExited,
      )
    )
      throw Error(`Incomplete timing/cleanup ${runtime}`);
    if (
      rows.some(
        (row) =>
          !row.markers.some(
            (marker) =>
              marker.pid === row.pid &&
              marker.name === r.milestones[runtime] &&
              marker.receivedFromSpawnMs === row.requestedVisibleMs,
          ),
      )
    )
      throw Error("Missing exact captured-PID lifecycle marker");
    const profiles = new Set(rows.map((row) => row.profile));
    if (profiles.size !== (mode === "warm" ? 1 : 10)) throw Error("Unexpected profile reuse");
    if (runtime === "rust") {
      const identifiers = new Set(rows.map((row) => row.webkitStoreIdentifier));
      if (
        identifiers.size !== (mode === "warm" ? 1 : 10) ||
        rows.some((row) => !/^([a-f0-9]{32})$/.test(row.webkitStoreIdentifier))
      )
        throw Error("Unexpected WebKit store reuse");
    }
    const values = rows.map((s) => s.requestedVisibleMs).sort((a, b) => a - b);
    statistics[runtime] = {
      n: values.length,
      medianMs: (values[4] + values[5]) / 2,
      p95NearestRankMs: values[9],
      minMs: values[0],
      maxMs: values[9],
    };
  }
  const a = statistics.original,
    b = statistics.rust;
  const comparison = {
    medianDifferenceMs: b.medianMs - a.medianMs,
    medianRustOverOriginal: b.medianMs / a.medianMs,
    medianReductionPercent: (1 - b.medianMs / a.medianMs) * 100,
    p95DifferenceMs: b.p95NearestRankMs - a.p95NearestRankMs,
  };
  return {
    ...r,
    statistics,
    comparison,
    input: { path: path.resolve(file), sha256: inputHash(file) },
  };
};
const warm = load(warmPath, "warm"),
  fresh = load(freshPath, "fresh-profile");
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
if (
  warm.sourceCommit !== fresh.sourceCommit ||
  !same(warm.artifacts, fresh.artifacts) ||
  !same(warm.milestones, fresh.milestones)
)
  throw Error("Artifact/source/milestone mismatch between modes");
for (const key of [
  "hostname",
  "platform",
  "architecture",
  "osRelease",
  "osVersion",
  "cpuModel",
  "logicalCpus",
  "totalMemoryBytes",
]) {
  if (warm.host[key] !== fresh.host[key]) throw Error(`Host changed: ${key}`);
}
const report = {
  kind: "desktop-lifecycle-comparison",
  generatedUtc: new Date().toISOString(),
  metric: "Parent process spawn to native visibility lifecycle marker receipt",
  sourceCommit: warm.sourceCommit,
  host: warm.host,
  artifacts: warm.artifacts,
  milestones: warm.milestones,
  definitions: {
    warm: "One untimed setup launch perruntime;10 measured launches reuse isolated application/browser profiles",
    freshProfile:
      "10 measured launches perruntime each use a fresh application/browser profile; OS/filesystem caches not purged",
    original:
      "Productionbundles in unpackaged Electron; real t3code://app/ mainclient navigation followed by show event",
    rust: "Optimized release app; benchmark-only Dioxus set_visible completion marker and explicit isolated WK UUID",
  },
  limitations: [
    ...new Set([
      ...warm.limitations,
      ...fresh.limitations,
      "The source apps differ in implemented features and startup work; this comparison does not isolate language effects",
      "Visibility lifecycle does not prove first paint, usable UI, connection readiness or total startup",
      "Observed descendant cleanup excludes OS-owned/shared XPC processes; no unrelated process is signaled",
    ]),
  ],
  runs: { warm, freshProfile: fresh },
};
fs.mkdirSync(path.dirname(outputBase), { recursive: true });
fs.writeFileSync(outputBase + ".json", JSON.stringify(report, null, 2) + "\n");
const ms = (n) => n.toFixed(2);
let markdown = `# Desktop window visibility lifecycle benchmark\n\nGenerated ${report.generatedUtc}; source ${report.sourceCommit}.\n\nMetric: ${report.metric}. This measures an instrumented native lifecycle stage, not first paint or usable UI.\n\n`;
markdown += `Host: ${report.host.hostname}; ${report.host.platform} ${report.host.osVersion} (${report.host.osRelease}); ${report.host.architecture}; ${report.host.cpuModel}; ${report.host.logicalCpus} logical CPUs; ${(report.host.totalMemoryBytes / 1024 ** 3).toFixed(1)} GiB RAM.\n\n`;
markdown +=
  "| Profile | Runtime | n | Median ms | p95 ms | Min ms | Max ms |\n|---|---|---:|---:|---:|---:|---:|\n";
for (const [mode, run] of [
  ["Warm", warm],
  ["Fresh profile", fresh],
])
  for (const runtime of ["original", "rust"]) {
    const s = run.statistics[runtime];
    markdown += `| ${mode} | ${runtime} | ${s.n} | ${ms(s.medianMs)} | ${ms(s.p95NearestRankMs)} | ${ms(s.minMs)} | ${ms(s.maxMs)} |\n`;
  }
markdown +=
  "\nOriginal marker: `electron-window-shown`. Rust marker: `rust-native-set-visible-completed`. Median is the midpoint of sorted observations5/6; p95 uses nearest rank (maximum for n=10).\n\n";
for (const [mode, run] of [
  ["Warm", warm],
  ["Fresh profile", fresh],
])
  markdown += `${mode}: Rust/original median ratio ${run.comparison.medianRustOverOriginal.toFixed(3)}; Rust minus original ${ms(run.comparison.medianDifferenceMs)} ms. Period ${run.startedUtc}–${run.finishedUtc}.\n\n`;
markdown +=
  "## Limits\n\n" + report.limitations.map((l) => "- " + l).join("\n") + "\n\n## Provenance\n\n";
for (const [file, hash] of Object.entries(report.artifacts)) markdown += `- ${file}: \`${hash}\`\n`;
markdown += `\nRaw samples, lifecycle markers, WebKit UUIDs, cleanup receipts and input hashes are preserved in ${path.basename(outputBase)}.json.\n`;
fs.writeFileSync(outputBase + ".md", markdown);
console.log(
  JSON.stringify({
    json: outputBase + ".json",
    markdown: outputBase + ".md",
    host: report.host,
    summary: { warm: warm.statistics, freshProfile: fresh.statistics },
  }),
);
