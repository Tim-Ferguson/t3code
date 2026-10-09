// Runs the unchanged `SubagentProjection.ts` helpers (child thread, thread title, conversation
// artifacts, a child run's result and the delegated-task progress of a parent) over scripted
// inputs and writes what they return. The Rust port replays every row (`subagents.rs`).
import { renameSync, writeFileSync } from "node:fs";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  delegatedTaskProgress,
  makeSubagentChildThread,
  makeSubagentConversationArtifacts,
  subagentResultForRun,
  subagentThreadTitle,
} from "../../apps/server/src/orchestration-v2/SubagentProjection.ts";

const at = (text) => DateTime.makeUnsafe(text);
const NOW = "2026-10-08T00:00:00.000Z";
const CREATED = "2026-10-01T00:00:00.000Z";
const iso = (value) =>
  value !== null && typeof value === "object" && DateTime.isDateTime(value)
    ? DateTime.formatIso(value)
    : value;
const plain = (value) => JSON.parse(JSON.stringify(value, (_key, item) => iso(item)));
const rows = [];
const add = (name, kind, input, output) => rows.push({ name, kind, input, output: plain(output) });

for (const [name, input] of Object.entries({
  "the title wins": {
    parentTitle: "Parent",
    title: "  Read the docs  ",
    prompt: "ignored",
    ordinal: 1,
  },
  "the prompt names an untitled child": {
    parentTitle: "Parent",
    title: null,
    prompt: "  Review the diff\n",
    ordinal: 2,
  },
  "a long prompt is clipped": { parentTitle: "Parent", prompt: "x".repeat(100), ordinal: 3 },
  "exactly 72 characters stay whole": { parentTitle: "Parent", prompt: "y".repeat(72), ordinal: 3 },
  "73 characters are clipped": { parentTitle: "Parent", prompt: "z".repeat(73), ordinal: 3 },
  "nothing to name it by": { parentTitle: "Parent", title: "  ", prompt: "   ", ordinal: 4 },
}))
  add(name, "title", input, subagentThreadTitle(input));

const parentThread = {
  createdBy: "user",
  creationSource: "web",
  id: "thread:parent",
  projectId: "project",
  title: "Parent",
  providerInstanceId: "codex",
  modelSelection: { instanceId: "codex", model: "gpt-5.4" },
  runtimeMode: "full-access",
  interactionMode: "default",
  branch: "feature",
  worktreePath: "/workspace",
  activeProviderThreadId: "provider-thread:parent",
  linkedPullRequest: { repository: "o/r", number: 1, url: "https://github.com/o/r/pull/1" },
  pullRequests: [{ host: "github.com", repository: "o/r", number: 1 }],
  historyOrigin: { type: "import" },
  lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: "thread:parent" },
  forkedFrom: null,
  createdAt: at(CREATED),
  updatedAt: at(CREATED),
  archivedAt: at(CREATED),
  settledOverride: "settled",
  settledAt: at(CREATED),
  snoozedUntil: at(NOW),
  snoozedAt: at(CREATED),
  lastVisitedAt: at(CREATED),
  deletedAt: null,
  titleRegeneration: { type: "regenerate" },
};
const nested = {
  ...parentThread,
  id: "thread:middle",
  lineage: {
    parentThreadId: "thread:parent",
    relationshipToParent: "subagent",
    rootThreadId: "thread:parent",
  },
};
for (const [name, thread, input] of [
  [
    "a provider child of a root thread",
    parentThread,
    {
      childThreadId: "thread:child",
      parentNodeId: "node:subagent",
      activeProviderThreadId: "provider-thread:child",
      providerInstanceId: "codex",
      modelSelection: { instanceId: "codex", model: "gpt-5.6-luna" },
      title: "Child",
      createdBy: "agent",
      creationSource: "provider",
    },
  ],
  [
    "a delegated child with no provider thread yet",
    parentThread,
    {
      childThreadId: "thread:task",
      parentNodeId: "node:task",
      activeProviderThreadId: null,
      providerInstanceId: "claude",
      modelSelection: { instanceId: "claude", model: "opus" },
      title: "Task",
      createdBy: "agent",
      creationSource: "mcp",
    },
  ],
  [
    "a child of a child keeps the root",
    nested,
    {
      childThreadId: "thread:grandchild",
      parentNodeId: "node:nested",
      activeProviderThreadId: "provider-thread:grandchild",
      providerInstanceId: "codex",
      modelSelection: { instanceId: "codex", model: "gpt-5.6-luna" },
      title: "Grandchild",
      createdBy: "agent",
      creationSource: "provider",
    },
  ],
]) {
  const full = { parentThread: thread, now: at(NOW), ...input };
  add(
    name,
    "childThread",
    { parentThread: plain(thread), ...plain(input), now: NOW },
    makeSubagentChildThread(full),
  );
}

for (const [name, input] of Object.entries({
  "an assistant message": {
    messageId: "message:a",
    turnItemId: "turn-item:a",
    threadId: "thread:child",
    rootNodeId: "node:root",
    providerThreadId: "provider-thread:child",
    providerTurnId: "provider-turn:child",
    nativeItemRef: { driver: "codex", nativeId: "native-a", strength: "strong" },
    role: "assistant",
    text: "Done.",
    ordinal: 5,
  },
  "a user message with no sender": {
    messageId: "message:u",
    turnItemId: "turn-item:u",
    threadId: "thread:child",
    rootNodeId: "node:root",
    providerThreadId: null,
    providerTurnId: null,
    nativeItemRef: null,
    role: "user",
    text: "Read package.json",
    ordinal: 0,
  },
  "a user message from the parent": {
    messageId: "message:s",
    senderThreadId: "thread:parent",
    turnItemId: "turn-item:s",
    threadId: "thread:child",
    rootNodeId: "node:root",
    providerThreadId: "provider-thread:child",
    providerTurnId: "provider-turn:child",
    nativeItemRef: null,
    role: "user",
    text: "Continue",
    ordinal: 1,
  },
  "an assistant message ignores a sender": {
    messageId: "message:i",
    senderThreadId: "thread:parent",
    turnItemId: "turn-item:i",
    threadId: "thread:child",
    rootNodeId: "node:root",
    providerThreadId: null,
    providerTurnId: null,
    nativeItemRef: null,
    role: "assistant",
    text: "Hi",
    ordinal: 2,
  },
}))
  add(name, "artifacts", input, makeSubagentConversationArtifacts({ ...input, now: at(NOW) }));

const message = (id, runId, role, text, updatedAt) => ({
  id,
  runId,
  role,
  text,
  updatedAt: at(updatedAt),
});
const assistant = (id, runId, text, ordinal) => ({
  id,
  runId,
  type: "assistant_message",
  text,
  ordinal,
  messageId: `message:${id}`,
});
const run = (id, status, extra = {}) => ({ id, status, ordinal: 1, startedAt: NOW, ...extra });
const resultCases = {
  "the latest assistant message": {
    run: run("run-1", "completed"),
    messages: [
      message("m1", "run-1", "assistant", "first", "2026-10-02T00:00:00.000Z"),
      message("m2", "run-1", "assistant", "second", "2026-10-03T00:00:00.000Z"),
      message("m3", "run-1", "user", "ignored", "2026-10-04T00:00:00.000Z"),
    ],
    turnItems: [assistant("i1", "run-1", "item text", 3)],
  },
  "a blank message falls back to the item": {
    run: run("run-1", "completed"),
    messages: [message("m1", "run-1", "assistant", "   ", "2026-10-02T00:00:00.000Z")],
    turnItems: [assistant("i1", "run-1", "item one", 1), assistant("i2", "run-1", "item two", 4)],
  },
  "nothing said on a completed run": {
    run: run("run-1", "completed"),
    messages: [],
    turnItems: [],
  },
  "nothing said on an interrupted run": {
    run: run("run-1", "interrupted"),
    messages: [],
    turnItems: [],
  },
  "a failed run reports its failure": {
    run: run("run-1", "failed"),
    messages: [message("m1", "run-1", "assistant", "partial", "2026-10-02T00:00:00.000Z")],
    turnItems: [
      assistant("i1", "run-1", "partial", 1),
      {
        id: "e1",
        runId: "run-1",
        type: "error",
        ordinal: 7,
        failure: { class: "provider_error", message: "Boom" },
      },
    ],
  },
  "a failed run without an error item": {
    run: run("run-1", "failed"),
    messages: [],
    turnItems: [],
  },
  "other runs are ignored": {
    run: run("run-1", "completed"),
    messages: [message("m1", "run-2", "assistant", "other", "2026-10-02T00:00:00.000Z")],
    turnItems: [assistant("i1", "run-2", "other", 1)],
  },
};
for (const [name, input] of Object.entries(resultCases)) {
  const decoded = {
    run: input.run,
    messages: input.messages.map((entry) => ({ ...entry, updatedAt: entry.updatedAt })),
    turnItems: input.turnItems,
  };
  const encoded = plain(decoded);
  add(name, "result", encoded, subagentResultForRun(decoded, decoded.run));
}

const progressRun = (id, ordinal, status, startedAt = NOW) => ({
  id,
  ordinal,
  status,
  startedAt,
  requestedAt: NOW,
  completedAt: status === "running" ? null : NOW,
});
const decodeRun = (entry) => ({
  ...entry,
  startedAt: entry.startedAt === null ? null : at(entry.startedAt),
  requestedAt: at(entry.requestedAt),
  completedAt: entry.completedAt === null ? null : at(entry.completedAt),
});
const task = (status, completionDelivery) => ({
  status,
  ...(completionDelivery === undefined
    ? {}
    : { completionDelivery: { state: completionDelivery } }),
});
const progress = {
  "a running run is working": {
    runs: [progressRun("r1", 1, "running")],
    messages: [],
    subagents: [],
    providerThreads: [],
  },
  "a finished run with nothing outstanding": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [],
    providerThreads: [],
  },
  "a running child holds the parent": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [task("running")],
    providerThreads: [],
  },
  "an idle child does not": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [task("idle")],
    providerThreads: [],
  },
  "an owed delivery holds the parent": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [task("completed", "pending"), task("completed", "delivered")],
    providerThreads: [],
  },
  "a claimed delivery holds the parent": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [task("completed", "claimed")],
    providerThreads: [],
  },
  "a roster holds the parent": {
    runs: [progressRun("r1", 1, "completed")],
    messages: [],
    subagents: [],
    providerThreads: [{ pendingBackgroundTasks: [{ taskId: "t", kind: "command" }] }],
  },
  "a rolled back run is no work": {
    runs: [progressRun("r1", 1, "completed"), progressRun("r2", 2, "rolled_back")],
    messages: [],
    subagents: [],
    providerThreads: [],
  },
  "a monitor's run is not work": {
    runs: [progressRun("r1", 1, "completed"), progressRun("r2", 2, "running")],
    messages: [{ runId: "r2", notification: { source: { kind: "monitor" } } }],
    subagents: [],
    providerThreads: [],
  },
  "the latest finished run is the result": {
    runs: [progressRun("r1", 1, "completed"), progressRun("r2", 2, "failed")],
    messages: [],
    subagents: [],
    providerThreads: [],
  },
  "a run that never started is no result": {
    runs: [progressRun("r1", 1, "completed"), progressRun("r2", 2, "cancelled", null)],
    messages: [],
    subagents: [],
    providerThreads: [],
  },
  "no runs yet": { runs: [], messages: [], subagents: [], providerThreads: [] },
};
for (const [name, input] of Object.entries(progress)) {
  const decoded = { ...input, runs: input.runs.map(decodeRun) };
  const output = delegatedTaskProgress(decoded);
  add(name, "progress", plain(decoded), {
    state: output.state,
    resultRun: output.resultRun?.id ?? null,
  });
}

writeFileSync(
  new URL("../crates/server/tests/fixtures/subagent-projection.jsonl.tmp", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
renameSync(
  new URL("../crates/server/tests/fixtures/subagent-projection.jsonl.tmp", import.meta.url),
  new URL("../crates/server/tests/fixtures/subagent-projection.jsonl", import.meta.url),
);
console.log(`${rows.length} subagent projection rows`);
