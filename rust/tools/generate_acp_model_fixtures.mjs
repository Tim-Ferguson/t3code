// Development oracle: Node24, original checkout and its installed dependencies.
// node rust/tools/generate_acp_model_fixtures.mjs
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as core from "../../apps/server/src/provider/acp/AcpCoreRuntimeEvents.ts";
import {
  acpContentBlockDisplayText,
  parseSessionModeState,
  parseSessionUpdateEvent,
} from "../../apps/server/src/provider/acp/AcpRuntimeModel.ts";
import {
  normalizeAcpRegistryLiveConfiguration,
  normalizeAcpRegistryCommands,
} from "../../apps/server/src/provider/acp/AcpRegistryProbe.ts";
import { createModelCapabilities } from "../../packages/shared/src/model.ts";
import { providerModelsFromSettings } from "../../apps/server/src/provider/providerSnapshot.ts";
const cases = [];
function extract(path, start, end) {
  const source = fs.readFileSync(new URL(path, import.meta.url), "utf8");
  const begin = source.indexOf(start);
  const finish = source.indexOf(end, begin);
  if (begin < 0 || finish < 0) throw new Error(`Missing source boundary: ${path}`);
  return stripTypeScriptTypes(source.slice(begin, finish));
}
const modelsFromDiscovery = new Function(
  "createModelCapabilities",
  "providerModelsFromSettings",
  `const EMPTY_CAPABILITIES=createModelCapabilities({optionDescriptors:[]});${extract("../../apps/server/src/provider/Drivers/AcpRegistryDriver.ts", "function modelsFromDiscovery(", "export function acpRegistrySnapshotReadiness")}return modelsFromDiscovery;`,
)(createModelCapabilities, providerModelsFromSettings);
const selectPermissionOptionId = new Function(
  `${extract("../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts", "function selectPermissionOptionId(", "/**\n * The runtime policy approves")}return selectPermissionOptionId;`,
)();
const add = (operation, input, output) => cases.push({ operation, input, output });
const base = {
  stamp: { eventId: "event", createdAt: "2026-10-08T00:00:00.000Z" },
  provider: "acpRegistry",
  threadId: "thread",
  turnId: "turn",
  source: "acp.jsonrpc",
  method: "session/request_permission",
  rawPayload: { opaque: true },
};
const calls = {
  "request.opened": core.makeAcpRequestOpenedEvent,
  "request.resolved": core.makeAcpRequestResolvedEvent,
  "turn.plan.updated": core.makeAcpPlanUpdatedEvent,
  tool: core.makeAcpToolCallEvent,
  assistant: core.makeAcpAssistantItemEvent,
  "content.delta": core.makeAcpContentDeltaEvent,
};
const event = (operation, input) => {
  const value = { ...base, ...input };
  add(operation, value, calls[operation](value));
};
for (const kind of [
  "execute",
  "read",
  "edit",
  "delete",
  "move",
  "search",
  "fetch",
  "think",
  "unknown",
  "",
]) {
  for (const decision of ["accept", "acceptAlways", "decline", "cancel"])
    event("request.resolved", { requestId: "request", permissionRequest: { kind }, decision });
  event("request.opened", {
    requestId: "request",
    permissionRequest: { kind },
    detail: "Approve?",
    args: { command: "pwd" },
  });
  for (const status of ["pending", "inProgress", "requiresAction", "completed", "failed"])
    event("tool", {
      toolCall: {
        toolCallId: "tool",
        kind,
        status,
        title: "Tool",
        detail: "detail",
        data: { rawInput: { command: "pwd" } },
      },
    });
}
event("request.opened", {
  requestId: "request",
  permissionRequest: { kind: "execute" },
  detail: "Approve?",
  args: null,
  approvalOptions: [],
});
for (const payload of [
  { kind: "items", plan: [{ step: "One", status: "pending" }] },
  { kind: "items", plan: [], explanation: null },
  { kind: "items", plan: [], explanation: "Why" },
  { kind: "removed" },
  { kind: "markdown", markdown: "Plan" },
  { kind: "file", uri: "file:///plan.md" },
  { kind: "unknown", contentType: "future" },
])
  event("turn.plan.updated", { payload });
for (const lifecycle of ["item.started", "item.completed"])
  event("assistant", { lifecycle, itemId: "message" });
for (const streamKind of [undefined, "assistant_text", "reasoning_text"])
  event("content.delta", {
    text: " spaces \n",
    ...(streamKind ? { streamKind } : {}),
    itemId: "message",
  });
event("content.delta", { text: "", itemId: "" });
event("tool", {
  toolCall: { toolCallId: "tool", data: {}, title: "", detail: "" },
  turnId: undefined,
});
for (const input of [
  { type: "text", text: " \nHello 😃\t" },
  { type: "text", text: "x".repeat(65537) },
  {
    type: "resource_link",
    uri: "https://example.com",
    name: " Name ",
    title: " Title ",
    description: " Description ",
  },
  { type: "resource_link", uri: " Data:secret ", name: "", description: "" },
  {
    type: "resource_link",
    uri: "https://example.com",
    title: "\ufefftrim\ufeff",
    description: "x".repeat(2050),
  },
  {
    type: "resource",
    resource: { uri: "file:///binary", mimeType: " application/octet-stream ", blob: "secret" },
  },
  { type: "resource", resource: { uri: "data:secret", blob: "secret" } },
  { type: "resource", resource: { uri: "file:///text", text: " body\n" } },
  { type: "image", mimeType: "image/png", data: "secret", uri: "data:secret" },
  { type: "image", mimeType: "", data: "secret", uri: "https://example.com/image" },
  { type: "audio", mimeType: " audio/wav ", data: "secret" },
  { type: "audio", mimeType: "", data: "secret" },
  { type: "_t3_unknown", originalType: " future ", raw: { secret: true } },
  { type: "_t3_unknown", originalType: "", raw: null },
])
  add("content", input, acpContentBlockDisplayText(input) ?? null);
const model = (value, name = value) => ({ value, name });
const config = (category, values, currentValue = "a", extra = {}) => ({
  id: category,
  name: category,
  type: "select",
  category,
  currentValue,
  options: values,
  ...extra,
});
const modes = {
  currentModeId: " normal ",
  availableModes: [
    { id: " normal ", name: " Normal " },
    { id: "plan", name: "Plan", description: " Review " },
  ],
};
const catalogs = [
  {},
  {
    models: { currentModelId: "legacy", availableModels: [{ modelId: "legacy", name: "Legacy" }] },
  },
  { configOptions: [config("model", [model("a"), model("b")], "b")] },
  { configOptions: [config("model", [model("a"), model("b")], "missing")] },
  {
    configOptions: [
      config(
        "model",
        [model(" a"), model("a "), model("a"), model("a", "duplicate"), model("b", "  Name  ")],
        "b",
      ),
    ],
  },
  {
    configOptions: [
      config("model", [{ group: "g", name: "Group", options: [model("a"), model("b")] }], "b"),
    ],
  },
  {
    configOptions: [config("model", [model("a")], "a")],
    models: { currentModelId: "legacy", availableModels: [{ modelId: "legacy", name: "Legacy" }] },
  },
  { configOptions: [config("model", [model("x".repeat(129)), model("a", "N".repeat(200))], "a")] },
  {
    configOptions: [
      config(
        "model",
        Array.from({ length: 258 }, (_, i) => model(`model-${i}`)),
        "model-257",
      ),
    ],
  },
  { configOptions: [config("thought_level", [model("low"), model("high")], "high")], modes },
  { configOptions: [{ id: "bool", name: "Boolean", type: "boolean", currentValue: false }], modes },
  { configOptions: [config("mode", [model("normal"), model("plan")], "normal")] },
  { configOptions: [config("thought_level", [model("normal"), model("plan")], "normal")], modes },
  {
    configOptions: [
      config("collaboration_mode", [model("normal"), model("plan")], "plan"),
      config("empty", [], "x"),
      config("empty", [model("x")], "x"),
    ],
  },
  {
    configOptions: Array.from({ length: 18 }, (_, i) =>
      config(
        `option-${i}`,
        Array.from({ length: 66 }, (_, j) => model(`choice-${j}`)),
        "choice-65",
      ),
    ),
    modes,
  },
];
for (const setup of catalogs)
  for (const custom of [[], [" custom ", "custom", "a", "", "another"]]) {
    const discovery = normalizeAcpRegistryLiveConfiguration(
      setup.configOptions ?? [],
      parseSessionModeState(setup),
    );
    add("catalog", { setup, custom }, modelsFromDiscovery(discovery, custom));
    if (custom.length === 0) add("live-configuration", setup, discovery);
  }
for (const commands of [
  [],
  [
    { name: "help", description: " Help ", input: { hint: " topic " } },
    { name: "HELP", description: "ignored" },
  ],
  [
    { name: "$space/skill", description: " Skill " },
    { name: "$a!'()*~", description: "" },
    { name: "$", description: "ignored" },
    { name: "$ spaced", description: "ignored" },
  ],
  [
    { name: "ΟΣ", description: "first" },
    { name: "ος", description: "second" },
    { name: "Σ", description: "" },
    { name: "σ", description: "duplicate" },
  ],
  [
    { name: " invalid ", description: "ignored" },
    { name: "x".repeat(129), description: "ignored" },
    { name: "ok", description: "x".repeat(1025) },
  ],
  Array.from({ length: 132 }, (_, i) => ({
    name: i === 0 ? "$" : "command-" + i,
    description: "",
  })),
])
  add("commands", commands, normalizeAcpRegistryCommands(commands));
for (const options of [
  [],
  [{ optionId: " once ", kind: "allow_once", name: "Once" }],
  [{ optionId: "always", kind: "allow_always", name: "Always" }],
  [{ optionId: "reject", kind: "reject_once", name: "Reject" }],
  [
    { optionId: "", kind: "allow_once", name: "Empty" },
    { optionId: "later", kind: "allow_once", name: "Later" },
  ],
]) {
  const request = {
    sessionId: "session",
    toolCall: { toolCallId: "tool", kind: "execute" },
    options,
  };
  for (const decision of ["accept", "acceptForSession", "decline", "cancel"]) {
    const option = decision === "cancel" ? undefined : selectPermissionOptionId(request, decision);
    add(
      "permission",
      { request, decision },
      option === undefined
        ? { outcome: { outcome: "cancelled" } }
        : { outcome: { outcome: "selected", optionId: option } },
    );
  }
}
for (const update of [
  { sessionUpdate: "plan", entries: [] },
  {
    sessionUpdate: "plan",
    entries: [
      { content: "   ", status: "in_progress", priority: "high" },
      { content: " Done ", status: "completed", priority: "medium" },
    ],
  },
  ...["items", "markdown", "file", "future"].flatMap((type) =>
    ["", " plan "].map((planId) => ({
      sessionUpdate: "plan_update",
      plan: {
        type,
        planId,
        entries: [
          { content: " one ", status: "inProgress" },
          { content: "", status: "unknown" },
          { content: 42 },
        ],
        content: " Markdown ",
        uri: "file:///plan.md",
      },
    })),
  ),
  ...["", " plan "].map((planId) => ({ sessionUpdate: "plan_removed", planId })),
])
  add(
    "plan",
    update,
    parseSessionUpdateEvent({ sessionId: "session", update }).events.find(
      (event) => event._tag === "PlanUpdated",
    )?.payload ?? null,
  );
for (const content of [
  { type: "text", text: "" },
  { type: "text", text: " think \n" },
  { type: "text", text: "x".repeat(65537) },
  { type: "image", mimeType: "image/png", data: "AQ==" },
  { type: "resource_link", name: "Document", uri: "file:///doc" },
])
  add(
    "thought",
    content,
    parseSessionUpdateEvent({
      sessionId: "session",
      update: { sessionUpdate: "agent_thought_chunk", content },
    }).events.find((event) => event._tag === "ThoughtDelta")?.text ?? null,
  );
const capabilities = extract(
  "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts",
  "export const AcpProviderCapabilitiesV2 =",
  "function negotiatedCapabilities(",
);
const value = new Function(
  capabilities.replace("export const", "const") + "return AcpProviderCapabilitiesV2;",
)();
fs.writeFileSync(
  new URL("../crates/server/src/acp-capabilities.json", import.meta.url),
  JSON.stringify(value, null, 2) + "\n",
);
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-model.jsonl", import.meta.url),
  cases.map((value) => JSON.stringify(value)).join("\n") + "\n",
);
console.log(`Generated ${cases.length} ACP model/core event fixtures`);
