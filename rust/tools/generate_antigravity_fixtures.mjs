// Development oracle for the Antigravity driver's pure behaviour: the unchanged TypeScript functions are run over
// inputs and their results written to tests/fixtures/antigravity-pure.json, which the Rust functions must
// reproduce. Covers the protocol (tool payload bounds, native tool fields, approval options, questions,
// subagents), the model rules, the Google sign-in URL and callback checks, the profile settings, the skills
// scan, the client file containment, the pinned release table and the snapshot merge.
import { mkdtempSync, mkdirSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as FileSystem from "../../apps/server/node_modules/effect/dist/FileSystem.js";
import * as Path from "../../apps/server/node_modules/effect/dist/Path.js";
import {
  ProviderInstanceId,
  ANTIGRAVITY_DEFAULT_MODEL,
} from "../../packages/contracts/src/index.ts";
import * as Protocol from "../../apps/server/src/provider/acp/AntigravityProtocol.ts";
import * as Support from "../../apps/server/src/provider/acp/AntigravityAcpSupport.ts";
import * as Files from "../../apps/server/src/provider/acp/AntigravityClientFiles.ts";
import * as AuthSupport from "../../apps/server/src/provider/antigravityAuthSupport.ts";
import * as Callback from "../../apps/server/src/provider/antigravityCallback.ts";
import { resolveAntigravityReleaseAsset } from "../../apps/server/src/provider/antigravityRelease.ts";
import { buildAntigravityModelsFromSession } from "../../apps/server/src/provider/AntigravityProvider.ts";
import { discoverAntigravitySkills } from "../../apps/server/src/provider/Drivers/AntigravitySkills.ts";
import { mergeProviderSnapshot } from "../../apps/server/src/provider/ProviderRegistry.ts";
import * as ModelManifest from "../../apps/server/src/provider/ModelManifest.ts";
import * as Crypto from "../../apps/server/node_modules/effect/dist/Crypto.js";
import { makeAntigravityAcpAdapterFlavor } from "../../apps/server/src/orchestration-v2/Adapters/AntigravityAdapterV2.ts";

const run = (effect) => Effect.runPromise(effect.pipe(Effect.provide(NodeServices.layer)));
const out = {};
const clean = (value) => JSON.parse(JSON.stringify(value === undefined ? null : value));

// ---- the tool payload bounds ----
const big = "y".repeat(70_000);
const deep = (n) => (n === 0 ? "leaf" : { next: deep(n - 1) });
out.sanitize = [
  "plain",
  "data:IMAGE/png;base64,AAAA",
  "x".repeat(9000),
  "é".repeat(9000),
  { type: "image", data: "AAAA", mimeType: "image/png", name: "shot" },
  { blob: "AAAA", mimeType: "image/png", other: 1 },
  { blob: "AAAA", mimeType: "text/plain" },
  {
    combinedOutput: "same",
    formatted_output: "same",
    formattedOutput: "other",
    combined_output: "x",
  },
  { a: big, b: big, c: "after" },
  Array.from({ length: 700 }, (_, i) => i),
  Object.fromEntries(Array.from({ length: 700 }, (_, i) => [`k${i}`, i])),
  deep(14),
  {
    nested: { list: ["a", { screenshot: "data:image/jpeg;base64,BBBB" }, null, true, 3] },
    nothing: null,
  },
  null,
  7,
].map((payload) => ({ payload, result: clean(Protocol.sanitizeAntigravityToolPayload(payload)) }));

const toolCall = (rest) => ({ toolCallId: "t1", data: {}, ...rest });
out.normalizeToolCall = [
  toolCall({
    title: "Run command",
    data: {
      rawInput: { CommandLine: " ls -la ", Cwd: "/w" },
      rawOutput: { combinedOutput: "a\n", exitCode: 0 },
    },
  }),
  toolCall({
    kind: "other",
    title: "Run",
    data: {
      rawInput: { command_line: "pwd", working_dir: "/x" },
      rawOutput: { combined_output: "out", exit_code: 2, imagePath: "/w/shot.png" },
    },
  }),
  toolCall({ title: "Run", command: "from-state", data: { rawInput: { command: "  " } } }),
  toolCall({ kind: "read", title: "Read", data: { rawInput: { command: "cat x" } } }),
  toolCall({ title: "Run", data: { rawInput: { CommandLine: 5 }, rawOutput: { exitCode: 1.5 } } }),
  toolCall({ title: "t".repeat(9000), detail: "d".repeat(9000), data: { rawInput: [1, 2] } }),
  toolCall({
    title: "Run",
    data: {
      rawInput: { commandLine: "echo" },
      rawOutput: { exitCode: 3, imagePath: "file:///w/a%20b.png", workingDir: "/wd" },
      item: { keep: 1 },
    },
  }),
  toolCall({
    title: "Run",
    data: {
      rawInput: { command: "x" },
      rawOutput: { imagePath: "https://example.com/a.png", combinedOutput: "z".repeat(9000) },
    },
  }),
  toolCall({
    title: "Run",
    data: { rawInput: { command: "x" }, rawOutput: { imagePath: "C:\\\\w\\\\a.png" } },
  }),
  toolCall({
    title: "Run",
    data: { rawInput: { command: "x" }, rawOutput: { imagePath: "file://host/a.png" } },
  }),
  toolCall({ kind: "execute", title: "Run", status: "inProgress", data: {} }),
].map((tool) => ({ tool, result: clean(Protocol.normalizeAntigravityToolCall(tool)) }));

const contentText = { type: "content", content: { type: "text", text: "hello" } };
out.sessionUpdates = [
  {
    sessionId: "s",
    update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hi" } },
  },
  {
    sessionId: "s",
    update: {
      sessionUpdate: "tool_call",
      toolCallId: "t",
      title: "z".repeat(9000),
      rawInput: { command: "x".repeat(9000), image: "data:image/png;base64,AA" },
      rawOutput: { combinedOutput: "o", formattedOutput: "o" },
      content: [
        contentText,
        { type: "image", data: "AAAA", mimeType: "image/png" },
        { type: "nonsense" },
        { type: "diff", path: "/a", newText: "n", oldText: null },
      ],
      _meta: { is_mcp_tool_call: true, big: "m".repeat(9000) },
    },
  },
  {
    sessionId: "s",
    update: {
      sessionUpdate: "tool_call_update",
      toolCallId: "t",
      status: "completed",
      content: [],
      rawOutput: null,
      _meta: "not an object",
    },
  },
  {
    sessionId: "s",
    update: {
      sessionUpdate: "tool_call_update",
      toolCallId: "t",
      status: "failed",
      _meta: { nested: { ok: 1 }, list: [1, "a"] },
    },
  },
].map((notification) => ({
  notification,
  result: clean(Support && Protocol.normalizeAntigravitySessionUpdate(notification)),
}));

// ---- permission requests ----
const option = (optionId, name, kind, meta) => ({
  optionId,
  name,
  kind,
  ...(meta === undefined ? {} : { _meta: meta }),
});
const request = (toolCallId, options, title) => ({
  sessionId: "s",
  options,
  toolCall: { toolCallId, ...(title === undefined ? {} : { title }) },
});
const approvals = [
  request("tool_1", [option("a", "Allow", "allow_once"), option("b", "Deny", "reject_once")]),
  request("tool_1", [
    option("a", "Allow", "allow_once"),
    option("al", "Always", "allow_always", {
      "agy.security.warning": { title: " Title ", message: " Careful " },
    }),
  ]),
  request("tool_1", [
    option("al", "Always", "allow_always", { "agy.security.warning": { title: "Only title" } }),
  ]),
  request("tool_1", [
    option("al", "Always", "allow_always", {
      "agy.security.warning": { message: "m".repeat(600) },
    }),
  ]),
  request("tool_1", [
    option("al", "Always", "allow_always", { "agy.security.warning": { title: 3 } }),
  ]),
  request("tool_1", [option(" ", "Blank", "allow_once"), option("r", "R", "reject_once")]),
  request("tool_1", []),
  request("interaction_2", [option("a", "A", "allow_once")]),
];
out.approvalOptions = approvals.map((req) => ({
  request: req,
  result: clean(Protocol.antigravityApprovalOptions(req)),
}));
const questions = [
  request(
    "interaction_1",
    [option("main", "main", "allow_once"), option("dev", " dev ", "allow_once")],
    "Which branch?",
  ),
  request("interaction_1", [option("a", "  ", "allow_once")], "   "),
  request("interaction_1", [option("a", "x".repeat(600), "allow_once")], "q".repeat(9000)),
  request("interaction_1", [option("a", "A", "allow_once"), option("a", "B", "allow_once")], "dup"),
  request("interaction_1", [option(" ", "A", "allow_once")], "blank"),
  request("interaction_1", [], "none"),
  request("tool_1", [option("a", "A", "allow_once")], "not a question"),
];
const answerSets = (req) => {
  const id = req.toolCall.toolCallId;
  return [
    { [id]: "dev" },
    { [id]: ["dev"] },
    { [id]: "dev " },
    { [id]: "main" },
    { [id]: "nope" },
    { [id]: ["a", "b"] },
    {},
    { [id]: 3 },
    { [id]: "A" },
    { [id]: "B" },
  ];
};
out.questions = questions.map((req) => ({
  request: req,
  question: clean(Protocol.extractAntigravityUserInputQuestion(req)),
  answers: answerSets(req).map((answers) => ({
    answers,
    response: clean(Protocol.makeAntigravityUserInputResponse(req, answers)),
  })),
}));

const flavor = makeAntigravityAcpAdapterFlavor({
  instanceId: ProviderInstanceId.make("antigravity-test"),
  crypto: undefined,
  fileSystem: undefined,
  path: undefined,
  idAllocator: undefined,
  serverConfig: undefined,
  selfInvocation: undefined,
  makeRuntime: () => Effect.die("not spawned"),
  withProcess: (_stop, task) => task,
  defaultModel: Effect.succeed(undefined),
});
out.subagents = [
  toolCall({
    toolCallId: "trajectory:4",
    title: "Running start_subagent",
    kind: "other",
    status: "completed",
    data: { rawOutput: "Started two agents." },
  }),
  toolCall({
    toolCallId: "trajectory:5",
    title: "Run start_subagent?",
    status: "pending",
    detail: "approve?",
    data: {},
  }),
  toolCall({
    toolCallId: "trajectory:6",
    title: "Running start_subagent",
    status: "failed",
    data: { rawOutput: "  boom  " },
  }),
  toolCall({
    toolCallId: "trajectory:7",
    title: "Running start_subagent",
    status: "inProgress",
    data: { meta: { is_mcp_tool_call: true } },
  }),
  toolCall({
    toolCallId: "trajectory:8",
    title: "Running start_subagent",
    kind: "execute",
    data: {},
  }),
  toolCall({ toolCallId: "trajectory:9", title: "Running something_else", data: {} }),
  toolCall({
    toolCallId: "trajectory:10",
    title: "Running start_subagent",
    status: "inProgress",
    data: {},
  }),
].map((tool) => ({ tool, result: clean(flavor.extractSubagentUpdate(tool)) }));

// ---- models ----
out.permissionModes = ["approval-required", "auto-accept-edits", "full-access", "auto"].map(
  (mode) => ({ mode, result: Support.antigravityPermissionMode(mode) }),
);
const select = (current, values, id = "model") => ({
  id,
  name: "Model",
  category: "model",
  type: "select",
  currentValue: current,
  options: values,
});
const flat = (values) => values.map((value) => ({ value, name: value.toUpperCase() }));
const grouped = [{ group: "g", name: "G", options: flat(["gemini-3.8-flash-high", "x"]) }];
const modelCases = [
  {
    configOptions: [
      select("gemini-3.8-flash-low", flat(["gemini-3.8-flash-high", "gemini-3.8-flash-low"])),
    ],
    model: ANTIGRAVITY_DEFAULT_MODEL,
    defaultModel: "gemini-3.8-flash-high",
  },
  {
    configOptions: [
      select("gemini-3.8-flash-high", flat(["gemini-3.8-flash-high", "gemini-3.8-flash-low"])),
    ],
    model: ANTIGRAVITY_DEFAULT_MODEL,
    defaultModel: "gemini-3.8-flash-high",
  },
  {
    configOptions: [select("a", flat(["a", "b"]))],
    model: ANTIGRAVITY_DEFAULT_MODEL,
    defaultModel: "gemini-3.8-flash-high",
  },
  { configOptions: [select("a", flat(["a", "b"]))], model: "b", defaultModel: undefined },
  { configOptions: [select("a", flat(["a", "b"]))], model: "a", defaultModel: undefined },
  { configOptions: [select("a", flat(["a", "b"]))], model: "missing", defaultModel: undefined },
  { configOptions: [select("a", flat(["a", "b"]))], model: "", defaultModel: "b" },
  { configOptions: [select("a", grouped)], model: "x", defaultModel: undefined },
  { configOptions: [select("a", grouped)], model: null, defaultModel: "gemini-3.8-flash-high" },
  { configOptions: [], model: "x", defaultModel: undefined },
  { configOptions: [], model: ANTIGRAVITY_DEFAULT_MODEL, defaultModel: "x" },
];
out.modelSelection = await Promise.all(
  modelCases.map(async ({ configOptions, model, defaultModel }) => {
    const calls = [];
    const result = await Effect.runPromise(
      Support.applyAntigravityAcpModelSelection({
        runtime: {
          getConfigOptions: Effect.succeed(configOptions),
          setModel: (value) =>
            Effect.sync(() => {
              calls.push(value);
            }),
        },
        model,
        defaultModel,
        mapError: (cause) => cause,
      }).pipe(Effect.result),
    );
    return {
      configOptions,
      model,
      defaultModel,
      calls,
      result:
        result._tag === "Success"
          ? { ok: result.success ?? null }
          : { error: result.failure.errorMessage },
    };
  }),
);
const sessionSetups = [
  {
    configOptions: [
      select(
        "gemini-3.8-flash-low",
        flat(["gemini-3.8-flash-high", "gemini-3.8-flash-low", "gemini-3.0-pro"]),
      ),
    ],
  },
  { configOptions: [select("b", grouped)] },
  {
    models: {
      currentModelId: "m1",
      availableModels: [
        { modelId: "m1", name: "M1" },
        { modelId: "m2", name: " " },
        { modelId: "m1", name: "dup" },
        { modelId: " ", name: "blank" },
      ],
    },
  },
  {
    configOptions: [
      {
        id: "other",
        name: "O",
        category: "model",
        type: "select",
        currentValue: "k",
        options: flat(["k"]),
      },
    ],
  },
  {
    configOptions: [
      { id: "toggle", name: "T", category: "model", type: "boolean", currentValue: true },
    ],
    models: { currentModelId: "zz", availableModels: [{ modelId: "zz", name: "ZZ" }] },
  },
  {},
];
const manifest = ModelManifest.BUNDLED_MODEL_MANIFEST;
const DRIVER = ProviderInstanceId.make("antigravity");
out.sessionModels = sessionSetups.map((setup) => {
  const models = buildAntigravityModelsFromSession(setup);
  const classified = ModelManifest.applyModelManifest({ models }, manifest, "antigravity").models;
  return { setup, models: clean(models), classified: clean(classified) };
});
out.manifestDefault = ModelManifest.manifestDefaultModel(manifest, "antigravity");
void DRIVER;

// ---- Google sign-in ----
const base =
  "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=c&redirect_uri=http%3A%2F%2F127.0.0.1%3A46353%2F&state=opaque";
const urls = [
  base,
  base.replace("https://accounts.google.com", "https://accounts.google.com.evil.com"),
  base.replace("https://", "http://"),
  base.replace("/o/oauth2/v2/auth", "/o/oauth2/auth"),
  base + "#frag",
  base + "&state=second",
  base.replace("response_type=code", "response_type=token"),
  base.replace("46353", "80"),
  base.replace("46353", "1023"),
  base.replace("127.0.0.1", "localhost"),
  base.replace("%2F&state", "%2Fcb&state"),
  base.replace("state=opaque", "state="),
  base.replace("state=opaque", "state=" + "s".repeat(513)),
  base + " x",
  "https://user:pw@accounts.google.com/o/oauth2/v2/auth?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A46353%2F&state=a",
  "https://accounts.google.com:444/o/oauth2/v2/auth?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A46353%2F&state=a",
  "not a url",
  "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A65535%2F&state=a",
  "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A65536%2F&state=a",
  base + "&" + "p".repeat(17000),
];
out.authorizationUrls = await Promise.all(
  urls.map(async (url) => {
    const parsed = await Effect.runPromise(
      AuthSupport.parseAntigravityAuthorizationUrl(url).pipe(Effect.result),
    );
    return { url, result: parsed._tag === "Success" ? clean(parsed.success) : null };
  }),
);
const pending = { redirectUri: "http://127.0.0.1:46353/", state: "opaque" };
const callbacks = [
  "http://127.0.0.1:46353/?state=opaque&code=abc",
  "http://127.0.0.1:46353/?state=opaque&error=access_denied",
  "http://127.0.0.1:46353/?state=opaque&code=abc&iss=https%3A%2F%2Faccounts.google.com",
  "http://127.0.0.1:46353/?state=opaque&code=abc&iss=https%3A%2F%2Fevil.example",
  "http://127.0.0.1:46353/?state=opaque&code=abc&iss=a&iss=b",
  "http://127.0.0.1:46353/?state=other&code=abc",
  "http://127.0.0.1:46353/?state=opaque&state=opaque&code=abc",
  "http://127.0.0.1:46353/?state=opaque",
  "http://127.0.0.1:46353/?state=opaque&code=",
  "http://127.0.0.1:46353/?state=opaque&code=a&code=b",
  "http://127.0.0.1:46353/?state=opaque&code=a&error=b",
  "http://127.0.0.1:46354/?state=opaque&code=abc",
  "http://127.0.0.1:46353/other?state=opaque&code=abc",
  "https://127.0.0.1:46353/?state=opaque&code=abc",
  "http://localhost:46353/?state=opaque&code=abc",
  "http://u:p@127.0.0.1:46353/?state=opaque&code=abc",
  "http://127.0.0.1:46353/?state=opaque&code=abc#f",
  "garbage",
  "http://127.0.0.1:46353/?" + "q".repeat(17000),
];
out.callbacks = await Promise.all(
  callbacks.map(async (callbackUrl) => {
    const result = await Effect.runPromise(
      Callback.validateAntigravityCallbackUrl(
        ProviderInstanceId.make("antigravity"),
        pending,
        callbackUrl,
      ).pipe(Effect.result),
    );
    return {
      callbackUrl,
      result:
        result._tag === "Success" ? { ok: result.success.href } : { error: result.failure.detail },
    };
  }),
);
// The lines the agent prints, and what the stderr handler finds in them.
const lines = [
  `${AuthSupport.ANTIGRAVITY_AUTH_STDOUT_PREFIX}${base}\n`,
  `${AuthSupport.ANTIGRAVITY_AUTH_BROWSER_MARKER}${JSON.stringify(base)}\n`,
  `${AuthSupport.ANTIGRAVITY_AUTH_BROWSER_MARKER}${base}\n`,
  `${AuthSupport.ANTIGRAVITY_AUTH_STDOUT_PREFIX}https://evil.example/\n`,
  "some other output\n",
  `${AuthSupport.ANTIGRAVITY_AUTH_BROWSER_MARKER}${JSON.stringify(base)}\r\n`,
];
out.stderrLines = await Promise.all(
  lines.map(async (line) => {
    const found = [];
    const handler = AuthSupport.makeAntigravityStderrHandler({
      onAuthorizationUrl: (url) =>
        Effect.sync(() => {
          found.push(url);
        }),
    });
    await Effect.runPromise(handler(line));
    return { line, urls: found };
  }),
);

// ---- the profile ----
const configs = [
  { authMethod: "oauth-personal", apiKey: "", gcpProject: "", gcpLocation: "" },
  { authMethod: "oauth-business", apiKey: "", gcpProject: "", gcpLocation: "" },
  { authMethod: "oauth-business", apiKey: "", gcpProject: "p", gcpLocation: "us-central1" },
  { authMethod: "gemini-api-key", apiKey: "", gcpProject: "", gcpLocation: "" },
  { authMethod: "gemini-api-key", apiKey: "k", gcpProject: "p", gcpLocation: "" },
  { authMethod: "agent-platform", apiKey: "", gcpProject: "p", gcpLocation: "" },
  { authMethod: "agent-platform", apiKey: "", gcpProject: "p", gcpLocation: "l" },
  { authMethod: "agent-platform", apiKey: "k", gcpProject: "", gcpLocation: "" },
];
out.authConfigs = configs.map((auth) => ({
  auth,
  issue: AuthSupport.antigravityAuthConfigIssue(auth),
  label: AuthSupport.antigravityAuthLabel(auth.authMethod),
  usesBrowser: AuthSupport.antigravityAuthUsesBrowser(auth.authMethod),
  settings: AuthSupport.antigravityProfileSettings(auth),
}));
out.directories = await Promise.all(
  ["antigravity", "Antigravity", "alt-instance"].map(async (id) => ({
    id,
    result: clean(
      await Effect.runPromise(
        AuthSupport.resolveAntigravityInstanceDirectories(
          "/state",
          ProviderInstanceId.make(id),
        ).pipe(Effect.provide(NodeServices.layer)),
      ),
    ),
  })),
);
const profile = {
  platform: "linux",
  geminiHome: "/p",
  acpDirectory: "/p/antigravity-acp",
  tokenPath: "/p/antigravity-acp/acp_token.json",
  tempDirectory: "/p/antigravity-acp/tmp",
  browserCommand: "BROWSERCMD",
};
const executable = {
  executablePath: "/x/agy_acp_server.par",
  harnessPath: "/x/localharness_external",
};
const baseEnv = {
  PATH: "/usr/bin",
  HOME: "/h",
  GEMINI_API_KEY: "leak",
  google_api_key: "leak2",
  GOOGLE_CLOUD_PROJECT: "leak3",
  BROWSER: "firefox",
  KEEP: "1",
};
out.spawnInputs = configs.map((auth) => ({
  auth,
  baseEnv,
  result: clean(
    AuthSupport.buildAntigravityAcpSpawnInput({
      installation: executable,
      profile,
      cwd: "/work",
      baseEnv,
      auth,
      runtimeTempDirectory: "/tmp/run-1",
    }),
  ),
}));

// ---- release table ----
out.releases = [
  ["darwin", "arm64"],
  ["darwin", "x64"],
  ["linux", "x64"],
  ["linux", "arm64"],
  ["win32", "x64"],
  ["win32", "arm64"],
  ["freebsd", "x64"],
  ["linux", "ia32"],
].map(([platform, arch]) => ({
  platform,
  arch,
  asset: clean(resolveAntigravityReleaseAsset(platform, arch)),
}));

// ---- snapshots ----
const provider = (rest) => ({
  instanceId: "antigravity",
  driver: "antigravity",
  enabled: true,
  installed: true,
  version: "1.3.0",
  status: "warning",
  auth: { status: "unknown", type: "oauth-personal" },
  checkedAt: "2026-10-01T00:00:00.000Z",
  models: [],
  slashCommands: [],
  skills: [],
  ...rest,
});
const authenticated = { status: "authenticated", type: "oauth-personal", label: "Google account" };
const model = (slug, extra = {}) => ({
  slug,
  name: slug,
  isCustom: false,
  capabilities: { optionDescriptors: [] },
  ...extra,
});
const merges = [
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ message: "Antigravity is installed. Google account access is not checked yet." }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ installed: false, status: "warning" }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ installed: false, status: "error", message: "gone" }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ enabled: false, status: "disabled" }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ auth: { status: "unknown", type: "gemini-api-key" } }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ auth: { status: "unauthenticated" }, status: "warning" }),
  ],
  [
    provider({ auth: authenticated, status: "ready", models: [model("m")] }),
    provider({ status: "error", message: "boom" }),
  ],
  [provider({ auth: { status: "unauthenticated" }, models: [model("m")] }), provider({})],
  [
    provider({
      auth: authenticated,
      status: "ready",
      models: [model("m")],
      slashCommands: [{ name: "plan" }],
      workspaceSnapshots: [{ cwd: "/w", checkedAt: "x", slashCommands: [], skills: [] }],
    }),
    provider({ models: [model("n")] }),
  ],
];
out.merges = merges.map(([previous, next]) => ({
  previous,
  next,
  result: clean(mergeProviderSnapshot(previous, next)),
}));
void Crypto;

// ---- the client file system ----
const root = mkdtempSync(join(tmpdir(), "t3-agy-files-"));
const workspace = join(root, "workspace");
const outside = join(root, "outside");
const attachments = join(root, "attachments");
for (const dir of [workspace, outside, attachments, join(workspace, "sub")])
  mkdirSync(dir, { recursive: true });
writeFileSync(join(workspace, "a.txt"), "one\ntwo\nthree\nfour");
writeFileSync(join(workspace, "..odd"), "dots");
writeFileSync(join(outside, "secret.txt"), "secret");
writeFileSync(join(attachments, "pasted.txt"), "pasted");
symlinkSync(join(outside, "secret.txt"), join(workspace, "link-out"));
symlinkSync(join(workspace, "a.txt"), join(workspace, "link-in"));
symlinkSync(join(root, "missing-target"), join(workspace, "dangling"));
symlinkSync(outside, join(workspace, "dir-out"));
const roots = [workspace, attachments];
const inRoot = (path) => path.replace("<root>", root);
const reads = [
  { path: "<root>/workspace/a.txt" },
  { path: "<root>/workspace/a.txt", line: 2 },
  { path: "<root>/workspace/a.txt", line: 2, limit: 2 },
  { path: "<root>/workspace/a.txt", limit: 1 },
  { path: "<root>/workspace/a.txt", line: 0, limit: 1 },
  { path: "<root>/workspace/a.txt", line: 9 },
  { path: "<root>/workspace/a.txt", line: null, limit: null },
  { path: "<root>/workspace/..odd" },
  { path: "<root>/workspace/sub" },
  { path: "<root>/workspace/missing.txt" },
  { path: "<root>/outside/secret.txt" },
  { path: "<root>/workspace/../outside/secret.txt" },
  { path: "<root>/workspace/link-out" },
  { path: "<root>/workspace/link-in" },
  { path: "<root>/workspace/dangling" },
  { path: "<root>/workspace/dir-out/secret.txt" },
  { path: "<root>/attachments/pasted.txt" },
  { path: "<root>" },
];
out.reads = await Promise.all(
  reads.map(async (req) => {
    const request = { sessionId: "s", ...req, path: inRoot(req.path) };
    const result = await run(
      Files.readAntigravityClientTextFile({
        fileSystem: await Effect.runPromise(
          FileSystem.FileSystem.pipe(Effect.provide(NodeServices.layer)),
        ),
        path: await Effect.runPromise(Path.Path.pipe(Effect.provide(NodeServices.layer))),
        allowedRoots: roots,
        request,
      }).pipe(Effect.result),
    );
    const strip = (text) => String(text).replaceAll(root, "<root>");
    return {
      request: { ...req },
      result:
        result._tag === "Success"
          ? { content: result.success.content }
          : { code: result.failure.code, message: strip(result.failure.errorMessage) },
    };
  }),
);
const writes = [
  { path: "<root>/workspace/new.txt", content: "fresh" },
  { path: "<root>/workspace/sub/deep/new.txt", content: "deep" },
  { path: "<root>/workspace/a.txt", content: "replaced" },
  { path: "<root>/outside/new.txt", content: "no" },
  { path: "<root>/workspace/link-out", content: "no" },
  { path: "<root>/workspace/dangling", content: "no" },
  { path: "<root>/workspace/dir-out/new.txt", content: "no" },
  { path: "<root>/workspace/../outside/x.txt", content: "no" },
  { path: "<root>/attachments/other.txt", content: "ok" },
];
out.writes = await Promise.all(
  writes.map(async (req) => {
    const request = { sessionId: "s", ...req, path: inRoot(req.path) };
    const fileSystem = await Effect.runPromise(
      FileSystem.FileSystem.pipe(Effect.provide(NodeServices.layer)),
    );
    const path = await Effect.runPromise(Path.Path.pipe(Effect.provide(NodeServices.layer)));
    const result = await run(
      Files.writeAntigravityClientTextFile({ fileSystem, path, allowedRoots: roots, request }).pipe(
        Effect.result,
      ),
    );
    const strip = (text) => String(text).replaceAll(root, "<root>");
    return {
      request: { ...req },
      result:
        result._tag === "Success"
          ? { ok: true }
          : { code: result.failure.code, message: strip(result.failure.errorMessage) },
    };
  }),
);

// ---- skills ----
const home = mkdtempSync(join(tmpdir(), "t3-agy-skills-"));
const cwd = join(home, "project");
const skill = (dir, file, contents) => {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, file), contents);
};
const fm = (frontmatter) => `---\n${frontmatter}\n---\nbody\n`;
skill(
  join(home, ".gemini", "config", "skills", "alpha"),
  "SKILL.md",
  fm("name: alpha\ndescription: Alpha user skill"),
);
skill(
  join(home, ".gemini", "config", "skills", "dupe"),
  "SKILL.md",
  fm("name: dupe\ndescription: user wins"),
);
skill(
  join(home, ".gemini", "config", "skills", "noname"),
  "skill.md",
  fm("description: just a description"),
);
skill(
  join(home, ".gemini", "config", "skills", "quoted"),
  "SKILL.md",
  fm("name: \"quoted name\"\ndescription: 'single'"),
);
skill(
  join(home, ".gemini", "config", "skills", "spaced"),
  "SKILL.md",
  fm('name: " padded "\ndescription: x'),
);
skill(
  join(home, ".gemini", "config", "skills", "numeric"),
  "SKILL.md",
  fm("name: 5\ndescription: x"),
);
skill(
  join(home, ".gemini", "config", "skills", "block"),
  "SKILL.md",
  fm("name: block\ndescription: |\n  line one\n  line two"),
);
skill(join(home, ".gemini", "config", "skills", "bad"), "SKILL.md", "no frontmatter here");
skill(join(home, ".gemini", "config", "skills", "empty"), "SKILL.md", fm(""));
skill(join(home, ".gemini", "config", "skills", "wrongcase"), "SKILL.MD", fm("name: wrongcase"));
skill(
  join(home, ".gemini", "antigravity-cli", "skills", "cli"),
  "SKILL.md",
  fm("name: cli\ndescription: from the cli dir"),
);
skill(
  join(cwd, ".gemini", "skills", "dupe"),
  "SKILL.md",
  fm("name: dupe\ndescription: project loses"),
);
skill(
  join(cwd, ".gemini", "skills", "proj"),
  "SKILL.md",
  fm("name: Zeta\ndescription: project skill"),
);
skill(join(cwd, ".agents", "skills"), "SKILL.md", fm("name: direct\ndescription: root skill"));
skill(join(cwd, ".agent", "skills", "né"), "SKILL.md", fm("name: ünï\ndescription: unicode"));
skill(join(cwd, ".agent", "skills", "a(b)"), "SKILL.md", fm("name: parens"));
out.skills = {
  expected: clean(
    await run(
      discoverAntigravitySkills({ cwd, userHome: home }).pipe(
        Effect.map((skills) =>
          skills.map((entry) => ({ ...entry, path: entry.path.replaceAll(home, "<home>") })),
        ),
      ),
    ),
  ),
};
out.skills.files = [
  [".gemini/config/skills/alpha/SKILL.md", fm("name: alpha\ndescription: Alpha user skill")],
  [".gemini/config/skills/dupe/SKILL.md", fm("name: dupe\ndescription: user wins")],
  [".gemini/config/skills/noname/skill.md", fm("description: just a description")],
  [".gemini/config/skills/quoted/SKILL.md", fm("name: \"quoted name\"\ndescription: 'single'")],
  [".gemini/config/skills/spaced/SKILL.md", fm('name: " padded "\ndescription: x')],
  [".gemini/config/skills/numeric/SKILL.md", fm("name: 5\ndescription: x")],
  [
    ".gemini/config/skills/block/SKILL.md",
    fm("name: block\ndescription: |\n  line one\n  line two"),
  ],
  [".gemini/config/skills/bad/SKILL.md", "no frontmatter here"],
  [".gemini/config/skills/empty/SKILL.md", fm("")],
  [".gemini/config/skills/wrongcase/SKILL.MD", fm("name: wrongcase")],
  [".gemini/antigravity-cli/skills/cli/SKILL.md", fm("name: cli\ndescription: from the cli dir")],
  ["project/.gemini/skills/dupe/SKILL.md", fm("name: dupe\ndescription: project loses")],
  ["project/.gemini/skills/proj/SKILL.md", fm("name: Zeta\ndescription: project skill")],
  ["project/.agents/skills/SKILL.md", fm("name: direct\ndescription: root skill")],
  ["project/.agent/skills/né/SKILL.md", fm("name: ünï\ndescription: unicode")],
  ["project/.agent/skills/a(b)/SKILL.md", fm("name: parens")],
];

writeFileSync(
  new URL("../crates/server/tests/fixtures/antigravity-pure.json", import.meta.url),
  JSON.stringify(out, null, 1) + "\n",
);
console.log("antigravity pure fixtures written");
process.exit(0);
