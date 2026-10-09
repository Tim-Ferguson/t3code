// Runs the unchanged `planThreadDeletion` (the plan both `thread.delete` and the project cascade
// commit) over scripted projections and writes the events and effects it plans. The Rust port replays
// every row. No store, provider or file system is involved.
import { renameSync, writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import { planThreadDeletion } from "../../apps/server/src/orchestration-v2/ThreadDeletion.ts";

const THREAD = "thread:delete-plan";
const CREATED = "2026-09-01T00:00:00.000Z";
const NOW = "2026-09-04T00:00:00.000Z";
const model = { instanceId: "codex", model: "gpt-5.4" };
const thread = (overrides = {}) => ({
  id: THREAD,
  createdBy: "user",
  creationSource: "web",
  projectId: "project:delete-plan",
  title: "Delete the thread",
  providerInstanceId: "codex",
  modelSelection: model,
  runtimeMode: "full-access",
  interactionMode: "default",
  branch: "feature",
  worktreePath: "/workspace/feature",
  activeProviderThreadId: null,
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: THREAD },
  forkedFrom: null,
  createdAt: CREATED,
  updatedAt: CREATED,
  archivedAt: null,
  settledOverride: null,
  settledAt: null,
  lastVisitedAt: null,
  deletedAt: null,
  titleRegeneration: { type: "regenerate" },
  ...overrides,
});
const STATUSES = [
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
  "completed",
  "failed",
  "interrupted",
];
const runs = (statuses = STATUSES) =>
  statuses.map((status, index) => ({
    id: `run:${status}`,
    threadId: THREAD,
    ordinal: index + 1,
    providerInstanceId: "codex",
    modelSelection: model,
    providerThreadId: "provider-thread:1",
    userMessageId: `message:${status}`,
    rootNodeId: `node:${status}`,
    activeAttemptId: `attempt:${status}`,
    status,
    queuePosition: status === "queued" ? 1 : null,
    requestedAt: CREATED,
    startedAt: CREATED,
    completedAt: ["completed", "failed", "interrupted"].includes(status) ? CREATED : null,
    checkpointId: null,
    contextHandoffId: null,
  }));
const attempts = (
  rows,
  attemptStatus = (run) =>
    ["completed", "failed", "interrupted"].includes(run.status) ? run.status : "running",
) =>
  rows.map((run) => ({
    id: run.activeAttemptId,
    runId: run.id,
    attemptOrdinal: 1,
    rootNodeId: run.rootNodeId,
    providerInstanceId: "codex",
    providerThreadId: "provider-thread:1",
    providerTurnId: null,
    reason: "initial",
    status: attemptStatus(run),
    startedAt: CREATED,
    completedAt: run.completedAt,
  }));
const nodes = (
  rows,
  nodeStatus = (run) =>
    ["completed", "failed", "interrupted"].includes(run.status) ? run.status : "waiting",
) =>
  rows.map((run) => ({
    id: run.rootNodeId,
    threadId: THREAD,
    runId: run.id,
    parentNodeId: null,
    rootNodeId: run.rootNodeId,
    kind: "root_turn",
    status: nodeStatus(run),
    countsForRun: true,
    providerThreadId: "provider-thread:1",
    providerTurnId: null,
    nativeItemRef: null,
    runtimeRequestId: null,
    checkpointScopeId: null,
    startedAt: CREATED,
    completedAt: run.completedAt,
  }));
const request = (status, node = "node:running") => ({
  id: `request:${status}`,
  nodeId: node,
  providerTurnId: null,
  nativeRequestRef: null,
  kind: "user_input",
  status,
  responseCapability: { type: "message" },
  createdAt: CREATED,
  resolvedAt: status === "pending" ? null : CREATED,
});
const session = (status) => ({
  id: `session:${status}`,
  driver: "codex",
  providerInstanceId: "codex",
  status,
  cwd: "/workspace/feature",
  model: null,
  capabilities: {},
  createdAt: CREATED,
  updatedAt: CREATED,
  lastError: null,
});
const base = (rowsOverride, extra = {}) => {
  const rows = rowsOverride ?? runs();
  return {
    thread: thread(),
    runs: rows,
    attempts: attempts(rows),
    nodes: nodes(rows),
    runtimeRequests: [],
    subagents: [],
    providerSessions: [],
    ...extra,
  };
};

const scenarios = [];
const add = (name, projection, attachmentIds = []) =>
  scenarios.push({ name, projection, attachmentIds });
add(
  "every-run-status",
  base(undefined, { runtimeRequests: [request("pending"), request("resolved")] }),
);
add("only-finished-runs", base(runs(["completed", "failed", "interrupted"])));
add("no-runs", base([]));
add("queued-only", base(runs(["queued"])));
add("preparing-only", base(runs(["preparing"])));
add(
  "pending-requests-everywhere",
  base(runs(["running", "waiting"]), {
    runtimeRequests: [
      request("pending"),
      request("expired"),
      { ...request("pending", "node:waiting"), id: "request:second" },
    ],
  }),
);
add(
  "attempt-already-settled",
  base(runs(["running"]), { attempts: attempts(runs(["running"]), () => "failed") }),
);
add(
  "node-already-settled",
  base(runs(["running"]), { nodes: nodes(runs(["running"]), () => "completed") }),
);
add(
  "pending-attempt-and-node",
  base(runs(["starting"]), {
    attempts: attempts(runs(["starting"]), () => "pending"),
    nodes: nodes(runs(["starting"]), () => "pending"),
  }),
);
add("earlier-deletion-timestamp-kept", {
  ...base(runs(["running"])),
  thread: thread({ deletedAt: CREATED }),
});
add("archived-thread", { ...base(runs(["completed"])), thread: thread({ archivedAt: CREATED }) });
add(
  "sessions",
  base(runs(["completed"]), {
    providerSessions: ["running", "stopped", "error", "ready", "starting"].map(session),
  }),
);
add("attachments-are-deduplicated", base(runs(["completed"])), [
  "shared_file",
  "other-file",
  "shared_file",
]);
add(
  "everything",
  {
    ...base(undefined, {
      runtimeRequests: [request("pending")],
      providerSessions: [session("ready"), session("stopped")],
    }),
    thread: thread({ deletedAt: CREATED }),
  },
  ["a", "b", "a"],
);

// Delegated work that still owes a completion is disposed with the thread.
const task = (id, runId, delivery) => ({
  id,
  threadId: THREAD,
  runId,
  parentNodeId: "node:running",
  origin: "app_owned",
  createdBy: "agent",
  driver: "codex",
  providerInstanceId: "codex",
  providerThreadId: null,
  childThreadId: `thread:${id}`,
  nativeTaskRef: null,
  prompt: "Do the work",
  title: null,
  model: null,
  status: "completed",
  result: "done",
  startedAt: CREATED,
  completedAt: CREATED,
  updatedAt: CREATED,
  ...(delivery === undefined
    ? {}
    : { completionDelivery: { state: delivery, observedByRunId: null } }),
});
const cohort = (disposition = "open", extra = {}) => ({
  disposition,
  nextGeneration: 3,
  delivery: null,
  ...extra,
});
const withCohort = (rows, runId, value) =>
  rows.map((run) => (run.id === runId ? { ...run, delegatedCompletion: value } : run));
{
  const finished = runs(["completed", "running"]);
  add(
    "cohort-with-owed-tasks",
    base(finished, {
      subagents: [
        task("task:one", "run:completed", "pending"),
        task("task:two", "run:completed", "delivered"),
        task("task:three", "run:completed"),
      ],
    }),
  );
  add(
    "cohort-run-with-an-open-cohort",
    base(
      withCohort(
        finished,
        "run:completed",
        cohort("open", {
          delivery: { generation: 2, messageId: "message:wake", taskIds: ["task:one"] },
        }),
      ),
      { subagents: [task("task:one", "run:completed", "claimed")] },
    ),
  );
  add(
    "cohort-on-an-active-run-is-cancelled-first",
    base(withCohort(finished, "run:running", cohort("stopped")), {
      subagents: [task("task:one", "run:running", "pending")],
    }),
  );
  add(
    "cohort-already-disposed",
    base(withCohort(finished, "run:completed", cohort("disposed")), {
      subagents: [task("task:one", "run:completed", "disposed")],
    }),
  );
  add("cohort-without-tasks", base(withCohort(finished, "run:completed", cohort("open"))));
  add(
    "tasks-of-two-runs",
    base(finished, {
      subagents: [
        task("task:one", "run:completed", "pending"),
        task("task:two", "run:running", "acknowledged"),
        task("task:three", "run:running", "pending"),
      ],
    }),
  );
  add(
    "provider-native-subagents-are-not-a-cohort",
    base(finished, {
      subagents: [{ ...task("task:native", "run:completed"), origin: "provider_native" }],
    }),
  );
}

const rows = [];
const allocator = await Effect.runPromise(
  Effect.gen(function* () {
    return yield* IdAllocator.IdAllocatorV2;
  }).pipe(Effect.provide(IdAllocator.layer)),
);
for (const scenario of scenarios) {
  const plan = await Effect.runPromise(
    planThreadDeletion({
      command: { type: "thread.delete", commandId: "command:delete-plan", threadId: THREAD },
      projection: scenario.projection,
      attachmentIds: scenario.attachmentIds,
      now: DateTime.makeUnsafe(NOW),
      idAllocator: allocator,
    }),
  );
  const plain = JSON.parse(
    JSON.stringify({
      events: plan.events.map((event) => ({ type: event.type, payload: event.payload })),
      effects: plan.effects.map((effect) => ({ id: effect.id, request: effect.request })),
    }),
  );
  rows.push({
    name: scenario.name,
    projection: scenario.projection,
    attachmentIds: scenario.attachmentIds,
    output: plain,
  });
}
const target = new URL("../crates/server/tests/fixtures/thread-deletion.jsonl", import.meta.url);
const staging = new URL(
  "../crates/server/tests/fixtures/thread-deletion.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${rows.length} rows`);
