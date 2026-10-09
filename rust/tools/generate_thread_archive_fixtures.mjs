// Runs the unchanged `dispatchThreadMutation` of Orchestrator.ts (sliced out and given scripted
// collaborators) for `thread.archive` and `thread.unarchive` over scripted projections, and writes
// the events and effects it decides. The Rust port replays every row. No store or provider runs.
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Ref from "../../apps/server/node_modules/effect/dist/Ref.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import { OrchestratorDispatchError } from "../../apps/server/src/orchestration-v2/Orchestrator.ts";

const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
const slice = (from, to) => {
  const begin = source.indexOf(from);
  const end = source.indexOf(to, begin);
  if (begin < 0 || end < 0) throw new Error(`Orchestrator source changed near ${from}`);
  return source.slice(begin, end);
};
const mutationSource = stripTypeScriptTypes(
  slice("  const dispatchThreadMutation = Effect.fn(", "  const dispatchProviderSessionDetach"),
  { mode: "strip" },
);
const watchSource = stripTypeScriptTypes(
  slice("function withPullRequestWatch(", "/** A legacy single-PR link as a link entry."),
  { mode: "strip" },
);
const make = new Function(
  "Effect",
  "Ref",
  "DateTime",
  "Option",
  "projectionStore",
  "mapDispatchError",
  "OrchestratorDispatchError",
  "emit",
  "loadProjectionForCommand",
  "disposeAllDelegatedCompletionCohorts",
  "getProjectionWithPendingEvents",
  `${watchSource}\n${mutationSource}\nreturn dispatchThreadMutation;`,
);

const NOW = "2026-09-04T00:00:00.000Z";
const CREATED = "2026-09-01T00:00:00.000Z";
const model = { instanceId: "codex", model: "gpt-5.4" };
const thread = (overrides = {}) => ({
  id: "thread:archive",
  createdBy: "user",
  creationSource: "web",
  projectId: "project:archive",
  title: "Archive me",
  providerInstanceId: "codex",
  modelSelection: model,
  runtimeMode: "full-access",
  interactionMode: "default",
  branch: null,
  worktreePath: null,
  activeProviderThreadId: null,
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: "thread:archive" },
  forkedFrom: null,
  createdAt: CREATED,
  updatedAt: CREATED,
  archivedAt: null,
  settledOverride: null,
  settledAt: null,
  lastVisitedAt: null,
  deletedAt: null,
  titleRegeneration: { requestId: "command:title", startedAt: CREATED },
  ...overrides,
});
const run = (status, ordinal) => ({
  id: `run:${status}:${ordinal}`,
  threadId: "thread:archive",
  ordinal,
  providerInstanceId: "codex",
  modelSelection: model,
  providerThreadId: "provider-thread:1",
  userMessageId: `message:${status}:${ordinal}`,
  rootNodeId: `node:${status}:${ordinal}`,
  activeAttemptId: `attempt:${status}:${ordinal}`,
  status,
  queuePosition: status === "queued" ? ordinal : null,
  requestedAt: CREATED,
  startedAt: status === "queued" ? null : CREATED,
  completedAt: ["completed", "failed", "interrupted", "cancelled"].includes(status)
    ? CREATED
    : null,
  checkpointId: null,
  contextHandoffId: null,
});
const attempt = (r, status) => ({
  id: r.activeAttemptId,
  runId: r.id,
  attemptOrdinal: 1,
  rootNodeId: r.rootNodeId,
  providerInstanceId: "codex",
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  reason: "initial",
  status,
  startedAt: CREATED,
  completedAt: null,
});
const node = (r, status) => ({
  id: r.rootNodeId,
  threadId: "thread:archive",
  runId: r.id,
  parentNodeId: null,
  rootNodeId: r.rootNodeId,
  kind: "root_turn",
  status,
  countsForRun: true,
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  nativeItemRef: null,
  runtimeRequestId: null,
  checkpointScopeId: null,
  startedAt: CREATED,
  completedAt: null,
});
const session = (id, status) => ({
  id,
  driver: "codex",
  providerInstanceId: "codex",
  status,
  cwd: "/workspace",
  model: null,
  capabilities: { sessions: { supportsRuntimeModeSwitchInSession: true } },
  createdAt: CREATED,
  updatedAt: CREATED,
  lastError: null,
});
const link = (number, watch) => ({
  host: "github.com",
  repository: "t3/code",
  number,
  url: `https://github.com/t3/code/pull/${number}`,
  source: "manual",
  linkedAt: CREATED,
  snapshot: null,
  stack: null,
  ...(watch ? { watch } : {}),
});

const projection = ({
  runs = [],
  attemptStatus = "pending",
  nodeStatus = "pending",
  sessions = [],
  threadOverrides = {},
} = {}) => ({
  thread: thread(threadOverrides),
  runs,
  attempts: runs.map((r) => attempt(r, attemptStatus)),
  nodes: runs.map((r) => node(r, nodeStatus)),
  subagents: [],
  messages: [],
  providerSessions: sessions,
});
const scenarios = [];
const add = (name, type, value) => scenarios.push({ name, type, projection: value });
add("idle-thread", "thread.archive", projection());
add(
  "finished-runs-and-stopped-session",
  "thread.archive",
  projection({
    runs: [run("completed", 1), run("failed", 2)],
    attemptStatus: "completed",
    nodeStatus: "completed",
    sessions: [session("session:stopped", "stopped"), session("session:error", "error")],
  }),
);
add(
  "live-session",
  "thread.archive",
  projection({
    runs: [run("completed", 1)],
    attemptStatus: "completed",
    nodeStatus: "completed",
    sessions: [session("session:ready", "ready")],
  }),
);
add(
  "several-sessions-some-live",
  "thread.archive",
  projection({
    sessions: [
      session("session:a", "running"),
      session("session:b", "stopped"),
      session("session:c", "waiting"),
      session("session:d", "starting"),
    ],
  }),
);
add(
  "running-run-with-live-session",
  "thread.archive",
  projection({
    runs: [run("running", 1)],
    attemptStatus: "running",
    nodeStatus: "running",
    sessions: [session("session:ready", "ready")],
  }),
);
add("queued-runs", "thread.archive", projection({ runs: [run("queued", 1), run("queued", 2)] }));
add(
  "running-and-queued",
  "thread.archive",
  projection({
    runs: [run("running", 1), run("queued", 2)],
    attemptStatus: "running",
    nodeStatus: "waiting",
    sessions: [session("session:ready", "ready")],
  }),
);
add(
  "queued-run-already-settled-rows",
  "thread.archive",
  projection({ runs: [run("queued", 1)], attemptStatus: "cancelled", nodeStatus: "completed" }),
);
add(
  "waiting-and-preparing-runs-are-left-to-their-sessions",
  "thread.archive",
  projection({
    runs: [run("preparing", 1), run("waiting", 2)],
    attemptStatus: "running",
    nodeStatus: "waiting",
  }),
);
add(
  "pull-request-watches-end",
  "thread.archive",
  projection({
    threadOverrides: {
      pullRequests: [
        link(1, { enabled: true, since: CREATED }),
        link(2),
        link(3, { enabled: false, since: CREATED }),
      ],
    },
  }),
);
add("already-archived", "thread.archive", projection({ threadOverrides: { archivedAt: CREATED } }));
add("deleted", "thread.archive", projection({ threadOverrides: { deletedAt: CREATED } }));
add(
  "unarchive",
  "thread.unarchive",
  projection({
    threadOverrides: { archivedAt: CREATED },
    sessions: [session("session:stopped", "stopped")],
  }),
);
add("unarchive-an-active-thread", "thread.unarchive", projection());

import * as Clock from "../../apps/server/node_modules/effect/dist/Clock.js";
const fixedMillis = Date.parse(NOW);
const clock = {
  currentTimeMillisUnsafe: () => fixedMillis,
  currentTimeMillis: Effect.succeed(fixedMillis),
  currentTimeNanosUnsafe: () => BigInt(fixedMillis) * 1_000_000n,
  currentTimeNanos: Effect.succeed(BigInt(fixedMillis) * 1_000_000n),
  sleep: () => Effect.void,
};
const plain = (value) => JSON.parse(JSON.stringify(value));
const rows = [];
for (const scenario of scenarios) {
  const records = scenario.projection;
  let counter = 0;
  const eventsRef = Ref.makeUnsafe([]);
  const effectsRef = Ref.makeUnsafe([]);
  const projectionStore = {
    getThread: () => Effect.succeed(records.thread),
    getThreadRecords: () => Effect.succeed(records),
    getMessageCount: () => Effect.succeed(0),
    getThreadProviderContext: () => Effect.succeed({ providerSessions: records.providerSessions }),
  };
  const mapDispatchError = (command) => (effect) =>
    Effect.mapError(
      effect,
      (cause) =>
        new OrchestratorDispatchError({
          commandId: command.commandId,
          commandType: command.type,
          cause,
        }),
    );
  const emit = (events) => (event) =>
    Effect.gen(function* () {
      const withId = { ...event, id: `event:${++counter}` };
      yield* Ref.update(events, (existing) => [...existing, withId]);
      return withId;
    });
  const dispatch = make(
    Effect,
    Ref,
    DateTime,
    Option,
    projectionStore,
    mapDispatchError,
    OrchestratorDispatchError,
    emit,
    () => Effect.succeed(records),
    () => Effect.void,
    () => Effect.succeed(records),
  );
  const command = { type: scenario.type, commandId: "command:archive", threadId: "thread:archive" };
  const exit = await Effect.runPromise(
    Effect.exit(
      dispatch(command, eventsRef, effectsRef).pipe(Effect.provideService(Clock.Clock, clock)),
    ),
  );
  const row = { name: scenario.name, command, projection: records };
  if (exit._tag === "Failure") {
    const failure = exit.cause.reasons?.[0]?.error ?? exit.cause.failures?.[0] ?? exit.cause;
    row.refused =
      typeof failure?.cause === "string" ? failure.cause : String(failure?.message ?? failure);
  } else {
    row.events = plain(
      (await Effect.runPromise(Ref.get(eventsRef))).map((event) => ({
        type: event.type,
        runId: event.runId,
        nodeId: event.nodeId,
        payload: event.payload,
      })),
    );
    row.effects = plain(
      (await Effect.runPromise(Ref.get(effectsRef))).map((effect) => ({
        id: effect.id,
        request: effect.request,
      })),
    );
  }
  rows.push(row);
}
const target = new URL("../crates/server/tests/fixtures/thread-archive.jsonl", import.meta.url);
const staging = new URL(
  "../crates/server/tests/fixtures/thread-archive.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${rows.length} rows`);
