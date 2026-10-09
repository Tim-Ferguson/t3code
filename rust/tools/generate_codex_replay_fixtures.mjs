// Runs the unchanged TypeScript orchestrator over every recorded Codex app-server transcript with
// the unchanged CodexAdapterV2, and records the user-visible projection it persisted next to each
// recording, for the Rust adapter to reproduce. No codex binary runs and nothing touches a network.
//
//   node rust/tools/generate_codex_replay_fixtures.mjs
//
// Writes crates/server/tests/fixtures/codex-replay.json. Scenarios that need orchestration the
// Rust port does not have are listed in EXCLUDED with the reason; the Rust test names them too.
import { readFileSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { CodexOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.testkit.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import { ORCHESTRATOR_REPLAY_FIXTURES } from "../../apps/server/src/orchestration-v2/testkit/fixtures/index.ts";
import { materializeFixtureInput } from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import { runOrchestratorV2ProviderReplayScenario } from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import {
  materializeReplayTranscriptWorkspace,
  readProviderReplayTranscript,
} from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";

const OUTPUT =
  process.argv[2] ??
  new URL("../crates/server/tests/fixtures/codex-replay.json", import.meta.url).pathname;
const pick = (object, keys) =>
  Object.fromEntries(
    keys.filter((key) => object?.[key] !== undefined).map((key) => [key, object[key]]),
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
  subagent: ["prompt", "result"],
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
  items: projection.turnItems
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.type !== "checkpoint")
    .sort((a, b) => a.item.ordinal - b.item.ordinal || a.index - b.index)
    .map(({ item }) => normalizeItem(item)),
  requests: projection.runtimeRequests.map((request) => ({
    kind: request.kind,
    status: request.status,
  })),
  plans: projection.plans.map((plan) => pick(plan, ["kind", "status", "steps", "markdown"])),
  providerThread: projection.providerThreads.map((thread) => thread.status),
});

// Registered scenarios whose behaviour needs orchestration the Rust port does not have.
const EXCLUDED = {
  thread_rollback_after_restart:
    "advances the test clock past the session idle timeout so the provider session is reopened (a second initialize and thread/resume mid-rollback); the port has no idle session expiry to drive",
};
// Recordings no registered scenario replays; each is driven by a dedicated test of the original.
const EXCLUDED_RECORDINGS = {
  delegated_task_status:
    "a delegated task's status through the orchestrator's delegated-task spawn and completion mailbox (the cohort is ported for Stop, cancel and delete; spawning a task and delivering its completion are not)",
  thread_fork_native:
    "native thread forking (thread/fork) needs thread fork orchestration, which the port does not have",
  thread_fork_native_continue:
    "native thread forking (thread/fork) needs thread fork orchestration, which the port does not have",
  thread_fork_native_prior_turn:
    "forking at a prior turn boundary needs thread fork orchestration, which the port does not have",
  thread_fork_native_siblings:
    "sibling native forks need thread fork orchestration, which the port does not have",
  thread_merge_back_continue:
    "merging a fork back into its source needs thread fork and context transfer orchestration, which the port does not have",
  thread_merge_back_siblings:
    "merging sibling forks back needs thread fork and context transfer orchestration, which the port does not have",
};
// Scenarios the port's orchestration has but whose Codex adapter half is not ported yet are listed
// here with the reason. Their expectations (including every child thread) are generated all the
// same and kept under `pending`, for the adapter work to replay; the Rust test lists them and does
// not replay them. Empty while every recording the port can orchestrate is replayed.
const PENDING = {};
const only = process.env.CODEX_REPLAY_ONLY?.split(",");

const rows = [];
const pending = [];
const excluded = Object.entries(EXCLUDED_RECORDINGS).map(([name, reason]) => ({ name, reason }));
for (const fixture of ORCHESTRATOR_REPLAY_FIXTURES) {
  const driver = fixture.providers.find((provider) => provider.driver === "codex");
  if (driver === undefined) continue;
  const name = fixture.name;
  if (EXCLUDED[name] !== undefined) {
    excluded.push({ name, reason: EXCLUDED[name] });
    continue;
  }
  if (only !== undefined && !only.includes(name)) continue;
  if (PENDING[name] !== undefined) excluded.push({ name, reason: PENDING[name] });
  const input = fixture.buildInput();
  const transcriptFile = driver.transcriptFile;
  const program = Effect.gen(function* () {
    const raw = yield* readProviderReplayTranscript(transcriptFile).pipe(
      Effect.provide(NodeServices.layer),
    );
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    const transcript = yield* CodexOrchestratorReplayHarness.decodeTranscript(
      materializeReplayTranscriptWorkspace(raw, workspace),
    );
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: driver.driver,
      modelSelection: driver.modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const scenario = {
      name: `${name}/codex`,
      transcript,
      commands: materialized.commands,
      steps: materialized.steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { ...driver.runtimePolicyOverride, cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(
      scenario,
      CodexOrchestratorReplayHarness,
      driver.runContinuationWorker === true ? { runContinuationWorker: true } : {},
    ).pipe(provideDeterministicTestRuntime);
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program);
  const projection = result.projections.get([...result.projections.keys()][0]);
  let entries = readFileSync(new URL(transcriptFile).pathname, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  // A fixture can use only the start of a recording (`queued_cancelled_while_active`).
  if (driver.transcriptEntriesThroughLabel !== undefined) {
    const end = entries.findIndex(
      (entry) =>
        entry.type === "emit_inbound" && entry.label === driver.transcriptEntriesThroughLabel,
    );
    entries = entries.slice(0, end + 1);
  }
  // Every thread the scenario created, the root first: a subagent's child thread and its own
  // children are threads of their own, found by title and lineage.
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
  (PENDING[name] === undefined ? rows : pending).push({
    name,
    model: driver.modelSelection.model,
    interactionMode: input.interactionMode ?? "default",
    runtimePolicy: driver.runtimePolicyOverride ?? null,
    steps: input.steps,
    transcriptThroughLabel: driver.transcriptEntriesThroughLabel ?? null,
    transcript: entries,
    expected: normalize(projection),
    ...(threads === undefined ? {} : { threads }),
    // The thread shell the scenario captured mid-run, with run ids as their position in the thread.
    shells: Object.fromEntries(
      [...result.capturedShellSnapshots.entries()].map(([key, snapshot]) => {
        const shell = snapshot.threads.find((thread) => thread.id === projection.thread.id);
        const index = (id) =>
          id === null || id === undefined
            ? null
            : projection.runs.findIndex((run) => run.id === id) + 1;
        return [
          key,
          {
            latestRun: index(shell.latestRunId),
            activeRun: index(shell.activeRunId),
            status: shell.status,
            activityRunStatus: shell.activityRunStatus ?? null,
            pendingBackgroundTasks: shell.pendingBackgroundTasks,
          },
        ];
      }),
    ),
  });
}
// The recording whose second turn runs after the orchestrator was recreated (resumed native thread).
// It has no projection of its own here: the Rust test replays it across a server restart and checks
// the frames the adapter sends and the assistant text it persists.
const resumeFile = new URL(
  "../../apps/server/src/orchestration-v2/testkit/fixtures/provider_thread_resume/codex_transcript.ndjson",
  import.meta.url,
);
const resumeEntries = readFileSync(resumeFile, "utf8")
  .split("\n")
  .filter(Boolean)
  .map((line) => JSON.parse(line));
const recovery = [
  {
    name: "provider_thread_resume",
    model: "gpt-6-luna",
    transcript: resumeEntries,
    prompts: resumeEntries
      .filter((entry) => entry.type === "expect_outbound" && entry.frame.method === "turn/start")
      .map((entry) => entry.frame.params.input[0].text),
    finals: resumeEntries
      .filter(
        (entry) =>
          entry.type === "emit_inbound" &&
          entry.frame.method === "item/completed" &&
          entry.frame.params.item.type === "agentMessage",
      )
      .map((entry) => entry.frame.params.item.text),
  },
];
writeFileSync(
  OUTPUT,
  JSON.stringify({ excluded, scenarios: rows, pending, recovery }, null, 1) + "\n",
);
console.log(`${rows.length} replay scenarios, ${excluded.length} excluded`);
process.exit(0);
