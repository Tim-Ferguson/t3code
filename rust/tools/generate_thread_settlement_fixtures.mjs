// Runs the unchanged automatic-settlement decisions of the original (`isAutoSettlementCandidate`,
// `threadHasQueuedTurnStart`, `resolveAutoSettlementAt`), lifted by source out of
// `ThreadSettlementService.ts` because that module also reaches git, source control and terminals, over
// scripted threads. The Rust port replays every row (`thread_settlement.rs`).
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url);
const source = readFileSync(
  new URL("apps/server/src/orchestration-v2/ThreadSettlementService.ts", root),
  "utf8",
);
const start = source.indexOf("const DAY_MS");
const end = source.indexOf("export class ThreadSettlementServiceV2");
const skipFrom = source.indexOf("function canonicalRepositoryKey(");
const skipTo = source.indexOf("/**\n * A recent user message stays queued");
if (start < 0 || end < 0 || skipFrom < 0 || skipTo < 0)
  throw new Error("settlement decisions not found");
const lifted = source.slice(start, skipFrom) + source.slice(skipTo, end);
const directory = mkdtempSync(join(tmpdir(), "thread-settlement-"));
const file = join(directory, "lifted.ts");
const href = (path) => pathToFileURL(new URL(path, root).pathname).href;
writeFileSync(
  file,
  stripTypeScriptTypes(
    [
      `import * as DateTime from ${JSON.stringify(href("apps/server/node_modules/effect/dist/DateTime.js"))};`,
      `import { backgroundWorkHoldsCompletion } from ${JSON.stringify(href("packages/shared/src/orchestrationV2PendingBackgroundWork.ts"))};`,
      `import { visibleThreadPullRequests } from ${JSON.stringify(href("packages/shared/src/threadPullRequests.ts"))};`,
      lifted.replace(/^export const QUEUED/m, "export const QUEUED"),
    ].join("\n"),
    { mode: "strip" },
  ),
);
const { isAutoSettlementCandidate, resolveAutoSettlementAt, threadHasQueuedTurnStart } =
  await import(pathToFileURL(file).href);
const DateTimeModule = await import(href("apps/server/node_modules/effect/dist/DateTime.js"));

const dateFields = [
  "createdAt",
  "updatedAt",
  "snoozedUntil",
  "snoozedAt",
  "latestRunRequestedAt",
  "latestRunStartedAt",
  "latestRunCompletedAt",
  "latestUserMessageAt",
  "latestUserAuthoredMessageAt",
  "archivedAt",
  "pinnedAt",
  "autoSettleDisabledAt",
];
const decode = (input) => {
  const decoded = { ...input };
  for (const field of dateFields) {
    if (typeof decoded[field] === "string")
      decoded[field] = DateTimeModule.makeUnsafe(decoded[field]);
  }
  return decoded;
};
const base = {
  id: "thread-1",
  projectId: "project-1",
  createdAt: "2026-01-01T00:00:00.000Z",
  updatedAt: "2026-01-02T00:10:00.000Z",
  archivedAt: null,
  settledOverride: null,
  pinnedAt: null,
  autoSettleDisabledAt: null,
  pendingRuntimeRequest: null,
  activityRunStatus: null,
  pendingBackgroundTasks: [],
  snoozedUntil: null,
  snoozedAt: null,
  latestRunId: "run-1",
  latestRunRequestedAt: "2026-01-02T00:00:00.000Z",
  latestRunStartedAt: "2026-01-02T00:00:01.000Z",
  latestRunCompletedAt: "2026-01-02T00:10:00.000Z",
  latestUserMessageAt: "2026-01-02T00:00:00.000Z",
  latestUserAuthoredMessageAt: "2026-01-02T00:00:00.000Z",
  status: "completed",
  pullRequests: [],
};
const link = (state, overrides = {}) => ({
  host: "github.com",
  repository: "o/r",
  number: 1,
  url: "https://github.com/o/r/pull/1",
  source: "manual",
  linkedAt: "2026-01-01T00:00:00.000Z",
  stack: null,
  snapshot:
    state === null
      ? null
      : {
          state,
          title: "t",
          headBranch: "h",
          baseBranch: "main",
          isDraft: false,
          updatedAt: null,
          syncedAt: "2026-01-03T00:00:00.000Z",
          mergedAt: state === "merged" ? "2026-01-03T00:00:00.000Z" : null,
          closedAt: state === "closed" ? "2026-01-03T00:00:00.000Z" : null,
        },
  ...overrides,
});
const threads = [
  {},
  { archivedAt: "2026-01-03T00:00:00.000Z" },
  { settledOverride: "settled" },
  { settledOverride: "active" },
  { pinnedAt: "2026-01-03T00:00:00.000Z" },
  { autoSettleDisabledAt: "2026-01-03T00:00:00.000Z" },
  { pendingRuntimeRequest: { id: "q", kind: "approval" } },
  { activityRunStatus: "running" },
  { pendingBackgroundTasks: [{ kind: "command" }] },
  { pendingBackgroundTasks: [{ kind: "subagent" }] },
  { pendingBackgroundTasks: [{ kind: "monitor" }] },
  { pendingBackgroundTasks: [{ kind: "background_task" }] },
  { snoozedUntil: "2026-02-01T00:00:00.000Z", snoozedAt: "2026-01-01T12:00:00.000Z" },
  { snoozedUntil: "2026-02-01T00:00:00.000Z", snoozedAt: "2026-01-02T12:00:00.000Z" },
  {
    snoozedUntil: "2026-02-01T00:00:00.000Z",
    snoozedAt: "2026-01-03T00:00:00.000Z",
    status: "failed",
  },
  { snoozedUntil: "2026-02-01T00:00:00.000Z", snoozedAt: null, status: "failed" },
  { snoozedUntil: "2026-01-05T00:00:00.000Z" },
  { latestUserMessageAt: "2026-01-10T00:00:30.000Z" },
  { latestUserMessageAt: "2026-01-10T00:00:30.000Z", latestRunId: null },
  { latestUserMessageAt: "2026-01-10T00:00:30.000Z", status: "failed" },
  { latestUserMessageAt: null },
  {
    latestRunStartedAt: null,
    latestRunCompletedAt: null,
    latestRunRequestedAt: null,
    latestUserMessageAt: null,
  },
  { pullRequests: [link("merged")] },
  { pullRequests: [link("closed")] },
  { pullRequests: [link("open")] },
  { pullRequests: [link(null)] },
  { pullRequests: [link("merged"), link("open", { number: 2 })] },
  {
    pullRequests: [
      link("merged"),
      link("closed", {
        number: 2,
        snapshot: { ...link("closed").snapshot, closedAt: "2026-01-04T00:00:00.000Z" },
      }),
    ],
  },
  { pullRequests: [link("open", { source: "stack-dismissed" })] },
  {
    pullRequests: [
      link("merged", {
        snapshot: { ...link("merged").snapshot, mergedAt: "2026-01-01T12:00:00.000Z" },
      }),
    ],
  },
  {
    pullRequests: [link("merged", { snapshot: { ...link("merged").snapshot, mergedAt: null } })],
    latestUserAuthoredMessageAt: null,
  },
  { latestUserAuthoredMessageAt: null },
];
const nowTimes = [
  "2026-01-02T00:00:30.000Z",
  "2026-01-04T00:00:00.000Z",
  "2026-01-10T00:00:00.000Z",
  "2026-03-10T00:00:00.000Z",
];
const pullRequests = [
  null,
  { state: "merged", mergedAt: "2026-01-03T00:00:00.000Z", closedAt: null },
  { state: "closed", mergedAt: null, closedAt: "2026-01-03T00:00:00.000Z" },
  { state: "open", mergedAt: null, closedAt: null },
  { state: "merged", mergedAt: null, closedAt: null },
];
const inputs = threads.map((overrides) => ({ ...base, ...overrides }));
const rows = [{ kind: "threads", threads: inputs }];
for (const [index, input] of inputs.entries()) {
  for (const now of nowTimes) {
    const nowMs = Date.parse(now);
    rows.push({
      kind: "queued",
      thread: index,
      nowMs,
      output: threadHasQueuedTurnStart(decode(input), nowMs),
    });
    rows.push({
      kind: "candidate",
      thread: index,
      nowMs,
      output: isAutoSettlementCandidate(decode(input), nowMs),
    });
    for (const pullRequest of pullRequests) {
      for (const afterDays of [null, 1, 3, 30]) {
        for (const onMerge of [false, true]) {
          const settledAt = resolveAutoSettlementAt({
            thread: decode(input),
            pullRequest,
            nowMs,
            autoSettleAfterDays: afterDays,
            autoSettleOnMerge: onMerge,
          });
          rows.push({
            kind: "settle",
            thread: index,
            nowMs,
            pullRequest,
            afterDays,
            onMerge,
            output: settledAt === null ? null : DateTimeModule.toEpochMillis(settledAt),
          });
        }
      }
    }
  }
}
writeFileSync(
  new URL("rust/crates/server/tests/fixtures/thread-settlement.jsonl", root),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} rows`);
