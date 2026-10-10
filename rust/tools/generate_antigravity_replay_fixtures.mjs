// The original has no recorded Antigravity sessions (its adapter is tested with a mock agent, not a replay
// transcript), so this runs the unchanged TypeScript orchestrator and the unchanged AntigravityAdapterV2 (with
// the AcpAdapterV2 it flavours) over scripted ACP peers instead: one transcript per scenario, authored here,
// in the shape of the agent's wire traffic (the initialize reply is the one the original keeps in
// packages/effect-acp/test/fixtures/antigravity-initialize.json). The replay agent compares every frame the
// adapter sends with the transcript and fails the scenario on the first difference, so a scenario that runs
// to the end proves the adapter's frames as well as its projection. The user-visible projection the
// orchestrator persisted is written next to each transcript for the Rust Antigravity adapter to reproduce.
// No agent runs and nothing touches a network.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Crypto from "../../apps/server/node_modules/effect/dist/Crypto.js";
import * as FileSystem from "../../apps/server/node_modules/effect/dist/FileSystem.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Path from "../../apps/server/node_modules/effect/dist/Path.js";
import { ChildProcessSpawner } from "../../apps/server/node_modules/effect/dist/process/index.js";
import { ProviderInstanceId } from "../../packages/contracts/src/index.ts";
import { resolveSelfInvocation } from "../../packages/shared/src/nodeRuntime.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as AcpSessionRuntime from "../../apps/server/src/provider/acp/AcpSessionRuntime.ts";
import { normalizeAntigravitySessionUpdate } from "../../apps/server/src/provider/acp/AntigravityProtocol.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import * as ProviderContinuationRequests from "../../apps/server/src/orchestration-v2/ProviderContinuationRequests.ts";
import * as ProviderAdapterRegistry from "../../apps/server/src/orchestration-v2/ProviderAdapterRegistry.ts";
import { makeAcpAdapterV2 } from "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts";
import {
  decodeAcpReplayTranscript,
  makeAcpReplayCompletenessAssertion,
} from "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.testkit.ts";
import { makeAntigravityAcpAdapterFlavor } from "../../apps/server/src/orchestration-v2/Adapters/AntigravityAdapterV2.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import { materializeFixtureInput } from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import {
  makeReplayServerConfig,
  runOrchestratorV2ProviderReplayScenario,
} from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import { ProviderDriverKind } from "../../packages/contracts/src/index.ts";

const DRIVER = ProviderDriverKind.make("antigravity");
const INSTANCE = ProviderInstanceId.make("antigravity");
/** The Rust test replays these; the generator names the scenarios it ran in the order the test checks. */
export const MODEL_DEFAULT = "antigravity-default";
const SESSION = "agy-session-1";
// The manifest's chat default, which the provider-default alias selects when the account offers it.
const MANIFEST_DEFAULT = "gemini-3.8-flash-high";
const only = process.argv.slice(2);

// ---- the scripted peer ----

const out = (label, method, params) => ({
  type: "expect_outbound",
  label,
  frame: { kind: params === undefined ? "request" : "request", method, params },
});
const expectRequest = (label, method, params) => ({
  type: "expect_outbound",
  label: `request:${label ?? method}`,
  frame: { kind: "request", method, params },
});
const respond = (method, result) => ({
  type: "emit_inbound",
  label: `response:${method}`,
  frame: { kind: "response", method, result },
});
const failRespond = (method, error) => ({
  type: "emit_inbound",
  label: `response:${method}`,
  frame: { kind: "response", method, error },
});
const notify = (label, update) => ({
  type: "emit_inbound",
  label,
  frame: { kind: "notification", method: "session/update", params: { sessionId: SESSION, update } },
});
const askClient = (label, method, params) => ({
  type: "emit_inbound",
  label,
  frame: { kind: "request", method, params },
});
const clientAnswered = (label, method, result) => ({
  type: "expect_outbound",
  label,
  frame: { kind: "response", method, result },
});
const text = (label, value) =>
  notify(label, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: value } });
const thought = (label, value) =>
  notify(label, { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: value } });

const MODELS = [
  { value: "gemini-3.8-flash-high", name: "Gemini 3.8 Flash (High)" },
  { value: "gemini-3.8-flash-medium", name: "Gemini 3.8 Flash (Medium)" },
  { value: "gemini-3.8-flash-low", name: "Gemini 3.8 Flash (Low)" },
  { value: "gemini-3.0-pro", name: "Gemini 3.0 Pro" },
];
const MODES = [
  { value: "default", name: "Default" },
  { value: "auto_edit", name: "Auto edit" },
  { value: "yolo", name: "YOLO" },
];
const configOptions = (model, mode) => [
  {
    id: "model",
    name: "Model",
    category: "model",
    type: "select",
    currentValue: model,
    options: MODELS,
  },
  {
    id: "mode",
    name: "Mode",
    category: "mode",
    type: "select",
    currentValue: mode,
    options: MODES,
  },
];
const initializeResult = JSON.parse(
  readFileSync(
    new URL("../../packages/effect-acp/test/fixtures/antigravity-initialize.json", import.meta.url),
    "utf8",
  ),
);
// The agent shape the original parses from the legacy ACP reply (protocol 1 wire format).
initializeResult.protocolVersion = 1;
const COMMANDS = notify("commands", {
  sessionUpdate: "available_commands_update",
  availableCommands: [
    { name: "plan", description: "Plan a task", input: { hint: "task" } },
    { name: "compact", description: "Compact the conversation" },
    { name: "logout", description: "Sign out" },
  ],
});
/**
 * The launch every scenario starts with: initialize, the explicit sign-in the driver configures, a new session,
 * the native commands, then the session set up for the turn: the provider-default alias selects the manifest's
 * default model, and the runtime mode selects the agent's permission mode.
 */
const launch = ({
  mode = "yolo",
  model = MANIFEST_DEFAULT,
  currentModel = "gemini-3.8-flash-low",
  current = "default",
  modes = "config",
} = {}) => [
  expectRequest("initialize", "initialize", {
    protocolVersion: 2,
    // The client's name and version are sent twice, and `capabilities` is empty: the replay protocol's own fields.
    info: { name: "t3-code", version: "0.0.0" },
    capabilities: {},
    clientCapabilities: { fs: { readTextFile: true, writeTextFile: true }, terminal: false },
    clientInfo: { name: "t3-code", version: "0.0.0" },
  }),
  respond("initialize", initializeResult),
  expectRequest("authenticate", "authenticate", { methodId: "oauth-personal" }),
  respond("authenticate", {}),
  expectRequest("session/new", "session/new", {
    cwd: "<workspace>",
    mcpServers: "<any>",
    additionalDirectories: ["<any>"],
  }),
  respond(
    "session/new",
    modes === "config"
      ? { sessionId: SESSION, configOptions: configOptions(currentModel, current) }
      : {
          sessionId: SESSION,
          configOptions: [configOptions(currentModel, current)[0]],
          modes: {
            currentModeId: current,
            availableModes: MODES.map(({ value, name }) => ({ id: value, name })),
          },
        },
  ),
  COMMANDS,
  ...(model === currentModel
    ? []
    : [
        expectRequest("session/set_config_option:model", "session/set_config_option", {
          sessionId: SESSION,
          configId: "model",
          value: model,
        }),
        respond("session/set_config_option", { configOptions: configOptions(model, current) }),
      ]),
  ...(mode === current
    ? []
    : modes === "config"
      ? [
          expectRequest("session/set_config_option:mode", "session/set_config_option", {
            sessionId: SESSION,
            configId: "mode",
            value: mode,
          }),
          respond("session/set_config_option", { configOptions: configOptions(model, mode) }),
        ]
      : [
          expectRequest("session/set_mode", "session/set_mode", {
            sessionId: SESSION,
            modeId: mode,
          }),
          respond("session/set_mode", {}),
        ]),
];
const prompt = () =>
  expectRequest("session/prompt", "session/prompt", { sessionId: SESSION, prompt: "<any>" });
const done = (stopReason = "end_turn") => respond("session/prompt", { stopReason });
const tail = [{ type: "runtime_exit", status: "success" }];

const SHELL_INPUT = { CommandLine: "ls -la", Cwd: "<workspace>" };
const scenarios = {
  // One turn: reasoning, a reply, token usage.
  antigravity_simple: {
    input: {
      steps: [{ type: "message", text: "Respond with the following text: fixture simple ok" }],
    },
    transcript: () => [
      ...launch(),
      prompt(),
      thought("reasoning.1", "I should "),
      thought("reasoning.2", "answer exactly."),
      text("assistant.1", "fixture simple "),
      text("assistant.2", "ok"),
      notify("usage", { sessionUpdate: "usage_update", used: 1200, size: 1_000_000 }),
      done(),
      ...tail,
    ],
  },
  // Two turns in one session: the second reuses the live process and the selections.
  antigravity_multi_turn: {
    input: {
      steps: [
        { type: "message", text: "First question" },
        { type: "message", text: "Second question" },
      ],
    },
    transcript: () => [
      ...launch(),
      prompt(),
      text("assistant.1", "first answer"),
      done(),
      prompt(),
      text("assistant.2", "second answer"),
      done(),
      ...tail,
    ],
  },
  // Native tool calls: a shell command reported as CommandLine/combinedOutput, a read, and an edit with a diff.
  antigravity_tools: {
    input: {
      steps: [{ type: "message", text: "List the files, read package.json and fix the typo" }],
      workspaceFiles: { "package.json": '{"name":"fixture"}\n' },
    },
    transcript: () => [
      ...launch(),
      prompt(),
      text("assistant.1", "Looking. "),
      notify("shell.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-shell",
        title: "Run command",
        kind: "other",
        status: "in_progress",
        rawInput: SHELL_INPUT,
      }),
      notify("shell.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-shell",
        status: "completed",
        rawOutput: {
          combinedOutput: "package.json\n",
          exitCode: 0,
          formattedOutput: "package.json\n",
          imagePath: "<workspace>/shot.png",
        },
      }),
      notify("read.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-read",
        title: "Read package.json",
        kind: "read",
        status: "in_progress",
        rawInput: { path: "<workspace>/package.json" },
      }),
      notify("read.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-read",
        status: "completed",
        content: [{ type: "content", content: { type: "text", text: '{"name":"fixture"}' } }],
      }),
      notify("edit.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-edit",
        title: "Edit package.json",
        kind: "edit",
        status: "in_progress",
        content: [
          {
            type: "diff",
            path: "<workspace>/package.json",
            oldText: '{"name":"fixture"}\n',
            newText: '{"name":"fixed"}\n',
          },
        ],
      }),
      notify("edit.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-edit",
        status: "completed",
      }),
      // An image payload and an oversized output are bounded before they are kept.
      notify("big.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-big",
        title: "Run command",
        kind: "other",
        status: "in_progress",
        rawInput: { command: "cat big.log" },
      }),
      notify("big.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-big",
        status: "completed",
        rawOutput: {
          combined_output: "x".repeat(9000),
          exit_code: 1,
          screenshot: "data:image/png;base64,AAAA",
        },
      }),
      text("assistant.2", "Done."),
      done(),
      ...tail,
    ],
  },
  // An approval the user grants for the thread, and one they decline.
  antigravity_approval: {
    input: {
      runtimeMode: "approval-required",
      steps: [
        { type: "message", text: "Run the tests" },
        { type: "approve_next_runtime_request", decision: "acceptForSession" },
        { type: "message", text: "Delete the build output" },
        { type: "approve_next_runtime_request", decision: "decline" },
      ],
    },
    transcript: () => [
      ...launch({ mode: "default" }),
      prompt(),
      notify("shell.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-test",
        title: "Run npm test",
        kind: "execute",
        status: "pending",
        rawInput: { CommandLine: "npm test" },
      }),
      askClient("permission.1", "session/request_permission", {
        sessionId: SESSION,
        toolCall: {
          toolCallId: "tool-test",
          title: "Run npm test",
          kind: "execute",
          rawInput: { CommandLine: "npm test" },
        },
        options: [
          { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
          {
            optionId: "allow-always",
            name: "Allow always",
            kind: "allow_always",
            _meta: {
              "agy.security.warning": {
                title: "Warning",
                message: "  Always allowing shell commands can be unsafe.  ",
              },
            },
          },
          { optionId: "reject", name: "Deny", kind: "reject_once" },
        ],
      }),
      clientAnswered("permission.1.answer", "session/request_permission", {
        outcome: { outcome: "selected", optionId: "allow-always" },
      }),
      notify("shell.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-test",
        status: "completed",
        rawOutput: { combinedOutput: "ok\n", exitCode: 0 },
      }),
      text("assistant.1", "Tests passed."),
      done(),
      prompt(),
      notify("rm.start", {
        sessionUpdate: "tool_call",
        toolCallId: "tool-rm",
        title: "Run rm -rf dist",
        kind: "execute",
        status: "pending",
        rawInput: { CommandLine: "rm -rf dist" },
      }),
      askClient("permission.2", "session/request_permission", {
        sessionId: SESSION,
        toolCall: {
          toolCallId: "tool-rm",
          title: "Run rm -rf dist",
          kind: "execute",
          rawInput: { CommandLine: "rm -rf dist" },
        },
        options: [
          { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
          { optionId: "reject", name: "Deny", kind: "reject_once" },
        ],
      }),
      clientAnswered("permission.2.answer", "session/request_permission", {
        outcome: { outcome: "selected", optionId: "reject" },
      }),
      notify("rm.done", {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-rm",
        status: "failed",
      }),
      text("assistant.2", "Not deleted."),
      done(),
      ...tail,
    ],
  },
  // A native question (an `interaction_*` permission request) answered by choosing one of its options.
  antigravity_question: {
    input: {
      steps: [
        { type: "message", text: "Which branch should I use?" },
        { type: "answer_next_user_input_request", answers: { interaction_1: "dev" } },
      ],
    },
    transcript: () => [
      ...launch(),
      prompt(),
      text("assistant.1", "Let me ask. "),
      askClient("question", "session/request_permission", {
        sessionId: SESSION,
        toolCall: { toolCallId: "interaction_1", title: "Which branch?" },
        options: [
          { optionId: "main", name: "main", kind: "allow_once" },
          { optionId: "dev", name: "dev", kind: "allow_once" },
        ],
      }),
      clientAnswered("question.answer", "session/request_permission", {
        outcome: { outcome: "selected", optionId: "dev" },
      }),
      text("assistant.2", "Using dev."),
      done(),
      ...tail,
    ],
  },
  // Stop mid-turn: the agent is asked to cancel and answers the prompt as cancelled.
  antigravity_interrupt: {
    input: {
      steps: [
        { type: "message", text: "Write a very long answer" },
        { type: "interrupt", targetRunIndex: 1 },
      ],
    },
    transcript: () => [
      ...launch(),
      prompt(),
      {
        type: "expect_outbound",
        label: "cancel",
        frame: { kind: "notification", method: "session/cancel", params: { sessionId: SESSION } },
      },
      // What the agent had written when it stopped.
      text("assistant.1", "Starting a long answer"),
      done("cancelled"),
      ...tail,
    ],
  },
  // The prompt fails: the run fails with the agent's message.
  antigravity_failure: {
    input: { steps: [{ type: "message", text: "Do something that fails" }] },
    transcript: () => [
      ...launch(),
      prompt(),
      text("assistant.1", "Trying. "),
      failRespond("session/prompt", {
        code: -32603,
        message: "Antigravity could not reach the model.",
      }),
      ...tail,
    ],
  },
  // A batch of subagents launched as an ordinary tool: it shows as delegated work and goes idle with the turn.
  antigravity_subagent: {
    input: { steps: [{ type: "message", text: "Run two agents in parallel" }] },
    transcript: () => [
      ...launch(),
      prompt(),
      notify("subagent.start", {
        sessionUpdate: "tool_call",
        toolCallId: "trajectory:4",
        title: "Running start_subagent",
        kind: "other",
        status: "in_progress",
        rawInput: { tasks: ["a", "b"] },
      }),
      notify("subagent.ack", {
        sessionUpdate: "tool_call_update",
        toolCallId: "trajectory:4",
        status: "completed",
        rawOutput: "Started two agents.",
      }),
      text("assistant.1", "Both agents are running."),
      done(),
      ...tail,
    ],
  },
  // `/compact` is sent as an ordinary prompt; a completed one leaves a compaction item.
  antigravity_compact: {
    input: {
      steps: [
        { type: "message", text: "Remember the plan" },
        { type: "message", text: "/compact" },
      ],
    },
    transcript: () => [
      ...launch(),
      prompt(),
      text("assistant.1", "Noted."),
      done(),
      prompt(),
      done(),
      ...tail,
    ],
  },
  // A model the user chose is applied as chosen, and the runtime mode selects the agent's matching mode.
  antigravity_explicit_model: {
    model: "gemini-3.0-pro",
    input: {
      runtimeMode: "auto-accept-edits",
      steps: [{ type: "message", text: "Use the pro model" }],
    },
    transcript: () => [
      ...launch({ model: "gemini-3.0-pro", mode: "auto_edit" }),
      prompt(),
      text("assistant.1", "On the pro model."),
      done(),
      ...tail,
    ],
  },
};
export const SCENARIO_NAMES = Object.keys(scenarios);

// ---- the original, driven by the peer ----

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
  approval_request: ["requestKind", "prompt", "options"],
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
  subagents: projection.subagents.map((subagent) =>
    pick(subagent, [
      "origin",
      "createdBy",
      "driver",
      "status",
      "prompt",
      "title",
      "model",
      "result",
    ]),
  ),
});

/** `makeAcpReplayRuntime` of the testkit, with the options the Antigravity runtime adds. */
function makeReplayRuntime({
  transcript,
  statusPath,
  scriptPath,
  childProcessSpawner,
  fileSystem,
}) {
  const transcriptPath = `${statusPath}.transcript.json`;
  return (runtimeInput) =>
    Effect.gen(function* () {
      yield* fileSystem
        .writeFileString(transcriptPath, JSON.stringify(transcript))
        .pipe(Effect.orDie);
      const context = yield* Layer.build(
        AcpSessionRuntime.layer({
          ...runtimeInput,
          spawn: {
            command: process.execPath,
            args: ["--experimental-strip-types", scriptPath],
            cwd: runtimeInput.cwd,
            env: {
              ...process.env,
              T3_ACP_REPLAY_TRANSCRIPT_PATH: transcriptPath,
              T3_ACP_REPLAY_STATUS_PATH: statusPath,
              T3_ACP_REPLAY_WORKSPACE: runtimeInput.cwd,
            },
          },
          // makeAntigravityAcpRuntime, with a scripted peer in place of the agent.
          authMethodId: "oauth-personal",
          authenticateEagerly: true,
          resumeMethod: "resume",
          cancelBehavior: "wait-for-prompt",
          clientCapabilities: {
            fs: {
              readTextFile: runtimeInput.clientFileSystem === true,
              writeTextFile: runtimeInput.clientFileSystem === true,
            },
            terminal: false,
          },
          transformSessionUpdate: normalizeAntigravitySessionUpdate,
        }).pipe(
          Layer.provide(
            Layer.succeed(ChildProcessSpawner.ChildProcessSpawner, childProcessSpawner),
          ),
        ),
      );
      return yield* Effect.service(AcpSessionRuntime.AcpSessionRuntime).pipe(
        Effect.provide(context),
      );
    });
}

function harnessFor(transcript) {
  const layerServerConfig = Layer.effect(
    ServerConfig.ServerConfig,
    makeReplayServerConfig(`antigravity-${transcript.scenario}`).pipe(Effect.orDie),
  ).pipe(Layer.provide(NodeServices.layer));
  return {
    driver: DRIVER,
    decodeTranscript: (raw) => decodeAcpReplayTranscript(raw, DRIVER),
    makeProviderAdapterRegistryLayer: () =>
      ProviderAdapterRegistry.layerFromAdaptersEffect(
        Effect.gen(function* () {
          const fileSystem = yield* FileSystem.FileSystem;
          const path = yield* Path.Path;
          const childProcessSpawner = yield* ChildProcessSpawner.ChildProcessSpawner;
          const crypto = yield* Crypto.Crypto;
          const idAllocator = yield* IdAllocator.IdAllocatorV2;
          const serverConfig = yield* ServerConfig.ServerConfig;
          const continuationRequests =
            yield* ProviderContinuationRequests.ProviderContinuationRequests;
          const replayDir = yield* fileSystem
            .makeTempDirectory({
              prefix: `t3-orchestration-v2-antigravity-replay-${transcript.scenario}-`,
            })
            .pipe(Effect.orDie);
          const statusPath = path.join(replayDir, "status.json");
          const scriptPath = yield* path
            .fromFileUrl(new URL("../../apps/server/scripts/acp-replay-agent.ts", import.meta.url))
            .pipe(Effect.orDie);
          const replay = makeReplayRuntime({
            transcript,
            statusPath,
            scriptPath,
            childProcessSpawner,
            fileSystem,
          });
          const flavor = makeAntigravityAcpAdapterFlavor({
            instanceId: INSTANCE,
            crypto,
            selfInvocation: yield* resolveSelfInvocation(),
            fileSystem,
            path,
            idAllocator,
            serverConfig,
            makeRuntime: (input) =>
              replay(input).pipe(Effect.provideService(Crypto.Crypto, crypto)),
            withProcess: (_stop, task) => task,
            defaultModel: Effect.succeed(MANIFEST_DEFAULT),
          });
          const adapter = makeAcpAdapterV2({
            instanceId: INSTANCE,
            flavor: {
              ...flavor,
              assertComplete: makeAcpReplayCompletenessAssertion(
                fileSystem,
                statusPath,
                transcript,
              ),
            },
            crypto,
            fileSystem,
            idAllocator,
            serverConfig,
            selfInvocation: yield* resolveSelfInvocation(),
            continuationRequests,
          });
          return [adapter];
        }),
      ).pipe(
        Layer.provide(Layer.mergeAll(layerServerConfig, NodeServices.layer, IdAllocator.layer)),
      ),
  };
}

const target = new URL("../crates/server/tests/fixtures/antigravity-replay/", import.meta.url);
mkdirSync(target, { recursive: true });
let count = 0;
for (const [name, scenario] of Object.entries(scenarios)) {
  if (only.length > 0 && !only.includes(name)) continue;
  const model = scenario.model ?? MODEL_DEFAULT;
  const modelSelection = { instanceId: INSTANCE, model };
  const input = scenario.input;
  let workspacePath = null;
  const program = Effect.gen(function* () {
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    workspacePath = workspace;
    // The scripted peer names the workspace the way the recordings do.
    const entries = JSON.parse(JSON.stringify(scenario.transcript()));
    const raw = {
      type: "transcript",
      provider: "antigravity",
      protocol: "acp.ndjson-jsonrpc",
      version: "1",
      scenario: name,
      entries,
    };
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: DRIVER,
      modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const harness = harnessFor(raw);
    const transcript = yield* harness.decodeTranscript(raw);
    const replay = {
      name: `${name}/antigravity`,
      transcript,
      commands: materialized.commands,
      steps: materialized.steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(replay, harness, {}).pipe(
      provideDeterministicTestRuntime,
    );
  }).pipe(Effect.scoped);
  const result = await Effect.runPromise(program.pipe(Effect.provide(NodeServices.layer)));
  const projection = result.projections.get([...result.projections.keys()][0]);
  const entries = scenario.transcript();
  const row = {
    name,
    runtimeMode: input.runtimeMode ?? "full-access",
    // The directory the original ran in: its paths in the projection are rewritten to the Rust one's.
    workspace: workspacePath,
    model,
    interactionMode: input.interactionMode ?? "default",
    workspaceFiles: input.workspaceFiles ?? {},
    steps: input.steps,
    transcript: entries,
    expected: normalize(projection),
  };
  writeFileSync(new URL(`${name}.json`, target), JSON.stringify(row, null, 1) + "\n");
  count += 1;
}
console.log(`${count} antigravity replay scenarios`);
process.exit(0);
