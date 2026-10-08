// Unchanged original eligibility helper; development fixture generation only.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/web/src/components/ChatView.logic.ts", import.meta.url),
  "utf8",
);
const start = source.indexOf("export function getStartedThreadModelChangeBlockReason(");
const tail = source.slice(start),
  end = /^}\s*$/m.exec(tail);
const body = stripTypeScriptTypes(tail.slice(0, end.index + 1).replace(/^export /, ""));
const oracle = new Function(body + ";return getStartedThreadModelChangeBlockReason;")();
const rows = [];
for (const providers of [
  [],
  [{ instanceId: "codex" }],
  [{ instanceId: "codex", requiresNewThreadForModelChange: true }],
  [
    { instanceId: "codex", requiresNewThreadForModelChange: false },
    { instanceId: "codex", requiresNewThreadForModelChange: true },
  ],
])
  for (const hasStartedSession of [false, true])
    for (const supportsProviderSwitchingViaHandoff of [undefined, false, true])
      for (const currentProviderInstanceId of [undefined, null, "codex", "codex_personal"])
        for (const nextModelSelection of [
          { instanceId: "codex", model: "same" },
          { instanceId: "codex", model: "next" },
          { instanceId: "grok", model: "same" },
          {
            instanceId: "codex",
            model: "same",
            options: [{ id: "reasoningEffort", value: "high" }],
          },
        ]) {
          const input = {
            providers,
            hasStartedSession,
            supportsProviderSwitchingViaHandoff,
            currentProviderInstanceId,
            currentModelSelection: { instanceId: "codex", model: "same" },
            nextModelSelection,
          };
          rows.push({ input, expected: oracle(input) });
        }
writeFileSync(
  new URL("../crates/client/tests/fixtures/started-thread.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${rows.length} original started-thread eligibility cases\n`);
const workflows = readFileSync(
  new URL("../../packages/client-runtime/src/state/threadWorkflows.ts", import.meta.url),
  "utf8",
);
function original(name) {
  const start = workflows.indexOf(`function ${name}(`);
  const tail = workflows.slice(start);
  const end = /^}\s*$/m.exec(tail);
  return stripTypeScriptTypes(tail.slice(0, end.index + 1));
}
const handoff = new Function(
  'const ACTIVE_RUN_STATUSES=new Set(["preparing","starting","running","waiting"]);' +
    original("resolveActiveThreadRun") +
    original("resolveThreadProviderSession") +
    original("threadSupportsProviderHandoff") +
    ";return threadSupportsProviderHandoff;",
)();
const handoffRows = [];
for (const status of ["running", "queued", "completed"])
  for (const attached of [false, true])
    for (const activeId of [null, "pt"])
      for (const nativeRef of [null, { nativeId: "native" }])
        for (const sessionStatus of [null, "ready", "stopped", "error"])
          for (const supports of [false, true])
            for (const historyOrigin of [undefined, "v1_import"]) {
              const input = {
                thread: {
                  id: "thread",
                  activeProviderThreadId: activeId,
                  modelSelection: { instanceId: "codex", model: "fixture" },
                  ...(historyOrigin ? { historyOrigin } : {}),
                },
                runs: [{ status, providerThreadId: activeId }],
                providerThreads: attached
                  ? [
                      {
                        id: "pt",
                        appThreadId: "thread",
                        providerSessionId: sessionStatus ? "session" : null,
                        providerInstanceId: "codex",
                        nativeThreadRef: nativeRef,
                      },
                    ]
                  : [],
                providerSessions: sessionStatus
                  ? [
                      {
                        id: "session",
                        status: sessionStatus,
                        capabilities: {
                          sessions: { supportsProviderSwitchingViaHandoff: supports },
                        },
                      },
                    ]
                  : [],
              };
              handoffRows.push({ input, expected: handoff(input) });
            }
handoffRows.push({
  input: { thread: { id: "empty" }, runs: [], providerThreads: [], providerSessions: [] },
  expected: handoff({
    thread: { id: "empty" },
    runs: [],
    providerThreads: [],
    providerSessions: [],
  }),
});
writeFileSync(
  new URL("../crates/client/tests/fixtures/thread-handoff.jsonl", import.meta.url),
  handoffRows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${handoffRows.length} original provider handoff cases\n`);
const execution = readFileSync(
  new URL("../../packages/client-runtime/src/state/threadExecution.ts", import.meta.url),
  "utf8",
);
const reportedStart = execution.indexOf("export function deriveReportedModelSelection("),
  reportedTail = execution.slice(reportedStart),
  reportedEnd = /^}\s*$/m.exec(reportedTail);
const reported = new Function(
  stripTypeScriptTypes(reportedTail.slice(0, reportedEnd.index + 1).replace(/^export /, "")) +
    ";return deriveReportedModelSelection;",
)();
const reportedRows = [];
for (const active of [null, "pt"])
  for (const instance of ["codex", "other"])
    for (const metadata of [
      null,
      {},
      {
        modelSelection: {
          instanceId: "codex",
          model: "model",
          options: [{ id: "reasoningEffort", value: "low" }],
        },
      },
    ]) {
      const input = {
        thread: {
          activeProviderThreadId: active,
          modelSelection: { instanceId: "codex", model: "model" },
        },
        providerThreads: [{ id: "pt", providerInstanceId: instance, nativeMetadata: metadata }],
      };
      reportedRows.push({ input, expected: reported(input) });
    }
writeFileSync(
  new URL("../crates/client/tests/fixtures/reported-model.jsonl", import.meta.url),
  reportedRows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${reportedRows.length} original reported model cases\n`);
