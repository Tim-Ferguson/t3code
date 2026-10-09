// Thread forks, sibling forks and merge-backs over the recorded Codex app-server and Claude Agent SDK
// transcripts, run on the unchanged TypeScript orchestrator and adapters (`ThreadFork.integration.test.ts`
// and `ThreadMergeBack.integration.test.ts` build the same commands). Used by
// generate_codex_replay_fixtures.mjs and generate_claude_replay_fixtures.mjs, which write the result
// under `forks` for tests/codex_replay.rs and tests/claude_fork_replay.rs. Each scenario is a list of declarative steps: the generator turns them into
// the original's commands, the Rust test into its own.
import { readFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import { ClaudeOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.testkit.ts";
import { CodexOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.testkit.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import * as ProviderAdapterRegistry from "../../apps/server/src/orchestration-v2/ProviderAdapterRegistry.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import * as Shared from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import { runOrchestratorV2ProviderReplayScenario } from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import {
  materializeReplayTranscriptWorkspace,
  readProviderReplayTranscript,
} from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";

const FIXTURES = new URL(
  "../../apps/server/src/orchestration-v2/testkit/fixtures/",
  import.meta.url,
);
const READ_ONLY_NEVER = {
  approvalPolicy: "never",
  sandboxPolicy: { type: "readOnly", access: { type: "fullAccess" }, networkAccess: false },
};
const MERGE_USER =
  "Retain the transferred fork marker for later. Respond with exactly: merge delta stored";
const FIRST_MERGE_USER =
  "Retain the first transferred marker for later. Respond with exactly: first merge delta stored";
const SECOND_MERGE_USER =
  "Retain the second transferred marker for later. Respond with exactly: second merge delta stored";

const message = (thread, text) => ({ type: "message", thread, text });
const fork = (from, to, title, sourcePoint, twice = false) => ({
  type: "fork",
  from,
  to,
  title,
  sourcePoint,
  twice,
});
const mergeBack = (from, into, sourcePoint) => ({ type: "merge_back", from, into, sourcePoint });
const rollback = (thread, ordinal) => ({ type: "rollback", thread, ordinal });
const latest = { type: "latest_stable" };
const runOf = (thread, ordinal) => ({ type: "run", thread, ordinal });

// name: the recording; model: the Codex model of the recording; policy: runtime policy the recording was
// made under; replace: text of the recording the original's test parameterises (a handoff that predates
// native injection; the Claude recordings carry it in the message instead, see `claudeReplace`).
// `claudeOnly` scenarios have a Claude recording only and take their prompts from its metadata.
const CLAUDE_MODEL = "claude-sonnet-4-6";
const compact = (text, max = 240) => {
  const compacted = text.replace(/\s+/gu, " ").trim();
  return compacted.length <= max ? compacted : `${compacted.slice(0, max - 3)}...`;
};
// The summary the fork delta carries, as the original's merge-back test builds it.
const forkDeltaSummary = (source, target, userText, assistantText) =>
  [
    "Merge-back context from forked conversation.",
    `Source thread: ${source}`,
    `Target thread: ${target}`,
    "Covered fork runs: 1-1",
    "",
    "Fork delta:",
    `- User: ${compact(userText)}`,
    `- Assistant: ${compact(assistantText)}`,
    "- Checkpoint: 0 files",
  ].join("\n");
const providerMessage = (summary, userText) =>
  [
    "Context handoff (merge_back / fork_delta_summary):",
    summary,
    "",
    "User message:",
    userText,
  ].join("\n");

export const FORK_SCENARIOS = [
  {
    name: "thread_fork_native",
    model: "gpt-5.4",
    threads: { source: "Source thread" },
    steps: [
      message("source", Shared.THREAD_FORK_NATIVE_SOURCE_PROMPT),
      fork("source", "fork", "Forked thread", latest, true),
      message("fork", Shared.THREAD_FORK_NATIVE_TARGET_PROMPT),
    ],
  },
  {
    name: "thread_fork_native_prior_turn",
    model: "gpt-5.4",
    policy: READ_ONLY_NEVER,
    threads: { source: "Source thread" },
    steps: [
      message("source", Shared.THREAD_FORK_NATIVE_PRIOR_TURN_ALPHA_PROMPT),
      message("source", Shared.THREAD_FORK_NATIVE_PRIOR_TURN_BETA_PROMPT),
      fork("source", "fork", "Forked from first response", runOf("source", 1)),
      message("fork", Shared.THREAD_FORK_NATIVE_PRIOR_TURN_REPEAT_PROMPT),
    ],
  },
  {
    name: "thread_fork_native_continue",
    model: "gpt-6-luna",
    threads: { source: "Continue source" },
    steps: [
      message("source", Shared.THREAD_FORK_NATIVE_CONTINUE_SOURCE_PROMPT),
      fork("source", "fork", "Continued fork", latest),
      message("fork", Shared.THREAD_FORK_NATIVE_CONTINUE_FIRST_PROMPT),
      message("fork", Shared.THREAD_FORK_NATIVE_CONTINUE_SECOND_PROMPT),
    ],
  },
  {
    name: "thread_fork_native_siblings",
    model: "gpt-6-sol",
    threads: { source: "Siblings source" },
    steps: [
      message("source", Shared.THREAD_FORK_NATIVE_SIBLINGS_SOURCE_PROMPT),
      fork("source", "first", "First sibling", latest),
      message("first", Shared.THREAD_FORK_NATIVE_SIBLINGS_FIRST_PROMPT),
      fork("source", "second", "Second sibling", latest),
      message("second", Shared.THREAD_FORK_NATIVE_SIBLINGS_SECOND_PROMPT),
    ],
  },
  {
    name: "thread_merge_back_continue",
    model: "gpt-6-sol",
    threads: { source: "Merge-back source" },
    replace: [[Shared.THREAD_MERGE_BACK_HANDOFF_PROMPT, MERGE_USER]],
    steps: [
      message("source", Shared.THREAD_MERGE_BACK_SOURCE_PROMPT),
      fork("source", "fork", "Merge-back fork", latest),
      message("fork", Shared.THREAD_MERGE_BACK_FORK_PROMPT),
      mergeBack("fork", "source", runOf("fork", 1)),
      message("source", MERGE_USER),
      message("source", Shared.THREAD_MERGE_BACK_RECALL_PROMPT),
    ],
  },
  {
    name: "thread_merge_back_siblings",
    model: "gpt-6-sol",
    threads: { source: "Merge-back siblings source" },
    replace: [
      [Shared.THREAD_MERGE_BACK_SIBLINGS_FIRST_HANDOFF_PROMPT, FIRST_MERGE_USER],
      [Shared.THREAD_MERGE_BACK_SIBLINGS_SECOND_HANDOFF_PROMPT, SECOND_MERGE_USER],
    ],
    steps: [
      message("source", Shared.THREAD_MERGE_BACK_SIBLINGS_SOURCE_PROMPT),
      fork("source", "first", "First merge-back sibling", latest),
      message("first", Shared.THREAD_MERGE_BACK_SIBLINGS_FIRST_FORK_PROMPT),
      fork("source", "second", "Second merge-back sibling", latest),
      message("second", Shared.THREAD_MERGE_BACK_SIBLINGS_SECOND_FORK_PROMPT),
      mergeBack("first", "source", runOf("first", 1)),
      message("source", FIRST_MERGE_USER),
      mergeBack("second", "source", runOf("second", 1)),
      message("source", SECOND_MERGE_USER),
      message("source", Shared.THREAD_MERGE_BACK_SIBLINGS_RECALL_PROMPT),
    ],
  },
  {
    name: "thread_fork_native_fork_local_rollback",
    claudeOnly: true,
    threads: { source: "Source thread" },
    steps: (prompts) => [
      message("source", prompts[0]),
      fork("source", "fork", "Forked thread", latest),
      message("fork", prompts[1]),
      message("fork", prompts[2]),
      rollback("fork", 1),
      message("fork", prompts[3]),
    ],
  },
];
const byName = (name) => FORK_SCENARIOS.find((scenario) => scenario.name === name);
// The Claude recordings carry the handoff in the prompt, as the original's merge-back test builds it.
byName("thread_merge_back_continue").claudeReplace = (id) => [
  [
    Shared.THREAD_MERGE_BACK_HANDOFF_PROMPT,
    providerMessage(
      forkDeltaSummary(
        id("fork"),
        id("source"),
        Shared.THREAD_MERGE_BACK_FORK_PROMPT,
        "merge fork stored",
      ),
      MERGE_USER,
    ),
  ],
];
byName("thread_merge_back_siblings").claudeReplace = (id) => [
  [
    Shared.THREAD_MERGE_BACK_SIBLINGS_FIRST_HANDOFF_PROMPT,
    providerMessage(
      forkDeltaSummary(
        id("first"),
        id("source"),
        Shared.THREAD_MERGE_BACK_SIBLINGS_FIRST_FORK_PROMPT,
        "first merge sibling stored",
      ),
      FIRST_MERGE_USER,
    ),
  ],
  [
    Shared.THREAD_MERGE_BACK_SIBLINGS_SECOND_HANDOFF_PROMPT,
    providerMessage(
      forkDeltaSummary(
        id("second"),
        id("source"),
        Shared.THREAD_MERGE_BACK_SIBLINGS_SECOND_FORK_PROMPT,
        "second merge sibling stored",
      ),
      SECOND_MERGE_USER,
    ),
  ],
];
const stepsOf = (scenario, prompts) =>
  typeof scenario.steps === "function" ? scenario.steps(prompts) : scenario.steps;

const replaceExact = (value, from, to) => {
  if (value === from) return to;
  if (Array.isArray(value)) return value.map((entry) => replaceExact(entry, from, to));
  if (typeof value !== "object" || value === null) return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, entry]) => [key, replaceExact(entry, from, to)]),
  );
};

const threadIdOf = (scenario, alias) => `thread-${scenario.name.replaceAll("_", "-")}-${alias}`;

// The commands the original's tests send, from the declarative steps.
const commandsOf = Effect.fn("commandsOf")(function* (scenario, steps_, ids, projectId, provider) {
  const model = { instanceId: provider.instanceId, model: provider.model(scenario) };
  const id = (alias) => threadIdOf(scenario, alias);
  const commands = [];
  const aliases = new Set(["source"]);
  const steps = [];
  const create = {
    type: "thread.create",
    createdBy: "user",
    creationSource: "web",
    commandId: `command-${scenario.name}-create`,
    threadId: id("source"),
    projectId,
    title: scenario.threads.source,
    modelSelection: model,
    runtimeMode: "full-access",
    interactionMode: "default",
    branch: null,
    worktreePath: null,
  };
  commands.push(create);
  steps.push({ type: "dispatch", command: create, await: true });
  steps.push({ type: "advance_clock", duration: "1 millis" });
  let index = 0;
  const run = (point) =>
    point.type === "run"
      ? {
          type: "run",
          runId: ids.derive.run({ threadId: id(point.thread), ordinal: point.ordinal }),
        }
      : point;
  for (const step of steps_) {
    index += 1;
    const commandId = `command-${scenario.name}-${index}`;
    if (step.type === "message") {
      const command = {
        type: "message.dispatch",
        createdBy: "user",
        creationSource: "web",
        commandId,
        threadId: id(step.thread),
        messageId: `message-${scenario.name}-${index}`,
        text: step.text,
        attachments: [],
        modelSelection: model,
        dispatchMode: { type: "start_immediately" },
      };
      commands.push(command);
      steps.push({ type: "dispatch", command, await: true });
      steps.push({ type: "await_thread_idle", threadId: id(step.thread) });
    } else if (step.type === "fork") {
      const command = {
        type: "thread.fork",
        createdBy: "user",
        creationSource: "web",
        commandId,
        sourceThreadId: id(step.from),
        targetThreadId: id(step.to),
        sourcePoint: run(step.sourcePoint),
        title: step.title,
      };
      aliases.add(step.to);
      commands.push(command);
      steps.push({ type: "dispatch", command, await: true });
      if (step.twice) steps.push({ type: "dispatch", command, await: true });
    } else if (step.type === "rollback") {
      const scopeId = yield* ids.allocate.checkpointScope({
        threadId: id(step.thread),
        name: "root",
      });
      const command = {
        type: "checkpoint.rollback",
        restoreFiles: false,
        commandId,
        threadId: id(step.thread),
        scopeId,
        checkpointId: yield* ids.allocate.checkpoint({
          checkpointScopeId: scopeId,
          name: String(step.ordinal),
        }),
      };
      commands.push(command);
      steps.push({ type: "dispatch", command, await: true });
      steps.push({
        type: "await_run_status",
        threadId: id(step.thread),
        runId: ids.derive.run({ threadId: id(step.thread), ordinal: step.ordinal + 1 }),
        status: "rolled_back",
      });
    } else {
      const command = {
        type: "thread.merge_back",
        createdBy: "user",
        creationSource: "web",
        commandId,
        sourceThreadId: id(step.from),
        targetThreadId: id(step.into),
        sourcePoint: run(step.sourcePoint),
      };
      commands.push(command);
      steps.push({ type: "dispatch", command, await: true });
    }
  }
  return { commands, steps, aliases: [...aliases] };
});

const pick = (object, keys) =>
  Object.fromEntries(
    keys.filter((key) => object?.[key] !== undefined).map((key) => [key, object[key]]),
  );

// What a thread persisted that a fork or a merge-back decides, with ids replaced by what they name
// so that another implementation's ids compare equal.
const describeThread = (scenario, projection, byThreadId, normalize) => {
  const aliasOf = (threadId) =>
    threadId === null || threadId === undefined
      ? null
      : (Object.keys(scenario.threadAliases).find(
          (alias) => scenario.threadAliases[alias] === threadId,
        ) ?? threadId);
  const projectionOf = (threadId) => byThreadId.get(threadId);
  const runOrdinal = (threadId, runId) =>
    runId === null || runId === undefined
      ? null
      : (projectionOf(threadId)?.runs.find((run) => run.id === runId)?.ordinal ?? null);
  const nativeOfProviderThread = (threadId, providerThreadId) =>
    projectionOf(threadId)?.providerThreads.find((thread) => thread.id === providerThreadId)
      ?.nativeThreadRef?.nativeId ?? null;
  const nativeOfProviderTurn = (threadId, providerTurnId) =>
    projectionOf(threadId)?.providerTurns.find((turn) => turn.id === providerTurnId)?.nativeTurnRef
      ?.nativeId ?? null;
  const thread = projection.thread;
  return {
    title: thread.title,
    parent: aliasOf(thread.lineage.parentThreadId),
    relationship: thread.lineage.relationshipToParent,
    root: aliasOf(thread.lineage.rootThreadId),
    forkedFrom:
      thread.forkedFrom === null
        ? null
        : {
            type: thread.forkedFrom.type,
            thread: aliasOf(thread.forkedFrom.threadId),
            runOrdinal: runOrdinal(thread.forkedFrom.threadId, thread.forkedFrom.runId),
          },
    expected: normalize(projection),
    providerThreads: projection.providerThreads.map((providerThread) => ({
      nativeId: providerThread.nativeThreadRef?.nativeId ?? null,
      handoffs: providerThread.handoffIds.length,
      forkedFrom:
        providerThread.forkedFrom === null || providerThread.forkedFrom === undefined
          ? null
          : {
              nativeThreadId: nativeOfProviderThread(
                thread.forkedFrom?.threadId,
                providerThread.forkedFrom.providerThreadId,
              ),
              nativeTurnId:
                providerThread.forkedFrom.providerTurnId === undefined
                  ? null
                  : nativeOfProviderTurn(
                      thread.forkedFrom?.threadId,
                      providerThread.forkedFrom.providerTurnId,
                    ),
            },
    })),
    runs: projection.runs.map((run) => ({
      status: run.status,
      carriesHandoff: run.contextHandoffId !== null,
    })),
    transfers: projection.contextTransfers.map((transfer) => ({
      type: transfer.type,
      source: aliasOf(transfer.sourceThreadId),
      target: aliasOf(transfer.targetThreadId),
      status: transfer.status,
      resolution:
        transfer.resolution === null
          ? null
          : transfer.resolution.strategy === "native_fork"
            ? {
                strategy: "native_fork",
                nativeId: transfer.resolution.providerThreadRef.nativeId,
              }
            : { strategy: transfer.resolution.strategy },
      sourceRunOrdinal: runOrdinal(transfer.sourceThreadId, transfer.sourcePoint.runId),
      baseRunOrdinal:
        transfer.basePoint === null
          ? null
          : runOrdinal(transfer.basePoint.threadId, transfer.basePoint.runId),
      targetRunOrdinal: runOrdinal(transfer.targetThreadId, transfer.targetRunId),
      sourceProviderInstanceId: transfer.sourceProviderInstanceId,
      targetProviderInstanceId: transfer.targetProviderInstanceId,
      error: transfer.error,
      consumed: transfer.consumedAt !== null,
    })),
    handoffs: projection.contextHandoffs.map((handoff) => ({
      strategy: handoff.strategy,
      status: handoff.status,
      summaryText: handoff.summaryText,
      coveredRunOrdinals: handoff.coveredRunOrdinals,
      delivery:
        handoff.delivery === undefined
          ? null
          : { status: handoff.delivery.status, items: handoff.delivery.itemIds.length },
      history: handoff.history?.messages.map((entry) =>
        pick(entry, ["role", "text", "kind", "status", "threadId"]),
      ),
    })),
    handoffItems: projection.turnItems
      .filter((item) => item.type === "handoff")
      .map((item) =>
        pick(item, [
          "title",
          "strategy",
          "fromProviderInstanceIds",
          "toProviderInstanceId",
          "toModel",
          "summary",
          "status",
        ]),
      ),
    visible: projection.visibleTurnItems.map((row) => ({
      visibility: row.visibility,
      type: row.item.type,
      ...(row.item.type === "user_message" || row.item.type === "assistant_message"
        ? { text: row.item.text }
        : {}),
    })),
  };
};

const PROVIDERS = {
  codex: {
    instanceId: "codex",
    model: (scenario) => scenario.model,
    file: "codex_transcript.ndjson",
    harness: CodexOrchestratorReplayHarness,
    decode: (raw, workspace) =>
      CodexOrchestratorReplayHarness.decodeTranscript(
        materializeReplayTranscriptWorkspace(raw, workspace),
      ),
    replace: (scenario) => scenario.replace ?? [],
    scenarios: () => FORK_SCENARIOS.filter((scenario) => !scenario.claudeOnly),
  },
  claude: {
    instanceId: "claudeAgent",
    model: () => CLAUDE_MODEL,
    file: "claude_transcript.ndjson",
    harness: ClaudeOrchestratorReplayHarness,
    decode: (raw) => ClaudeOrchestratorReplayHarness.decodeTranscript(raw),
    replace: (scenario) => scenario.claudeReplace?.((alias) => threadIdOf(scenario, alias)) ?? [],
    scenarios: () => FORK_SCENARIOS,
  },
};

export const forkScenarios = (normalize, providerName = "codex") =>
  Effect.gen(function* () {
    const provider = PROVIDERS[providerName];
    const results = [];
    for (const scenario of provider.scenarios()) {
      const transcriptFile = new URL(`${scenario.name}/${provider.file}`, FIXTURES);
      let raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
        Effect.provide(NodeServices.layer),
      );
      for (const [from, to] of provider.replace(scenario)) raw = replaceExact(raw, from, to);
      const workspace = yield* checkpointWorkspace(scenario.name, {
        "README.md": "# thread fork\n",
      });
      const transcript = yield* provider.decode(raw, workspace);
      const prompts = raw.metadata?.prompts;
      const scenarioSteps = stepsOf(scenario, prompts);
      scenario.resolved = scenarioSteps;
      const materialized = yield* Effect.gen(function* () {
        const ids = yield* IdAllocator.IdAllocatorV2;
        const projectId = yield* ids.allocate.project({ fixtureName: scenario.name });
        return {
          ids,
          projectId,
          ...(yield* commandsOf(scenario, scenarioSteps, ids, projectId, provider)),
        };
      }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
      scenario.threadAliases = Object.fromEntries(
        materialized.aliases.map((alias) => [alias, threadIdOf(scenario, alias)]),
      );
      // The recorded rollouts predate native history injection: the adapter's injection is answered
      // here and recorded, as the original's own merge-back test does.
      const injections = [];
      const History =
        providerName !== "codex"
          ? provider.harness
          : {
              ...CodexOrchestratorReplayHarness,
              makeProviderAdapterRegistryLayer: (recorded, options) =>
                Layer.effect(
                  ProviderAdapterRegistry.ProviderAdapterRegistryV2,
                  Effect.gen(function* () {
                    const registry = yield* ProviderAdapterRegistry.ProviderAdapterRegistryV2;
                    return {
                      ...registry,
                      get: (id) =>
                        registry.get(id).pipe(
                          Effect.map((adapter) => ({
                            ...adapter,
                            openSession: (input) =>
                              adapter.openSession(input).pipe(
                                Effect.map((session) => ({
                                  ...session,
                                  injectHistory: (history) =>
                                    Effect.sync(() => {
                                      injections.push({
                                        messages: history.messages.map((entry) =>
                                          pick(entry, [
                                            "role",
                                            "text",
                                            "kind",
                                            "status",
                                            "threadId",
                                          ]),
                                        ),
                                        context: history.context,
                                      });
                                      return true;
                                    }),
                                })),
                              ),
                          })),
                        ),
                    };
                  }),
                ).pipe(
                  Layer.provide(
                    CodexOrchestratorReplayHarness.makeProviderAdapterRegistryLayer(
                      recorded,
                      options,
                    ),
                  ),
                ),
            };
      const result = yield* runOrchestratorV2ProviderReplayScenario(
        {
          name: `${scenario.name}/${providerName}`,
          transcript,
          commands: materialized.commands,
          steps: materialized.steps,
          projectionThreadIds: materialized.aliases.map((alias) => threadIdOf(scenario, alias)),
          runtimePolicyOverride: {
            cwd: workspace,
            ...(providerName === "codex" ? scenario.policy : {}),
          },
        },
        History,
      ).pipe(provideDeterministicTestRuntime);
      const byThreadId = new Map(
        [...result.projections.values()].map((projection) => [projection.thread.id, projection]),
      );
      const shellOf = (threadId) => {
        const shell = result.shellSnapshot.threads.find((candidate) => candidate.id === threadId);
        return shell === undefined
          ? null
          : {
              visibleItemCount: shell.visibleItemCount,
              relationship: shell.lineage.relationshipToParent,
              forkedFrom: shell.forkedFrom?.type ?? null,
            };
      };
      results.push({
        name: scenario.name,
        model: provider.model(scenario),
        policy: providerName === "codex" ? (scenario.policy ?? null) : null,
        threadIds: scenario.threadAliases,
        sourceTitle: scenario.threads.source,
        steps: scenarioSteps,
        transcript: readFileSync(transcriptFile.pathname, "utf8")
          .split("\n")
          .filter(Boolean)
          .map((line) =>
            provider
              .replace(scenario)
              .reduce((entry, [from, to]) => replaceExact(entry, from, to), JSON.parse(line)),
          ),
        threads: Object.fromEntries(
          materialized.aliases.map((alias) => {
            const threadId = threadIdOf(scenario, alias);
            return [
              alias,
              {
                ...describeThread(scenario, byThreadId.get(threadId), byThreadId, normalize),
                shell: shellOf(threadId),
              },
            ];
          }),
        ),
        injections,
        // How many times a duplicate fork command was committed: once.
        forkTransfers: result.domainEvents.filter(
          (event) => event.type === "context-transfer.created",
        ).length,
      });
    }
    return results;
  }).pipe(Effect.scoped);
