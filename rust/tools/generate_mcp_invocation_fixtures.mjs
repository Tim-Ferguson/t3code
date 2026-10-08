// Development-only oracle runs unchanged MCP context and live-caller guards.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Context from "../../apps/server/src/mcp/McpInvocationContext.ts";
import { OrchestratorMcpFailure } from "../../packages/contracts/src/orchestratorMcp.ts";
import { stripTypeScriptTypes } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
const source = readFileSync(
  new URL("../../apps/server/src/mcp/threadAccess.ts", import.meta.url),
  "utf8",
);
const live = source.slice(
  source.indexOf("export function assertLiveCaller"),
  source.indexOf("/**", source.indexOf("export function assertLiveCaller") + 1),
);
const assertLiveCaller = new Function(
  "Effect",
  "OrchestratorMcpFailure",
  stripTypeScriptTypes(live.replace("export function", "function"), { mode: "strip" }) +
    ";return assertLiveCaller;",
)(Effect, OrchestratorMcpFailure);
const rows = [];
async function result(effect) {
  return Effect.runPromise(
    Effect.match(effect, {
      onFailure: (error) => ({ error: JSON.parse(JSON.stringify(error)) }),
      onSuccess: () => ({ ok: true }),
    }),
  );
}
const capabilities = ["preview", "orchestration", "worktree", "device", "pull-requests"];
for (const thread of [
  undefined,
  { threadId: "thread:1", providerSessionId: "provider-session-1", providerInstanceId: "codex" },
]) {
  for (let bits = 0; bits < 32; bits++) {
    const caps = capabilities.filter((_, index) => bits & (1 << index));
    const scope = {
      environmentId: "environment-1",
      requestNamespace: "provider-session-1",
      issuedAt: 1,
      thread,
      capabilities: new Set(caps),
    };
    const input = { ...scope, capabilities: caps };
    for (const capability of capabilities) {
      rows.push({
        type: "capability",
        scope: input,
        capability,
        result: await result(
          Context.requireMcpCapability(capability).pipe(
            Effect.provideService(Context.McpInvocationContext, scope),
          ),
        ),
      });
    }
    for (const capability of ["preview", "device"]) {
      rows.push({
        type: "threadCapability",
        scope: input,
        capability,
        result: await result(
          Context.requireThreadMcpCapability(capability).pipe(
            Effect.provideService(Context.McpInvocationContext, scope),
          ),
        ),
      });
    }
    rows.push({
      type: "thread",
      scope: input,
      result: await result(Context.requireThreadScope(scope, "This tool")),
    });
  }
}
for (const access of [
  undefined,
  "read-only",
  "approval-required",
  "auto-accept-edits",
  "auto",
  "full-access",
]) {
  const client =
    access === undefined ? undefined : { sessionId: "client-1", label: "Client", access };
  rows.push({ type: "ceiling", client, result: Context.clientRuntimeModeCeiling(client) });
}
for (const archivedAt of [null, "2026-01-01T00:00:00Z"]) {
  for (const activeRunId of [null, "run-1"]) {
    for (const providerInstanceId of ["codex", "different"]) {
      const caller = { archivedAt, activeRunId, providerInstanceId };
      const scope = { thread: { providerInstanceId: "codex" } };
      rows.push({
        type: "live",
        caller,
        result: await result(assertLiveCaller({ caller, scope })),
      });
    }
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-invocation.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
