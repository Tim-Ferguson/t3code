// Runs the unchanged context handoff builders (`ContextHandoffBudget`, `ContextHandoffService`), the
// fork planner (`ThreadForkService`) and the capability checks of `CommandPolicy` over scripted inputs
// and writes what they return. The Rust port (`context_handoff.rs`, `thread_fork.rs`,
// `context_transfer.rs`) replays every row. Ids the service allocates are random, so a row's output
// has them replaced by `<id>`.
import { renameSync, writeFileSync } from "node:fs";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Budget from "../../apps/server/src/orchestration-v2/ContextHandoffBudget.ts";
import * as HandoffService from "../../apps/server/src/orchestration-v2/ContextHandoffService.ts";
import * as CommandPolicy from "../../apps/server/src/orchestration-v2/CommandPolicy.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import * as ThreadForkService from "../../apps/server/src/orchestration-v2/ThreadForkService.ts";
import { CodexProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts";
import { ClaudeProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";

const NOW = "2026-09-04T00:00:00.000Z";
const plain = (value) => JSON.parse(JSON.stringify(value));
const rows = [];

const item = (type, id, run, extra = {}) => ({
  id,
  threadId: "thread:fork",
  runId: run,
  nodeId: "node:1",
  providerThreadId: "provider-thread:1",
  providerTurnId: null,
  nativeItemRef: null,
  parentItemId: null,
  ordinal: 1,
  status: "completed",
  title: null,
  startedAt: NOW,
  completedAt: NOW,
  updatedAt: NOW,
  type,
  ...extra,
});
const user = (id, run, text) =>
  item("user_message", id, run, {
    text,
    messageId: `m:${id}`,
    attachments: [],
    inputIntent: "turn_start",
    createdBy: "user",
    creationSource: "web",
  });
const assistant = (id, run, text) =>
  item("assistant_message", id, run, { text, messageId: `m:${id}` });
const command = (id, run, input, exitCode, output) =>
  item("command_execution", id, run, { input, exitCode, output });
const files = (id, run, name) =>
  item("file_change", id, run, { fileName: name, changeKind: "modified" });
const checkpoint = (id, run, count) =>
  item("checkpoint", id, run, {
    checkpointId: `c:${id}`,
    files: Array.from({ length: count }, (_, i) => ({ path: `f${i}` })),
  });
const failure = (id, run, message) => item("error", id, run, { failure: { message } });

// --- historical messages and selection ---------------------------------------------------------
const longText = (n) => "word ".repeat(n);
const conversations = {
  short: [
    user("u1", "r1", "Remember   the marker"),
    assistant("a1", "r1", "stored"),
    command("c1", "r1", "ls -la", 0, "total 0"),
    command("c2", "r1", "false", null, null),
    files("f1", "r1", "src/a.ts"),
    failure("e1", "r1", "boom"),
  ],
  unicode: [
    user("u1", "r1", "café 😀   line"),
    assistant("a1", "r1", 'réponse "quoted" \\ back\nslash'),
  ],
  long: Array.from({ length: 12 }, (_, i) =>
    i % 2 === 0
      ? user(`u${i}`, "r1", longText(40 + i))
      : assistant(`a${i}`, "r1", longText(60 + i)),
  ),
  oversized: [
    user("u1", "r1", "first"),
    assistant("a1", "r1", longText(5000)),
    user("u2", "r2", "latest"),
    assistant("a2", "r2", "answer"),
  ],
  none: [checkpoint("k1", "r1", 2)],
};
for (const [name, items] of Object.entries(conversations)) {
  const messages = items.flatMap((entry) => {
    const message = Budget.historicalMessage(entry);
    return message === null ? [] : [message];
  });
  rows.push({ kind: "historical_messages", name, items, output: plain(messages) });
  for (const budget of [0, 600, 1500, 3000, 16000]) {
    const coverage = Budget.handoffCoverage({
      threadId: "thread:fork",
      coveredRunOrdinals: { from: 1, to: 2 },
      items,
    });
    const selected = Budget.selectHistory({ messages, coverage, budget });
    rows.push({
      kind: "select_history",
      name: `${name}/${budget}`,
      messages: plain(messages),
      coverage,
      omittedItems: 0,
      budget,
      output: {
        ...plain(selected),
        cost: Budget.historyCost(selected.messages, selected.context),
        rendered: Budget.renderHistory(selected.messages, selected.context),
        responseItems: Budget.historyResponseItems(selected.messages, selected.context),
      },
    });
  }
  rows.push({
    kind: "select_history",
    name: `${name}/omitted`,
    messages: plain(messages),
    coverage: "coverage",
    omittedItems: 7,
    budget: 2000,
    output: plain(
      Budget.selectHistory({ messages, coverage: "coverage", omittedItems: 7, budget: 2000 }),
    ),
  });
}
rows.push({
  kind: "coverage",
  name: "empty",
  threadId: "thread:x",
  covered: { from: 2, to: 4 },
  items: [],
  output: Budget.handoffCoverage({
    threadId: "thread:x",
    coveredRunOrdinals: { from: 2, to: 4 },
    items: [],
  }),
});

// --- budget --------------------------------------------------------------------------------------
const providerThread = (contextUsage) => ({ id: "provider-thread:1", contextUsage });
for (const [name, usage, estimate, window, text, attachments] of [
  ["no usage", undefined, 0, undefined, "hi", []],
  ["estimate", undefined, 40000, undefined, "hi", []],
  ["measured", { usedTokens: 20000, maxTokens: 200000 }, 0, undefined, "hello", []],
  ["full window", { usedTokens: 100000, maxTokens: 128000 }, 0, undefined, "hi", []],
  [
    "compact threshold",
    { usedTokens: 10000, maxTokens: 400000, autoCompactThreshold: 60000 },
    0,
    undefined,
    "hi",
    [],
  ],
  ["model window", undefined, 0, 1000000, "hi", []],
  ["model window smaller", { usedTokens: 1000, maxTokens: 50000 }, 0, 400000, "hi", []],
  [
    "attachments",
    { usedTokens: 20000, maxTokens: 200000 },
    0,
    undefined,
    "hello",
    [{ type: "image" }, { type: "file" }],
  ],
  ["unicode text", undefined, 0, undefined, "café 😀".repeat(500), []],
]) {
  for (const tokenCap of [1024, 16000, 64000]) {
    rows.push({
      kind: "budget",
      name: `${name}/${tokenCap}`,
      tokenCap,
      userText: text,
      attachments,
      providerThread: providerThread(usage),
      nativeContextEstimate: estimate,
      modelContextWindow: window ?? null,
      output: Budget.handoffBudget({
        tokenCap,
        userText: text,
        attachments,
        providerThread: providerThread(usage),
        nativeContextEstimate: estimate,
        modelContextWindow: window,
      }),
    });
  }
}

// --- prepared handoffs ---------------------------------------------------------------------------
const withoutIds = (handoff) => ({ ...plain(handoff), id: "<id>" });
await Effect.runPromise(
  Effect.gen(function* () {
    const service = yield* HandoffService.ContextHandoffServiceV2;
    const createdAt = DateTime.makeUnsafe(NOW);
    for (const [name, items] of Object.entries(conversations)) {
      const ordinals = { from: 1, to: 2 };
      const delta = yield* service.prepareForkDelta({
        sourceThreadId: "thread:fork",
        targetThreadId: "thread:source",
        targetRunId: "run:target",
        transferId: "context-transfer:1",
        fromProviderThreadIds: ["provider-thread:1"],
        toProviderThreadId: "provider-thread:2",
        fromProviderInstanceId: "codex",
        toProviderInstanceId: "claudeAgent",
        coveredRunOrdinals: ordinals,
        deltaItems: items,
        createdAt,
      });
      rows.push({
        kind: "fork_delta",
        name,
        items,
        covered: ordinals,
        output: withoutIds(delta),
      });
      const runs = [
        { id: "r1", status: "completed" },
        { id: "r2", status: "failed" },
      ];
      for (const strategy of ["full_thread_summary", "delta_since_target_last_seen"]) {
        const handoff = yield* service.prepareProviderHandoff({
          threadId: "thread:source",
          targetRunId: "run:target",
          transferId: "context-transfer:1",
          fromProviderThreadIds: ["provider-thread:1"],
          toProviderThreadId: "provider-thread:2",
          fromProviderInstanceId: "codex",
          toProviderInstanceId: "claudeAgent",
          coveredRunOrdinals: ordinals,
          runs,
          strategy,
          items,
          createdAt,
        });
        rows.push({
          kind: "provider_handoff",
          name: `${name}/${strategy}`,
          strategy,
          items,
          runs,
          covered: ordinals,
          output: withoutIds(handoff),
        });
      }
    }
  }).pipe(Effect.provide(HandoffService.layer.pipe(Layer.provide(IdAllocator.layer)))),
);

// --- fork planning -------------------------------------------------------------------------------
const thread = (overrides = {}) => ({
  createdBy: "user",
  creationSource: "web",
  id: "thread:source",
  projectId: "project:1",
  title: "Source",
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt-5.4" },
  runtimeMode: "full-access",
  interactionMode: "plan",
  branch: "feature/source",
  worktreePath: "/tmp/source-worktree",
  activeProviderThreadId: "provider-thread:1",
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: "thread:source" },
  forkedFrom: null,
  createdAt: "2026-09-01T00:00:00.000Z",
  updatedAt: "2026-09-02T00:00:00.000Z",
  archivedAt: "2026-09-02T01:00:00.000Z",
  settledOverride: "settled",
  settledAt: "2026-09-02T02:00:00.000Z",
  snoozedUntil: "2026-09-10T00:00:00.000Z",
  snoozedAt: "2026-09-02T03:00:00.000Z",
  lastVisitedAt: "2026-09-02T04:00:00.000Z",
  deletedAt: null,
  ...overrides,
});
const run = (status) => ({
  id: "run:1",
  threadId: "thread:source",
  ordinal: 1,
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt-5.4" },
  providerThreadId: "provider-thread:1",
  userMessageId: "message:1",
  rootNodeId: null,
  activeAttemptId: null,
  status,
  queuePosition: null,
  requestedAt: "2026-09-01T00:00:00.000Z",
  startedAt: "2026-09-01T00:00:00.000Z",
  completedAt: null,
  checkpointId: null,
  contextHandoffId: null,
});
const providerThreadRow = (strength) => ({
  id: "provider-thread:1",
  driver: "codex",
  providerInstanceId: "codex",
  providerSessionId: null,
  appThreadId: "thread:source",
  ownerNodeId: null,
  nativeThreadRef: strength === null ? null : { driver: "codex", nativeId: "native-1", strength },
  nativeConversationHeadRef: null,
  status: "idle",
  firstRunOrdinal: 1,
  lastRunOrdinal: 1,
  handoffIds: [],
  forkedFrom: null,
  createdAt: "2026-09-01T00:00:00.000Z",
  updatedAt: "2026-09-01T00:00:00.000Z",
});
const point = { threadId: "thread:source", runId: "run:1" };
await Effect.runPromise(
  Effect.gen(function* () {
    const service = yield* ThreadForkService.ThreadForkServiceV2;
    for (const status of [
      "completed",
      "waiting",
      "failed",
      "interrupted",
      "cancelled",
      "running",
      "queued",
      "rolled_back",
    ]) {
      for (const [strength, title, source] of [
        ["strong", "Named", thread()],
        [
          "weak",
          undefined,
          thread({
            lineage: {
              parentThreadId: "thread:root",
              relationshipToParent: "fork",
              rootThreadId: "thread:root",
            },
          }),
        ],
        [null, undefined, thread({ title: "T" })],
      ]) {
        const input = {
          sourceProjection: { thread: source },
          sourceRun: run(status),
          sourceProviderThread: strength === null ? null : providerThreadRow(strength),
          canonicalSourcePoint: point,
          transferId: "context-transfer:1",
          targetThreadId: "thread:target",
          ...(title === undefined ? {} : { title }),
          createdBy: "user",
          creationSource: "web",
          createdAt: DateTime.makeUnsafe(NOW),
        };
        const result = yield* Effect.exit(
          service.plan({ ...input, sourceProviderThread: input.sourceProviderThread ?? undefined }),
        );
        rows.push({
          kind: "fork_plan",
          name: `${status}/${strength}`,
          input: plain({ ...input, createdAt: NOW }),
          output:
            result._tag === "Success"
              ? { ok: plain(result.value) }
              : { error: String(result.cause.reasons?.[0]?.error?.cause ?? result.cause) },
        });
      }
    }
  }).pipe(Effect.provide(ThreadForkService.layer)),
);

// --- capability checks ---------------------------------------------------------------------------
const policy = CommandPolicy.layer;
const check = (effect) =>
  Effect.runPromise(
    Effect.exit(effect.pipe(Effect.provide(policy))).pipe(
      Effect.map((exit) =>
        exit._tag === "Success"
          ? { ok: exit.value ?? null }
          : { error: exit.cause.reasons?.[0]?.error?.message ?? String(exit.cause) },
      ),
    ),
  );
const base = { commandId: "command:1", threadId: "thread:1", providerInstanceId: "codex" };
for (const [name, capabilities] of [
  ["codex", CodexProviderCapabilitiesV2],
  ["claude", ClaudeProviderCapabilitiesV2],
]) {
  const variants = [["as declared", capabilities]];
  for (const [flag, group, key] of [
    ["no native fork", "threads", "canForkThread"],
    ["no fork from turn", "threads", "canForkFromTurn"],
    ["no handoff summaries", "context", "canConsumeHandoffSummaries"],
    ["no synthetic context", "context", "acceptsSyntheticUserContext"],
    ["no delta handoff", "context", "supportsDeltaHandoff"],
    ["no full handoff", "context", "supportsFullThreadHandoff"],
  ]) {
    variants.push([flag, { ...capabilities, [group]: { ...capabilities[group], [key]: false } }]);
  }
  variants.push([
    "weak thread ids",
    { ...capabilities, identity: { ...capabilities.identity, nativeThreadIds: "weak" } },
  ]);
  for (const [variant, caps] of variants) {
    rows.push({ kind: "capabilities", name: `${name}/${variant}`, capabilities: plain(caps) });
    for (const strategy of ["fork_delta_context", "delta_context", "full_thread_summary"]) {
      rows.push({
        kind: "ensure_context_handoff",
        name: `${name}/${variant}/${strategy}`,
        capabilities: `${name}/${variant}`,
        strategy,
        output: await check(
          Effect.gen(function* () {
            const policy = yield* CommandPolicy.CommandPolicyV2;
            return yield* policy.ensureContextHandoff({ ...base, capabilities: caps, strategy });
          }),
        ),
      });
    }
    for (const fromSpecificTurn of [false, true]) {
      rows.push({
        kind: "ensure_native_fork",
        name: `${name}/${variant}/${fromSpecificTurn}`,
        capabilities: `${name}/${variant}`,
        fromSpecificTurn,
        output: await check(
          Effect.gen(function* () {
            const policy = yield* CommandPolicy.CommandPolicyV2;
            return yield* policy.ensureNativeFork({
              ...base,
              capabilities: caps,
              fromSpecificTurn,
            });
          }),
        ),
      });
      for (const sameProvider of [true, false]) {
        for (const hasStrongNativeSource of [true, false]) {
          for (const sourceRunStatus of [
            "completed",
            "waiting",
            "failed",
            "interrupted",
            "cancelled",
          ]) {
            rows.push({
              kind: "decide_fork_execution",
              name: `${name}/${variant}/${fromSpecificTurn}/${sameProvider}/${hasStrongNativeSource}/${sourceRunStatus}`,
              capabilities: `${name}/${variant}`,
              fromSpecificTurn,
              sameProvider,
              hasStrongNativeSource,
              sourceRunStatus,
              output: await check(
                Effect.gen(function* () {
                  const policy = yield* CommandPolicy.CommandPolicyV2;
                  return yield* policy.decideForkExecution({
                    ...base,
                    capabilities: caps,
                    sameProvider,
                    hasStrongNativeSource,
                    sourceRunStatus,
                    fromSpecificTurn,
                  });
                }),
              ),
            });
          }
        }
      }
    }
  }
}

const target = new URL("../crates/server/tests/fixtures/context-handoff.jsonl", import.meta.url);
const staging = new URL(
  "../crates/server/tests/fixtures/context-handoff.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${rows.length} rows`);
process.exit(0);
