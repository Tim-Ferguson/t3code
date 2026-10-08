// Development oracle: Node24, unchanged original checkout and dependencies.
// node rust/tools/generate_acp_tool_fixtures.mjs
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import {
  parseSessionUpdateEvent,
  mergeToolCallState,
  toolCallProgressLength,
  decideToolCallUpdateEmission,
} from "../../apps/server/src/provider/acp/AcpRuntimeModel.ts";
import {
  deriveToolActivityPresentation,
  mergeToolActivityData,
  collectToolFilePaths,
  formatSearchToolLabel,
  formatReadToolLabel,
} from "../../packages/shared/src/toolActivity.ts";

const fixtures = [];
const adapterSource = fs.readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts", import.meta.url),
  "utf8",
);
function extract(start, end) {
  const begin = adapterSource.indexOf(start);
  const finish = adapterSource.indexOf(end, begin);
  if (begin < 0 || finish < 0) throw new Error("ACP adapter oracle boundary missing");
  return stripTypeScriptTypes(adapterSource.slice(begin, finish)).replaceAll(
    "export function",
    "function",
  );
}
const unknownRecord = (value) =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value : undefined;
const textFromUnknown = new Function(
  "unknownRecord",
  `${extract("function decodeByteText(", "function commandExitCode(")}return textFromUnknown;`,
)(unknownRecord);
const projectedCode = new Function(
  "unknownRecord",
  `${extract("function commandExitCode(", "function structuredFileChanges(")}return acpProjectedCommandExitCode;`,
)(unknownRecord);
const projectMarker = adapterSource.indexOf("const projectAsCommandExecution =");
const readBegin = adapterSource.indexOf('case "read":', projectMarker);
const readEnd = adapterSource.indexOf('case "search":', readBegin);
if (projectMarker < 0 || readBegin < 0 || readEnd < 0)
  throw new Error("ACP read projection boundary missing");
const readProjection = new Function(
  "base",
  "path",
  "rawInputRecord",
  "rawOutput",
  "title",
  "formatReadToolLabel",
  stripTypeScriptTypes(
    `function project(){let turnItem; switch("read"){${adapterSource.slice(readBegin, readEnd)}}return turnItem;}`,
  ) + "return project();",
);
const add = (operation, input, output) => fixtures.push({ operation, input, output });
const parse = (update) =>
  parseSessionUpdateEvent({ sessionId: "session", update }).events.find(
    (event) => event._tag === "ToolCallUpdated",
  )?.toolCall;
const content = (text) => ({ type: "content", content: { type: "text", text } });
const base = {
  sessionUpdate: "tool_call",
  toolCallId: " tool ",
  kind: "execute",
  title: "Terminal",
  rawInput: { executable: "bun", args: [" run ", "test"] },
};
for (const kind of [
  "execute",
  "read",
  "edit",
  "move",
  "delete",
  "search",
  "fetch",
  "think",
  "other",
  " EXECUTE ",
  "",
]) {
  for (const status of [
    undefined,
    "pending",
    "in_progress",
    "inProgress",
    "completed",
    "failed",
    "requiresAction",
    "unknown",
  ]) {
    for (const sessionUpdate of ["tool_call", "tool_call_update"]) {
      const update = {
        ...base,
        kind,
        status,
        sessionUpdate,
        content: [content(" Running checks ")],
        locations: [{ path: " src/a.ts ", line: 7 }],
      };
      add("parse", update, parse(update) ?? null);
    }
  }
}
for (const extra of [
  { toolCallId: " " },
  { title: null, kind: null, status: null },
  { title: "Run ` pwd `", rawInput: {} },
  { kind: "read", title: "Read ` src/a.ts `", rawInput: {} },
  { rawInput: { command: [" pwd ", 42, "", null, " -P "] } },
  { rawInput: { command: " ", executable: " bash ", args: " -lc pwd " } },
  { rawInput: null, rawOutput: null },
  { _meta: { toolName: "shell", serverId: "mcp" } },
  { locations: null, rawInput: { path: "a" } },
  { locations: [], rawInput: { path: "a" } },
  {
    locations: [{ path: " a ", line: 3 }, { path: "a", line: 4 }, { path: "b" }],
    rawInput: { path: "c" },
    rawOutput: { file_path: "d" },
  },
  {
    content: [{ type: "diff", path: "diff", oldText: "old", newText: "new" }],
    rawInput: { file_path: "input" },
  },
  {
    content: [
      { type: "diff", changes: [{ path: "v2", operation: "add" }], patch: { text: "patch" } },
    ],
  },
  { content: [content("x".repeat(8001))] },
  { content: [content(" ".repeat(8001))] },
  {
    content: [
      content("a".repeat(6000)),
      { type: "diff", path: "a", newText: "b" },
      content("b".repeat(6000)),
      content("  "),
    ],
  },
  {
    content: [
      content("a".repeat(9000)),
      { type: "content", content: { type: "image", data: "secret", mimeType: "image/png" } },
      content("b".repeat(1000)),
    ],
  },
  {
    content: [
      { type: "terminal", terminalId: "terminal" },
      { type: "_t3_unknown", originalType: " future ", raw: { secret: true } },
    ],
  },
  {
    rawOutput: {
      content: "x".repeat(8001),
      stdout: "y".repeat(9000),
      stderr: "z",
      output: " ".repeat(8001),
      untouched: "u".repeat(9000),
    },
  },
]) {
  const update = { ...base, ...extra };
  add("parse", update, parse(update) ?? null);
}

for (const updates of [
  [
    base,
    { sessionUpdate: "tool_call_update", toolCallId: "tool", rawInput: {}, status: "completed" },
  ],
  [
    { ...base, rawInput: { path: "a", command: "pwd" } },
    { sessionUpdate: "tool_call_update", toolCallId: "tool", rawInput: { filePath: "b" } },
  ],
  [
    { ...base, locations: [{ path: "a" }] },
    { sessionUpdate: "tool_call_update", toolCallId: "tool", locations: [] },
  ],
  [
    { ...base, rawOutput: { stdout: "old" } },
    { sessionUpdate: "tool_call_update", toolCallId: "tool", rawOutput: null },
  ],
  [
    base,
    ...Array.from({ length: 12 }, (_, i) => ({
      sessionUpdate: "tool_call_update",
      toolCallId: "tool",
      content: [content(String(i))],
    })),
    { sessionUpdate: "tool_call_update", toolCallId: "tool", status: "completed" },
  ],
  [
    { ...base, content: [content("same")] },
    ...Array.from({ length: 12 }, () => ({
      sessionUpdate: "tool_call_update",
      toolCallId: "tool",
      content: [content("same")],
    })),
  ],
  [
    { ...base, rawOutput: "same" },
    ...Array.from({ length: 12 }, () => ({
      sessionUpdate: "tool_call_update",
      toolCallId: "tool",
      rawOutput: "same",
    })),
  ],
  [
    base,
    ...Array.from({ length: 12 }, () => ({
      sessionUpdate: "tool_call_update",
      toolCallId: "tool",
    })),
  ],
  [
    base,
    ...Array.from({ length: 5 }, (_, i) => ({
      sessionUpdate: "tool_call_update",
      toolCallId: "tool",
      rawOutput: { stdout: "x".repeat(i * 300) },
    })),
  ],
]) {
  let previous,
    lastEmittedDetailLength,
    skippedSinceEmit = 0;
  const output = updates.map((update) => {
    const next = mergeToolCallState(previous, parse(update));
    const decision = decideToolCallUpdateEmission({
      previous,
      next,
      lastEmittedDetailLength,
      skippedSinceEmit,
    });
    skippedSinceEmit = decision.skippedSinceEmit;
    if (decision.emit) lastEmittedDetailLength = toolCallProgressLength(next);
    previous = next;
    return { state: next, decision, progress: toolCallProgressLength(next) };
  });
  add("sequence", updates, output);
}
for (const data of [
  {},
  { kind: "read", rawInput: { path: " a ", other: { path: "ignored" } } },
  { toolName: "Read_File", rawInput: { file_path: "a" } },
  { toolName: "read file", rawInput: { file_path: "a" } },
  { toolName: "github.read_file", rawInput: { file_path: "a" } },
  { toolName: "mcp__db__find", rawInput: { path: "a" } },
  { kind: "execute", rawInput: { executable: "bash", args: [" -lc ", " pwd "] } },
  {
    item: {
      tool: "shell",
      command: "pwd",
      input: { command: "input" },
      result: { command: "output" },
    },
  },
  { rawInput: { pattern: "query", glob: "*.ts", path: "/a/work/" } },
  { rawInput: { glob: "*.ts", path: "C:\\work\\." } },
  { rawInput: { query: "query" } },
  { rawInput: { cwd: "/" } },
  { rawInput: {}, input: { glob_pattern: "*.rs", directory: "/workspace" } },
  { changes: Array.from({ length: 10 }, (_, i) => ({ path: `file-${i}` })) },
]) {
  add("paths", data, collectToolFilePaths(data));
  add("search", data, formatSearchToolLabel(data) ?? null);
  for (const itemType of [
    undefined,
    "command_execution",
    "file_change",
    "dynamic_tool_call",
    "web_search",
  ]) {
    for (const detail of [
      " detail ",
      "Tool started",
      "abc <exited with exit code 0>",
      "İ <exited with exit code 42>",
    ]) {
      const input = { itemType, title: " Tool ", detail, data, fallbackSummary: "Fallback" };
      add("presentation", input, deriveToolActivityPresentation(input));
    }
  }
}
for (const previous of [
  undefined,
  null,
  {},
  { rawInput: { path: "a", command: "pwd" }, content: [content("old")] },
]) {
  for (const next of [
    null,
    {},
    { rawInput: {} },
    { rawInput: null },
    { rawInput: { filePath: "b" }, rawOutput: null },
  ]) {
    add("data", { previous, next }, mergeToolActivityData(previous, next) ?? null);
  }
}
for (const value of [
  null,
  "",
  " padded ",
  [65, 66],
  [239, 187, 191, 65],
  [256, -1, 511],
  [],
  [240, 159],
  [1.5, 65],
  ["one", { text: "two" }],
  { stdout: "", stderr: "err" },
  { output_for_prompt: "prompt", stdout: "out" },
  { Result: { stdout: [65] } },
  { content: [{ type: "content", content: { type: "text", text: "body" } }] },
])
  add("output", value, textFromUnknown(value) ?? null);
for (const status of ["pending", "running", "waiting", "completed", "failed", "interrupted"])
  for (const output of [
    { exitCode: 0 },
    { exit_code: -1 },
    { code: 1.0 },
    { exitCode: 1.5, code: 2 },
    { exitCode: null, exit_code: 4 },
    {},
  ])
    add("exit", { status, output }, projectedCode(status, output) ?? null);
for (const input of [
  null,
  "scalar",
  ["array"],
  {},
  { path: "doc" },
  { filePath: "doc" },
  { file_path: "other", retained: true },
])
  for (const path of [undefined, "doc"])
    add(
      "readInput",
      { input, path },
      readProjection({}, path, unknownRecord(input), undefined, null, formatReadToolLabel).input,
    );
const nonEmptyText = new Function(
  `${extract("function nonEmptyText(", "function decodeByteText(")}return nonEmptyText;`,
)();
const labelStart = adapterSource.indexOf("const label = nonEmptyText(toolCall.data.title");
const labelEnd = adapterSource.indexOf(";", labelStart);
if (labelStart < 0 || labelEnd < 0) throw new Error("ACP backend search label boundary missing");
const sourceLabel = new Function(
  "toolCall",
  "title",
  "nonEmptyText",
  `${adapterSource.slice(labelStart, labelEnd + 1)}return label;`,
);
for (const dataTitle of [null, " ", "Web search:", " Web search::  ", "Web : ", "Web search", 4])
  for (const title of [null, " fallback "])
    for (const query of [null, "query"]) {
      const input = { dataTitle, title, query };
      const label = sourceLabel({ data: { title: dataTitle } }, title, nonEmptyText);
      add("backendTitle", input, query === null ? label : `${label}: ${query}`);
    }
const changes = new Function(
  "unknownRecord",
  `${extract("function structuredFileChanges(", "// Past this edit distance")}return structuredFileChanges;`,
)(unknownRecord);
const backendSearch = new Function(
  "unknownRecord",
  `${extract("function acpBackendWebSearch(", "function providerRequestKind(")}return acpBackendWebSearch;`,
)(unknownRecord);
for (const content of [
  null,
  [],
  [
    {
      type: "diff",
      changes: [
        {
          path: " a ",
          operation: " add ",
          oldPath: " old ",
          fileType: " text ",
          mimeType: " text/plain ",
          extra: 1,
        },
        { path: "b", operation: "modify" },
      ],
    },
  ],
  [
    {
      type: "diff",
      changes: [null, {}, { path: " ", operation: "add" }, { path: "a", operation: " " }],
    },
  ],
  [{ type: "other", changes: [{ path: "x", operation: "add" }] }],
])
  add("changes", { content }, changes({ data: { content } }));
for (const input of [
  { variant: "WebSearch", backend: true },
  { variant: "XSearch" },
  { variant: " websearch " },
  {},
])
  for (const output of [
    {},
    { input: '{"query":" query "}' },
    { input: '{"first":"x","second":1.0,"tiny":1e-8,"huge":1e21,"ignored":true}' },
    {
      input: "bad",
      action: {
        type: "search",
        query: " direct ",
        sources: [
          { url: " https://a " },
          { url: "https://a" },
          { url: " " },
          null,
          { url: "https://b" },
        ],
      },
    },
  ])
    add("backendSearch", { input, output }, backendSearch(input, output) ?? null);
for (const input of [
  {},
  { variant: " Monitor ", command: " cmd " },
  { variant: "monitor", command: [] },
  { variant: "ordinary" },
])
  for (const output of [
    {},
    { type: " Bash ", exit_code: 0, command: " output " },
    { type: "bash", exit_code: 1.5 },
    { type: "bash", code: 2 },
  ]) {
    const variant = typeof input.variant === "string" ? input.variant.trim().toLowerCase() : "";
    const command =
      (typeof input.command === "string" && input.command.trim()
        ? input.command.trim()
        : undefined) ??
      (typeof output.command === "string" && output.command.trim()
        ? output.command.trim()
        : undefined);
    add(
      "monitor",
      { input, output },
      {
        project:
          variant === "monitor" ||
          (typeof output.type === "string" &&
            output.type.trim().toLowerCase() === "bash" &&
            projectedCode("completed", output) !== undefined),
        command: command ?? null,
      },
    );
  }
const scopedIdentity = new Function(
  `${extract("export function acpScopedNativeId(", "export interface AcpAdapterV2SubagentUpdate")}return acpProviderItemNativeId;`,
)();
for (const instanceId of ["first", "second", "space / 😃", "%:!"])
  for (const nativeId of ["native-session:tool:command", "native / 😃"])
    for (const itemIdentityVersion of [undefined, 2]) {
      const input = { instanceId, nativeId, itemIdentityVersion };
      const scoped = scopedIdentity(input);
      add("identity", input, {
        item: ["turn-item", "provider", "acpRegistry", "native-item", scoped]
          .map((part, index) => (index === 0 ? part : encodeURIComponent(String(part))))
          .join(":"),
        node: ["node", "provider", "acpRegistry", "native-item", scoped]
          .map((part, index) => (index === 0 ? part : encodeURIComponent(String(part))))
          .join(":"),
      });
    }
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-tools.jsonl", import.meta.url),
  fixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
const unicodeFixtures = [
  [content("😃".repeat(5000))],
  [content("😃" + "x".repeat(7999))],
  [content("x".repeat(7999) + "😃")],
  [content("😃" + "x".repeat(3998)), content("y".repeat(4000))],
].map((content) => {
  const input = { ...base, content };
  const original = parse(input);
  const displayState = JSON.parse(
    JSON.stringify(original, (_, value) =>
      typeof value === "string" ? new TextDecoder().decode(new TextEncoder().encode(value)) : value,
    ),
  );
  const units = (value) =>
    Array.from({ length: value.length }, (_, index) => value.charCodeAt(index));
  const nativeUnits = original.data.content.map((entry) => units(entry.content.text));
  const displayUnits = displayState.data.content.map((entry) => units(entry.content.text));
  return {
    input,
    nativeUnits,
    displayState,
    wirePreserved: JSON.stringify(nativeUnits) === JSON.stringify(displayUnits),
  };
});
// Kept separate: two cases expose the known UTF8-string/lone-surrogate wire gap.
// These are display-equivalence evidence, never counted as codec wire parity.
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-tool-unicode.jsonl", import.meta.url),
  unicodeFixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
console.log(`Generated ${fixtures.length} ACP tool-state and presentation fixtures`);
