// Development-only oracle executes original registered handlers and validation.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpSchema from "../../apps/server/node_modules/effect/dist/ai/McpSchema.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as DeviceService from "../../apps/server/src/device/DeviceService.ts";
import * as McpHttpServer from "../../apps/server/src/mcp/McpHttpServer.ts";
import * as Invocation from "../../apps/server/src/mcp/McpInvocationContext.ts";
import * as Testkit from "../../apps/server/src/mcp/McpToolAccess.testkit.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import { writeFileSync } from "node:fs";
const layer = McpHttpServer.layerDeviceToolkit.pipe(
  Layer.provideMerge(McpServer.McpServer.layer),
  Layer.provideMerge(Testkit.liveThreadsLayer),
  Layer.provide(
    Layer.mock(DeviceService.DeviceService)({
      list: Effect.succeed({ hostStatus: "disabled" }),
      sessionsForThread: () => Effect.succeed([]),
      close: () => Effect.void,
    }),
  ),
  Layer.provide(
    Layer.succeed(ServerConfig.ServerConfig, { stateDir: "/private/tmp/source-mcp-calls" }),
  ),
  Layer.provide(NodeServices.layer),
);
const initialize = {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "source-fixture", version: "1" },
};
const client = McpSchema.McpServerClient.of({
  clientId: 1,
  clientCapabilities: {},
  clientInfo: initialize.clientInfo,
  protocolVersion: initialize.protocolVersion,
  initializePayload: initialize,
  getClient: Effect.die("unused"),
});
const rows = await Effect.runPromise(
  Effect.scoped(
    Effect.gen(function* () {
      const server = yield* McpServer.McpServer;
      const rows = [];
      for (const name of ["device_list", "device_open", "device_close", "device_screenshot"])
        for (const args of [
          {},
          [],
          null,
          false,
          { hostId: 7 },
          { deviceId: "" },
          { platform: "invalid" },
          { shutdown: "yes" },
          { hostId: null },
          { deviceId: null },
          { platform: null },
          { shutdown: null },
          { hostId: "" },
          { hostId: " x " },
          { deviceId: "x".repeat(257) },
          { hostId: "x".repeat(129) },
          { deviceId: 7, hostId: 7, platform: 7, shutdown: 7 },
        ])
          for (const device of [false, true]) {
            const invocation = {
              environmentId: "environment-1",
              requestNamespace: "source-session",
              thread: {
                threadId: "thread:1",
                providerInstanceId: "codex",
                providerSessionId: "source-session",
              },
              capabilities: new Set(device ? ["device"] : []),
              issuedAt: 1,
            };
            const result = yield* server
              .callTool({ name, arguments: args })
              .pipe(
                Effect.provideService(Invocation.McpInvocationContext, invocation),
                Effect.provideService(McpSchema.McpServerClient, client),
                Effect.match({
                  onSuccess: (result) => ({ result: JSON.parse(JSON.stringify(result)) }),
                  onFailure: (error) => ({ error: JSON.parse(JSON.stringify(error)) }),
                }),
              );
            rows.push({ name, input: args, device, result });
          }
      return rows;
    }),
  ).pipe(Effect.provide(layer)),
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-device-calls.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
