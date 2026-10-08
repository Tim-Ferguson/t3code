// Execute the unchanged original queue admission prefix, with only its I/O
// dependencies mocked. Graph allocation/delivery is covered by native Store tests.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  latestExecutedRun,
  latestRootProviderFailure,
  usageLimitBlockedRun,
} from "../../packages/shared/src/orchestrationV2ThreadError.ts";
import { queuedRunsInDeliveryOrder } from "../../apps/server/src/orchestration-v2/QueuedRunOrder.ts";

const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
function functionBlock(name) {
  const start = source.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`Missing original ${name}`);
  const end = source.indexOf("\n}", start);
  return source.slice(start, end + 2);
}
const begin = source.indexOf("  const startNextQueuedRun =");
const end = source.indexOf("      const rootNodeId = queuedRun.rootNodeId;", begin);
if (begin < 0 || end < 0) throw new Error("Original queue admission boundary changed");
const admissionSource = source.slice(begin, end) + 'return {type:"start",runId:queuedRun.id};\n});';
const create = new Function(
  "Effect",
  "DateTime",
  "latestExecutedRun",
  "latestRootProviderFailure",
  "usageLimitBlockedRun",
  "queuedRunsInDeliveryOrder",
  "projectionStore",
  "readCommandProjection",
  "writeSystemEvents",
  stripTypeScriptTypes(
    functionBlock("isBlockingRun") + "\n" + functionBlock("nextQueuedRun") + "\n" + admissionSource,
    { mode: "strip" },
  ) + "\nreturn startNextQueuedRun;",
);
const timestamp = "2026-01-01T00:00:00.000Z";
const later = "2026-01-02T00:00:00.000Z";
const rows = [];
function base() {
  return {
    thread: { id: "thread", providerInstanceId: "codex", archivedAt: null, deletedAt: null },
    runs: [],
    messages: [],
    turnItems: [],
    providerSessions: [],
  };
}
function run(id, ordinal, status, extra = {}) {
  return {
    id,
    ordinal,
    status,
    providerInstanceId: "codex",
    userMessageId: `message:${id}`,
    rootNodeId: `node:${id}`,
    startedAt: status === "queued" ? null : timestamp,
    completedAt:
      status === "queued" || ["preparing", "starting", "running", "waiting"].includes(status)
        ? null
        : timestamp,
    ...extra,
  };
}
function hydrate(projection) {
  const copy = structuredClone(projection);
  for (const field of ["runs", "turnItems", "providerSessions"])
    for (const row of copy[field])
      for (const key of ["updatedAt", "startedAt", "completedAt"])
        if (typeof row[key] === "string") row[key] = DateTime.makeUnsafe(row[key]);
  return copy;
}
async function record(projection, failedRunId) {
  const held = [];
  const hydrated = hydrate(projection);
  const original = create(
    Effect,
    DateTime,
    latestExecutedRun,
    latestRootProviderFailure,
    usageLimitBlockedRun,
    queuedRunsInDeliveryOrder,
    { canStartQueuedRun: () => Effect.succeed(true) },
    () => Effect.succeed(hydrated),
    (events) => Effect.sync(() => held.push(...events.map((event) => event.payload.id))),
  );
  const result = await Effect.runPromise(
    original("thread", failedRunId === undefined ? undefined : { failedRunId }),
  );
  rows.push({
    projection,
    failedRunId: failedRunId ?? null,
    result: result ?? (held.length ? { type: "hold", runIds: held } : { type: "blocked" }),
    order: queuedRunsInDeliveryOrder(hydrated).map((run) => run.id),
  });
}
for (const active of [
  null,
  "preparing",
  "starting",
  "running",
  "waiting",
  "completed",
  "failed",
  "interrupted",
  "cancelled",
])
  for (const state of ["normal", "archived", "deleted", "held"]) {
    const projection = base();
    if (active) projection.runs.push(run("prior", 1, active));
    projection.runs.push(run("queued", 2, "queued"));
    if (state === "archived") projection.thread.archivedAt = timestamp;
    if (state === "deleted") projection.thread.deletedAt = timestamp;
    if (state === "held") projection.runs[projection.runs.length - 1].queueHeld = true;
    await record(projection);
  }
for (const classification of [
  "usage_limit",
  "validation_error",
  "authentication_error",
  "internal_error",
])
  for (const sessionError of [null, "failure", "different"])
    for (const failedId of [null, "prior", "other"])
      for (const sameProvider of [true, false]) {
        const projection = base();
        projection.runs = [
          run("prior", 1, "failed"),
          run("queued", 2, "queued", {
            providerInstanceId: sameProvider ? "codex" : "claudeAgent",
          }),
        ];
        projection.turnItems = [
          {
            id: "error",
            type: "error",
            status: "failed",
            runId: "prior",
            nodeId: "node:prior",
            ordinal: 1,
            updatedAt: timestamp,
            failure: { class: classification, message: "failure" },
          },
        ];
        projection.providerSessions = [
          {
            id: "session",
            providerInstanceId: "codex",
            updatedAt: timestamp,
            lastError: sessionError,
          },
        ];
        await record(projection, failedId ?? undefined);
      }
for (const automatic of [null, "first", "second", "both"])
  for (const leftPosition of [null, 1, 5])
    for (const rightPosition of [null, 1, 5]) {
      const projection = base();
      projection.runs = [
        run("first", 1, "queued", { queuePosition: leftPosition }),
        run("second", 2, "queued", { queuePosition: rightPosition }),
      ];
      for (const id of ["first", "second"])
        projection.messages.push({
          id: `message:${id}`,
          ...(automatic === id || automatic === "both" ? { delegatedCompletion: null } : {}),
        });
      await record(projection);
    }
// Execution order follows completion, not submission. A never-started canceled
// run does not supersede the subscription-limited run.
for (const status of ["completed", "cancelled"])
  for (const started of [null, timestamp]) {
    const projection = base();
    projection.runs = [
      run("prior", 1, "failed", { completedAt: later }),
      run("newer", 2, status, { startedAt: started }),
      run("queued", 3, "queued"),
    ];
    projection.turnItems = [
      {
        id: "error",
        type: "error",
        status: "failed",
        runId: "prior",
        nodeId: "node:prior",
        ordinal: 1,
        updatedAt: timestamp,
        failure: { class: "usage_limit", message: "failure" },
      },
    ];
    await record(projection);
  }
// Equal-timestamp sessions preserve the original first session as the latest.
for (const errors of [
  ["failure", "different"],
  ["different", "failure"],
]) {
  const projection = base();
  projection.runs = [run("prior", 1, "failed"), run("queued", 2, "queued")];
  projection.turnItems = [
    {
      id: "error",
      type: "error",
      status: "failed",
      runId: "prior",
      nodeId: "node:prior",
      ordinal: 1,
      updatedAt: timestamp,
      failure: { class: "usage_limit", message: "failure" },
    },
  ];
  projection.providerSessions = errors.map((lastError, index) => ({
    id: `session:${index}`,
    providerInstanceId: "codex",
    updatedAt: timestamp,
    lastError,
  }));
  await record(projection);
}
const target = new URL("../crates/server/tests/fixtures/message-queue.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
