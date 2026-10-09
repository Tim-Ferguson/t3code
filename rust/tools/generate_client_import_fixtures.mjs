// Runs the original's project-import grouping and the tracked clone's wording over fixed inputs,
// for `tests/import_clone_parity.rs`.
//   partitionOnboardingProjects, groupOnboardingProjects   apps/web/src/onboarding/projectImport.logic.ts
//   projectCloneDisplayName, projectCloneProgressSummary    packages/contracts/src/projectClone.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const logic = read("apps/web/src/onboarding/projectImport.logic.ts");
const clone = read("packages/contracts/src/projectClone.ts");
const body = [
  cut(logic, "const RECENT_PROJECT_WINDOW_MS", "/** Use the server's project match"),
  cut(clone, "function projectCloneStageLabel", null),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-import-"));
const file = NodePath.join(scratch, "logic.mjs");
writeFileSync(
  file,
  `${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { partitionOnboardingProjects, groupOnboardingProjects, projectCloneDisplayName, projectCloneProgressSummary };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
const NOW = Date.parse("2026-10-09T12:00:00Z");
const day = 24 * 60 * 60 * 1000;
const at = (offset) => new Date(NOW - offset).toISOString();
const candidate = (path, over = {}) => ({
  path,
  title: path.split("/").pop(),
  sources: ["claudeAgent"],
  threadCount: 5,
  lastActiveAt: at(day),
  alreadyImported: false,
  git: { remoteKey: null, repository: null },
  ...over,
});
const sets = [
  { name: "empty", candidates: [] },
  {
    name: "mixed",
    candidates: [
      candidate("/work/widgets", {
        git: { remoteKey: "github.com/acme/widgets", repository: "acme/widgets" },
        threadCount: 9,
      }),
      candidate("/work/widgets-2", {
        git: { remoteKey: "github.com/acme/widgets", repository: "acme/widgets" },
        lastActiveAt: at(2 * day),
        sources: ["codex"],
      }),
      candidate("/work/solo", { git: { remoteKey: null, repository: null }, threadCount: 3 }),
      candidate("/work/old", {
        git: { remoteKey: "gitlab.com/x/y", repository: null },
        lastActiveAt: at(60 * day),
      }),
      candidate("/work/few", { threadCount: 2 }),
      candidate("/work/folder", { git: null }),
      candidate("/work/never", { lastActiveAt: null }),
      candidate("/work/future", { lastActiveAt: new Date(NOW + day).toISOString() }),
      candidate("/work/imported", {
        alreadyImported: true,
        projectId: "p1",
        git: { remoteKey: "a.com/b", repository: "b/c" },
      }),
      candidate("/work/legacy", { git: undefined }),
    ],
  },
  {
    name: "ties by activity",
    candidates: [
      candidate("/b", { git: { remoteKey: "k1", repository: "z/b" }, lastActiveAt: at(day) }),
      candidate("/a", { git: { remoteKey: "k2", repository: "z/a" }, lastActiveAt: at(day) }),
      candidate("/n", { git: { remoteKey: "k3", repository: "z/n" }, lastActiveAt: null }),
    ],
  },
];
for (const set of sets) {
  const partitioned = original.partitionOnboardingProjects(set.candidates, NOW);
  const grouped = original.groupOnboardingProjects(set.candidates);
  rows.push({
    type: "import",
    name: set.name,
    now: NOW,
    candidates: JSON.parse(JSON.stringify(set.candidates)),
    recent: partitioned.recent.map((entry) => entry.path),
    repositories: grouped.repositories.map((group) => ({
      key: group.key,
      label: group.label,
      repository: group.repository,
      paths: group.candidates.map((entry) => entry.path),
      threadCount: group.threadCount,
      lastActiveAt: group.lastActiveAt,
    })),
    other: grouped.other.map((entry) => entry.path),
  });
}
const snapshots = [
  {
    repository: { nameWithOwner: "acme/widgets" },
    destinationPath: "/tmp/widgets",
    stage: "receiving",
    percent: 45,
    detail: "12.3 MiB | 5.0 MiB/s",
  },
  {
    repository: null,
    destinationPath: "/tmp/some/dir/",
    stage: "connecting",
    percent: null,
    detail: null,
  },
  {
    repository: null,
    destinationPath: "C:\\code\\proj",
    stage: "counting",
    percent: 0,
    detail: "",
  },
  { repository: null, destinationPath: "/", stage: "resolving", percent: 100, detail: null },
  {
    repository: null,
    destinationPath: "/x/checkout",
    stage: "checkout",
    percent: 7,
    detail: "files",
  },
];
for (const snapshot of snapshots) {
  rows.push({
    type: "clone",
    snapshot,
    name: original.projectCloneDisplayName(snapshot),
    summary: original.projectCloneProgressSummary(snapshot),
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/import-clone.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
