// Pure Grok logic computed by the unchanged TypeScript, for the Rust port to reproduce: the xAI extension
// decoding over every tool call and text chunk in the recorded Grok transcripts (merged the way the adapter
// merges them), the model, slash command and usage decoding, and the launch and approval helpers.
import { readFileSync, readdirSync, writeFileSync, existsSync } from "node:fs";
import {
  extractXAiMonitorTaskId,
  isXAiPersistentMonitor,
  extractXAiBackgroundTaskCompletion,
  extractXAiKilledBackgroundTasks,
  normalizeXAiAcpToolCallState,
  resolveXAiAcpToolTitle,
  extractXAiAcpBackgroundToolMutation,
  extractXAiAcpSubagentEndNotice,
  extractXAiAcpSubagentUpdate,
  xAiBackgroundTaskLifecycleMutation,
  xAiSubagentFinishedNotice,
  xAiPromptCompleteFromSessionUpdate,
  isXAiTaskCompletedWakeNotification,
  extractXAiAskUserQuestions,
  extractXAiAskUserQuestionIdentity,
  makeXAiAskUserQuestionResponse,
  extractXAiExitPlanMarkdown,
  isGrokPlanMarkdownPath,
  extractGrokPlanMarkdownFromToolCallData,
} from "../../apps/server/src/provider/acp/XAiAcpExtension.ts";
import {
  grokAcpSpawnArgs,
  grokApprovalOptions,
  resolveGrokAcpBaseModelId,
  isValidGrokReasoningEffortToken,
} from "../../apps/server/src/provider/acp/GrokAcpSupport.ts";
import {
  parseGrokModelsCliOutput,
  buildGrokModelsFromSessionModelState,
  grokSlashCommandsFromInitialize,
} from "../../apps/server/src/provider/GrokProvider.ts";
import { grokUsageResponseToLimits } from "../../apps/server/src/provider/grokUsageLimits.ts";
import {
  mergeToolCallState,
  parseSessionUpdateEvent,
} from "../../apps/server/src/provider/acp/AcpRuntimeModel.ts";

const fixtures = new URL(
  "../../apps/server/src/orchestration-v2/testkit/fixtures/",
  import.meta.url,
).pathname;
const transcripts = readdirSync(fixtures)
  .filter((directory) => existsSync(`${fixtures}${directory}/grok_transcript.ndjson`))
  .sort();

const tools = [];
const texts = [];
const notifications = [];
for (const name of transcripts) {
  const merged = new Map();
  const entries = readFileSync(`${fixtures}${name}/grok_transcript.ndjson`, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  for (const entry of entries) {
    if (entry.type !== "emit_inbound") continue;
    const frame = entry.frame;
    if (frame.kind !== "notification") continue;
    if (frame.method === "session/update") {
      const update = frame.params.update;
      if (update.sessionUpdate === "tool_call" || update.sessionUpdate === "tool_call_update") {
        for (const event of parseSessionUpdateEvent(frame.params).events) {
          if (event._tag !== "ToolCallUpdated") continue;
          const state = mergeToolCallState(merged.get(event.toolCall.toolCallId), event.toolCall);
          const tool = normalizeXAiAcpToolCallState(state);
          // The adapter keeps the normalized state, which the next update merges onto.
          merged.set(state.toolCallId, tool);
          tools.push({
            scenario: name,
            update,
            sequence: tools.filter((row) => row.scenario === name).length,
            expected: {
              title: resolveXAiAcpToolTitle(state) ?? null,
              normalized: { title: tool.title ?? null, status: tool.status ?? null },
              taskId: extractXAiMonitorTaskId(tool) ?? null,
              persistent: isXAiPersistentMonitor(tool),
              completion: extractXAiBackgroundTaskCompletion(tool),
              killed: extractXAiKilledBackgroundTasks(tool),
              subagent: extractXAiAcpSubagentUpdate(tool) ?? null,
            },
          });
        }
      } else if (
        (update.sessionUpdate === "agent_message_chunk" ||
          update.sessionUpdate === "agent_thought_chunk") &&
        update.content?.type === "text"
      ) {
        const text = update.content.text;
        texts.push({
          text,
          mutations: extractXAiAcpBackgroundToolMutation(text),
          endNotice: extractXAiAcpSubagentEndNotice(text) ?? null,
        });
      }
      notifications.push({
        method: frame.method,
        params: frame.params,
        wake: isXAiTaskCompletedWakeNotification(frame.params),
      });
    } else if (frame.method.includes("x.ai/")) {
      const lifecycle = ["running", "completed"].flatMap((status) =>
        /task_(backgrounded|completed)$/.test(frame.method)
          ? [
              {
                status,
                mutation:
                  xAiBackgroundTaskLifecycleMutation(
                    frame.params,
                    frame.method.endsWith("task_backgrounded") ? "running" : "completed",
                  ) ?? null,
              },
            ].slice(0, 1)
          : [],
      );
      const hasUpdate = frame.params?.update !== undefined;
      notifications.push({
        method: frame.method,
        params: frame.params,
        lifecycle: hasUpdate ? (lifecycle[0]?.mutation ?? null) : null,
        finished: hasUpdate ? (xAiSubagentFinishedNotice(frame.params) ?? null) : null,
        promptComplete: hasUpdate
          ? (xAiPromptCompleteFromSessionUpdate(frame.params) ?? null)
          : null,
      });
    }
  }
}
const dedupeBy = (rows, key) => {
  const seen = new Set();
  return rows.filter((row) => {
    const id = JSON.stringify(key(row));
    if (seen.has(id)) return false;
    seen.add(id);
    return true;
  });
};

// Hand-written inputs for the shapes the recordings do not contain.
const extraTexts = [
  "Monitor 01a0d754-c2a6-7e51-b5d9-710c43051847 ended (exit code 1) with an error",
  'Monitor "01a0d754-c2a6-7e51-b5d9-710c43051847" ended cleanly\n<system-reminder>done</system-reminder>',
  '<monitor-event task_id="01a0d754-c2a6-7e51-b5d9-710c43051847">tick 1</monitor-event><monitor-event task_id=01a0d754-c2a6-7e51-b5d9-710c43051848>tick 2</monitor-event>',
  '2 monitor events from 1 monitor:\n<monitor description="x" task_id="01a0d754-c2a6-7e51-b5d9-710c43051847">\n[1] a\n[2] b\n</monitor>',
  'Background subagent "01a0d754-c2a6-7e51-b5d9-710c43051847" (general-purpose: "Check (backend) failed") completed successfully.',
  'Background subagent "01a0d754-c2a6-7e51-b5d9-710c43051847" (x) crashed.',
  "Background subagent 01a0d754-c2a6-7e51-b5d9-710c43051847 finished",
  "Background subagent 01a0d754-c2a6-7e51-b5d9-710c43051847",
];
for (const text of extraTexts) {
  texts.push({
    text,
    mutations: extractXAiAcpBackgroundToolMutation(text),
    endNotice: extractXAiAcpSubagentEndNotice(text) ?? null,
  });
}
const questionParams = {
  sessionId: "s1",
  toolCallId: "t1",
  mode: "default",
  questions: [
    {
      id: "a",
      question: "Pick one",
      options: [{ label: "Alpha", description: "first", preview: "A preview" }, { label: "Beta" }],
    },
    { question: "Pick many", multiSelect: true, options: [] },
  ],
};
const answerCases = [
  { a: "Alpha", "Pick many": ["x", "y"] },
  { a: ["Beta", "free text"], "Pick many": [] },
  { a: "something else" },
  {},
];
const exitPlanCases = [
  [{ sessionId: "s", toolCallId: "t", planContent: "  # Plan  " }, null],
  [{ sessionId: "s", toolCallId: "t", planContent: "  " }, " fallback "],
  [{ method: "x.ai/exit_plan_mode", params: { sessionId: "s", toolCallId: "t" } }, null],
];
const planPathCases = [
  "/home/u/.grok/sessions/%2Fw/abc/plan.md",
  "/Users/u/.grok/sessions/x/y/plan.md",
  "/work/docs/plan.md",
  "/home/u/.grok/sessions/../plan.md",
  "/tmp/grok-home/sessions/enc/id/plan.md",
  "C:\\Users\\u\\.grok\\sessions\\a\\b\\plan.md",
];
const planDataCases = [
  { rawInput: { file_path: "/home/u/.grok/sessions/a/b/plan.md", content: " # P " } },
  { rawInput: { path: "/home/u/.grok/sessions/a/b/plan.md", content: "  " } },
  { content: [{ type: "diff", path: "/Users/u/.grok/sessions/a/b/plan.md", newText: "new" }] },
  { rawInput: { file_path: "/work/plan.md", content: "x" } },
];
const host = { platform: "linux", environment: { HOME: "/home/u", GROK_HOME: "/tmp/grok-home" } };

const rows = {
  tools: dedupeBy(tools, (row) => [row.scenario, row.update]),
  texts: dedupeBy(texts, (row) => row.text),
  notifications: dedupeBy(notifications, (row) => [row.method, row.params]).filter(
    (row) => row.method !== "session/update" || row.wake,
  ),
  questions: {
    params: questionParams,
    questions: extractXAiAskUserQuestions(questionParams),
    identity: extractXAiAskUserQuestionIdentity(questionParams),
    answers: answerCases.map((answers) => ({
      answers,
      response: makeXAiAskUserQuestionResponse(questionParams, answers),
    })),
  },
  exitPlan: exitPlanCases.map(([params, fallback]) => ({
    params,
    fallback,
    markdown: extractXAiExitPlanMarkdown(params, fallback),
  })),
  planPaths: planPathCases.map((path) => ({
    path,
    linux: isGrokPlanMarkdownPath(path, host),
    win32: isGrokPlanMarkdownPath(path, { ...host, platform: "win32" }),
  })),
  planData: planDataCases.map((data) => ({
    data,
    markdown: extractGrokPlanMarkdownFromToolCallData(data, host) ?? null,
  })),
  host,
  launch: {
    spawnArgs: [undefined, "auto", "full-access", "approval-required", "auto-accept-edits"].map(
      (mode) => ({ mode: mode ?? null, args: grokAcpSpawnArgs(mode) }),
    ),
    approvals: [
      ["reject_once", "allow_once"],
      ["allow_always:allow-edits-session", "allow_once", "reject_always"],
      ["allow_always:other"],
      [],
    ].map((spec) => {
      const options = spec.map((entry, index) => {
        const [kind, optionId] = entry.split(":");
        return { optionId: optionId ?? `o${index}`, name: kind, kind };
      });
      const request = { sessionId: "s", toolCall: { toolCallId: "t" }, options };
      return { request, options: grokApprovalOptions(request) };
    }),
    models: [undefined, null, "", "  ", "grok-4.7", " grok-build "].map((model) => ({
      model: model ?? null,
      id: resolveGrokAcpBaseModelId(model),
    })),
    efforts: ["high", "x-high", "a", "", "-no", "a".repeat(32), "a".repeat(33), "ok.1_2"].map(
      (value) => ({ value, valid: isGrokReasoningEffortTokenSafe(value) }),
    ),
  },
};
function isGrokReasoningEffortTokenSafe(value) {
  return isValidGrokReasoningEffortToken(value);
}
const initialize = JSON.parse(
  readFileSync(`${fixtures}simple/grok_transcript.ndjson`, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line))
    .find((entry) => entry.label === "response:initialize") === undefined
    ? "null"
    : JSON.stringify(
        readFileSync(`${fixtures}simple/grok_transcript.ndjson`, "utf8")
          .split("\n")
          .filter(Boolean)
          .map((line) => JSON.parse(line))
          .find((entry) => entry.label === "response:initialize").frame.result,
      ),
);
const modelState = initialize._meta.modelState;
rows.provider = {
  modelsOutputs: [
    "You are logged in with grok.com.\nDefault model: grok-4.6\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n  - grok-4.5\r\n  * grok-build",
    "Not authenticated. Run `grok login`.\n",
    "usage: grok [options]\n  - something",
    "",
  ].map((output) => ({ output, parsed: parseGrokModelsCliOutput(output) })),
  modelState,
  models: buildGrokModelsFromSessionModelState(modelState),
  modelStates: [
    {
      currentModelId: "b",
      availableModels: [
        { modelId: "a", name: " " },
        { modelId: "b", name: "B", _meta: { supportsReasoningEffort: false } },
        { modelId: "a", name: "dup" },
      ],
    },
    { currentModelId: "x", availableModels: [] },
    {
      currentModelId: " a ",
      availableModels: [
        {
          modelId: "a ",
          name: "A",
          _meta: {
            reasoningEfforts: [{ id: "bad id" }, { id: "hi", isDefault: true }, { value: "hi" }],
            reasoningEffort: "zzz",
          },
        },
      ],
    },
  ].map((state) => ({ state, models: buildGrokModelsFromSessionModelState(state) })),
  slashCommands: [
    { _meta: initialize._meta },
    {
      _meta: {
        availableCommands: [
          { name: " compact ", description: "own", input: { hint: " what " } },
          { name: "Always-Approve", description: "x" },
          { name: "CONTEXT", description: "x" },
          { name: "init" },
          { name: "review", description: " r ", input: null },
          { name: "bad", description: "x", input: { nohint: 1 } },
          "nope",
        ],
      },
    },
    {},
  ].map((init) => ({ initialize: init, commands: grokSlashCommandsFromInitialize(init) })),
  usage: [
    {},
    { config: {} },
    {
      config: {
        creditUsagePercent: 42.5,
        currentPeriod: { type: "USAGE_PERIOD_TYPE_WEEKLY", end: "2026-10-12T00:00:00Z" },
      },
    },
    { config: { creditUsagePercent: 140, currentPeriod: { type: "USAGE_PERIOD_TYPE_MONTHLY" } } },
    { config: { creditUsagePercent: -3, currentPeriod: { type: "OTHER", end: "not a date" } } },
  ].map((response) => ({
    response,
    limits: grokUsageResponseToLimits(response, "2026-10-09T00:00:00.000Z"),
  })),
};
writeFileSync(
  new URL("../crates/server/tests/fixtures/grok-pure.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(
  `${rows.tools.length} tool updates, ${rows.texts.length} texts, ${rows.notifications.length} notifications`,
);
process.exit(0);
