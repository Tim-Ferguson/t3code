// Register original thread-control tool descriptions and schemas, without effects.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as Http from "../../apps/server/src/mcp/McpHttpServer.ts";
import { OrchestratorToolkit } from "../../apps/server/src/mcp/toolkits/orchestrator/tools.ts";
import * as Handlers from "../../apps/server/src/mcp/toolkits/orchestrator/handlers.ts";
import * as Orchestrator from "../../apps/server/src/mcp/OrchestratorMcpService.ts";
import * as Metadata from "../../apps/server/src/mcp/ThreadMetadataMcpService.ts";
import * as Testkit from "../../apps/server/src/mcp/McpToolAccess.testkit.ts";
import { writeFileSync } from "node:fs";
const names = [
  "t3_thread_list",
  "t3_thread_read",
  "t3_thread_send",
  "t3_thread_wait",
  "t3_thread_interrupt",
  "t3_thread_update",
];
const layer = Http.toolkitRegistration(OrchestratorToolkit, Handlers.layer).pipe(
  Layer.provideMerge(McpServer.McpServer.layer),
  Layer.provide(Testkit.liveThreadsLayer),
  Layer.provide(Layer.mock(Orchestrator.OrchestratorMcpService)({})),
  Layer.provide(Layer.mock(Metadata.ThreadMetadataMcpService)({})),
);
const tools = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      return server.tools
        .map(({ tool }) => JSON.parse(JSON.stringify(tool)))
        .filter((tool) => names.includes(tool.name));
    }),
  ).pipe(Effect.provide(layer)),
);
writeFileSync(
  new URL("../crates/server/src/mcp_control_catalog.json", import.meta.url),
  JSON.stringify(tools, null, 2) + "\n",
);
console.log(JSON.stringify({ tools: tools.length }));
