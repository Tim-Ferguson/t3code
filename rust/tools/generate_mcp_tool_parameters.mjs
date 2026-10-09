// Runs the original's own tool parameter decoding over cases derived from each tool's input
// schema, nested unions included, and records what it accepts and the message it refuses with.
// `tests/fixtures/mcp-tool-parameters.jsonl` is the witness the port's admission is checked
// against (`mcp_tool_input.rs`). The handlers never run: each fails at once, so only the decode shows.
import { writeFileSync, readFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpSchema from "../../apps/server/node_modules/effect/dist/ai/McpSchema.js";
import { OrchestratorToolkit } from "../../apps/server/src/mcp/toolkits/orchestrator/tools.ts";
import { ThreadToolkit } from "../../apps/server/src/mcp/toolkits/thread/tools.ts";
import { AttachmentToolkit } from "../../apps/server/src/mcp/toolkits/attachment/tools.ts";
import { ProjectToolkit } from "../../apps/server/src/mcp/toolkits/project/tools.ts";
import { EnvironmentToolkit } from "../../apps/server/src/mcp/toolkits/environment/tools.ts";
import { WorktreeToolkit } from "../../apps/server/src/mcp/toolkits/worktree/tools.ts";
import { PullRequestsToolkit } from "../../apps/server/src/mcp/toolkits/pullRequests/tools.ts";
import { OrchestratorMcpFailure } from "../../packages/contracts/src/index.ts";

const catalog = JSON.parse(
  readFileSync(new URL("../crates/server/src/mcp_tool_catalog.json", import.meta.url), "utf8"),
);
// The first four tools have their own witness (generate_mcp_control_parameters.mjs).
const skipped = new Set([
  "t3_thread_list",
  "t3_thread_read",
  "t3_thread_wait",
  "t3_thread_interrupt",
]);
const toolkits = [
  OrchestratorToolkit,
  ThreadToolkit,
  AttachmentToolkit,
  ProjectToolkit,
  EnvironmentToolkit,
  WorktreeToolkit,
  PullRequestsToolkit,
];

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

// A value the schema holds, as plain as possible.
const sample = (schema) => {
  if (schema.enum) return schema.enum[0];
  if (schema.anyOf)
    return sample(schema.anyOf.find((branch) => branch.type !== "null") ?? schema.anyOf[0]);
  switch (schema.type) {
    case "string":
      return schema.pattern
        ? "pending-00000000-0000-4000-8000-000000000000-txt"
        : schema.minLength > 1
          ? "x".repeat(schema.minLength)
          : "x";
    case "integer":
    case "number":
      return Math.max(1, schema.minimum ?? 1);
    case "boolean":
      return true;
    case "array":
      return schema.items ? [sample(schema.items)] : [];
    case "object": {
      const out = {};
      for (const key of schema.required ?? []) out[key] = sample(schema.properties[key]);
      return out;
    }
    default:
      return "x";
  }
};
const wrong = [
  null,
  false,
  0,
  1,
  -1,
  1.5,
  9007199254740992,
  "",
  "  ",
  " x ",
  [],
  {},
  "x".repeat(300),
  ["x"],
  [1],
  { unexpected: 1 },
];
const mutations = (schema, good, depth) => {
  const values = [good, ...wrong];
  if (schema.enum) values.push("not-in-the-enum");
  if (schema.type === "string" && schema.maxLength)
    values.push("x".repeat(schema.maxLength + 1), "x".repeat(schema.maxLength));
  if (schema.type === "array" && schema.items)
    values.push([sample(schema.items), sample(schema.items)], Array(9).fill(sample(schema.items)));
  return values;
};
const casesFor = (schema) => {
  const good = sample(schema);
  const cases = [{}, null, [], false, 0, "", good, { ...good, unknown: true }];
  for (const key of schema.required ?? []) {
    const { [key]: _omitted, ...rest } = good;
    cases.push(rest);
  }
  const visit = (node, path, base) => {
    for (const [key, property] of Object.entries(node.properties ?? {})) {
      for (const value of mutations(property, sample(property))) {
        const next = structuredClone(base);
        let cursor = next;
        for (const step of path) cursor = cursor[step];
        cursor[key] = value;
        cases.push(next);
      }
      // One level into every object branch of a union or object, so nested unions show their messages.
      const branches = (property.anyOf ?? [property]).filter(
        (branch) => branch.type === "object" && branch.properties,
      );
      const items =
        property.type === "array" && property.items
          ? (property.items.anyOf ?? [property.items]).filter(
              (branch) => branch.type === "object" && branch.properties,
            )
          : [];
      for (const branch of branches) {
        const inner = sample(branch);
        for (const [nested, nestedSchema] of Object.entries(branch.properties)) {
          for (const value of mutations(nestedSchema, sample(nestedSchema))) {
            const next = structuredClone(base);
            next[key] = { ...inner, [nested]: value };
            cases.push(next);
          }
        }
        for (const required of branch.required ?? []) {
          const { [required]: _gone, ...rest } = inner;
          const next = structuredClone(base);
          next[key] = rest;
          cases.push(next);
        }
      }
      for (const branch of items) {
        const inner = sample(branch);
        for (const [nested, nestedSchema] of Object.entries(branch.properties)) {
          for (const value of mutations(nestedSchema, sample(nestedSchema))) {
            const next = structuredClone(base);
            next[key] = [{ ...inner, [nested]: value }];
            cases.push(next);
          }
        }
      }
    }
  };
  if (schema.type === "object") visit(schema, [], good);
  return cases;
};

const rows = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      const out = [];
      for (const { tool } of catalog) {
        if (skipped.has(tool.name) || !toolkits.some((toolkit) => tool.name in toolkit.tools))
          continue;
        for (const input of casesFor(tool.inputSchema)) {
          decoded = undefined;
          const exit = yield* Effect.exit(
            server
              .callTool({ name: tool.name, arguments: input })
              .pipe(Effect.provideService(McpSchema.McpServerClient, client)),
          );
          const raw = JSON.parse(JSON.stringify(exit));
          out.push({
            name: tool.name,
            input,
            accepted: raw._tag === "Success" || decoded !== undefined,
            ...(decoded !== undefined
              ? { decoded }
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
  JSON.stringify({ cases: rows.length, accepted: rows.filter((row) => row.accepted).length }),
);
