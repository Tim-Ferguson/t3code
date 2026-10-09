// Runs the original's fork, merge-back and lineage logic over fixed arrangements, for
// `crates/client/tests/thread_relations_parity.rs`.
//   deriveThreadRelationshipGraph, immediateThreadRelationships, orderWebThreadLineageRows,
//   threadRelationshipRowStatus, resolveMergeBackTargetThreadId  state/threadRelationships.ts
//   resolveLatestMergeBackRun, canForkProjectedAssistantItem        state/threadWorkflows.ts
//   resolveHandoffEndpoints                                         handoff.ts
//   threadRelationshipStatusLabel                                   components/chat/ThreadRelationshipIcon.tsx
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as DateTime from "../../packages/client-runtime/node_modules/effect/dist/DateTime.js";
import {
  deriveThreadRelationshipGraph,
  immediateThreadRelationships,
  orderWebThreadLineageRows,
  resolveMergeBackTargetThreadId,
  threadRelationshipRowStatus,
} from "../../packages/client-runtime/src/state/threadRelationships.ts";
import {
  canForkProjectedAssistantItem,
  resolveLatestMergeBackRun,
} from "../../packages/client-runtime/src/state/threadWorkflows.ts";
import { resolveHandoffEndpoints } from "../../packages/client-runtime/src/handoff.ts";

const iconSource = readFileSync(
  new URL("../../apps/web/src/components/chat/ThreadRelationshipIcon.tsx", import.meta.url),
  "utf8",
);
const start = iconSource.indexOf("export function threadRelationshipStatusLabel(");
const tail = iconSource.slice(start);
const statusLabel = new Function(
  stripTypeScriptTypes(tail.slice(0, /^}\s*$/m.exec(tail).index + 1).replace(/^export /, "")) +
    ";return threadRelationshipStatusLabel;",
)();

const rows = [];

for (const status of [
  null,
  "preparing",
  "starting",
  "running",
  "in_progress",
  "pending",
  "queued",
  "waiting",
  "blocked",
  "completed",
  "failed",
  "error",
  "cancelled",
  "interrupted",
  "rolled_back",
  "resolved_native",
  "resolved_portable",
  "consumed",
  "superseded",
  "idle",
  "nonsense",
]) {
  rows.push({ type: "status", status, output: statusLabel(status) });
}

// Shells: the Rust side reads `createdAtMs`; the original reads a DateTime.
const shell = (id, extra = {}) => ({
  id,
  title: id,
  projectId: "project",
  providerInstanceId: "codex",
  status: "idle",
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: id },
  forkedFrom: null,
  createdAtMs: 1000,
  ...extra,
});
const original = (shells) =>
  shells.map(({ createdAtMs, ...rest }) => ({
    ...rest,
    createdAt: createdAtMs === undefined ? undefined : DateTime.makeUnsafe(createdAtMs),
  }));
const fork = (id, parent, extra = {}) =>
  shell(id, {
    lineage: { parentThreadId: parent, relationshipToParent: "fork", rootThreadId: parent },
    ...extra,
  });
const runFork = (id, parent, runThread, extra = {}) =>
  fork(id, parent, { forkedFrom: { type: "run", threadId: runThread, runId: "run-1" }, ...extra });
const agent = (id, parent, extra = {}) =>
  shell(id, {
    lineage: { parentThreadId: parent, relationshipToParent: "subagent", rootThreadId: parent },
    ...extra,
  });

const graphCases = [
  { shells: [], projection: null },
  { shells: [shell("a")], projection: null },
  { shells: [shell("a"), fork("b", "a"), fork("c", "a", { createdAtMs: 3000 })], projection: null },
  {
    shells: [shell("a"), runFork("b", "x", "a"), fork("c", "a", { activityRunStatus: "running" })],
    projection: null,
  },
  {
    shells: [
      shell("a"),
      agent("b", "a", { status: "completed" }),
      agent("c", "a", { activityRunStatus: "running" }),
    ],
    projection: null,
  },
  {
    shells: [
      shell("a", { createdAtMs: undefined }),
      fork("b", "a", { createdAtMs: 2000 }),
      fork("c", "a", { createdAtMs: 2000 }),
      fork("d", "a", { createdAtMs: 5000 }),
    ],
    projection: null,
  },
  { shells: [shell("a"), fork("b", "a"), shell("b", { title: "stale" })], projection: null },
  { shells: [fork("b", "missing")], projection: null },
  {
    shells: [shell("a"), fork("b", "a"), agent("c", "a", { activityRunStatus: "waiting" })],
    projection: {
      thread: { id: "a" },
      subagents: [
        { childThreadId: "c", status: "completed" },
        { childThreadId: "d", status: "failed" },
        { childThreadId: null, status: "running" },
      ],
      contextTransfers: [
        { sourceThreadId: "b", targetThreadId: "a", status: "pending" },
        { sourceThreadId: "a", targetThreadId: "a", status: "consumed" },
        { sourceThreadId: "a", targetThreadId: "b", status: "superseded" },
      ],
    },
  },
  {
    shells: [
      shell("b", {
        lineage: { parentThreadId: "a", relationshipToParent: "fork", rootThreadId: "a" },
      }),
      shell("a"),
    ],
    projection: {
      thread: { id: "b" },
      subagents: [],
      contextTransfers: [{ sourceThreadId: "a", targetThreadId: "b", status: "consumed" }],
    },
  },
];
for (const input of graphCases) {
  const graph = deriveThreadRelationshipGraph({
    threads: original(input.shells),
    projection: input.projection,
  });
  const ids = [...new Set([...graph.nodes.keys(), "a", "b"])];
  for (const current of ids) {
    const immediate = immediateThreadRelationships(graph, current);
    const merge = resolveMergeBackTargetThreadId(
      input.projection === null
        ? { thread: input.shells.find((s) => s.id === current) ?? shell(current) }
        : { thread: input.shells.find((s) => s.id === current) ?? shell(current) },
    );
    for (const mergeTarget of [null, merge]) {
      const ordered = orderWebThreadLineageRows({
        graph,
        rows: immediate,
        currentThreadId: current,
        mergeTargetThreadId: mergeTarget,
      });
      rows.push({
        type: "graph",
        input,
        current,
        mergeTarget,
        edges: graph.edges,
        immediate: immediate.map((row) => ({ thread: row.threadId, edge: row.edge })),
        ordered: ordered.map((row) => ({
          thread: row.threadId,
          status: threadRelationshipStatusLabelOf(graph, row),
        })),
        merge,
      });
    }
  }
}
function threadRelationshipStatusLabelOf(graph, row) {
  return threadRelationshipRowStatus(graph, row);
}

// Merge-back targets from a thread alone.
for (const thread of [
  shell("a"),
  fork("b", "a"),
  runFork("c", "a", "z"),
  agent("d", "a"),
  shell("e", {
    lineage: { parentThreadId: null, relationshipToParent: "fork", rootThreadId: "e" },
  }),
]) {
  rows.push({ type: "merge-target", thread, output: resolveMergeBackTargetThreadId({ thread }) });
}

// The newest run that can be merged back, and what blocks it.
const statuses = [
  "queued",
  "preparing",
  "starting",
  "running",
  "waiting",
  "completed",
  "failed",
  "interrupted",
  "rolled_back",
];
const runList = (list) =>
  list.map(([status, ordinal], index) => ({ id: `run-${index}`, status, ordinal }));
for (const a of statuses)
  for (const b of statuses)
    for (const c of ["completed", "running", "waiting", null]) {
      const runs = runList([[a, 1], [b, 2], ...(c === null ? [] : [[c, 3]])]);
      const output = resolveLatestMergeBackRun({ runs });
      rows.push({ type: "merge-run", runs, output: output === null ? null : output.id });
    }
rows.push({ type: "merge-run", runs: [], output: null });
rows.push({
  type: "merge-run",
  runs: runList([
    ["completed", 5],
    ["completed", 2],
    ["running", 3],
  ]),
  output:
    resolveLatestMergeBackRun({
      runs: runList([
        ["completed", 5],
        ["completed", 2],
        ["running", 3],
      ]),
    })?.id ?? null,
});

// Which assistant answers can be forked.
const capabilityCases = [
  undefined,
  {
    threads: { canForkThread: true, canForkFromTurn: true },
    identity: { nativeThreadIds: "strong" },
    context: { supportsFullThreadHandoff: false },
  },
  {
    threads: { canForkThread: true, canForkFromTurn: true },
    identity: { nativeThreadIds: "weak" },
    context: { supportsFullThreadHandoff: false },
  },
  {
    threads: { canForkThread: true, canForkFromTurn: false },
    identity: { nativeThreadIds: "strong" },
    context: { supportsFullThreadHandoff: false },
  },
  {
    threads: { canForkThread: false, canForkFromTurn: true },
    identity: { nativeThreadIds: "strong" },
    context: { supportsFullThreadHandoff: false },
  },
  {
    threads: { canForkThread: false, canForkFromTurn: false },
    identity: { nativeThreadIds: "none" },
    context: { supportsFullThreadHandoff: true },
  },
  {
    threads: { canForkThread: false, canForkFromTurn: false },
    identity: { nativeThreadIds: "none" },
    context: { supportsFullThreadHandoff: false },
  },
];
for (const type of ["assistant_message", "user_message", "reasoning"])
  for (const status of ["completed", "running", "failed"])
    for (const runId of ["run-1", null])
      for (const capabilities of capabilityCases) {
        const item = { type, status, runId };
        rows.push({
          type: "can-fork",
          item,
          capabilities: capabilities ?? null,
          output: canForkProjectedAssistantItem({ projectedItem: { item }, capabilities }),
        });
      }

// Handoff endpoints, with and without the models stamped on the item.
const handoffRuns = [
  {
    id: "r1",
    ordinal: 1,
    providerInstanceId: "codex",
    modelSelection: { instanceId: "codex", model: "m1" },
  },
  {
    id: "r2",
    ordinal: 2,
    providerInstanceId: "claude",
    modelSelection: { instanceId: "claude", model: "c1" },
  },
  {
    id: "r3",
    ordinal: 3,
    providerInstanceId: "codex",
    modelSelection: { instanceId: "codex", model: "m2" },
  },
  {
    id: "r4",
    ordinal: 4,
    providerInstanceId: "claude",
    modelSelection: { instanceId: "claude", model: "c2" },
  },
];
for (const runId of [null, "r3", "r4", "missing"])
  for (const toModel of [undefined, "stamped"])
    for (const from of [
      { fromProviderInstanceIds: [] },
      { fromProviderInstanceIds: ["codex"] },
      { fromProviderInstanceIds: ["codex", "claude"] },
      { fromProviderInstanceIds: ["codex"], fromModelSelections: [] },
      {
        fromProviderInstanceIds: ["codex"],
        fromModelSelections: [{ instanceId: "codex", model: "stamped-from" }],
      },
      { fromProviderInstanceIds: ["gone"] },
    ])
      for (const toProviderInstanceId of ["claude", "codex"]) {
        const item = {
          runId,
          toProviderInstanceId,
          ...(toModel === undefined ? {} : { toModel }),
          ...from,
        };
        rows.push({
          type: "handoff",
          item,
          runs: handoffRuns,
          output: resolveHandoffEndpoints(item, handoffRuns),
        });
      }

writeFileSync(
  new URL("../crates/client/tests/fixtures/thread-relations.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${rows.length} original thread relation cases\n`);
