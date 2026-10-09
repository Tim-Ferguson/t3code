// Runs the unchanged `limitRecoveryCommand` of the original usage-limit recovery worker, and the shared
// thread-error functions it depends on, over scripted candidates. The Rust port replays every row
// (`usage_limits.rs`). `limitRecoveryCommand` is lifted by source because its module reaches the scheduler
// and the projection store, which a script has no use for.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  latestExecutedRun,
  latestRootProviderFailure,
  threadErrorSummary,
  usageLimitBlockedRun,
  usageLimitRunPresentedAsLatest,
} from "../../packages/shared/src/orchestrationV2ThreadError.ts";

const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/UsageLimitRecoveryWorker.ts", import.meta.url),
  "utf8",
);
const start = source.indexOf("export function limitRecoveryCommand(");
const end = source.indexOf("\n}\n", start);
if (start < 0 || end < 0) throw new Error("limitRecoveryCommand not found");
const directory = mkdtempSync(join(tmpdir(), "usage-limit-recovery-"));
const file = join(directory, "lifted.ts");
writeFileSync(
  file,
  [
    `import * as DateTime from ${JSON.stringify(pathToFileURL(join(process.cwd(), "apps/server/node_modules/effect/dist/DateTime.js")).href)};`,
    "const CommandId = { make: (value: string) => value };",
    "const MessageId = { make: (value: string) => value };",
    source
      .slice(start, end + 3)
      .replace(/ProjectionStore\.ProjectionLimitRecoveryCandidate/, "any")
      .replace(/OrchestrationV2Command \| null/, "any"),
  ].join("\n"),
);
const { limitRecoveryCommand } = await import(pathToFileURL(file).href);

const reset = "2026-03-01T12:00:00.000Z";
const resetMs = Date.parse(reset);
const iso = (ms) => new Date(ms).toISOString();
const candidate = (overrides = {}) => ({
  id: "thread-1",
  status: "failed",
  lastErrorClass: "usage_limit",
  latestRunId: "run-1",
  usageLimitResetAt: reset,
  archivedAt: null,
  settledOverride: null,
  pendingRuntimeRequest: null,
  latestRunCompletedAt: iso(resetMs - 3_600_000),
  updatedAt: iso(resetMs - 3_500_000),
  limitRecovery: null,
  snoozedUntil: null,
  ...overrides,
});
const armed = (overrides = {}) => ({
  runId: "run-1",
  resetAt: reset,
  autoResume: true,
  snooze: false,
  requestId: "arm-1",
  ...overrides,
});
const toDate = (value) => (typeof value === "string" ? DateTime.makeUnsafe(value) : value);
const decode = (input) => ({
  ...input,
  latestRunCompletedAt:
    input.latestRunCompletedAt === null ? null : toDate(input.latestRunCompletedAt),
  updatedAt: toDate(input.updatedAt),
  snoozedUntil: input.snoozedUntil === null ? null : toDate(input.snoozedUntil),
});
const rows = [];
const times = [resetMs - 7_200_000, resetMs - 1, resetMs, resetMs + 1];
const candidates = [
  candidate(),
  candidate({ status: "completed" }),
  candidate({ lastErrorClass: "provider_error" }),
  candidate({ latestRunId: "" }),
  candidate({ usageLimitResetAt: null }),
  candidate({ archivedAt: iso(resetMs - 10) }),
  candidate({ settledOverride: "settled" }),
  candidate({ settledOverride: "active" }),
  candidate({ pendingRuntimeRequest: { id: "request-1" } }),
  candidate({ latestRunCompletedAt: null }),
  candidate({ latestRunCompletedAt: iso(resetMs + 1000) }),
  candidate({ latestRunCompletedAt: reset }),
  candidate({ usageLimitResetAt: "not a time" }),
  candidate({ limitRecovery: armed() }),
  candidate({ limitRecovery: armed({ autoResume: false }) }),
  candidate({ limitRecovery: armed({ runId: "run-0" }) }),
  candidate({ limitRecovery: armed({ resetAt: "2026-03-01T13:00:00.000Z" }) }),
  candidate({ limitRecovery: armed({ requestId: undefined }) }),
  candidate({ limitRecovery: armed(), snoozedUntil: iso(resetMs + 60_000) }),
  candidate({ limitRecovery: armed(), snoozedUntil: iso(resetMs - 60_000) }),
  candidate({ limitRecovery: armed({ snooze: true }), snoozedUntil: reset }),
];
for (const input of candidates) {
  for (const autoResume of [false, true]) {
    for (const snooze of [false, true]) {
      for (const nowMs of times) {
        const output = limitRecoveryCommand(decode(input), autoResume, nowMs, snooze);
        rows.push({ kind: "command", input, autoResume, snooze, nowMs, output });
      }
    }
  }
}

// The summary a thread shell shows for a failure, and the limited run it is read from.
const failure = (overrides = {}) => ({
  class: "usage_limit",
  message: "limit",
  resetAt: reset,
  ...overrides,
});
const run = (id, ordinal, status, overrides = {}) => ({
  id,
  ordinal,
  status,
  rootNodeId: `${id}:root`,
  requestedAt: DateTime.makeUnsafe(iso(resetMs - 7_000_000 + ordinal)),
  startedAt: DateTime.makeUnsafe(iso(resetMs - 7_000_000 + ordinal)),
  completedAt:
    status === "running" ? null : DateTime.makeUnsafe(iso(resetMs - 6_000_000 + ordinal)),
  ...overrides,
});
const errorItem = (runId, theFailure, overrides = {}) => ({
  id: `${runId}:error`,
  type: "error",
  status: "failed",
  runId,
  nodeId: `${runId}:root`,
  ordinal: 1,
  updatedAt: DateTime.makeUnsafe(iso(resetMs - 6_000_000)),
  failure: theFailure,
  ...overrides,
});
const plain = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, found) =>
      found && typeof found === "object" && "epochMilliseconds" in found ? undefined : found,
    ),
  );
const scenes = [
  { runs: [run("r1", 1, "failed")], items: [errorItem("r1", failure())], sessionError: null },
  { runs: [run("r1", 1, "failed")], items: [errorItem("r1", failure())], sessionError: "limit" },
  {
    runs: [run("r1", 1, "failed")],
    items: [errorItem("r1", failure())],
    sessionError: "the process exited",
  },
  {
    runs: [run("r1", 1, "failed")],
    items: [errorItem("r1", failure({ class: "provider_error" }))],
    sessionError: null,
  },
  {
    runs: [run("r1", 1, "failed")],
    items: [errorItem("r1", failure({ resetAt: undefined }))],
    sessionError: null,
  },
  { runs: [run("r1", 1, "failed")], items: [], sessionError: null },
  { runs: [run("r1", 1, "completed")], items: [errorItem("r1", failure())], sessionError: null },
  {
    runs: [run("r1", 1, "failed"), run("r2", 2, "queued")],
    items: [errorItem("r1", failure())],
    sessionError: null,
  },
  {
    runs: [run("r1", 1, "failed"), run("r2", 2, "cancelled", { startedAt: null })],
    items: [errorItem("r1", failure())],
    sessionError: null,
  },
  {
    runs: [run("r1", 1, "failed"), run("r2", 2, "queued", { queueHeld: true })],
    items: [errorItem("r1", failure())],
    sessionError: null,
  },
  {
    runs: [run("r1", 1, "failed"), run("r2", 2, "completed")],
    items: [errorItem("r1", failure())],
    sessionError: null,
  },
];
for (const scene of scenes) {
  const blocked = usageLimitBlockedRun(scene.runs, scene.items, scene.sessionError);
  const latest = latestExecutedRun(scene.runs);
  const root = latestRootProviderFailure(latest, scene.items);
  const presented = usageLimitRunPresentedAsLatest(scene.runs, scene.items, scene.sessionError);
  rows.push({
    kind: "summary",
    runs: scene.runs.map((entry) => ({
      id: entry.id,
      ordinal: entry.ordinal,
      status: entry.status,
      rootNodeId: entry.rootNodeId,
      startedAt: entry.startedAt === null ? null : DateTime.formatIso(entry.startedAt),
      completedAt: entry.completedAt === null ? null : DateTime.formatIso(entry.completedAt),
      ...(entry.queueHeld ? { queueHeld: true } : {}),
    })),
    items: scene.items.map((item) => ({ ...item, updatedAt: DateTime.formatIso(item.updatedAt) })),
    sessionError: scene.sessionError,
    blockedRunId: blocked?.id ?? null,
    presentedAsLatestRunId: presented?.id ?? null,
    summary: root === undefined ? null : plain(threadErrorSummary(root, scene.sessionError)),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/usage-limit-recovery.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} rows`);
