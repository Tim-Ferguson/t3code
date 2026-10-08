// Development oracle: Node24 and the unchanged original checkout/dependencies.
// node rust/tools/generate_acp_mcp_tool_fixtures.mjs
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import {
  parseSessionUpdateEvent,
  mergeToolCallState,
  extractMcpToolCallIdentity,
} from "../../apps/server/src/provider/acp/AcpRuntimeModel.ts";
import { mcpToolPresentation } from "../../apps/server/src/provider/McpToolPresentation.ts";
import { T3_MCP_TOOL_NAMES } from "../../packages/shared/src/t3McpToolPresentation.ts";
const fixtures = [];
const add = (operation, input, output) => fixtures.push({ operation, input, output });
const parse = (update) =>
  parseSessionUpdateEvent({ sessionId: "session", update }).events.find(
    (event) => event._tag === "ToolCallUpdated",
  ).toolCall;
const base = { sessionUpdate: "tool_call", toolCallId: "tool", status: "pending", kind: "other" };
for (const name of T3_MCP_TOOL_NAMES)
  for (const title of [
    `t3-code___${name}`,
    `t3-code-${name}`,
    `mcp__t3_code__${name}: {"mode":"async"}`,
    `t3-code__${name}: {"mode":"async"}`,
    `${name}_t3-code`,
    `t3-code/${name}`,
    `${name}: {"mode":"async"}`,
    `${name} (t3-code MCP Server): {}`,
    name,
  ]) {
    const tool = parse({ ...base, title });
    add("identity", { tool }, extractMcpToolCallIdentity(tool) ?? null);
  }
for (const extra of [
  {
    kind: "execute",
    title: "mcp.t3-code.orchestrator_capabilities",
    rawInput: { server: "t3-code", tool: "orchestrator_capabilities", arguments: {} },
    _meta: { is_mcp_tool_call: true },
  },
  { kind: "execute", title: "terminal", rawInput: { server: "production", tool: "deploy" } },
  {
    title: "Checking",
    _meta: { goose: { toolCall: { toolName: "t3-code__task_status", extensionName: "t3-code" } } },
  },
  {
    title: "foreign misleading",
    _meta: { toolName: "mcp::t3-code::t3_thread_send", serverId: "t3-code" },
  },
  {
    title: "t3-code_delegate_task",
    _meta: { toolName: "delegate_task", serverId: "other-orchestrator" },
  },
  ...["developer", "weather", "t3-code"].map((extensionName) => ({
    title: "t3-code_delegate_task",
    _meta: { goose: { toolCall: { toolName: "developer__shell", extensionName } } },
  })),
  ...["mcp__weather__get_weather", "mcp__t3-code__task_status", "mcp__weather__"].map(
    (toolName) => ({ _meta: { claudeCode: { toolName } } }),
  ),
  ...["mcp::weather::get_weather", "mcp__weather__get_weather", "weather__get_weather", ""].map(
    (toolName) => ({ _meta: { toolName, serverId: "weather" } }),
  ),
  ...[
    "mcp::t3-code::task_status",
    "future_task_status",
    "atask_status",
    "t3-code/README.md",
    "t3-code_not_a_real_tool",
    "delegate_task completed",
    "",
  ].map((title) => ({ title, _meta: { serverId: "t3-code", toolName: title } })),
  ...[
    "t3-code/README.md",
    "t3-code_not_a_real_tool",
    "cat package.json",
    "delegate_task completed",
    " mcp__db__read ",
    "t3-code___unknown",
  ].map((title) => ({ title })),
]) {
  const tool = parse({ ...base, ...extra });
  add("identity", { tool }, extractMcpToolCallIdentity(tool) ?? null);
}
for (const command of [
  'acp-mcp-call delegate_task {"task":"x"}',
  '/usr/bin/node bin.ts acp-mcp-call delegate_task \'{"task":"x"}\'',
  'acp-mcp-call delegate_task "{\\"task\\":\\"x\\"}"',
  "acp-mcp-call delegate_task nope",
  "acp-mcp-call delegate_task []",
  "acp-mcp-call unknown_tool {}",
  "bash -lc ls",
  "prefixacp-mcp-call delegate_task {}",
  "acp-mcp-call delegate_task",
]) {
  for (const source of ["command", "title", "embedded"]) {
    const tool = parse({
      ...base,
      ...(source === "title" ? { title: command } : {}),
      ...(source === "command" ? { kind: "execute", rawInput: { command } } : {}),
    });
    const embedded = source === "embedded" ? [command] : [];
    add(
      "identity",
      { tool, embedded },
      extractMcpToolCallIdentity(tool, { embeddedTerminalCommands: embedded }) ?? null,
    );
  }
}
for (const title of [
  "mcp__weather__get\rweather",
  "mcp__weather__get\u2028weather",
  "mcp__weather\u2029__get",
  "t3-code_delegate_task:\u2028tail",
  "mcp__weather__get\nweather",
]) {
  const tool = parse({ ...base, title });
  add("identity", { tool }, extractMcpToolCallIdentity(tool) ?? null);
}
const before = parse({ ...base, title: "t3-code_orchestrator_capabilities", rawInput: {} });
const after = mergeToolCallState(
  before,
  parse({
    ...base,
    sessionUpdate: "tool_call_update",
    title: undefined,
    status: "completed",
    rawOutput: { content: [{ type: "text", text: "ok" }] },
  }),
);
add("identity", { tool: after }, extractMcpToolCallIdentity(after) ?? null);
const adapterSource = fs.readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts", import.meta.url),
  "utf8",
);
const begin = adapterSource.indexOf("function acpMcpToolCallOutput(");
const end = adapterSource.indexOf("function nonEmptyText(", begin);
if (begin < 0 || end < 0) throw new Error("MCP output source oracle boundary missing");
const output = new Function(
  "unknownRecord",
  stripTypeScriptTypes(adapterSource.slice(begin, end)) + "return acpMcpToolCallOutput;",
)((value) =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value : undefined,
);
for (const input of [
  null,
  "raw",
  [],
  {},
  { content: [1] },
  { result: { structuredContent: { ok: true }, content: [{ type: "text", text: "fallback" }] } },
  { result: { structuredContent: null, content: [] } },
  { result: {} },
  { result: null },
  { error: { message: "bad" } },
  { error: { message: "bad" }, result: { structuredContent: 0 } },
  { error: { message: 4 }, result: { content: "yes" } },
  { error: { message: "bad" }, result: { content: null } },
  { error: { message: "" }, result: { content: "" } },
])
  add("output", input, output(input));
for (const serverName of [
  undefined,
  "t3-code",
  "t3_code",
  "t3code",
  "t3 code",
  "weather",
  " weather ",
  " ",
  "a".repeat(161),
])
  for (const toolName of [
    "delegate_task",
    "read_file",
    "mcp__weather__get_weather",
    "mcp__t3-code__delegate_task",
    "multi--word_name",
    undefined,
  ]) {
    add("presentation", { serverName, toolName }, mcpToolPresentation({ serverName, toolName }));
  }
for (const input of [
  {
    serverName: "weather",
    toolName: "get_weather",
    title: " Custom \n title ",
    source: {
      name: " Weather \t Service ",
      logoUrl: "https://example.test/icon.png",
      logoUrlDark: "http://example.test/dark.png",
    },
  },
  {
    serverName: "weather",
    toolName: "get_weather",
    iconUrl: "https://EXAMPLE.test:443/a/../icon",
    source: { name: "Other", logoUrl: "https://fallback.test" },
  },
  ...[
    "javascript:alert(1)",
    "file:///x",
    "",
    " https://example.test ",
    "http://例え.test/x",
    "http:example.test",
    "http://example.test:80",
    "http://example.test/a?x=😃",
    "http://example.test/" + "x".repeat(4090),
  ].map((iconUrl) => ({ serverName: "weather", toolName: "get_weather", iconUrl })),
  {
    serverName: "weather",
    toolName: "read",
    title: "a".repeat(161),
    serverDisplayName: " ",
    source: { name: " \u0085 ", logoUrlDark: "https://dark.test" },
  },
  {
    serverName: "t3-code",
    toolName: "delegate_task",
    title: "override",
    iconUrl: "https://example.test/icon",
  },
])
  add("presentation", input, mcpToolPresentation(input));
fs.writeFileSync(
  new URL("../crates/server/src/acp-mcp-tool-names.json", import.meta.url),
  JSON.stringify([...T3_MCP_TOOL_NAMES], null, 2) + "\n",
);
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-mcp-tools.jsonl", import.meta.url),
  fixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
console.log(`Generated ${fixtures.length} ACP MCP identity/output/presentation source cases`);
