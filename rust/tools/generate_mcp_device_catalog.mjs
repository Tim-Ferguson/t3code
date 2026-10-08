// Build original registered device tools, without executing any handler or acquiring tools.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as DeviceService from "../../apps/server/src/device/DeviceService.ts";
import * as McpHttpServer from "../../apps/server/src/mcp/McpHttpServer.ts";
import * as Testkit from "../../apps/server/src/mcp/McpToolAccess.testkit.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import { writeFileSync } from "node:fs";
const layer = McpHttpServer.layerDeviceToolkit.pipe(
  Layer.provideMerge(McpServer.McpServer.layer),
  Layer.provideMerge(Testkit.liveThreadsLayer),
  Layer.provide(Layer.mock(DeviceService.DeviceService)({})),
  Layer.provide(
    Layer.succeed(ServerConfig.ServerConfig, { stateDir: "/private/tmp/source-mcp-catalog" }),
  ),
  Layer.provide(NodeServices.layer),
);
const tools = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      return server.tools.map(({ tool }) => JSON.parse(JSON.stringify(tool)));
    }),
  ).pipe(Effect.provide(layer)),
);
writeFileSync(
  new URL("../crates/server/src/mcp_device_catalog.json", import.meta.url),
  JSON.stringify(tools, null, 2) + "\n",
);
console.log(JSON.stringify({ tools: tools.length }));
