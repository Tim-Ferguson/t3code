// Runs the unchanged wake text and terminal-status mapping of the original orchestrator, lifted out of
// `Orchestrator.ts` by source (they are module-private there), over scripted inputs. The Rust port replays
// every row (`delegated_completion.rs`). The delivery planning itself is a closure of the running
// orchestrator; the scenarios that exercise it are `tests/mcp_tools_flow.rs`.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
const lift = (name) => {
  const start = source.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`${name} not found`);
  const end = source.indexOf("\n}\n", start);
  return source.slice(start, end + 3).replace(/^function /, "export function ");
};
const directory = mkdtempSync(join(tmpdir(), "delegated-completion-"));
const file = join(directory, "lifted.ts");
writeFileSync(
  file,
  [
    "type Status = string;",
    lift("delegatedCompletionWakeDetail").replace("ReadonlyArray<string>", "ReadonlyArray<string>"),
    lift("isTerminalDelegatedTaskStatus").replace('OrchestrationV2Subagent["status"]', "Status"),
    lift("delegatedTaskTerminalStatus")
      .replace('OrchestrationV2Run["status"]', "Status")
      .replace('OrchestrationV2Subagent["status"] | null', "Status | null"),
  ].join("\n\n"),
);
const lifted = await import(pathToFileURL(file).href);
const rows = [];
for (const taskIds of [
  ["a"],
  ["node:delegated-task:command%3Amcp%3Ax"],
  ["a", "b"],
  ["a", "b", "c"],
  [],
]) {
  rows.push({ kind: "wake", taskIds, text: lifted.delegatedCompletionWakeDetail(taskIds) });
}
for (const status of [
  "completed",
  "failed",
  "cancelled",
  "interrupted",
  "rolled_back",
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
]) {
  rows.push({
    kind: "status",
    status,
    terminal: lifted.delegatedTaskTerminalStatus(status),
    taskTerminal: lifted.isTerminalDelegatedTaskStatus(status),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/delegated-completion.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ rows: rows.length }));
