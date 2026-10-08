// Development oracle; imports unchanged original implementation.
// Regeneration requires the pinned original checkout alongside rust/.
import fs from "node:fs";
import {
  acpSelectionTransition,
  turnScopedSelectionTransition,
} from "../../apps/server/src/orchestration-v2/ProviderSelectionTransition.ts";

const rows = [];
const selections = [
  { instanceId: "agent", model: "model" },
  { instanceId: "agent", model: "model", options: [] },
  { instanceId: "agent", model: "model", options: [{ id: "effort", value: "high" }] },
  {
    instanceId: "agent",
    model: "model",
    options: [
      { id: "fast", value: true },
      { id: "effort", value: "high" },
    ],
  },
  { instanceId: "agent", model: "other" },
];
for (const driver of ["acpRegistry", "codex", "claudeCode", "cursor", "pi", "antigravity"]) {
  for (const current of selections)
    for (const target of selections) {
      for (const supported of [false, true]) {
        const input = {
          driver,
          current,
          target,
          sessionCapabilities: { sessions: { supportsModelSwitchInSession: supported } },
        };
        rows.push({
          operation: "selection",
          input,
          output:
            driver === "acpRegistry" || driver === "antigravity"
              ? acpSelectionTransition(input)
              : turnScopedSelectionTransition(),
        });
      }
    }
}
// Extract the original command's runtime detach selection and live-session filter.
const source = fs.readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
const runtimeStart = source.indexOf(': command.type === "thread.runtime-mode.set"\n            ?');
const runtimeEnd = source.indexOf(
  ": (providerSwitchPlan?.releaseProviderSessionIds ?? [])",
  runtimeStart,
);
const runtimeExpression = source
  .slice(
    runtimeStart + ': command.type === "thread.runtime-mode.set"\n            ?'.length,
    runtimeEnd,
  )
  .trim();
// Keep this explicit assertion so source restructuring cannot silently change the oracle.
if (
  !runtimeExpression.includes("!session.capabilities.sessions.supportsRuntimeModeSwitchInSession")
)
  throw new Error("Original runtime selection expression moved");
const runtimeIds = new Function("providerContext", `return ${runtimeExpression}`);
const liveStart = source.indexOf("const liveSessions =", runtimeEnd);
const liveEnd = source.indexOf("yield* Effect.forEach(", liveStart);
const liveStatement = source.slice(liveStart, liveEnd).replace("const liveSessions =", "return");
const liveSessions = new Function("providerContext", "detachSessionIds", liveStatement);
for (const status of ["ready", "busy", "starting", "stopped", "error"]) {
  for (const supported of [false, true, null, undefined, 0]) {
    const input = {
      id: "session",
      status,
      capabilities: { sessions: { supportsRuntimeModeSwitchInSession: supported } },
    };
    const providerContext = { providerSessions: [input] };
    const ids = new Set(runtimeIds(providerContext));
    rows.push({
      operation: "runtimeDetach",
      input,
      output: liveSessions(providerContext, ids).length > 0,
    });
  }
}
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/provider-selection-transitions.json", import.meta.url),
  JSON.stringify(rows, null, 2) + "\n",
);
console.log(`${rows.length} unchanged-source provider selection/runtime detach witnesses`);
