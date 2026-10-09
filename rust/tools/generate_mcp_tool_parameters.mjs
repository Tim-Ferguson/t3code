// Runs the original's own tool parameter decoding over cases derived from each tool's Effect Schema
// AST, and records what it accepts and the message it refuses with.
// `tests/fixtures/mcp-tool-parameters.jsonl` is the witness the port's decoding is checked against
// (`mcp_tool_input.rs`). The handlers never run: each fails at once, so only the decode shows.
//
// The cases are walked from the same AST the port interprets, so every field of every tool is
// reached: at each slot of the input (property, element, record entry, union member) a plain value
// the schema holds is replaced in turn by wrong types, null, blanks, values around each bound a
// check states (counted in UTF-16 units), a violation of each pattern, a missing required key and
// an unknown key; every union member is tried and malformed; and where a hand-written refinement
// reads several fields, every combination of the optional ones that it can tell apart. A string
// repeated many times is written as {"$repeat": [unit, count]}, and an array of one element repeated
// as {"$items": [element, count]}, in both input and decoded value.
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpSchema from "../../apps/server/node_modules/effect/dist/ai/McpSchema.js";
import { casesFor } from "./schema_cases.mjs";
import { OrchestratorToolkit } from "../../apps/server/src/mcp/toolkits/orchestrator/tools.ts";
import { ThreadToolkit } from "../../apps/server/src/mcp/toolkits/thread/tools.ts";
import { AttachmentToolkit } from "../../apps/server/src/mcp/toolkits/attachment/tools.ts";
import { ProjectToolkit } from "../../apps/server/src/mcp/toolkits/project/tools.ts";
import { EnvironmentToolkit } from "../../apps/server/src/mcp/toolkits/environment/tools.ts";
import { PreviewControlsToolkit } from "../../apps/server/src/mcp/toolkits/previewControls/tools.ts";
import { WorktreeToolkit } from "../../apps/server/src/mcp/toolkits/worktree/tools.ts";
import { PullRequestsToolkit } from "../../apps/server/src/mcp/toolkits/pullRequests/tools.ts";
import { PreviewStandardToolkit } from "../../apps/server/src/mcp/toolkits/preview/tools.ts";
import { HtmlRenderToolkit } from "../../apps/server/src/mcp/toolkits/html/tools.ts";
import { DeviceStandardToolkit } from "../../apps/server/src/mcp/toolkits/device/tools.ts";
import { OrchestratorMcpFailure } from "../../packages/contracts/src/index.ts";

// The first four thread tools have their own witness (generate_mcp_control_parameters.mjs). The
// three image tools are registered by hand in McpHttpServer.ts, which decodes with default options.
const toolkits = [
  OrchestratorToolkit,
  ThreadToolkit,
  AttachmentToolkit,
  ProjectToolkit,
  EnvironmentToolkit,
  PreviewControlsToolkit,
  WorktreeToolkit,
  PullRequestsToolkit,
  PreviewStandardToolkit,
  HtmlRenderToolkit,
  DeviceStandardToolkit,
];
const skipped = new Set([
  "t3_thread_list",
  "t3_thread_read",
  "t3_thread_wait",
  "t3_thread_interrupt",
]);

let decoded;
const layers = toolkits.map((toolkit) => {
  const handlers = Object.fromEntries(
    Object.keys(toolkit.tools).map((name) => [
      name,
      (input) => {
        decoded = input;
        return Effect.fail(
          new OrchestratorMcpFailure({ code: "thread_not_found", message: "Source refusal" }),
        );
      },
    ]),
  );
  return McpServer.toolkit(toolkit).pipe(Layer.provide(toolkit.toLayer(handlers)));
});
const layer = Layer.mergeAll(...layers).pipe(Layer.provideMerge(McpServer.McpServer.layer));
const client = McpSchema.McpServerClient.of({
  clientId: 1,
  clientCapabilities: {},
  clientInfo: { name: "fixture", version: "1" },
  protocolVersion: "2025-06-18",
  initializePayload: {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "fixture", version: "1" },
  },
  getClient: Effect.die("unused"),
});

// A long run of one unit is written as a marker.
const compress = (value) => {
  if (typeof value === "string") {
    const unit = [...value][0];
    const count = unit ? [...value].length : 0;
    if (count >= 64 && unit.repeat(count) === value) return { $repeat: [unit, count] };
    return value;
  }
  if (Array.isArray(value)) {
    if (
      value.length > 20 &&
      value.every((item) => JSON.stringify(item) === JSON.stringify(value[0]))
    )
      return { $items: [compress(value[0]), value.length] };
    return value.map(compress);
  }
  if (value && typeof value === "object")
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, compress(item)]));
  return value;
};

const rows = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      const out = [];
      for (const toolkit of toolkits)
        for (const tool of Object.values(toolkit.tools)) {
          if (skipped.has(tool.name)) continue;
          for (const input of casesFor(tool.parametersSchema.ast)) {
            decoded = undefined;
            const exit = yield* Effect.exit(
              server
                .callTool({ name: tool.name, arguments: input })
                .pipe(Effect.provideService(McpSchema.McpServerClient, client)),
            );
            const raw = JSON.parse(JSON.stringify(exit));
            const accepted = raw._tag === "Success" || decoded !== undefined;
            out.push({
              name: tool.name,
              input: compress(input),
              accepted,
              ...(decoded !== undefined
                ? { decoded: compress(JSON.parse(JSON.stringify(decoded))) }
                : { error: raw.cause?.failures?.[0]?.error?.message ?? null }),
            });
          }
        }
      return out;
    }),
  ).pipe(Effect.provide(layer)),
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-tool-parameters.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    cases: rows.length,
    accepted: rows.filter((row) => row.accepted).length,
    tools: new Set(rows.map((row) => row.name)).size,
  }),
);
