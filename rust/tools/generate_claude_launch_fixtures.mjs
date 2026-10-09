// Records what the real Claude Agent SDK (0.3.276) spawns for the options the unchanged T3 adapter
// builds: the SDK is given a fake process, so the executable arguments, environment and first
// stdin line are the SDK's own. No claude binary runs.
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { writeFileSync } from "node:fs";
import { query } from "../../apps/server/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs";
import * as Adapter from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";
import { buildClaudeCapabilitiesProbeQueryOptions } from "../../apps/server/src/provider/ClaudeProvider.ts";
import * as McpProviderSession from "../../apps/server/src/mcp/McpProviderSession.ts";
import { buildRuntimeInstructions } from "../../apps/server/src/provider/RuntimeInstructions.ts";
import { T3_CODE_ORCHESTRATION_INSTRUCTIONS } from "../../apps/server/src/provider/T3OrchestrationInstructions.ts";

// The prompt text is large and identical across rows: keep which of the two source forms was sent.
const runtimeText = buildRuntimeInstructions({ harness: "Claude Code" });
const appendForm = (text) =>
  text === runtimeText
    ? "runtime"
    : text === runtimeText + T3_CODE_ORCHESTRATION_INSTRUCTIONS
      ? "runtime+orchestration"
      : (() => {
          throw new Error("unexpected system prompt append");
        })();

const record = async (options) => {
  const captured = { stdin: [] };
  const spawnClaudeCodeProcess = (spawn) => {
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const events = new EventEmitter();
    stdin.on("data", (chunk) => captured.stdin.push(chunk.toString()));
    captured.spawn = { command: spawn.command, args: spawn.args, cwd: spawn.cwd, env: spawn.env };
    return {
      stdin,
      stdout,
      killed: false,
      exitCode: null,
      signalCode: null,
      kill() {
        this.killed = true;
        stdout.end();
      },
      on: (event, listener) => events.on(event, listener),
      once: (event, listener) => events.once(event, listener),
      off: (event, listener) => events.off(event, listener),
    };
  };
  const q = query({
    prompt: (async function* () {
      await new Promise(() => {});
    })(),
    options: { ...options, spawnClaudeCodeProcess },
  });
  await new Promise((resolve) => setTimeout(resolve, 150));
  q.close();
  return captured;
};

const rows = [];
const threadId = "thread:launch";
const selections = [
  {
    instanceId: "claudeAgent",
    model: "claude-sonnet-4-6",
    options: [{ id: "effort", value: "high" }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-6",
    options: [
      { id: "contextWindow", value: "1m" },
      { id: "fastMode", value: true },
      { id: "effort", value: "ultrathink" },
    ],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-8",
    options: [{ id: "effort", value: "ultracode" }],
  },
  { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
  { instanceId: "claudeAgent", model: "custom-model-slug" },
];
const settingsVariants = [
  { binaryPath: "/usr/local/bin/claude", launchArgs: "", autoCompactWindow: "" },
  { binaryPath: "claude", launchArgs: "--chrome --effort low", autoCompactWindow: "150000" },
  { binaryPath: "/opt/claude", launchArgs: "--permission-mode plan", autoCompactWindow: "" },
  {
    binaryPath: "/opt/claude",
    launchArgs: "--dangerously-skip-permissions",
    autoCompactWindow: "",
  },
  {
    binaryPath: "/opt/claude",
    launchArgs: '--thinking-display omitted --add-dir "/a b"',
    autoCompactWindow: "",
  },
  {
    binaryPath: "/opt/claude",
    launchArgs: "--thinking-display=summarized --verbose",
    autoCompactWindow: "",
  },
];
const environments = [
  { PATH: "/usr/bin" },
  { PATH: "/usr/bin", CLAUDE_CONFIG_DIR: "/home/u/.claude-work", CUSTOM: "1" },
];
const mcps = [
  null,
  { endpoint: "http://127.0.0.1:4100/mcp", authorizationHeader: "Bearer token-1" },
];
let index = 0;
for (const runtimeMode of ["approval-required", "auto-accept-edits", "auto", "full-access"])
  for (const interactionMode of ["default", "plan"])
    for (const modelSelection of selections)
      for (const mcp of mcps) {
        const settings = settingsVariants[index % settingsVariants.length];
        const environment = environments[index % environments.length];
        const resume = index % 3 !== 0;
        const resumeSessionAt = index % 5 === 0 ? "msg-uuid-1" : undefined;
        index += 1;
        const input = {
          runtimeMode,
          interactionMode,
          modelSelection,
          mcp,
          settings: { enabled: true, homePath: "", customModels: [], ...settings },
          environment,
          resume,
          resumeSessionAt: resumeSessionAt ?? null,
          nativeThreadId: "77171d01-fb4f-4dff-a961-d9dd334be93d",
          cwd: "/tmp/workspace",
          attachmentsDir: "/tmp/state/attachments",
        };
        if (mcp)
          McpProviderSession.setMcpProviderSession({
            environmentId: "env",
            threadId,
            providerSessionId: "s",
            providerInstanceId: "claudeAgent",
            endpoint: mcp.endpoint,
            authorizationHeader: mcp.authorizationHeader,
            browserToolsAvailable: true,
          });
        else McpProviderSession.clearMcpProviderSession(threadId);
        const policy = Adapter.claudeRuntimeQueryPolicyForRuntimePolicy({
          runtimeMode,
          interactionMode,
          cwd: input.cwd,
        });
        const overrides = Adapter.claudeMcpQueryOverrides({
          threadId,
          readOnlySandbox: false,
          ...(policy.allowedTools === undefined ? {} : { allowedTools: policy.allowedTools }),
        });
        const options = Adapter.makeClaudeQueryOptions({
          modelSelection,
          nativeThreadId: input.nativeThreadId,
          resume,
          ...(resumeSessionAt === undefined ? {} : { resumeSessionAt }),
          cwd: input.cwd,
          attachmentsDir: input.attachmentsDir,
          settings: input.settings,
          environment,
          tools: policy.tools ?? { type: "preset", preset: "claude_code" },
          ...overrides,
          permissionMode: policy.permissionMode,
          ...(policy.allowDangerouslySkipPermissions === undefined
            ? {}
            : { allowDangerouslySkipPermissions: true }),
          canUseTool: async () => ({ behavior: "allow" }),
          onUserDialog: async () => ({ behavior: "cancelled" }),
          supportedDialogKinds: ["resume_return"],
        });
        const captured = await record(options);
        rows.push({
          input,
          policy: {
            permissionMode: policy.permissionMode,
            installPermissionCallback: policy.installPermissionCallback,
            allowDangerouslySkipPermissions: policy.allowDangerouslySkipPermissions === true,
          },
          command: captured.spawn.command,
          args: captured.spawn.args,
          cwd: captured.spawn.cwd,
          env: captured.spawn.env,
          initialize: (({ appendSystemPrompt, ...rest }) => ({
            ...rest,
            appendSystemPrompt: appendForm(appendSystemPrompt),
          }))(JSON.parse(captured.stdin[0]).request),
        });
      }
const probes = [];
for (const [binaryPath, cwd, environment] of [
  ["/usr/local/bin/claude", "/tmp/workspace", { PATH: "/usr/bin" }],
  [
    "claude",
    undefined,
    { PATH: "/bin", CLAUDE_CONFIG_DIR: "/home/u/.claude-work", FORCE_CODE_TERMINAL: "1" },
  ],
]) {
  const options = buildClaudeCapabilitiesProbeQueryOptions({
    executablePath: binaryPath,
    abortController: new AbortController(),
    environment,
    cwd,
  });
  const captured = await record(options);
  const { FORCE_CODE_TERMINAL: _unset, ...env } = captured.spawn.env;
  probes.push({
    input: {
      binaryPath,
      cwd: cwd ?? "/tmp/workspace",
      environment: Object.fromEntries(
        Object.entries(environment).filter(([key]) => key !== "FORCE_CODE_TERMINAL"),
      ),
    },
    args: captured.spawn.args,
    env,
    initialize: JSON.parse(captured.stdin[0]).request,
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-probe-launch.json", import.meta.url),
  JSON.stringify(probes, null, 1) + "\n",
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-launch.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} SDK launch recordings`);
process.exit(0);
