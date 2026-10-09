// Runs the unchanged `derivePendingBackgroundWork` and `pendingBackgroundTurnItems` over scripted
// threads and writes the pending-work list and the items Stop ends for each. The Rust port replays
// every row (`background_work.rs`). No store, provider or file system is involved.
import { renameSync, writeFileSync } from "node:fs";
import {
  derivePendingBackgroundWork,
  pendingBackgroundTurnItems,
} from "../../packages/shared/src/orchestrationV2PendingBackgroundWork.ts";

const run = (ordinal, status, extra = {}) => ({ id: `run-${ordinal}`, ordinal, status, ...extra });
const item = (id, type, status, extra = {}) => ({
  id,
  type,
  status,
  title: null,
  runId: "run-1",
  nativeItemRef: null,
  ...extra,
});
const watch = { threshold: "any" };
const link = (number, extra = {}) => ({
  host: "github.com",
  repository: "Owner/Repo",
  number,
  url: `https://github.com/owner/repo/pull/${number}`,
  source: "manual",
  ...extra,
});
const rows = [];
const add = (name, input) => {
  const { hasActiveRun: _ignored, ...rest } = input;
  const runs = input.runs ?? [];
  const latestRun =
    "latestRun" in input
      ? input.latestRun
      : (runs.findLast((candidate) => candidate.status !== "queued") ?? null);
  const derived = {
    latestRun,
    providerThreads: input.providerThreads ?? [],
    turnItems: input.turnItems ?? [],
    runs,
    ...(input.activeProviderThreadId === undefined
      ? {}
      : { activeProviderThreadId: input.activeProviderThreadId }),
    ...(input.pullRequests === undefined ? {} : { pullRequests: input.pullRequests }),
  };
  const tasks = derivePendingBackgroundWork(derived);
  const pending = pendingBackgroundTurnItems({ turnItems: derived.turnItems, runs });
  rows.push({
    name,
    input: {
      ...derived,
      latestRun,
      pullRequests: input.pullRequests ?? null,
      activeProviderThreadId: input.activeProviderThreadId ?? null,
    },
    tasks,
    pendingItemIds: pending.map((candidate) => candidate.id),
  });
};

const command = item("cmd", "command_execution", "running", {
  input: "bun run dev",
  nativeItemRef: { nativeId: "dev-server" },
});
add("failed run keeps its running command", { runs: [run(1, "failed")], turnItems: [command] });
add("completed run keeps its running command", {
  runs: [run(1, "completed")],
  turnItems: [command],
});
add("waiting run counts as settled", { runs: [run(1, "waiting")], turnItems: [command] });
add("a running foreground run hides the list", { runs: [run(1, "running")], turnItems: [command] });
add("a starting run hides the list", {
  runs: [run(1, "completed"), run(2, "starting")],
  turnItems: [command],
});
add("a rolled back latest run is not settled", {
  runs: [run(1, "rolled_back")],
  turnItems: [command],
});
add("an interrupted run keeps pending work", {
  runs: [run(1, "interrupted")],
  turnItems: [command],
});
add("a queued latest run is not settled", {
  runs: [run(1, "completed"), run(2, "queued")],
  turnItems: [command],
});
add("a settled command is not pending", {
  runs: [run(1, "failed")],
  turnItems: [item("cmd", "command_execution", "completed", { input: "ls" })],
});
add("a pending and a waiting item both count", {
  runs: [run(1, "completed")],
  turnItems: [
    item("a", "dynamic_tool", "pending", { toolName: "monitor" }),
    item("b", "command_execution", "waiting", { input: "  tail -f log  " }),
  ],
});
add("other item types never count", {
  runs: [run(1, "failed")],
  turnItems: [item("m", "assistant_message", "running"), item("r", "reasoning", "running")],
});
add("persistent dynamic tools are not pending work", {
  runs: [run(1, "completed")],
  turnItems: [
    item("p", "dynamic_tool", "running", { toolName: "monitor", input: { persistent: true } }),
    item("q", "dynamic_tool", "running", { toolName: "watch", input: { persistent: false } }),
  ],
});
add("items of a rolled back run are abandoned", {
  runs: [run(1, "rolled_back"), run(2, "completed")],
  turnItems: [
    item("old", "command_execution", "running", { input: "old" }),
    item("new", "command_execution", "running", { runId: "run-2", input: "new" }),
  ],
});
add("an item without a run stays eligible", {
  runs: [run(1, "failed")],
  turnItems: [item("loose", "command_execution", "running", { runId: null, input: "loose" })],
});
add("a title wins over the command", {
  runs: [run(1, "failed")],
  turnItems: [
    item("cmd", "command_execution", "running", {
      title: " Start dev server ",
      input: "bun run dev",
      nativeItemRef: { nativeId: "t" },
    }),
  ],
});
add("a subagent names its child thread and prompt", {
  runs: [run(1, "completed")],
  turnItems: [
    item("sub", "subagent", "running", {
      prompt: "  Review the diff ",
      childThreadId: "thread:child",
      nativeItemRef: { nativeId: "native-sub" },
    }),
  ],
});
add("a subagent without a child thread", {
  runs: [run(1, "completed")],
  turnItems: [item("sub", "subagent", "pending", { prompt: "", childThreadId: null })],
});
add("the roster of a provider thread is listed and deduplicated against items", {
  runs: [run(1, "completed")],
  providerThreads: [
    {
      id: "pt-1",
      pendingBackgroundTasks: [
        { taskId: "dev-server", kind: "command", description: "  from roster " },
        { taskId: "other", kind: "monitor" },
        { taskId: "", kind: "command" },
      ],
    },
  ],
  turnItems: [command],
});
add("only the active provider thread's roster counts", {
  runs: [run(1, "completed")],
  activeProviderThreadId: "pt-2",
  providerThreads: [
    { id: "pt-1", pendingBackgroundTasks: [{ taskId: "one", kind: "command" }] },
    {
      id: "pt-2",
      pendingBackgroundTasks: [{ taskId: "two", kind: "background_task", description: "Two" }],
    },
  ],
});
add("a roster alone keeps a settled thread waiting", {
  runs: [run(1, "failed")],
  providerThreads: [
    {
      id: "pt-1",
      pendingBackgroundTasks: [{ taskId: "t", kind: "subagent", childThreadId: "thread:child" }],
    },
  ],
});
add("a thread that never ran waits only on watches", {
  runs: [],
  pullRequests: [link(7, { watch }), link(8)],
  providerThreads: [],
  turnItems: [command],
});
add("watches are monitors and dismissed stack members are not", {
  runs: [run(1, "completed")],
  pullRequests: [link(7, { watch }), link(9, { watch, source: "stack-dismissed" }), link(10)],
  turnItems: [command],
});
add("watches with an active run list nothing", {
  runs: [run(1, "running")],
  pullRequests: [link(7, { watch })],
});
add("nothing pending", { runs: [run(1, "completed")] });

writeFileSync(
  new URL("../crates/server/tests/fixtures/background-work.jsonl.tmp", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
renameSync(
  new URL("../crates/server/tests/fixtures/background-work.jsonl.tmp", import.meta.url),
  new URL("../crates/server/tests/fixtures/background-work.jsonl", import.meta.url),
);
console.log(`${rows.length} background work rows`);
