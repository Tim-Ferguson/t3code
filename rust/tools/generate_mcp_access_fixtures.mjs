// Execute the original sealed access declarations with injected thread reads.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { Tool, Toolkit } from "../../apps/server/node_modules/effect/dist/ai/index.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpSchema from "../../apps/server/node_modules/effect/dist/ai/McpSchema.js";
import * as Access from "../../apps/server/src/mcp/McpToolAccess.ts";
import * as Http from "../../apps/server/src/mcp/McpHttpServer.ts";
import * as Invocation from "../../apps/server/src/mcp/McpInvocationContext.ts";
import * as Threads from "../../apps/server/src/orchestration-v2/ThreadManagementService.ts";
import * as Testkit from "../../apps/server/src/mcp/McpToolAccess.testkit.ts";
import { OrchestratorMcpFailure, ThreadId } from "../../packages/contracts/src/index.ts";
import { writeFileSync } from "node:fs";
const names = [
  "reads",
  "reads_as_caller",
  "acts_as_caller",
  "writes",
  "writes_threads",
  "writes_environment",
];
const toolkit = Toolkit.make(
  ...names.map((name) =>
    Tool.make(name, {
      parameters: Schema.Struct({ threadId: Schema.optional(ThreadId) }),
      success: Schema.Struct({ ran: Schema.Boolean }),
      failure: OrchestratorMcpFailure,
      failureMode: "return",
      dependencies: [Invocation.McpInvocationContext, Threads.ThreadManagementService],
    }),
  ),
);
const ran = () => Effect.succeed({ ran: true });
const handlers = Access.toLayer(toolkit, {
  reads: Access.reads(ran),
  reads_as_caller: Access.readsAsCaller(ran),
  acts_as_caller: Access.actsAsCaller(ran),
  writes: Access.writes(ran),
  writes_threads: Access.writesThreads((input) => [input.threadId], ran),
  writes_environment: Access.writesEnvironment(ran),
});
const client = McpSchema.McpServerClient.of({
  clientId: 1,
  protocolVersion: "2025-06-18",
  clientCapabilities: {},
  clientInfo: { name: "source-access", version: "1" },
  initializePayload: {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "source-access", version: "1" },
  },
  getClient: Effect.die("unused"),
});
const rows = [];
for (const mode of ["approval-required", "auto-accept-edits", "auto", "full-access", "read-only"])
  for (const interactionMode of ["plan", "default"])
    for (const condition of [
      "live",
      "idle",
      "archived",
      "deleted",
      "missing",
      "switched",
      "client",
      "anonymous",
    ])
      for (const capability of [false, true]) {
        if (mode === "read-only" && condition !== "client") continue;
        const scope = {
          environmentId: "environment-1",
          requestNamespace: "source-access",
          issuedAt: 1,
          capabilities: new Set(capability ? ["orchestration"] : []),
          ...(condition === "client"
            ? { client: { sessionId: "client", label: "Client", access: mode } }
            : condition === "anonymous"
              ? {}
              : {
                  thread: {
                    threadId: "thread:1",
                    providerSessionId: "session",
                    providerInstanceId: "codex",
                  },
                }),
        };
        const shell = Testkit.liveThreadShell(ThreadId.make("thread:1"), {
          runtimeMode: mode,
          interactionMode,
          activeRunId: condition === "idle" ? null : undefined,
        });
        if (condition === "archived") shell.archivedAt = shell.createdAt;
        if (condition === "deleted") shell.deletedAt = shell.createdAt;
        if (condition === "switched") shell.providerInstanceId = "different";
        const target = Testkit.liveThreadShell(ThreadId.make("target"), {
          runtimeMode: "full-access",
        });
        const layer = Http.toolkitRegistration(toolkit, handlers).pipe(
          Layer.provideMerge(McpServer.McpServer.layer),
          Layer.provide(
            Layer.mock(Threads.ThreadManagementService)({
              getThreadShell: (id) =>
                Effect.succeed(
                  id === "thread:1" ? (condition === "missing" ? null : shell) : target,
                ),
            }),
          ),
        );
        await Effect.runPromise(
          Effect.scoped(
            Effect.gen(function* () {
              const server = yield* McpServer.McpServer;
              for (const name of names)
                for (const targetId of ["thread:1", "target"]) {
                  const result = yield* server
                    .callTool({ name, arguments: { threadId: targetId } })
                    .pipe(
                      Effect.provideService(Invocation.McpInvocationContext, scope),
                      Effect.provideService(McpSchema.McpServerClient, client),
                    );
                  const value = JSON.parse(
                    result.content
                      .filter((part) => part.type === "text")
                      .map((part) => part.text)
                      .join(""),
                  );
                  rows.push({
                    access: name,
                    scope: { ...scope, capabilities: [...scope.capabilities] },
                    condition,
                    mode,
                    interactionMode,
                    targetId,
                    result: value,
                  });
                }
            }),
          ).pipe(Effect.provide(layer)),
        );
      }
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-access.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
