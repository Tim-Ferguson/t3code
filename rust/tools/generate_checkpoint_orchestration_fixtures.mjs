// Runs the unchanged orchestration-v2 checkpoint sources (ref and id derivation, the diff query,
// the capture service over the real checkpoint service) against scripted stores, and records
// inputs and outputs for the Rust port to replay. No git runs here: stores are fakes.
import { renameSync, writeFileSync } from "node:fs";
import * as NodeCrypto from "../../apps/server/node_modules/@effect/platform-node/dist/NodeCrypto.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Ref from "../../apps/server/node_modules/effect/dist/Ref.js";
import {
  OrchestrationGetFullThreadDiffError,
  OrchestrationGetTurnDiffError,
} from "../../packages/contracts/src/checkpointDiff.ts";
import { VcsUnsupportedOperationError } from "../../packages/contracts/src/vcs.ts";
import * as CheckpointDiffQuery from "../../apps/server/src/checkpointing/CheckpointDiffQuery.ts";
import * as CheckpointStore from "../../apps/server/src/checkpointing/CheckpointStore.ts";
import * as CheckpointService from "../../apps/server/src/orchestration-v2/CheckpointService.ts";
import * as CheckpointCaptureService from "../../apps/server/src/orchestration-v2/CheckpointCaptureService.ts";
import * as EventSink from "../../apps/server/src/orchestration-v2/EventSink.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import * as ProjectionStore from "../../apps/server/src/orchestration-v2/ProjectionStore.ts";
import * as ThreadManagement from "../../apps/server/src/orchestration-v2/ThreadManagementService.ts";

const rows = [];
const run = (effect) => Effect.runPromise(effect);

// ---- ids and refs ----------------------------------------------------------------------
const scopeIds = [
  "scope:a",
  "checkpoint-scope:thread:thread%3A1:name:root",
  "scope with spaces/ü",
  "s".repeat(80),
];
for (const scopeId of scopeIds) {
  for (const ordinal of [0, 1, 7, 120]) {
    const output = await run(
      Effect.gen(function* () {
        const ids = yield* IdAllocator.IdAllocatorV2;
        return {
          ref: yield* CheckpointService.checkpointRefForScopeOrdinal({
            scopeId,
            ordinalWithinScope: ordinal,
          }),
          checkpointId: yield* ids.allocate.checkpoint({
            checkpointScopeId: scopeId,
            name: String(ordinal),
          }),
        };
      }).pipe(Effect.provide(Layer.mergeAll(NodeCrypto.layer, IdAllocator.layer))),
    );
    rows.push({ operation: "ids", scopeId, ordinal, output });
  }
}
for (const threadId of ["thread:1", "thread 2/é"]) {
  const scopeId = await run(
    Effect.gen(function* () {
      const ids = yield* IdAllocator.IdAllocatorV2;
      return yield* ids.allocate.checkpointScope({ threadId, name: "root" });
    }).pipe(Effect.provide(IdAllocator.layer)),
  );
  rows.push({ operation: "rootScope", threadId, output: scopeId });
}

// ---- diff query ------------------------------------------------------------------------
const threadId = "thread:diff";
const refFor = (scopeId, ordinal) =>
  run(
    CheckpointService.checkpointRefForScopeOrdinal({ scopeId, ordinalWithinScope: ordinal }).pipe(
      Effect.provide(NodeCrypto.layer),
    ),
  );
const scopeA = "scope:diff:a";
const scopeB = "scope:diff:b";
const checkpoint = async (scopeId, runId, ordinal, status = "ready") => ({
  scopeId,
  runId,
  appRunOrdinal: ordinal,
  status,
  ref: await refFor(scopeId, ordinal),
});
const completedRuns = [1, 2, 3].map((ordinal) => ({
  id: `run:${ordinal}`,
  ordinal,
  status: "completed",
}));
const rootScope = (id, runId, cwd = "/repo") => ({ id, runId, kind: "root_run", cwd });
const projections = {
  healthy: {
    runs: completedRuns,
    checkpointScopes: [rootScope(scopeA, "run:3")],
    checkpoints: [
      await checkpoint(scopeA, "run:1", 1),
      await checkpoint(scopeA, "run:2", 2),
      await checkpoint(scopeA, "run:3", 3),
    ],
  },
  "failed-middle-run": {
    runs: [completedRuns[0], { id: "run:2", ordinal: 2, status: "failed" }, completedRuns[2]],
    checkpointScopes: [rootScope(scopeA, "run:3")],
    checkpoints: [
      await checkpoint(scopeA, "run:1", 1),
      await checkpoint(scopeA, "run:2", 2),
      await checkpoint(scopeA, "run:3", 3),
    ],
  },
  "stale-and-missing": {
    runs: completedRuns,
    checkpointScopes: [rootScope(scopeA, "run:3")],
    checkpoints: [
      await checkpoint(scopeA, "run:1", 1, "stale"),
      await checkpoint(scopeA, "run:2", 2, "missing"),
      await checkpoint(scopeA, "run:3", 3),
    ],
  },
  "two-scopes": {
    runs: completedRuns,
    // Listed in the order the SQL projection returns scopes: by id.
    checkpointScopes: [rootScope(scopeA, "run:1"), rootScope(scopeB, "run:3", "/second")],
    checkpoints: [
      await checkpoint(scopeA, "run:1", 1),
      await checkpoint(scopeB, "run:2", 2),
      await checkpoint(scopeB, "run:3", 3),
    ],
  },
  "no-scope": {
    runs: completedRuns,
    checkpointScopes: [],
    checkpoints: [await checkpoint(scopeA, "run:1", 1), await checkpoint(scopeA, "run:3", 3)],
  },
  "non-root-first": {
    runs: completedRuns,
    checkpointScopes: [
      { id: "scope:tool", runId: "run:1", kind: "tool", cwd: "/tool" },
      rootScope(scopeA, "run:3"),
    ],
    checkpoints: [
      await checkpoint(scopeA, "run:1", 1),
      await checkpoint("scope:tool", "run:2", 2),
      await checkpoint(scopeA, "run:3", 3),
    ],
  },
  "baseline-only": { runs: [], checkpointScopes: [rootScope(scopeA, "run:1")], checkpoints: [] },
  "null-run-checkpoint": {
    runs: completedRuns,
    checkpointScopes: [rootScope(scopeA, "run:3")],
    checkpoints: [
      { ...(await checkpoint(scopeA, "run:1", 1)), runId: null },
      await checkpoint(scopeA, "run:2", 2),
    ],
  },
};

const encodeCause = (error) => ({
  name: error.name,
  message: error.message,
});
const diffRows = [];
const failures = [false, true];
for (const [name, projection] of Object.entries(projections)) {
  for (const failDiff of failures) {
    for (const missingThread of name === "healthy" && !failDiff ? [false, true] : [false]) {
      const calls = [];
      const layer = CheckpointDiffQuery.layer.pipe(
        Layer.provide(
          Layer.mergeAll(
            Layer.mock(ThreadManagement.ThreadManagementService)({
              getCheckpointContext: () =>
                missingThread
                  ? Effect.fail({ _tag: "OrchestratorProjectionError", threadId })
                  : Effect.succeed(projection),
            }),
            Layer.mock(CheckpointStore.CheckpointStore)({
              diffCheckpoints: (input) => {
                calls.push(input);
                return failDiff
                  ? Effect.fail(
                      new VcsUnsupportedOperationError({
                        operation: "CheckpointStore.diffCheckpoints",
                        kind: "unknown",
                        detail: "No supported VCS repository was detected.",
                      }),
                    )
                  : Effect.succeed(
                      `diff ${input.fromCheckpointRef.split("/").slice(-1)[0]}..${input.toCheckpointRef.split("/").slice(-1)[0]} ws=${input.ignoreWhitespace}`,
                    );
              },
            }),
          ),
        ),
        Layer.provideMerge(NodeCrypto.layer),
      );
      const invoke = (kind, input) => {
        calls.length = 0;
        return run(
          Effect.gen(function* () {
            const query = yield* CheckpointDiffQuery.CheckpointDiffQuery;
            const exit = yield* (
              kind === "turn" ? query.getTurnDiff(input) : query.getFullThreadDiff(input)
            ).pipe(Effect.result);
            if (exit._tag === "Success") return { ok: exit.success };
            const error = exit.failure;
            const Wire =
              kind === "turn" ? OrchestrationGetTurnDiffError : OrchestrationGetFullThreadDiffError;
            const wrapped = new Wire({
              message:
                kind === "turn" ? "Failed to load turn diff" : "Failed to load full thread diff",
              cause: error,
            });
            const wire = Schema.encodeSync(Wire)(wrapped);
            return {
              error: {
                tag: error._tag ?? error.name,
                message: error.message,
                cause: encodeCause(error),
              },
              wire,
            };
          }).pipe(Effect.provide(layer)),
        ).then((output) => ({ ...output, calls: JSON.parse(JSON.stringify(calls)) }));
      };
      for (const [from, to] of [
        [0, 0],
        [0, 1],
        [0, 2],
        [0, 3],
        [1, 2],
        [1, 3],
        [2, 3],
        [2, 2],
        [0, 4],
        [3, 4],
        [1, 1],
      ]) {
        for (const ignoreWhitespace of [undefined, true, false]) {
          const input = {
            threadId,
            fromTurnCount: from,
            toTurnCount: to,
            ...(ignoreWhitespace === undefined ? {} : { ignoreWhitespace }),
          };
          diffRows.push({
            operation: "turnDiff",
            projection: name,
            failDiff,
            missingThread,
            input,
            ...(await invoke("turn", input)),
          });
        }
      }
      for (const to of [0, 1, 2, 3, 4]) {
        for (const ignoreWhitespace of [undefined, false]) {
          const input = {
            threadId,
            toTurnCount: to,
            ...(ignoreWhitespace === undefined ? {} : { ignoreWhitespace }),
          };
          diffRows.push({
            operation: "fullDiff",
            projection: name,
            failDiff,
            missingThread,
            input,
            ...(await invoke("full", input)),
          });
        }
      }
    }
  }
}

// ---- capture ---------------------------------------------------------------------------
const nowIso = () => DateTime.formatIso(DateTime.nowUnsafe());
const baseTime = DateTime.makeUnsafe("2026-03-01T10:00:00.000Z");
const captureThread = "thread:capture";
const captureScopeId = "checkpoint-scope:thread:thread%3Acapture:name:root";
const scopeFixture = {
  id: captureScopeId,
  threadId: captureThread,
  runId: "run:2",
  nodeId: "node:root:2",
  parentScopeId: null,
  providerThreadId: "provider-thread:1",
  kind: "root_run",
  ordinalWithinParent: 0,
  advancesAppRunCount: true,
  cwd: "/repo",
  createdAt: baseTime,
};
const providerThreadFixture = {
  id: "provider-thread:1",
  driver: "codex",
  providerInstanceId: "codex",
  providerSessionId: null,
  appThreadId: captureThread,
  ownerNodeId: null,
  nativeThreadRef: null,
  nativeConversationHeadRef: null,
  status: "idle",
  firstRunOrdinal: 1,
  lastRunOrdinal: 2,
  handoffIds: [],
  forkedFrom: null,
  createdAt: baseTime,
  updatedAt: baseTime,
};
const runFixture = (ordinal, status, overrides = {}) => ({
  id: `run:${ordinal}`,
  threadId: captureThread,
  ordinal,
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt" },
  providerThreadId: "provider-thread:1",
  userMessageId: `message:${ordinal}`,
  rootNodeId: `node:root:${ordinal}`,
  activeAttemptId: `attempt:${ordinal}`,
  status,
  requestedAt: baseTime,
  startedAt: baseTime,
  completedAt: null,
  checkpointId: null,
  contextHandoffId: null,
  ...overrides,
});
const nodeFixture = (ordinal, status, overrides = {}) => ({
  id: `node:root:${ordinal}`,
  threadId: captureThread,
  runId: `run:${ordinal}`,
  parentNodeId: null,
  rootNodeId: `node:root:${ordinal}`,
  kind: "root_turn",
  status,
  countsForRun: true,
  providerThreadId: "provider-thread:1",
  providerTurnId: `provider-turn:${ordinal}`,
  nativeItemRef: null,
  runtimeRequestId: null,
  checkpointScopeId: captureScopeId,
  startedAt: baseTime,
  completedAt: null,
  ...overrides,
});
const numstatZ = (...files) =>
  files.map(([additions, deletions, path]) => `${additions}\t${deletions}\t${path}\0`).join("");
const captureScenarios = [];
const add = (name, context, store = {}) =>
  captureScenarios.push({ name, context, store: { repository: true, existing: [], ...store } });
const waiting = (ordinal, ready) => ({
  run: runFixture(ordinal, "waiting"),
  rootNode: nodeFixture(ordinal, "waiting"),
  scope: scopeFixture,
  providerThread: providerThreadFixture,
  readyCheckpointOrdinals: ready,
});
add("first-turn", waiting(1, []), { numstat: numstatZ([1, 0, "a.txt"]) });
add("first-turn-baseline-ref-present", waiting(1, []), {
  existing: [0],
  numstat: numstatZ([2, 1, "src/b.rs"], [0, 5, "a.txt"]),
});
add("second-turn-no-baselines-recorded", waiting(2, []), {
  existing: [0, 1],
  numstat: numstatZ([3, 3, "z.txt"], [1, 1, "a.txt"]),
});
add("second-turn-baselines-recorded", waiting(2, [0, 1]), {
  existing: [0, 1],
  numstat: numstatZ([1, 1, "a.txt"]),
});
add("second-turn-only-start-recorded", waiting(2, [0]), { existing: [0] });
add("second-turn-only-previous-recorded", waiting(2, [1]), { existing: [1] });
add("third-turn-previous-ref-missing", waiting(3, [0]), { existing: [0] });
add("not-a-repository", waiting(2, []), { repository: false });
add("capture-fails", waiting(2, []), { existing: [0, 1], captureFails: true });
add("ref-lookup-fails", waiting(2, []), { existing: [0, 1], hasRefFails: true });
add("diff-summary-fails", waiting(2, [0, 1]), { existing: [0, 1], diffFails: true });
add("binary-and-rename", waiting(2, [0, 1]), {
  existing: [0, 1],
  numstat: numstatZ(["-", "-", "image.png"], [4, 0, "dir/new name.txt"]),
});
for (const status of ["interrupted", "cancelled"]) {
  add(
    `stopped-${status}`,
    {
      run: runFixture(2, status, { completedAt: baseTime }),
      rootNode: nodeFixture(2, status, { completedAt: baseTime }),
      scope: scopeFixture,
      providerThread: providerThreadFixture,
      readyCheckpointOrdinals: [0, 1],
    },
    { existing: [0, 1], numstat: numstatZ([1, 1, "a.txt"]) },
  );
}
add("already-completed", {
  ...waiting(2, [0, 1]),
  run: runFixture(2, "completed", { checkpointId: "checkpoint:x", completedAt: baseTime }),
});
add("already-stopped-with-checkpoint", {
  ...waiting(2, [0, 1]),
  run: runFixture(2, "interrupted", { checkpointId: "checkpoint:x" }),
});
add("rolled-back", {
  ...waiting(2, [0, 1]),
  run: runFixture(2, "rolled_back", { completedAt: baseTime }),
});
add("completed-without-checkpoint", {
  ...waiting(2, [0, 1]),
  run: runFixture(2, "completed", { completedAt: baseTime }),
});
add("running", { ...waiting(2, [0, 1]), run: runFixture(2, "running") });
add("no-run", { ...waiting(2, [0, 1]), run: undefined });
add("no-root-node", { ...waiting(2, [0, 1]), rootNode: undefined });
add("no-scope", { ...waiting(2, [0, 1]), scope: undefined });
add("no-provider-thread", { ...waiting(2, [0, 1]), providerThread: undefined });
add("node-scope-mismatch", {
  ...waiting(2, [0, 1]),
  rootNode: nodeFixture(2, "waiting", { checkpointScopeId: "scope:other" }),
});
add(
  "delegated-completion-kept-out",
  {
    ...waiting(2, [0, 1]),
    run: runFixture(2, "waiting", {
      delegatedCompletion: { disposition: "open", nextGeneration: 1, delivery: null },
    }),
  },
  { existing: [0, 1] },
);

for (const scenario of captureScenarios) {
  const store = scenario.store;
  const refs = await Promise.all([0, 1, 2, 3, 4].map((ordinal) => refFor(captureScopeId, ordinal)));
  const present = new Set(store.existing.map((ordinal) => refs[ordinal]));
  const commits = [];
  const calls = [];
  const layer = CheckpointCaptureService.layer.pipe(
    Layer.provide(
      Layer.mergeAll(
        CheckpointService.layer.pipe(
          Layer.provide(
            Layer.mock(CheckpointStore.CheckpointStore)({
              isGitRepository: () => Effect.succeed(store.repository),
              hasCheckpointRef: ({ checkpointRef }) => {
                calls.push(`has ${refs.indexOf(checkpointRef)}`);
                return store.hasRefFails
                  ? Effect.fail(
                      new VcsUnsupportedOperationError({
                        operation: "x",
                        kind: "git",
                        detail: "has",
                      }),
                    )
                  : Effect.succeed(present.has(checkpointRef));
              },
              captureCheckpoint: ({ checkpointRef }) => {
                calls.push(`capture ${refs.indexOf(checkpointRef)}`);
                if (store.captureFails)
                  return Effect.fail(
                    new VcsUnsupportedOperationError({
                      operation: "x",
                      kind: "git",
                      detail: "capture",
                    }),
                  );
                present.add(checkpointRef);
                return Effect.void;
              },
              diffCheckpoints: (input) => {
                calls.push(
                  `diff ${refs.indexOf(input.fromCheckpointRef)}..${refs.indexOf(input.toCheckpointRef)} ${input.format} ws=${input.ignoreWhitespace}`,
                );
                return store.diffFails
                  ? Effect.fail(
                      new VcsUnsupportedOperationError({
                        operation: "x",
                        kind: "git",
                        detail: "diff",
                      }),
                    )
                  : Effect.succeed(store.numstat ?? "");
              },
            }),
          ),
        ),
        Layer.mock(EventSink.EventSinkV2)({
          commitCommand: (input) =>
            Effect.sync(() => {
              commits.push(input);
              return {};
            }),
        }),
        Layer.mock(ProjectionStore.ProjectionStoreV2)({
          getCheckpointCaptureContext: () => Effect.succeed(scenario.context),
        }),
      ),
    ),
    Layer.provideMerge(Layer.mergeAll(NodeCrypto.layer, IdAllocator.layer)),
  );
  const outcome = await run(
    Effect.gen(function* () {
      const capture = yield* CheckpointCaptureService.CheckpointCaptureServiceV2;
      return yield* capture
        .execute({
          threadId: captureThread,
          runId: scenario.context.run?.id ?? "run:2",
          scopeId: captureScopeId,
        })
        .pipe(Effect.result);
    }).pipe(Effect.provide(layer)),
  );
  const normalise = (value, now) =>
    JSON.parse(JSON.stringify(value, (_key, item) => (item === now ? "NOW" : item)));
  let output;
  if (outcome._tag === "Failure") {
    output = { error: outcome.failure.cause };
  } else if (commits.length === 0) {
    output = { skip: true };
  } else {
    const [commit] = commits;
    const now = DateTime.formatIso(commit.acceptedAt);
    output = normalise(
      {
        commandId: commit.commandId,
        commandType: commit.commandType,
        threadId: commit.threadId,
        effects: commit.effects,
        events: commit.events.map((event) => ({
          type: event.type,
          runId: event.runId ?? null,
          nodeId: event.nodeId ?? null,
          driver: event.driver ?? null,
          providerInstanceId: event.providerInstanceId,
          occurredAt: event.occurredAt,
          payload: event.payload,
        })),
      },
      now,
    );
  }
  rows.push({
    operation: "capture",
    scenario: scenario.name,
    runId: scenario.context.run?.id ?? "run:2",
    context: JSON.parse(JSON.stringify(scenario.context ?? {})),
    store,
    calls,
    output,
  });
}

rows.push({ operation: "projections", projections });
rows.push(...diffRows);

const target = new URL(
  "../crates/server/tests/fixtures/checkpoint-orchestration.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
