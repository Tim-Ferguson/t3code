// Runs the original's queue workflow state over fixed projections, for
// `crates/client/tests/queue_workflow_parity.rs`.
//   deriveThreadQueueWorkflowState, resolveActiveThreadRun   packages/client-runtime/src/state/threadWorkflows.ts
import { writeFileSync } from "node:fs";
import {
  deriveThreadQueueWorkflowState,
  resolveActiveThreadRun,
} from "../../packages/client-runtime/src/state/threadWorkflows.ts";

const rows = [];
const capabilityCases = [
  { activeSteering: false, restart: false },
  { activeSteering: true, restart: false },
  { activeSteering: false, restart: true },
  { activeSteering: true, restart: true },
];
const session = (id, status, { activeSteering, restart }) => ({
  id,
  status,
  capabilities: {
    turns: {
      supportsQueuedMessages: true,
      supportsActiveSteering: activeSteering,
      supportsSteeringByInterruptRestart: restart,
    },
  },
});
for (const runStatus of ["queued", "preparing", "running", "waiting", "completed"])
  for (const attempt of [null, "attempt-1"])
    for (const turnStatus of [null, "running", "completed"])
      for (const turnAttempt of ["attempt-1", "attempt-2"])
        for (const link of ["run", "thread", "none"])
          for (const sessionStatus of ["ready", "stopped", "error"])
            for (const capabilities of capabilityCases) {
              const projection = {
                thread: { id: "thread", activeProviderThreadId: link === "thread" ? "pt" : null },
                runs: [
                  {
                    id: "run-1",
                    status: runStatus,
                    ordinal: 1,
                    activeAttemptId: attempt,
                    providerThreadId: link === "run" ? "pt" : null,
                    userMessageId: "m1",
                    queuePosition: null,
                  },
                  {
                    id: "run-2",
                    status: "queued",
                    ordinal: 2,
                    userMessageId: "m2",
                    queuePosition: 1,
                    queueHeld: runStatus === "completed",
                  },
                ],
                messages: [
                  { id: "m1", text: "first" },
                  { id: "m2", text: "second" },
                ],
                providerTurns:
                  turnStatus === null ? [] : [{ runAttemptId: turnAttempt, status: turnStatus }],
                providerThreads:
                  link === "none"
                    ? []
                    : [{ id: "pt", appThreadId: "thread", providerSessionId: "session" }],
                providerSessions: [
                  session("session", sessionStatus, capabilities),
                  session("other", "ready", {
                    activeSteering: !capabilities.activeSteering,
                    restart: false,
                  }),
                ],
              };
              const state = deriveThreadQueueWorkflowState(projection);
              rows.push({
                projection,
                activeRun: resolveActiveThreadRun(projection)?.id ?? null,
                canPromoteToSteer: state.canPromoteToSteer,
                isHeld: state.isHeld,
                queued: state.queuedRuns.map((entry) => entry.run.id),
              });
            }
// A session named by the thread that no longer exists falls to nothing, not to another session.
rows.push(
  (() => {
    const projection = {
      thread: { id: "thread", activeProviderThreadId: "pt" },
      runs: [
        {
          id: "run-1",
          status: "running",
          ordinal: 1,
          activeAttemptId: "a",
          providerThreadId: null,
          userMessageId: "m1",
        },
      ],
      messages: [{ id: "m1", text: "x" }],
      providerTurns: [{ runAttemptId: "a", status: "running" }],
      providerThreads: [{ id: "pt", appThreadId: "thread", providerSessionId: "gone" }],
      providerSessions: [session("session", "ready", { activeSteering: true, restart: true })],
    };
    const state = deriveThreadQueueWorkflowState(projection);
    return {
      projection,
      activeRun: "run-1",
      canPromoteToSteer: state.canPromoteToSteer,
      isHeld: state.isHeld,
      queued: [],
    };
  })(),
);
writeFileSync(
  new URL("../crates/client/tests/fixtures/queue-workflow.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${rows.length} original queue workflow cases\n`);
