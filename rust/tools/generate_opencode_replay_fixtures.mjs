// Runs the unchanged TypeScript orchestrator over the recorded OpenCode transcripts with the unchanged
// OpenCode adapters (1.x over the SDK boundary, 2.x over the HTTP boundary), and records the user-visible
// projection it persisted next to each recording, one file per scenario, for the Rust OpenCode adapter to
// reproduce. No opencode binary runs and nothing touches a network.
import { mkdirSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { OpenCodeOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/OpenCodeAdapterV2.testkit.ts";
import {
  OPENCODE2_HTTP_PROTOCOL,
  OpenCode2OrchestratorReplayHarness,
} from "../../apps/server/src/orchestration-v2/Adapters/OpenCode2AdapterV2.testkit.ts";
import {
  CommandId,
  MessageId,
  ProjectId,
  ThreadId,
} from "../../apps/server/node_modules/@t3tools/contracts/src/index.ts";
import { decodeProviderReplayNdjson } from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import { ORCHESTRATOR_REPLAY_FIXTURES } from "../../apps/server/src/orchestration-v2/testkit/fixtures/index.ts";
import { materializeFixtureInput } from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import { runOrchestratorV2ProviderReplayScenario } from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import {
  readProviderReplayTranscript,
  materializeReplayTranscriptRuntimeInstructions,
} from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";

// Every recorded OpenCode scenario of the original's replay registry: six on the 1.x SDK boundary, sixteen
// on the 2.x HTTP boundary.
const NAMES = [
  "simple",
  "turn_interrupt",
  "plan_questions",
  "opencode_subagent",
  "opencode_child_approval",
  "opencode_running_child_approval",
  "opencode2_simple",
  "opencode2_tool_call",
  "opencode2_interrupt",
  "opencode2_inbox",
  "opencode2_revert",
  "opencode2_permission",
  "opencode2_question",
  "opencode2_subagent",
  "opencode2_background",
  "opencode2_nested_background",
  "opencode2_compaction",
  "opencode2_resume_after_restart",
  "opencode2_command",
  "opencode2_skill",
];
// The two recordings the original drives with scripted commands of its own (`OpenCode2OrchestratorV2.integration.test.ts`).
const SCRIPTED = ["opencode2_switch", "opencode2_fork"];
const only = process.argv.slice(2);
const bigPickle = { instanceId: "opencode", model: "opencode/big-pickle" };
const nemotron = { instanceId: "opencode", model: "opencode/nemotron-3.5-lightning-free" };
const pick = (object, keys) =>
  Object.fromEntries(
    keys.filter((key) => object[key] !== undefined).map((key) => [key, object[key]]),
  );
// The user-visible content of a turn item: what a client renders, without ids and timestamps.
const BY_TYPE = {
  user_message: ["text"],
  assistant_message: ["text", "streaming"],
  reasoning: ["text", "streaming"],
  command_execution: ["input", "output", "exitCode"],
  file_change: ["fileName", "oldStr", "newStr", "diffStr"],
  file_search: ["pattern", "results"],
  web_search: ["patterns", "results"],
  dynamic_tool: ["toolName", "input", "output", "viewedImagePath", "toolSource"],
  todo_list: ["steps"],
  proposed_plan: ["markdown"],
  approval_request: ["requestKind", "prompt", "options"],
  user_input_request: ["questions"],
  run_interrupt_request: ["message"],
  run_interrupt_result: ["message"],
  compaction: ["summary"],
  subagent: ["prompt", "result"],
  system_notice: ["message"],
};
const normalizeItem = (item) => ({
  type: item.type,
  status: item.status,
  title: item.title ?? null,
  ...(item.toolNonExecutionKind === undefined
    ? {}
    : { toolNonExecutionKind: item.toolNonExecutionKind }),
  ...pick(item, BY_TYPE[item.type] ?? []),
  ...(item.type === "error"
    ? { failure: pick(item.failure, ["class", "message", "code", "retryable"]) }
    : {}),
});
const normalize = (projection) => ({
  runs: projection.runs.map((run) => run.status),
  attempts: projection.attempts.map((attempt) => attempt.status),
  providerTurns: projection.providerTurns.map((turn) => ({
    status: turn.status,
    turnTokenUsage: turn.turnTokenUsage ?? null,
    tokenUsage:
      turn.tokenUsage === undefined
        ? null
        : pick(turn.tokenUsage, [
            "usedTokens",
            "maxTokens",
            "inputTokens",
            "cachedInputTokens",
            "outputTokens",
            "reasoningOutputTokens",
          ]),
  })),
  items: [...projection.turnItems]
    .filter((item) => item.type !== "checkpoint")
    .sort((a, b) => a.ordinal - b.ordinal)
    .map(normalizeItem),
  requests: projection.runtimeRequests.map((request) => ({
    kind: request.kind,
    status: request.status,
  })),
  plans: projection.plans.map((plan) => pick(plan, ["kind", "status", "steps", "markdown"])),
  providerThread: projection.providerThreads.map((thread) => thread.status),
});

const target = new URL("../crates/server/tests/fixtures/opencode-replay/", import.meta.url);
mkdirSync(target, { recursive: true });
let count = 0;
for (const name of NAMES) {
  if (only.length > 0 && !only.includes(name)) continue;
  const fixture = ORCHESTRATOR_REPLAY_FIXTURES.find((candidate) => candidate.name === name);
  const driver = fixture.providers.find((provider) => provider.driver === "opencode");
  const input = fixture.buildInput();
  const transcriptFile = driver.transcriptFile;
  const materializedReplay = Effect.gen(function* () {
    const raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
      Effect.provide(NodeServices.layer),
    );
    return materializeReplayTranscriptRuntimeInstructions(raw, {
      driver: driver.driver,
      model: driver.modelSelection.model,
      modelSelection: driver.modelSelection,
    });
  }).pipe(Effect.scoped);
  let workspacePath = null;
  let harness = null;
  const program = Effect.gen(function* () {
    const replay = yield* materializedReplay;
    harness =
      replay.protocol === OPENCODE2_HTTP_PROTOCOL
        ? OpenCode2OrchestratorReplayHarness
        : OpenCodeOrchestratorReplayHarness;
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    workspacePath = workspace;
    const transcript = yield* harness.decodeTranscript(replay);
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: driver.driver,
      modelSelection: driver.modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const scenario = {
      name: `${name}/opencode`,
      transcript,
      commands: materialized.commands,
      steps: materialized.steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { ...driver.runtimePolicyOverride, cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(
      scenario,
      harness,
      driver.runContinuationWorker === true ? { runContinuationWorker: true } : {},
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program);
  const projection = result.projections.get([...result.projections.keys()][0]);
  const replayed = await Effect.runPromise(materializedReplay);
  const threadsById = [...result.projections.values()];
  const titleOf = (id) =>
    threadsById.find((candidate) => candidate.thread.id === id)?.thread.title ?? null;
  const threads =
    threadsById.length === 1
      ? undefined
      : threadsById.map((candidate) => ({
          title: candidate.thread.title,
          parent: titleOf(candidate.thread.lineage.parentThreadId),
          relationship: candidate.thread.lineage.relationshipToParent,
          subagents: candidate.subagents.map((subagent) => ({
            ...pick(subagent, [
              "origin",
              "createdBy",
              "driver",
              "status",
              "prompt",
              "title",
              "model",
              "result",
            ]),
            child: titleOf(subagent.childThreadId),
          })),
          expected: normalize(candidate),
        }));
  const row = {
    name,
    protocol: replayed.protocol,
    version: replayed.version,
    approvalPolicy: driver.runtimePolicyOverride?.approvalPolicy ?? null,
    sandboxPolicy: driver.runtimePolicyOverride?.sandboxPolicy ?? null,
    runtimeMode: input.runtimeMode ?? "full-access",
    interactionMode: input.interactionMode ?? "default",
    runContinuationWorker: driver.runContinuationWorker === true,
    // The directory the original ran in: its paths in the projection are rewritten to the Rust one's.
    workspace: workspacePath,
    model: driver.modelSelection.model,
    modelSelection: driver.modelSelection,
    steps: input.steps,
    transcript: replayed.entries,
    expected: normalize(projection),
    ...(threads === undefined ? {} : { threads }),
  };
  writeFileSync(new URL(`${name}.json`, target), JSON.stringify(row, null, 1) + "\n");
  console.log(
    `${name}: ${row.expected.items.length} items, ${row.transcript.length} entries, steps ${input.steps.map((s) => s.type).join(",")}`,
  );
  count += 1;
}
// A recording with its scrubbed `<work>` directory replaced by `directory`.
const withDirectory = (value, directory) => {
  const replace = (entry) =>
    entry === "<work>"
      ? directory
      : Array.isArray(entry)
        ? entry.map(replace)
        : typeof entry === "object" && entry !== null
          ? Object.fromEntries(Object.entries(entry).map(([key, inner]) => [key, replace(inner)]))
          : entry;
  return replace(value);
};
for (const name of SCRIPTED) {
  if (only.length > 0 && !only.includes(name)) continue;
  const thread = (threadName, cwd) => {
    const threadId = ThreadId.make(`thread:${threadName}`);
    const command = (key) => CommandId.make(`command:${threadName}:${key}`);
    return {
      threadId,
      create: {
        type: "thread.create",
        createdBy: "user",
        creationSource: "web",
        commandId: command("create"),
        threadId,
        projectId: ProjectId.make(`project:${threadName}`),
        title: threadName,
        modelSelection: bigPickle,
        runtimeMode: "full-access",
        interactionMode: "default",
        branch: null,
        worktreePath: cwd,
      },
      message: (key, modelSelection = bigPickle, text = `Reply with exactly: ${key}`) => ({
        type: "message.dispatch",
        createdBy: "user",
        creationSource: "web",
        commandId: command(key),
        threadId,
        messageId: MessageId.make(`message:${threadName}:${key}`),
        text,
        attachments: [],
        modelSelection,
        dispatchMode: { type: "start_immediately" },
      }),
      interactionMode: (key, interactionMode) => ({
        type: "thread.interaction-mode.set",
        commandId: command(key),
        threadId,
        interactionMode,
      }),
      command,
    };
  };
  let workspacePath = null;
  let scripted = null;
  const program = Effect.gen(function* () {
    const workspace = yield* checkpointWorkspace(name, undefined);
    workspacePath = workspace;
    const text = yield* Effect.promise(() =>
      import("node:fs/promises").then((fs) =>
        fs.readFile(
          new URL(
            `../../apps/server/src/orchestration-v2/testkit/fixtures/${name}/opencode_transcript.ndjson`,
            import.meta.url,
          ),
          "utf8",
        ),
      ),
    );
    const recorded = yield* decodeProviderReplayNdjson(text);
    const transcript = yield* OpenCode2OrchestratorReplayHarness.decodeTranscript(
      withDirectory(recorded, workspace),
    );
    // The recordings name the thread in the rules they expect (`t3-code-thread_<name>_*`), as the original's test named it.
    const threadName = name === "opencode2_fork" ? "opencode2-fork" : name;
    const source = thread(threadName, workspace);
    let commands;
    let script;
    let projectionThreadIds = [source.threadId];
    if (name === "opencode2_switch") {
      commands = [
        source.create,
        source.interactionMode("mode-plan", "plan"),
        source.message(
          "plan",
          bigPickle,
          "Create a file named plan_probe.txt containing HI using the write tool.",
        ),
        source.interactionMode("mode-default", "default"),
        source.message("switched", nemotron, "Reply exactly SWITCHED."),
      ];
      script = [
        { type: "mode", mode: "plan" },
        {
          type: "message",
          text: "Create a file named plan_probe.txt containing HI using the write tool.",
          modelSelection: bigPickle,
        },
        { type: "mode", mode: "default" },
        { type: "message", text: "Reply exactly SWITCHED.", modelSelection: nemotron },
      ];
    } else {
      const ids = yield* IdAllocator.IdAllocatorV2;
      const target = ThreadId.make(`thread:${threadName}:target`);
      projectionThreadIds = [source.threadId, target];
      commands = [
        source.create,
        source.message("one"),
        source.message("two"),
        {
          type: "thread.fork",
          createdBy: "user",
          creationSource: "web",
          commandId: source.command("fork"),
          sourceThreadId: source.threadId,
          targetThreadId: target,
          sourcePoint: {
            type: "run",
            runId: ids.derive.run({ threadId: source.threadId, ordinal: 1 }),
          },
        },
        {
          ...source.message("repeat"),
          threadId: target,
          messageId: MessageId.make(`message:${threadName}:repeat`),
        },
      ];
      script = [
        { type: "message", text: "Reply with exactly: one", modelSelection: bigPickle },
        { type: "message", text: "Reply with exactly: two", modelSelection: bigPickle },
        { type: "fork", sourceRunIndex: 1 },
        {
          type: "message",
          text: "Reply with exactly: repeat",
          modelSelection: bigPickle,
          thread: "target",
        },
      ];
    }
    scripted = { script, recorded: transcript };
    const steps = commands.flatMap((command) => [
      { type: "dispatch", command },
      { type: "advance_clock", duration: "1 millis" },
      ...(command.type === "message.dispatch"
        ? [{ type: "await_thread_idle", threadId: command.threadId }]
        : []),
    ]);
    return yield* runOrchestratorV2ProviderReplayScenario(
      { name, transcript, commands, steps, projectionThreadIds },
      OpenCode2OrchestratorReplayHarness,
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.provide(IdAllocator.layer), Effect.provide(NodeServices.layer), Effect.scoped);
  const result = await Effect.runPromise(program);
  const projections = [...result.projections.values()];
  const root = projections[0];
  const forked = projections[1];
  const row = {
    name,
    protocol: "opencode2-http.sse",
    version: "2.0.18",
    approvalPolicy: null,
    sandboxPolicy: null,
    runtimeMode: "full-access",
    interactionMode: "default",
    runContinuationWorker: false,
    workspace: workspacePath,
    model: bigPickle.model,
    modelSelection: bigPickle,
    steps: scripted.script,
    transcript: (
      await Effect.runPromise(
        Effect.promise(async () =>
          (await import("node:fs/promises")).readFile(
            new URL(
              `../../apps/server/src/orchestration-v2/testkit/fixtures/${name}/opencode_transcript.ndjson`,
              import.meta.url,
            ),
            "utf8",
          ),
        ),
      )
    )
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line))
      .filter((entry) => entry.type !== "transcript_start")
      .map((entry) =>
        JSON.parse(JSON.stringify(entry).replaceAll('"<work>"', JSON.stringify(workspacePath))),
      ),
    expected: normalize(root),
    ...(forked === undefined
      ? {}
      : {
          forked: {
            expected: normalize(forked),
            strategy: forked.contextTransfers[0]?.resolution?.strategy ?? null,
            nativeSession: forked.providerThreads[0]?.nativeThreadRef?.nativeId ?? null,
            visibleAssistantTexts: forked.visibleTurnItems.flatMap((entry) =>
              entry.item.type === "assistant_message" ? [entry.item.text] : [],
            ),
          },
        }),
  };
  writeFileSync(new URL(`${name}.json`, target), JSON.stringify(row, null, 1) + "\n");
  console.log(`${name}: scripted, ${row.expected.items.length} items`);
  count += 1;
}
console.log(`${count} opencode replay scenarios`);
process.exit(0);
