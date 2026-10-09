// Runs the unchanged TypeScript orchestrator over the recorded ACP registry replay transcripts with the
// unchanged AcpRegistryAdapterV2 and AcpAdapterV2, and records the user-visible projection it produced
// next to each recording, for the Rust adapter to reproduce. No ACP agent runs and nothing touches a network.
import { readFileSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { AcpRegistryOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/AcpRegistryAdapterV2.testkit.ts";
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

const NAMES = [
  "simple",
  "multi_turn",
  "queued_turn",
  "todo_list",
  "message_steering",
  "turn_interrupt",
  "tool_call_read_only",
  "tool_call_read_only_on_request",
  "acp_elicitation",
  "stop_background_work_after_failed_turn",
  "stop_background_work_after_release",
];
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

const rows = [];
for (const name of NAMES) {
  const fixture = ORCHESTRATOR_REPLAY_FIXTURES.find((candidate) => candidate.name === name);
  const driver = fixture.providers.find((provider) => provider.driver === "acpRegistry");
  const input = fixture.buildInput();
  const transcriptFile = driver.transcriptFile;
  const program = Effect.gen(function* () {
    const raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
      Effect.provide(NodeServices.layer),
    );
    const replay = materializeReplayTranscriptRuntimeInstructions(raw, {
      driver: driver.driver,
      model: driver.modelSelection.model,
    });
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    const transcript = yield* AcpRegistryOrchestratorReplayHarness.decodeTranscript(replay);
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: driver.driver,
      modelSelection: driver.modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const scenario = {
      name: `${name}/claude`,
      transcript,
      commands: materialized.commands,
      steps: materialized.steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { ...driver.runtimePolicyOverride, cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(
      scenario,
      AcpRegistryOrchestratorReplayHarness,
      driver.runContinuationWorker === true ? { runContinuationWorker: true } : {},
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program);
  const projection = result.projections.get([...result.projections.keys()][0]);
  // The transcript as the adapter was driven with it: runtime instructions appended to each prompt.
  const entries = (
    await Effect.runPromise(
      Effect.gen(function* () {
        const raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
          Effect.provide(NodeServices.layer),
        );
        return materializeReplayTranscriptRuntimeInstructions(raw, {
          driver: driver.driver,
          model: driver.modelSelection.model,
        });
      }).pipe(Effect.scoped),
    )
  ).entries;
  rows.push({
    name,
    approvalPolicy: driver.runtimePolicyOverride?.approvalPolicy ?? null,
    model: driver.modelSelection.model,
    interactionMode: input.interactionMode ?? "default",
    steps: input.steps,
    transcript: entries,
    expected: normalize(projection),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-replay.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} replay scenarios`);
process.exit(0);
