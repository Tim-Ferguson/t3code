// Runs the unchanged TypeScript orchestrator over the recorded Grok ACP transcripts with the unchanged
// GrokAdapterV2 (and the AcpAdapterV2 it flavours), and records the user-visible projection it persisted
// next to each recording, one file per scenario, for the Rust Grok adapter to reproduce. No grok binary
// runs and nothing touches a network.
import { mkdirSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { GrokOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/GrokAdapterV2.testkit.ts";
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

// Every recorded Grok scenario of the original's replay registry.
const NAMES = [
  "simple",
  "multi_turn",
  "queued_turn",
  "todo_list",
  "message_steering",
  "turn_interrupt",
  "tool_call_read_only",
  "tool_call_read_only_on_request",
  "plan_questions",
  "grok_prompt_error",
  "grok_auto_blocked_command",
  "grok_subagent_lineage",
  "grok_background_bash",
  "grok_background_bash_fast_wake",
  "grok_background_subagent",
  "grok_monitor",
];
const only = process.argv.slice(2);
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
  file_change: ["fileName", "diffStr"],
  web_search: ["patterns", "results"],
  dynamic_tool: ["toolName", "input", "output", "viewedImagePath", "toolSource"],
  todo_list: ["steps"],
  proposed_plan: ["markdown"],
  approval_request: ["requestKind", "prompt"],
  user_input_request: ["questions"],
  run_interrupt_request: ["message"],
  run_interrupt_result: ["message"],
  compaction: [],
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

const target = new URL("../crates/server/tests/fixtures/grok-replay/", import.meta.url);
mkdirSync(target, { recursive: true });
let count = 0;
for (const name of NAMES) {
  if (only.length > 0 && !only.includes(name)) continue;
  const fixture = ORCHESTRATOR_REPLAY_FIXTURES.find((candidate) => candidate.name === name);
  const driver = fixture.providers.find((provider) => provider.driver === "grok");
  const input = fixture.buildInput();
  const transcriptFile = driver.transcriptFile;
  const materializedReplay = Effect.gen(function* () {
    const raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
      Effect.provide(NodeServices.layer),
    );
    return materializeReplayTranscriptRuntimeInstructions(raw, {
      driver: driver.driver,
      model: driver.modelSelection.model,
    });
  }).pipe(Effect.scoped);
  let workspacePath = null;
  const program = Effect.gen(function* () {
    const replay = yield* materializedReplay;
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    workspacePath = workspace;
    const transcript = yield* GrokOrchestratorReplayHarness.decodeTranscript(replay);
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: driver.driver,
      modelSelection: driver.modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const scenario = {
      name: `${name}/grok`,
      transcript,
      commands: materialized.commands,
      steps: materialized.steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { ...driver.runtimePolicyOverride, cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(
      scenario,
      GrokOrchestratorReplayHarness,
      driver.runContinuationWorker === true ? { runContinuationWorker: true } : {},
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program);
  const projection = result.projections.get([...result.projections.keys()][0]);
  const entries = (await Effect.runPromise(materializedReplay)).entries;
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
    approvalPolicy: driver.runtimePolicyOverride?.approvalPolicy ?? null,
    sandboxPolicy: driver.runtimePolicyOverride?.sandboxPolicy ?? null,
    runtimeMode: input.runtimeMode ?? "full-access",
    // The directory the original ran in: its paths in the projection are rewritten to the Rust one's.
    workspace: workspacePath,
    model: driver.modelSelection.model,
    interactionMode: input.interactionMode ?? "default",
    steps: input.steps,
    transcript: entries,
    expected: normalize(projection),
    ...(threads === undefined ? {} : { threads }),
  };
  writeFileSync(new URL(`${name}.json`, target), JSON.stringify(row, null, 1) + "\n");
  count += 1;
}
console.log(`${count} grok replay scenarios`);
process.exit(0);
