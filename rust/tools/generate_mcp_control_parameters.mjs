import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpSchema from "../../apps/server/node_modules/effect/dist/ai/McpSchema.js";
import * as Http from "../../apps/server/src/mcp/McpHttpServer.ts";
import { OrchestratorToolkit } from "../../apps/server/src/mcp/toolkits/orchestrator/tools.ts";
import * as Handlers from "../../apps/server/src/mcp/toolkits/orchestrator/handlers.ts";
import * as Orchestrator from "../../apps/server/src/mcp/OrchestratorMcpService.ts";
import * as Metadata from "../../apps/server/src/mcp/ThreadMetadataMcpService.ts";
import * as Invocation from "../../apps/server/src/mcp/McpInvocationContext.ts";
import * as Testkit from "../../apps/server/src/mcp/McpToolAccess.testkit.ts";
import { OrchestratorMcpFailure } from "../../packages/contracts/src/index.ts";
let decoded;
const failed = (_scope, input) => {
  decoded = input;
  return Effect.fail(
    new OrchestratorMcpFailure({ code: "thread_not_found", message: "Source refusal" }),
  );
};
const layer = Http.toolkitRegistration(OrchestratorToolkit, Handlers.layer).pipe(
  Layer.provideMerge(McpServer.McpServer.layer),
  Layer.provide(Testkit.liveThreadsLayer),
  Layer.provide(
    Layer.mock(Orchestrator.OrchestratorMcpService)({
      listThreads: failed,
      readThread: failed,
      waitForThread: failed,
      interruptThread: failed,
      sendToThread: failed,
    }),
  ),
  Layer.provide(Layer.mock(Metadata.ThreadMetadataMcpService)({ update: failed })),
);
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
const scope = {
  environmentId: "environment-1",
  issuedAt: 1,
  requestNamespace: "fixture",
  capabilities: new Set(["orchestration"]),
  client: { sessionId: "client", label: "Client", access: "full-access" },
};

import { writeFileSync } from "node:fs";
const specs = {
  t3_thread_list: {
    projectId: "project:1",
    statuses: ["idle"],
    titleContains: "title",
    settled: true,
    snoozed: true,
    includeSubagents: true,
    cursor: 0,
    limit: 50,
  },
  t3_thread_read: {
    threadId: "thread:1",
    itemId: "item:1",
    textOffset: 0,
    view: "messages",
    afterPosition: 0,
    limit: 50,
    runLimit: 10,
    maxCharsPerItem: 20000,
  },
  t3_thread_wait: { threadId: "thread:1", runId: "run:1", timeoutMs: 1000 },
  t3_thread_interrupt: {
    threadId: "thread:1",
    runId: "run:1",
    reason: "reason",
    clientRequestId: "key",
  },
};
const rows = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      const out = [];
      for (const [name, fields] of Object.entries(specs)) {
        const base = name === "t3_thread_list" ? {} : { threadId: "thread:1" };
        const cases = [
          {},
          null,
          [],
          false,
          0,
          "",
          fields,
          { ...fields, unknown: true },
          { threadId: null, runId: false },
        ];
        for (const [key, good] of Object.entries(fields))
          for (const value of [
            good,
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
            "x".repeat(key === "reason" ? 2001 : 257),
            ["idle", "running"],
            ["invalid"],
            Array(11).fill("idle"),
          ])
            cases.push({ ...base, [key]: value });
        for (const input of cases) {
          decoded = undefined;
          const exit = yield* Effect.exit(
            server
              .callTool({ name, arguments: input })
              .pipe(
                Effect.provideService(Invocation.McpInvocationContext, scope),
                Effect.provideService(McpSchema.McpServerClient, client),
              ),
          );
          const raw = JSON.parse(JSON.stringify(exit));
          out.push({
            name,
            input,
            accepted: raw._tag === "Success",
            ...(raw._tag === "Success"
              ? { decoded }
              : { error: raw.cause.failures[0].error.message }),
          });
        }
      }
      return out;
    }),
  ).pipe(Effect.provide(layer)),
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-control-parameters.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
