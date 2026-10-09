// Runs the unchanged TypeScript orchestrator over the recorded Claude replay fixtures with the
// unchanged ClaudeAdapterV2, and records the user-visible projection it produced next to each
// recording, for the Rust adapter to reproduce. No claude binary runs and nothing touches a network.
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { ClaudeOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.testkit.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import { ORCHESTRATOR_REPLAY_FIXTURES } from "../../apps/server/src/orchestration-v2/testkit/fixtures/index.ts";
import { materializeFixtureInput } from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import { runOrchestratorV2ProviderReplayScenario } from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import { forkScenarios } from "./fork_scenarios.mjs";
import {
  readProviderReplayTranscript,
  materializeReplayTranscriptRuntimeInstructions,
} from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";

// Every recorded Claude scenario of the original's replay registry, except the forks and merge-backs
// (several threads over one recording: see `forks`) and the one replayed across a restart (see `recovery`).
const SUBAGENT_AND_WAKE = [
  "subagent",
  "claude_nested_subagent_model",
  "claude_nested_background_subagent_wake",
  "claude_background_subagent_after_root",
  "claude_background_subagent_lifecycle",
  "claude_background_task_after_root",
  "claude_background_task_interrupt",
  "claude_background_task_wake",
  "claude_background_monitor_wake",
  "claude_background_wake_before_queued_prompt",
  "claude_background_wake_before_queued_prompt_no_echo",
  "claude_compact_after_resume_wake",
  "claude_compact_after_peer_turn",
];
const NAMES = [
  "simple",
  "multi_turn",
  "multi_turn_restart",
  "queued_turn",
  "message_steering",
  "tool_call_denied_write",
  "tool_call_read_only",
  "tool_call_read_only_on_request",
  "tool_call_workspace_never",
  "tool_call_restricted_granular",
  "turn_interrupt",
  "turn_interrupt_mid_tool",
  "turn_interrupt_restart",
  "web_search",
  "thread_rollback",
  "claude_idle_resume",
  "claude_result_is_error",
  "claude_mcp_tool_presentation",
  "claude_local_bash_task",
  "claude_compact_after_peer_turn_no_echo",
  ...SUBAGENT_AND_WAKE,
];
const FORKS = [
  "thread_fork_native",
  "thread_fork_native_continue",
  "thread_fork_native_fork_local_rollback",
  "thread_fork_native_prior_turn",
  "thread_fork_native_siblings",
  "thread_merge_back_continue",
  "thread_merge_back_siblings",
];
const EXCLUDED = [];
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

const rows = [];
for (const name of NAMES) {
  const fixture = ORCHESTRATOR_REPLAY_FIXTURES.find((candidate) => candidate.name === name);
  const driver = fixture.providers.find((provider) => provider.driver === "claudeAgent");
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
    const transcript = yield* ClaudeOrchestratorReplayHarness.decodeTranscript(replay);
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
      ClaudeOrchestratorReplayHarness,
      driver.runContinuationWorker === true ? { runContinuationWorker: true } : {},
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program);
  const projection = result.projections.get([...result.projections.keys()][0]);
  const entries = readFileSync(new URL(transcriptFile).pathname, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
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
  rows.push({
    name,
    model: driver.modelSelection.model,
    interactionMode: input.interactionMode ?? "default",
    steps: input.steps,
    transcript: entries,
    expected: normalize(projection),
    ...(threads === undefined ? {} : { threads }),
  });
}
const fixtures = new URL(
  "../../apps/server/src/orchestration-v2/testkit/fixtures/",
  import.meta.url,
).pathname;
const recorded = readdirSync(fixtures).filter((directory) =>
  existsSync(`${fixtures}${directory}/claude_transcript.ndjson`),
);
const unaccounted = recorded.filter(
  (name) =>
    !NAMES.includes(name) &&
    !FORKS.includes(name) &&
    name !== "claude_subagent_resume_after_restart" &&
    !EXCLUDED.some((entry) => entry.name === name),
);
if (unaccounted.length > 0)
  throw new Error(`recordings neither replayed nor excluded: ${unaccounted}`);
// A subagent launched before a server restart and resumed after it. The original replays it as two
// runtimes over one database (OrchestratorReplayRecovery.integration.test.ts), so it has no projection
// here; the Rust test restarts its server between the prompts and checks what that test checks.
const resumeEntries = readFileSync(
  `${fixtures}claude_subagent_resume_after_restart/claude_transcript.ndjson`,
  "utf8",
)
  .split("\n")
  .filter(Boolean)
  .map((line) => JSON.parse(line));
const recovery = [
  {
    name: "claude_subagent_resume_after_restart",
    model: resumeEntries[0].metadata.model,
    prompts: resumeEntries[0].metadata.prompts,
    transcript: resumeEntries,
  },
];
const forks = await Effect.runPromise(forkScenarios(normalize, "claude"));
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-replay.json", import.meta.url),
  JSON.stringify(
    { recorded: recorded.length, scenarios: rows, forks, recovery, excluded: EXCLUDED },
    null,
    1,
  ) + "\n",
);
console.log(`${rows.length} replay scenarios`);
process.exit(0);
