// Runs the unchanged preparation sources and writes what the Rust port replays:
// - `prepared-run.progress`, `.fail` and `.retry` admission and events, sliced out of Orchestrator.ts
//   and given scripted collaborators;
// - the temporary worktree branch names of `@t3tools/shared/git`.
// No git runs here.
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Ref from "../../apps/server/node_modules/effect/dist/Ref.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import {
  buildTemporaryWorktreeBranchName,
  flattenTemporaryWorktreeBranchName,
  isTemporaryWorktreeBranch,
} from "../../packages/shared/src/git.ts";

const run = (effect) => Effect.runPromise(effect);
const plain = (value) => JSON.parse(JSON.stringify(value));
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
const strip = (text) => stripTypeScriptTypes(text, { mode: "strip" });
const helpers = strip(
  slice("/** A reopened preparation item drops", '/**\n * A parent thread is "live"'),
);
const state = strip(
  slice("  const preparedRunState = (", "  const dispatchPreparedRunRelease = ("),
);
const failAndRetry = strip(
  slice(
    "  const dispatchPreparedRunFail = (",
    "  /**\n   * Ends the background work a settled thread",
  ),
);
const make = new Function(
  "Effect",
  "Ref",
  "DateTime",
  "loadProjectionForCommand",
  "OrchestratorDispatchError",
  "emit",
  "idAllocator",
  "nextTurnItemOrdinal",
  "WORKSPACE_PREPARATION_INPUT",
  "ORCHESTRATION_V2_WORKSPACE_PREPARATION_FAILURE_CODE",
  `${helpers}\n${state}\n${failAndRetry}\nreturn { dispatchPreparedRunProgress, dispatchPreparedRunFail, dispatchPreparedRunRetry };`,
);
class DispatchError extends Schema.TaggedError()("OrchestratorDispatchError", {
  commandId: Schema.String,
  commandType: Schema.String,
  cause: Schema.optional(Schema.Defect()),
}) {}

const NOW = "2030-01-01T00:00:00.000Z";
const EARLIER = "2026-04-01T00:00:00.000Z";
const FIXED = DateTime.makeUnsafe(NOW);
const encode = (part) => encodeURIComponent(String(part));
const idAllocator = {
  derive: {
    turnItemFromProviderItem: (input) =>
      `turn-item:provider:${encode(input.driver)}:native-item:${encode(input.nativeItemId)}`,
  },
};

const rows = [];
rows.push({
  operation: "branch-names",
  cases: ["0123456789abcdef", "A1B2C3D4-e5f6-4789-8abc-def012345678", "zz12", "", "deadbeef00"].map(
    (token) => ({ token, name: buildTemporaryWorktreeBranchName(() => token) }),
  ),
});
rows.push({
  operation: "temporary-branches",
  cases: [
    "t3/0123abcd",
    " T3-0123ABCD ",
    "t3code/0123abcd",
    "t3code-0123abcd",
    "t3code/6f9619ff-8b86-4d11-b42d-00c04fc964ff",
    "t3/0123abc",
    "t3/0123abcde",
    "t3",
    "feature/0123abcd",
    "t3code/6f9619ff-8b86-1d11-b42d-00c04fc964ff",
    "t3-0123abcd/x",
    "T3/DEADBEEF",
  ].map((name) => ({
    name,
    temporary: isTemporaryWorktreeBranch(name),
    flat: flattenTemporaryWorktreeBranchName(name),
  })),
});

const thread = (overrides = {}) => ({
  id: "thread:1",
  projectId: "project:1",
  archivedAt: null,
  deletedAt: null,
  worktreePath: null,
  branch: null,
  ...overrides,
});
const providerThread = { id: "provider-thread:1", driver: "codex" };
const runRow = (status, overrides = {}) => ({
  id: "run:1",
  threadId: "thread:1",
  ordinal: 1,
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt" },
  providerThreadId: "provider-thread:1",
  userMessageId: "message:1",
  rootNodeId: "node:1",
  activeAttemptId: "attempt:1",
  status,
  requestedAt: EARLIER,
  startedAt: null,
  completedAt: null,
  checkpointId: null,
  contextHandoffId: null,
  workspacePreparation: { type: "worktree", baseRef: "main" },
  ...overrides,
});
const attempt = (status = "pending") => ({
  id: "attempt:1",
  runId: "run:1",
  attemptOrdinal: 1,
  rootNodeId: "node:1",
  providerInstanceId: "codex",
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  reason: "initial",
  status,
  startedAt: null,
  completedAt: null,
});
const node = (status = "pending") => ({
  id: "node:1",
  threadId: "thread:1",
  runId: "run:1",
  parentNodeId: null,
  rootNodeId: "node:1",
  kind: "root_turn",
  status,
  countsForRun: true,
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  nativeItemRef: null,
  runtimeRequestId: null,
  checkpointScopeId: null,
  startedAt: null,
  completedAt: null,
});
const item = (id, ordinal, extra) => ({
  id,
  threadId: "thread:1",
  runId: "run:1",
  nodeId: "node:1",
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  nativeItemRef: null,
  parentItemId: null,
  ordinal,
  startedAt: EARLIER,
  completedAt: null,
  updatedAt: EARLIER,
  title: null,
  ...extra,
});
const user = item("turn-item:message:message%3A1", 0, {
  status: "completed",
  type: "user_message",
  messageId: "message:1",
  text: "Go",
  attachments: [],
  createdBy: "user",
  creationSource: "web",
});
const preparing = (extra = {}) =>
  item("workspace-preparation:run:1", 1, {
    status: "running",
    title: "Preparing workspace",
    type: "command_execution",
    input: "Preparing workspace",
    ...extra,
  });
const failure = {
  class: "validation_error",
  message: "Workspace preparation failed during provision worktree: no space",
  code: null,
  retryable: false,
};
const failureItem = (extra = {}) =>
  item("turn-item:provider:codex:native-item:workspace-preparation-failure%3Arun%3A1", 2, {
    status: "failed",
    title: "Workspace preparation failed",
    completedAt: EARLIER,
    type: "error",
    failure: { ...failure, code: "workspace_preparation_failed" },
    ...extra,
  });
const projection = (overrides = {}) => ({
  thread: thread(),
  runs: [runRow("preparing")],
  attempts: [attempt()],
  nodes: [node()],
  providerThreads: [providerThread],
  turnItems: [user, preparing()],
  ...overrides,
});
const failed = (overrides = {}) =>
  projection({
    runs: [runRow("failed", { completedAt: EARLIER })],
    attempts: [attempt("failed")],
    nodes: [node("failed")],
    turnItems: [
      user,
      preparing({
        status: "failed",
        title: "Workspace preparation failed",
        output: failure.message,
        exitCode: 1,
        completedAt: EARLIER,
      }),
      failureItem(),
    ],
    ...overrides,
  });

const scenarios = [];
const add = (name, command, view) => scenarios.push({ name, command, projection: view });
const base = { commandId: "command:p", threadId: "thread:1", runId: "run:1" };
add(
  "progress-worktree",
  { type: "prepared-run.progress", ...base, phase: "worktree" },
  projection(),
);
add("progress-setup", { type: "prepared-run.progress", ...base, phase: "setup" }, projection());
add(
  "progress-run-already-started",
  { type: "prepared-run.progress", ...base, phase: "setup" },
  projection({ runs: [runRow("starting")] }),
);
add(
  "progress-unknown-run",
  { type: "prepared-run.progress", ...base, runId: "run:9", phase: "setup" },
  projection(),
);
add(
  "progress-without-preparation-item",
  { type: "prepared-run.progress", ...base, phase: "setup" },
  projection({ turnItems: [user] }),
);
add(
  "progress-without-root-node",
  { type: "prepared-run.progress", ...base, phase: "setup" },
  projection({ nodes: [] }),
);
add("fail", { type: "prepared-run.fail", ...base, failure }, projection());
add(
  "fail-keeps-failure-fields",
  {
    type: "prepared-run.fail",
    ...base,
    failure: {
      class: "validation_error",
      message: "Workspace preparation failed: Worktree setup cancelled.",
      code: "other",
      retryable: true,
    },
  },
  projection(),
);
add(
  "fail-next-ordinal-follows-the-transcript",
  { type: "prepared-run.fail", ...base, failure },
  projection({
    turnItems: [
      user,
      preparing(),
      item("later", 7, {
        status: "completed",
        type: "user_message",
        messageId: "m",
        text: "",
        attachments: [],
        createdBy: "user",
        creationSource: "web",
      }),
    ],
  }),
);
add(
  "fail-run-not-preparing",
  { type: "prepared-run.fail", ...base, failure },
  projection({ runs: [runRow("running")] }),
);
add(
  "fail-other-driver",
  { type: "prepared-run.fail", ...base, failure },
  projection({ providerThreads: [{ id: "provider-thread:1", driver: "claude Agent/1" }] }),
);
add("retry", { type: "prepared-run.retry", ...base, commandId: "command:r" }, failed());
add(
  "retry-drops-the-old-result",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    turnItems: [
      user,
      preparing({
        status: "failed",
        output: "x",
        exitCode: 1,
        outputIndicatesFailure: true,
        completedAt: EARLIER,
      }),
      failureItem(),
    ],
  }),
);
add(
  "retry-run-still-preparing",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  projection(),
);
add(
  "retry-no-failure-item",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({ turnItems: [user, preparing({ status: "failed" })] }),
);
add(
  "retry-other-failure",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    turnItems: [
      user,
      preparing({ status: "failed" }),
      failureItem({ failure: { ...failure, code: "provider_failed" } }),
    ],
  }),
);
add(
  "retry-failure-already-cancelled",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    turnItems: [user, preparing({ status: "failed" }), failureItem({ status: "cancelled" })],
  }),
);
add(
  "retry-without-recorded-preparation",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({ runs: [runRow("failed", { workspacePreparation: undefined })] }),
);
add(
  "retry-archived-thread",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({ thread: thread({ archivedAt: EARLIER }) }),
);
add(
  "retry-deleted-thread",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({ thread: thread({ deletedAt: EARLIER }) }),
);
add(
  "retry-another-run-active",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    runs: [
      runRow("failed", { completedAt: EARLIER }),
      runRow("running", { id: "run:2", ordinal: 2 }),
    ],
  }),
);
add(
  "retry-another-run-finished",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    runs: [
      runRow("failed", { completedAt: EARLIER }),
      runRow("completed", { id: "run:2", ordinal: 2 }),
    ],
  }),
);
add(
  "retry-existing-worktree-strategy",
  { type: "prepared-run.retry", ...base, commandId: "command:r" },
  failed({
    runs: [
      runRow("failed", {
        completedAt: EARLIER,
        workspacePreparation: { type: "existing_worktree", worktreePath: "/work" },
      }),
    ],
  }),
);

for (const scenario of scenarios) {
  const events = await run(Ref.make([]));
  const effects = await run(Ref.make([]));
  const nextOrdinal = (view) =>
    Effect.succeed(view.turnItems.reduce((ordinal, row) => Math.max(ordinal, row.ordinal + 1), 0));
  const handlers = make(
    Effect,
    Ref,
    { now: Effect.succeed(FIXED) },
    () => Effect.succeed(scenario.projection),
    DispatchError,
    (eventsRef, _command) => (event) => Ref.update(eventsRef, (existing) => [...existing, event]),
    idAllocator,
    nextOrdinal,
    "Preparing workspace",
    "workspace_preparation_failed",
  );
  const handler = {
    "prepared-run.progress": handlers.dispatchPreparedRunProgress,
    "prepared-run.fail": handlers.dispatchPreparedRunFail,
    "prepared-run.retry": handlers.dispatchPreparedRunRetry,
  }[scenario.command.type];
  const outcome = await run(handler(scenario.command, events).pipe(Effect.result));
  const output =
    outcome._tag === "Failure"
      ? { error: String(outcome.failure.cause) }
      : {
          events: plain(await run(Ref.get(events))).map((event) => ({
            type: event.type,
            payload: event.payload,
          })),
        };
  rows.push({
    operation: "prepared-run",
    name: scenario.name,
    command: scenario.command,
    projection: scenario.projection,
    output,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/workspace-preparation.jsonl",
  import.meta.url,
);
const staging = new URL(
  "../crates/server/tests/fixtures/workspace-preparation.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${rows.length} rows`);
