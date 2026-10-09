// Runs the unchanged rollback sources: the `checkpoint.rollback` admission in Orchestrator.ts
// (sliced out and given scripted collaborators), CheckpointRestoreSafety, the full
// CheckpointRollbackService over a real CheckpointService with fake stores, the Codex rollback
// turn count and the Codex thread revert. Output is replayed by the Rust port. No git runs here.
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import nodePath from "node:path";
import * as NodeCrypto from "../../apps/server/node_modules/@effect/platform-node/dist/NodeCrypto.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Ref from "../../apps/server/node_modules/effect/dist/Ref.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as FileSystem from "../../apps/server/node_modules/effect/dist/FileSystem.js";
import * as Path from "../../apps/server/node_modules/effect/dist/Path.js";
import { latestProviderTurnForAttempt } from "../../packages/contracts/src/orchestrationV2.ts";
import * as CommandPolicy from "../../apps/server/src/orchestration-v2/CommandPolicy.ts";
import * as CheckpointService from "../../apps/server/src/orchestration-v2/CheckpointService.ts";
import * as CheckpointRollbackService from "../../apps/server/src/orchestration-v2/CheckpointRollbackService.ts";
import * as CheckpointRestoreSafety from "../../apps/server/src/orchestration-v2/CheckpointRestoreSafety.ts";
import * as CheckpointStore from "../../apps/server/src/checkpointing/CheckpointStore.ts";
import * as EventSink from "../../apps/server/src/orchestration-v2/EventSink.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import * as ProjectionStore from "../../apps/server/src/orchestration-v2/ProjectionStore.ts";
import * as ProjectStore from "../../apps/server/src/orchestration-v2/ProjectStore.ts";
import * as ProviderSessionManager from "../../apps/server/src/orchestration-v2/ProviderSessionManager.ts";
import * as RuntimePolicy from "../../apps/server/src/orchestration-v2/RuntimePolicy.ts";
import { resolveCodexRollbackTurnCount } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts";
import { revertCodexThread } from "../../apps/server/src/provider/CodexThreadRevert.ts";

const rows = [];
const run = (effect) => Effect.runPromise(effect);
const text = (value) => (typeof value === "string" ? value : (value?.message ?? String(value)));
const plain = (value) => JSON.parse(JSON.stringify(value));

// ---- admission --------------------------------------------------------------------------
const orchestrator = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
const slice = (from, to) => {
  const begin = orchestrator.indexOf(from);
  const end = orchestrator.indexOf(to, begin);
  if (begin < 0 || end < 0) throw new Error(`Orchestrator source changed near ${from}`);
  return orchestrator.slice(begin, end);
};
const dispatchSource = stripTypeScriptTypes(
  slice(
    "  const dispatchCheckpointRollback = (",
    "  /**\n   * Records a provider rollback that failed",
  ),
  { mode: "strip" },
);
const providerTurnSource = stripTypeScriptTypes(
  slice("function providerTurnForRun(", "function contextSourcePointForRun("),
  { mode: "strip" },
);
const makeDispatch = new Function(
  "Effect",
  "Ref",
  "DateTime",
  "loadProjectionForCommand",
  "providerAdapters",
  "OrchestratorProviderAdapterError",
  "OrchestratorDispatchError",
  "enforceCommandPolicy",
  "commandPolicy",
  "isCheckpointRestoreIsolated",
  "projects",
  "path",
  "fileSystem",
  "projectionStore",
  "emit",
  "SHARED_WORKSPACE_RESTORE_MESSAGE",
  "latestProviderTurnForAttempt",
  `${providerTurnSource}\n${dispatchSource}\nreturn dispatchCheckpointRollback;`,
);

class DispatchError extends Schema.TaggedError()("OrchestratorDispatchError", {
  commandId: Schema.String,
  commandType: Schema.String,
  cause: Schema.optional(Schema.Defect()),
}) {}
const commandPolicy = await run(
  Effect.gen(function* () {
    return yield* CommandPolicy.CommandPolicyV2;
  }).pipe(Effect.provide(CommandPolicy.layer)),
);

const NOW = "2026-04-01T00:00:00.000Z";
const providerThread = (overrides = {}) => ({
  id: "provider-thread:1",
  driver: "codex",
  providerInstanceId: "codex",
  providerSessionId: "session:1",
  appThreadId: "thread:1",
  ownerNodeId: null,
  nativeThreadRef: { driver: "codex", nativeId: "native-1", strength: "strong" },
  nativeConversationHeadRef: null,
  status: "idle",
  firstRunOrdinal: 1,
  lastRunOrdinal: 2,
  handoffIds: [],
  forkedFrom: null,
  createdAt: NOW,
  updatedAt: NOW,
  ...overrides,
});
const thread = (overrides = {}) => ({
  createdBy: "user",
  creationSource: "web",
  id: "thread:1",
  projectId: "project:1",
  title: "T",
  modelSelection: { instanceId: "codex", model: "gpt" },
  providerInstanceId: "codex",
  runtimeMode: "approval-required",
  interactionMode: "default",
  branch: null,
  activeProviderThreadId: "provider-thread:1",
  worktreePath: "/work",
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: "thread:1" },
  forkedFrom: null,
  createdAt: NOW,
  archivedAt: null,
  deletedAt: null,
  updatedAt: NOW,
  ...overrides,
});
const scope = (overrides = {}) => ({
  id: "scope:1",
  threadId: "thread:1",
  runId: "run:2",
  nodeId: "node:2",
  parentScopeId: null,
  providerThreadId: "provider-thread:1",
  kind: "root_run",
  ordinalWithinParent: 0,
  advancesAppRunCount: true,
  cwd: "/work",
  createdAt: NOW,
  ...overrides,
});
const checkpointRow = (ordinal, overrides = {}) => ({
  id: `checkpoint:${ordinal}`,
  threadId: "thread:1",
  scopeId: "scope:1",
  runId: ordinal === 0 ? null : `run:${ordinal}`,
  nodeId: `node:${ordinal || 1}`,
  parentCheckpointId: ordinal === 0 ? null : `checkpoint:${ordinal - 1}`,
  ordinalWithinScope: ordinal,
  appRunOrdinal: ordinal === 0 ? null : ordinal,
  ref: `refs/test/${ordinal}`,
  status: "ready",
  files: [],
  capturedAt: NOW,
  ...overrides,
});
const runRow = (ordinal, status = "completed", overrides = {}) => ({
  id: `run:${ordinal}`,
  threadId: "thread:1",
  ordinal,
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt" },
  providerThreadId: "provider-thread:1",
  userMessageId: `message:${ordinal}`,
  rootNodeId: `node:${ordinal}`,
  activeAttemptId: `attempt:${ordinal}`,
  status,
  requestedAt: NOW,
  startedAt: NOW,
  completedAt: NOW,
  checkpointId: null,
  contextHandoffId: null,
  ...overrides,
});
const attemptRow = (ordinal, overrides = {}) => ({
  id: `attempt:${ordinal}`,
  runId: `run:${ordinal}`,
  attemptOrdinal: 1,
  providerTurnId: `turn:${ordinal}`,
  status: "completed",
  ...overrides,
});
const nodeRow = (ordinal, status = "completed") => ({
  id: `node:${ordinal}`,
  threadId: "thread:1",
  runId: `run:${ordinal}`,
  parentNodeId: null,
  rootNodeId: `node:${ordinal}`,
  kind: "root_turn",
  status,
  countsForRun: true,
  providerThreadId: "provider-thread:1",
  providerTurnId: `turn:${ordinal}`,
  nativeItemRef: null,
  runtimeRequestId: null,
  checkpointScopeId: "scope:1",
  startedAt: NOW,
  completedAt: NOW,
});
const turnRow = (ordinal, overrides = {}) => ({
  id: `turn:${ordinal}`,
  providerThreadId: "provider-thread:1",
  nodeId: `node:${ordinal}`,
  runAttemptId: `attempt:${ordinal}`,
  nativeTurnRef: { driver: "codex", nativeId: `native-turn-${ordinal}`, strength: "strong" },
  ordinal,
  status: "completed",
  startedAt: NOW,
  completedAt: NOW,
  ...overrides,
});
const baseProjection = (overrides = {}) => ({
  thread: thread(),
  providerThreads: [providerThread()],
  providerSessions: [],
  checkpointScopes: [scope()],
  checkpoints: [checkpointRow(0), checkpointRow(1), checkpointRow(2)],
  runs: [runRow(1), runRow(2)],
  attempts: [attemptRow(1), attemptRow(2)],
  nodes: [nodeRow(1), nodeRow(2)],
  providerTurns: [turnRow(1), turnRow(2)],
  ...overrides,
});
const caps = (overrides = {}) => ({
  threads: { canRollbackThread: true, ...(overrides.threads ?? {}) },
  checkpointing: {
    providerCanRollbackConversation: true,
    providerRollbackReturnsSnapshot: true,
    ...(overrides.checkpointing ?? {}),
  },
});

const admission = [];
const admit = (name, projection, command = {}, extras = {}) =>
  admission.push({
    name,
    projection,
    command: {
      type: "checkpoint.rollback",
      commandId: "command:rb",
      threadId: "thread:1",
      scopeId: "scope:1",
      checkpointId: "checkpoint:1",
      ...command,
    },
    capabilities: caps(extras.capabilities),
    isolated: extras.isolated ?? true,
  });
admit("to-first-checkpoint", baseProjection());
admit("to-latest-checkpoint", baseProjection(), { checkpointId: "checkpoint:2" });
admit("to-baseline", baseProjection(), { checkpointId: "checkpoint:0" });
admit(
  "conversation-only-skips-isolation",
  baseProjection(),
  { restoreFiles: false },
  { isolated: "throw" },
);
admit("files-explicitly-restored", baseProjection(), { restoreFiles: true });
admit("not-isolated", baseProjection(), {}, { isolated: false });
admit(
  "no-active-provider-thread",
  baseProjection({ thread: thread({ activeProviderThreadId: null }) }),
);
admit(
  "active-provider-thread-missing",
  baseProjection({ thread: thread({ activeProviderThreadId: "provider-thread:gone" }) }),
);
admit(
  "no-session",
  baseProjection({ providerThreads: [providerThread({ providerSessionId: null })] }),
);
admit(
  "capability-rollback-off",
  baseProjection(),
  {},
  { capabilities: { threads: { canRollbackThread: false } } },
);
admit(
  "capability-conversation-off",
  baseProjection(),
  {},
  { capabilities: { checkpointing: { providerCanRollbackConversation: false } } },
);
admit(
  "capability-snapshot-off",
  baseProjection(),
  {},
  { capabilities: { checkpointing: { providerRollbackReturnsSnapshot: false } } },
);
admit("checkpoint-unknown", baseProjection(), { checkpointId: "checkpoint:nope" });
for (const status of ["missing", "error", "stale"]) {
  admit(
    `checkpoint-${status}`,
    baseProjection({
      checkpoints: [checkpointRow(0), checkpointRow(1, { status }), checkpointRow(2)],
    }),
  );
}
admit("scope-missing", baseProjection({ checkpointScopes: [] }));
admit("scope-mismatch", baseProjection(), { scopeId: "scope:other" });
admit("no-run-for-ordinal", baseProjection({ runs: [runRow(2)] }));
admit(
  "run-without-attempt",
  baseProjection({ runs: [runRow(1, "completed", { activeAttemptId: null }), runRow(2)] }),
);
admit(
  "attempt-without-turn",
  baseProjection({
    providerTurns: [turnRow(2)],
    attempts: [attemptRow(1, { providerTurnId: null }), attemptRow(2)],
  }),
);
admit(
  "turn-via-attempt-record",
  baseProjection({ providerTurns: [turnRow(1, { runAttemptId: null }), turnRow(2)] }),
);
admit(
  "latest-turn-of-attempt-wins",
  baseProjection({
    providerTurns: [
      turnRow(1),
      turnRow(5, {
        id: "turn:1b",
        runAttemptId: "attempt:1",
        providerThreadId: "provider-thread:1",
      }),
      turnRow(2),
    ],
  }),
);
admit(
  "turn-on-another-provider-thread",
  baseProjection({
    providerTurns: [turnRow(1, { providerThreadId: "provider-thread:other" }), turnRow(2)],
  }),
);

const admissionRows = [];
for (const scenario of admission) {
  const events = await run(Ref.make([]));
  const projection = scenario.projection;
  const isolation = (_thread, _scope) => {
    if (scenario.isolated === "throw") throw new Error("isolation consulted");
    return Effect.succeed(scenario.isolated);
  };
  const dispatch = makeDispatch(
    Effect,
    Ref,
    DateTime,
    () => Effect.succeed(projection),
    { get: () => Effect.succeed({ getCapabilities: () => Effect.succeed(scenario.capabilities) }) },
    class {},
    DispatchError,
    (command) => (effect) =>
      effect.pipe(
        Effect.mapError(
          (cause) =>
            new DispatchError({ commandId: command.commandId, commandType: command.type, cause }),
        ),
      ),
    commandPolicy,
    isolation,
    {},
    { relative: nodePath.relative, isAbsolute: nodePath.isAbsolute, sep: nodePath.sep },
    {},
    {},
    (eventsRef, _command) => (event) => Ref.update(eventsRef, (existing) => [...existing, event]),
    CheckpointRestoreSafety.SHARED_WORKSPACE_RESTORE_MESSAGE,
    latestProviderTurnForAttempt,
  );
  const effects = await run(Ref.make([]));
  const outcome = await run(dispatch(scenario.command, events, effects).pipe(Effect.result));
  let output;
  if (outcome._tag === "Failure") {
    output = { error: text(outcome.failure.cause) };
  } else {
    const written = plain(await run(Ref.get(events)));
    const queued = plain(await run(Ref.get(effects)));
    const stamp = written[0].payload.updatedAt;
    output = JSON.parse(
      JSON.stringify(
        {
          events: written.map((event) => ({ type: event.type, payload: event.payload })),
          effects: queued,
        },
        (_key, value) => (value === stamp ? "NOW" : value),
      ),
    );
  }
  admissionRows.push({
    operation: "admission",
    name: scenario.name,
    projection,
    command: scenario.command,
    capabilities: scenario.capabilities,
    isolated: scenario.isolated,
    output,
  });
}
rows.push(...admissionRows);

// ---- restore safety ---------------------------------------------------------------------
const isolationRows = [];
const neighbour = (id, overrides = {}) => ({
  thread: {
    id,
    projectId: "project:other",
    worktreePath: null,
    deletedAt: null,
    ...(overrides.thread ?? {}),
  },
  checkpointScopes: overrides.scopes ?? [],
  providerSessions: overrides.sessions ?? [],
});
const session = (cwd, overrides = {}) => ({
  status: "ready",
  cwd,
  capabilities: { sessions: { supportsMultipleProviderThreadsPerSession: false } },
  ...overrides,
});
const roots = { "project:other": "/projects/other", "project:inside": "/work/nested" };
const isolationCases = [
  ["lone-worktree", {}, []],
  ["no-worktree", { worktree: null }, []],
  ["scope-elsewhere", { scopeCwd: "/elsewhere" }, []],
  ["link-resolves-equal", { scopeCwd: "/link", realPaths: { "/link": "/work" } }, []],
  ["neighbour-same-worktree", {}, [neighbour("n", { thread: { worktreePath: "/work" } })]],
  ["neighbour-inside", {}, [neighbour("n", { thread: { worktreePath: "/work/sub" } })]],
  ["neighbour-parent", {}, [neighbour("n", { thread: { worktreePath: "/" } })]],
  ["neighbour-prefix-sibling", {}, [neighbour("n", { thread: { worktreePath: "/workshop" } })]],
  ["neighbour-sibling", {}, [neighbour("n", { thread: { worktreePath: "/other" } })]],
  [
    "neighbour-project-root-overlaps",
    {},
    [neighbour("n", { thread: { projectId: "project:inside" } })],
  ],
  ["neighbour-project-root-apart", {}, [neighbour("n")]],
  ["neighbour-project-missing", {}, [neighbour("n", { thread: { projectId: "project:gone" } })]],
  [
    "neighbour-deleted",
    {},
    [neighbour("n", { thread: { worktreePath: "/work", deletedAt: NOW } })],
  ],
  [
    "neighbour-scope-overlaps",
    {},
    [neighbour("n", { thread: { worktreePath: "/other" }, scopes: [{ cwd: "/work/deep" }] })],
  ],
  [
    "neighbour-live-session-overlaps",
    {},
    [neighbour("n", { thread: { worktreePath: "/other" }, sessions: [session("/work")] })],
  ],
  [
    "neighbour-stopped-session-ignored",
    {},
    [
      neighbour("n", {
        thread: { worktreePath: "/other" },
        sessions: [session("/work", { status: "stopped" })],
      }),
    ],
  ],
  [
    "neighbour-shared-session-ignored",
    {},
    [
      neighbour("n", {
        thread: { worktreePath: "/other" },
        sessions: [
          session("/work", {
            capabilities: { sessions: { supportsMultipleProviderThreadsPerSession: true } },
          }),
        ],
      }),
    ],
  ],
  [
    "neighbour-path-vanished",
    { missing: ["/gone"] },
    [neighbour("n", { thread: { worktreePath: "/gone" } })],
  ],
  [
    "neighbour-path-unreadable",
    { denied: ["/locked"] },
    [neighbour("n", { thread: { worktreePath: "/locked" } })],
  ],
  [
    "neighbour-symlink-into-worktree",
    { realPaths: { "/shortcut": "/work/inner" } },
    [neighbour("n", { thread: { worktreePath: "/shortcut" } })],
  ],
  ["own-path-vanished", { missing: ["/work"] }, []],
  [
    "second-neighbour-decides",
    {},
    [
      neighbour("a", { thread: { worktreePath: "/other" } }),
      neighbour("b", { thread: { worktreePath: "/work" } }),
    ],
  ],
];
for (const [name, options, neighbours] of isolationCases) {
  const worktreePath = options.worktree === undefined ? "/work" : options.worktree;
  const subject = thread({ worktreePath });
  const scopeCwd = options.scopeCwd ?? "/work";
  const realPaths = options.realPaths ?? {};
  const fileSystem = {
    realPath: (path) => {
      if (options.missing?.includes(path)) return Effect.fail({ reason: { _tag: "NotFound" } });
      if (options.denied?.includes(path))
        return Effect.fail({ reason: { _tag: "PermissionDenied" } });
      return Effect.succeed(realPaths[path] ?? path);
    },
  };
  const projections = {
    getShellSnapshot: () =>
      Effect.succeed({ threads: neighbours.map((entry) => entry.thread), archivedThreads: [] }),
    getCheckpointContext: (id) =>
      Effect.succeed({
        checkpointScopes: neighbours.find((entry) => entry.thread.id === id).checkpointScopes,
      }),
    getThreadProviderContext: (id) =>
      Effect.succeed({
        providerSessions: neighbours.find((entry) => entry.thread.id === id).providerSessions,
      }),
  };
  const projects = {
    get: (projectId) =>
      Effect.succeed(
        roots[projectId] === undefined
          ? Option.none()
          : Option.some({ workspaceRoot: roots[projectId] }),
      ),
  };
  const outcome = await run(
    CheckpointRestoreSafety.isCheckpointRestoreIsolated(
      subject,
      { cwd: scopeCwd },
      {
        fileSystem,
        projections,
        projects,
        path: { relative: nodePath.relative, isAbsolute: nodePath.isAbsolute, sep: nodePath.sep },
      },
    ).pipe(Effect.result),
  );
  isolationRows.push({
    operation: "isolation",
    name,
    thread: subject,
    scopeCwd,
    realPaths,
    missing: options.missing ?? [],
    denied: options.denied ?? [],
    roots,
    neighbours,
    output: outcome._tag === "Success" ? { isolated: outcome.success } : { error: true },
  });
}
rows.push(...isolationRows);

// ---- executor ---------------------------------------------------------------------------
const executorRows = [];
const requestId = {
  threadId: "thread:1",
  providerThreadId: "provider-thread:1",
  checkpointId: "checkpoint:1",
  scopeId: "scope:1",
};
const execute = (name, projection, overrides = {}, behaviour = {}) =>
  executorRows.push({ name, projection, request: { ...requestId, ...overrides }, behaviour });
const afterTarget = (statuses) =>
  baseProjection({
    runs: [runRow(1), ...statuses.map((status, index) => runRow(index + 2, status))],
    attempts: [attemptRow(1), ...statuses.map((_status, index) => attemptRow(index + 2))],
    nodes: [nodeRow(1), ...statuses.map((status, index) => nodeRow(index + 2, status))],
    providerTurns: [
      turnRow(1),
      ...statuses.map((status, index) =>
        turnRow(index + 2, { status: status === "rolled_back" ? "completed" : status }),
      ),
    ],
    checkpoints: [
      checkpointRow(0),
      checkpointRow(1),
      ...statuses.map((_status, index) => checkpointRow(index + 2)),
    ],
  });
execute("latest-no-provider", baseProjection(), { checkpointId: "checkpoint:2" });
execute("rewind-one-turn", baseProjection());
execute("rewind-to-baseline", baseProjection(), { checkpointId: "checkpoint:0" });
execute("files-skipped", baseProjection(), { restoreFiles: false });
execute(
  "later-run-statuses",
  afterTarget(["completed", "failed", "interrupted", "cancelled", "rolled_back", "running"]),
);
execute(
  "only-ready-later-checkpoints-go-stale",
  baseProjection({
    checkpoints: [
      checkpointRow(0),
      checkpointRow(1),
      checkpointRow(2, { status: "missing" }),
      checkpointRow(3, { status: "ready" }),
      checkpointRow(4, { scopeId: "scope:other" }),
    ],
  }),
);
execute(
  "rolled-back-turns-dropped-from-conversation",
  baseProjection({
    runs: [runRow(1), runRow(2, "rolled_back"), runRow(3)],
    attempts: [attemptRow(1), attemptRow(2), attemptRow(3)],
    providerTurns: [turnRow(1), turnRow(2), turnRow(3)],
    nodes: [nodeRow(1), nodeRow(2, "rolled_back"), nodeRow(3)],
    checkpoints: [checkpointRow(0), checkpointRow(1), checkpointRow(3)],
  }),
);
execute(
  "turns-with-no-attempt-kept",
  baseProjection({ providerTurns: [turnRow(1), turnRow(2), turnRow(9, { runAttemptId: null })] }),
);
execute(
  "restore-refuses-stale-record",
  baseProjection({ checkpoints: [checkpointRow(0), checkpointRow(1, { status: "stale" })] }),
);
execute("unknown-provider-thread", baseProjection(), { providerThreadId: "provider-thread:gone" });
execute("unknown-checkpoint", baseProjection(), { checkpointId: "checkpoint:gone" });
execute("unknown-scope", baseProjection(), { scopeId: "scope:gone" });
execute(
  "checkpoint-of-another-scope",
  baseProjection({
    checkpointScopes: [scope(), scope({ id: "scope:2" })],
    checkpoints: [checkpointRow(0), checkpointRow(1, { scopeId: "scope:2" })],
  }),
  { scopeId: "scope:1" },
);
execute(
  "no-session",
  baseProjection({ providerThreads: [providerThread({ providerSessionId: null })] }),
);
execute(
  "another-provider-thread-is-active",
  baseProjection({ thread: thread({ activeProviderThreadId: "provider-thread:2" }) }),
);
execute(
  "provider-instance-changed",
  baseProjection({ thread: thread({ modelSelection: { instanceId: "claude", model: "m" } }) }),
);
execute("thread-has-no-worktree", baseProjection({ thread: thread({ worktreePath: null }) }));
execute(
  "no-worktree-but-files-skipped",
  baseProjection({ thread: thread({ worktreePath: null }) }),
  { restoreFiles: false },
);
execute("target-turn-missing", baseProjection({ providerTurns: [turnRow(2)] }));
execute(
  "target-turn-on-another-provider-thread",
  baseProjection({
    providerTurns: [turnRow(1, { providerThreadId: "provider-thread:other" }), turnRow(2)],
  }),
);
execute("restore-finds-no-ref", baseProjection(), {}, { restored: false });

for (const scenario of executorRows) {
  const calls = [];
  const written = [];
  const projection = scenario.projection;
  const store = Layer.mock(CheckpointStore.CheckpointStore)({
    isGitRepository: () => Effect.succeed(true),
    restoreCheckpoint: ({ checkpointRef }) => {
      calls.push({ restore: checkpointRef });
      return Effect.succeed(scenario.behaviour.restored ?? true);
    },
    deleteCheckpointRefs: ({ checkpointRefs }) => {
      calls.push({ delete: checkpointRefs });
      return Effect.void;
    },
  });
  const sessions = Layer.mock(ProviderSessionManager.ProviderSessionManagerV2)({
    open: () =>
      Effect.succeed({
        rollbackThread: (input) => {
          calls.push({
            rollback: {
              target: plain(input.target),
              turns: input.providerThreadTurns.map((turn) => turn.id),
            },
          });
          return Effect.succeed({
            providerThread: {
              ...input.providerThread,
              nativeConversationHeadRef: {
                driver: "codex",
                nativeId: "rewound",
                strength: "strong",
              },
            },
          });
        },
      }),
  });
  const layer = CheckpointRollbackService.layer.pipe(
    Layer.provide(
      Layer.mergeAll(
        CheckpointService.layer.pipe(Layer.provide(store)),
        Layer.mock(EventSink.EventSinkV2)({
          write: ({ events }) =>
            Effect.sync(() => {
              written.push(...events);
            }),
        }),
        Layer.mock(ProjectionStore.ProjectionStoreV2)({
          getThreadRecords: () => Effect.succeed(projection),
          getShellSnapshot: () => Effect.succeed({ threads: [], archivedThreads: [] }),
        }),
        sessions,
        Layer.mock(RuntimePolicy.RuntimePolicyV2)({ resolve: () => Effect.succeed({}) }),
        Layer.mock(FileSystem.FileSystem)({ realPath: (path) => Effect.succeed(path) }),
        Layer.succeed(
          Path.Path,
          Path.Path.of({
            relative: nodePath.relative,
            isAbsolute: nodePath.isAbsolute,
            sep: nodePath.sep,
          }),
        ),
        Layer.mock(ProjectStore.ProjectStoreV2)({ get: () => Effect.succeed(Option.none()) }),
      ),
    ),
    Layer.provideMerge(Layer.mergeAll(NodeCrypto.layer, IdAllocator.layer)),
  );
  const outcome = await run(
    Effect.gen(function* () {
      const service = yield* CheckpointRollbackService.CheckpointRollbackServiceV2;
      return yield* service.execute(scenario.request).pipe(Effect.result);
    }).pipe(Effect.provide(layer)),
  );
  let output;
  if (outcome._tag === "Failure") {
    output = { error: { reason: outcome.failure.reason, message: outcome.failure.message } };
  } else {
    const events = plain(written);
    const stamp = events[0].payload.updatedAt;
    output = JSON.parse(
      JSON.stringify(
        { events: events.map((event) => ({ type: event.type, payload: event.payload })), calls },
        (_key, value) => (value === stamp ? "NOW" : value),
      ),
    );
  }
  rows.push({
    operation: "execute",
    ...scenario,
    output:
      scenario.behaviour.restored === false || outcome._tag === "Success"
        ? output
        : { ...output, calls },
  });
}

// ---- Codex ------------------------------------------------------------------------------
const codexTurn = (ordinal, overrides = {}) => turnRow(ordinal, overrides);
const countCases = [
  [
    "start-all-terminal",
    { type: "thread_start", checkpointId: "c", appRunOrdinal: 0 },
    [1, 2, 3].map((n) => codexTurn(n)),
  ],
  [
    "start-skips-running-and-unreferenced",
    { type: "thread_start", checkpointId: "c", appRunOrdinal: 0 },
    [
      codexTurn(1),
      codexTurn(2, { status: "running" }),
      codexTurn(3, { nativeTurnRef: null }),
      codexTurn(4, { status: "failed" }),
      codexTurn(5, { status: "interrupted" }),
      codexTurn(6, { status: "cancelled" }),
    ],
  ],
  ["start-no-turns", { type: "thread_start", checkpointId: "c", appRunOrdinal: 0 }, []],
  [
    "after-first",
    { type: "provider_turn", checkpointId: "c", appRunOrdinal: 1, providerTurn: codexTurn(1) },
    [1, 2, 3].map((n) => codexTurn(n)),
  ],
  [
    "after-last",
    { type: "provider_turn", checkpointId: "c", appRunOrdinal: 3, providerTurn: codexTurn(3) },
    [1, 2, 3].map((n) => codexTurn(n)),
  ],
  [
    "after-with-gaps",
    { type: "provider_turn", checkpointId: "c", appRunOrdinal: 1, providerTurn: codexTurn(1) },
    [
      codexTurn(1),
      codexTurn(4, { status: "running" }),
      codexTurn(7),
      codexTurn(9, { nativeTurnRef: null }),
    ],
  ],
  [
    "target-missing",
    { type: "provider_turn", checkpointId: "c", appRunOrdinal: 5, providerTurn: codexTurn(5) },
    [1, 2].map((n) => codexTurn(n)),
  ],
  [
    "target-on-other-thread",
    {
      type: "provider_turn",
      checkpointId: "c",
      appRunOrdinal: 1,
      providerTurn: codexTurn(1, { providerThreadId: "provider-thread:other" }),
    },
    [codexTurn(1), codexTurn(2)],
  ],
];
for (const [name, target, turns] of countCases) {
  const outcome = await run(
    resolveCodexRollbackTurnCount({
      providerThread: providerThread(),
      target,
      providerThreadTurns: turns,
    }).pipe(Effect.result),
  );
  rows.push({
    operation: "codexTurnCount",
    name,
    target,
    providerThreadTurns: turns,
    providerThread: providerThread(),
    output:
      outcome._tag === "Success"
        ? { count: outcome.success }
        : { error: text(outcome.failure.cause) },
  });
}
const revertCases = [
  ["one-turn", 1, [{ data: [{ id: "t3" }, { id: "t2" }], nextCursor: null }]],
  [
    "three-turns-one-page",
    3,
    [{ data: [{ id: "t5" }, { id: "t4" }, { id: "t3" }, { id: "t2" }], nextCursor: null }],
  ],
  [
    "pages",
    3,
    [
      { data: [{ id: "t5" }], nextCursor: "c1" },
      { data: [{ id: "t4" }, { id: "t3" }], nextCursor: null },
    ],
  ],
  ["more-than-history", 4, [{ data: [{ id: "t2" }, { id: "t1" }], nextCursor: null }]],
  ["empty-history", 2, [{ data: [], nextCursor: null }]],
  [
    "repeated-cursor",
    5,
    [
      { data: [{ id: "t5" }], nextCursor: null },
      { data: [], nextCursor: null },
    ],
    "repeat",
  ],
  [
    "limit-capped",
    250,
    [
      { data: Array.from({ length: 100 }, (_v, i) => ({ id: `a${i}` })), nextCursor: "c1" },
      { data: Array.from({ length: 100 }, (_v, i) => ({ id: `b${i}` })), nextCursor: "c2" },
      { data: Array.from({ length: 50 }, (_v, i) => ({ id: `c${i}` })), nextCursor: null },
    ],
  ],
];
for (const [name, turns, pages, mode] of revertCases) {
  const calls = [];
  let page = 0;
  const client = {
    request: (method, params) => {
      calls.push({ method, params: plain(params) });
      if (method === "thread/turns/list") {
        const response =
          mode === "repeat"
            ? { data: [{ id: "t5" }], nextCursor: params.cursor === null ? "same" : "same" }
            : pages[page++];
        return Effect.succeed(response);
      }
      return Effect.succeed({ thread: { id: "native-1" } });
    },
  };
  const outcome = await run(revertCodexThread(client, "native-1", turns).pipe(Effect.result));
  rows.push({
    operation: "codexRevert",
    name,
    turns,
    pages,
    mode: mode ?? null,
    calls,
    output:
      outcome._tag === "Success"
        ? { ok: true }
        : { error: text(outcome.failure.message ?? outcome.failure) },
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/checkpoint-rollback.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(
  JSON.stringify({
    admission: admissionRows.length,
    isolation: isolationRows.length,
    execute: executorRows.length,
    cases: rows.length,
  }),
);
