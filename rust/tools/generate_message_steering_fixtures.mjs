// Execute unchanged source decision functions. Effect's typed error constructor
// is replaced only with its cause text; native actor delivery has separate tests.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { latestProviderTurnForAttempt } from "../../packages/contracts/src/orchestrationV2.ts";
const policy = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/CommandPolicy.ts", import.meta.url),
  "utf8",
);
const orchestrator = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
const begin = policy.indexOf("export function resolveMessageDispatchIntent("),
  end = policy.indexOf("\n}", begin) + 2;
if (begin < 0 || end < 2) throw new Error("Source intent decision changed");
const resolve = new Function(
  stripTypeScriptTypes(policy.slice(begin, end).replace("export ", ""), { mode: "strip" }) +
    "\nreturn resolveMessageDispatchIntent;",
)();
const completedBegin = orchestrator.indexOf('      if (dispatchMode.type === "steer_active") {');
const completedEnd = orchestrator.indexOf(
  "      if (\n        command.notification",
  completedBegin,
);
if (completedBegin < 0 || completedEnd < 0)
  throw new Error("Source completed steering decision changed");
const completed = new Function(
  "projection",
  "dispatchMode",
  "latestProviderTurnForAttempt",
  stripTypeScriptTypes(orchestrator.slice(completedBegin, completedEnd), { mode: "strip" }) +
    "\nreturn dispatchMode;",
);
const policyBegin = policy.indexOf("const decideSteeringExecution:"),
  policyEnd = policy.indexOf("\n};", policyBegin) + 3;
if (policyBegin < 0 || policyEnd < 3) throw new Error("Source steering policy changed");
const choose = new Function(
  "Effect",
  "unsupported",
  stripTypeScriptTypes(policy.slice(policyBegin, policyEnd), { mode: "strip" }) +
    "\nreturn decideSteeringExecution;",
)(Effect, (_input, capability, message) => ({ capability, message }));
const rows = [];
const capabilities = (mask) => ({
  turns: {
    supportsActiveSteering: !!(mask & 1),
    supportsQueuedMessages: !!(mask & 2),
    supportsSteeringByInterruptRestart: !!(mask & 4),
    supportsInterrupt: !!(mask & 8),
  },
});
for (const status of [
  null,
  "preparing",
  "starting",
  "running",
  "waiting",
  "completed",
  "failed",
  "interrupted",
])
  for (const intent of [undefined, "auto", "steer", "restart"])
    for (let mask = 0; mask < 8; mask++) {
      const projection = {
        runs: status === null ? [] : [{ id: "active", status, providerThreadId: "provider" }],
        providerThreads: [{ id: "provider", providerSessionId: "session" }],
        providerSessions: [{ id: "session", capabilities: capabilities(mask) }],
      };
      const command = {
        dispatchMode: { type: "defer_start", workspaceStrategy: { type: "root" } },
        ...(intent === undefined ? {} : { deliveryIntent: intent }),
      };
      rows.push({
        operation: "intent",
        command,
        projection,
        output: resolve(projection, command.dispatchMode, intent),
      });
    }
// findLast follows projection ordering even if the earlier active run has a
// larger ordinal. Session absent must never use adapter defaults for auto.
for (const intent of ["auto", "steer", "restart"]) {
  const projection = {
    runs: [
      { id: "earlier", ordinal: 99, status: "running" },
      { id: "last", ordinal: 1, status: "waiting" },
    ],
    providerThreads: [],
    providerSessions: [],
  };
  const command = { dispatchMode: { type: "start_immediately" }, deliveryIntent: intent };
  rows.push({
    operation: "intent",
    command,
    projection,
    output: resolve(projection, command.dispatchMode, intent),
  });
}
for (const status of [null, "completed", "running", "waiting", "interrupted", "starting"])
  for (const turnStatus of ["completed", "running", "interrupted"])
    for (const ordinalPattern of ["increasing", "tie", "wrong-root", "wrong-attempt"]) {
      const projection = {
        runs:
          status === null
            ? []
            : [{ id: "target", status, rootNodeId: "root", activeAttemptId: "attempt" }],
        providerTurns: [
          { id: "first", nodeId: "root", runAttemptId: "attempt", ordinal: 1, status: turnStatus },
          {
            id: "second",
            nodeId: ordinalPattern === "wrong-root" ? "other" : "root",
            runAttemptId: ordinalPattern === "wrong-attempt" ? "other" : "attempt",
            ordinal: ordinalPattern === "tie" ? 1 : 2,
            status: "running",
          },
        ],
      };
      const mode = { type: "steer_active", targetRunId: "target" };
      rows.push({
        operation: "completed",
        projection,
        mode,
        output: completed(projection, mode, latestProviderTurnForAttempt),
      });
    }
for (let mask = 0; mask < 16; mask++)
  for (const forceRestart of [false, true]) {
    const input = {
      commandId: "command",
      threadId: "thread",
      providerInstanceId: "provider",
      capabilities: capabilities(mask),
      forceRestart,
    };
    const output = await Effect.runPromise(
      Effect.match(choose(input), {
        onFailure: (error) => ({ error: error.message }),
        onSuccess: (policy) => ({ policy }),
      }),
    );
    rows.push({ operation: "policy", input, output });
  }
const target = new URL("../crates/server/tests/fixtures/message-steering.json", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, JSON.stringify(rows, null, 2) + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
