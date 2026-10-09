// Runs the unchanged `Notification.ts` helpers over scripted reports and messages and writes what
// they return: the notification a provider's wake names (`backgroundWorkNotification`) and the
// timeline item a message that carries one is shown as (`notificationTurnItem`). The Rust port
// replays every row (`notification.rs`).
import { renameSync, writeFileSync } from "node:fs";
import {
  backgroundWorkNotification,
  notificationTurnItem,
} from "../../apps/server/src/orchestration-v2/Notification.ts";

const rows = [];
const long = "x".repeat(120);
const reports = {
  "one command": [{ kind: "command", label: "sleep 5", outcome: "completed", exitCode: 0 }],
  "a failed command with an exit code": [
    { kind: "command", label: "make test", outcome: "failed", exitCode: 2 },
  ],
  "an unnamed monitor": [{ kind: "monitor", outcome: "updated" }],
  "a stopped subagent that opens its thread": [
    { kind: "subagent", label: "Agent B", outcome: "cancelled", childThreadId: "thread:b" },
  ],
  "two subagents of one kind": [
    { kind: "subagent", label: "A", outcome: "completed", childThreadId: "thread:a" },
    { kind: "subagent", label: "B", outcome: "completed", childThreadId: "thread:b" },
  ],
  "mixed kinds": [
    { kind: "command", label: "build", outcome: "completed" },
    { kind: "monitor", label: "tail", outcome: "failed" },
  ],
  "more than three": ["a", "b", "c", "d"].map((label) => ({
    kind: "background_task",
    label,
    outcome: "completed",
  })),
  "none named": [
    { kind: "command", outcome: "completed" },
    { kind: "command", outcome: "completed" },
  ],
  "a long first line": [{ kind: "background_task", label: `${long}\nsecond`, outcome: "unknown" }],
  "no reports": [],
};
for (const [name, input] of Object.entries(reports)) {
  rows.push({ name, kind: "notification", input, output: backgroundWorkNotification(input) });
}
const item = {
  id: "item",
  type: "user_message",
  messageId: "m",
  inputIntent: "turn_start",
  text: "Background task completed.",
  attachments: [],
  createdBy: "agent",
  creationSource: "provider",
  status: "completed",
  ordinal: 3,
};
const notification = {
  source: { kind: "command" },
  outcome: "completed",
  summary: "Command finished",
};
for (const [name, input, message] of [
  ["a message that reports background work", item, { notification }],
  ["a plain message", item, {}],
  ["an item that is not a user message", { ...item, type: "assistant_message" }, { notification }],
]) {
  rows.push({
    name,
    kind: "item",
    input: { item: input, message },
    output: notificationTurnItem(input, message, []),
  });
}
const target = new URL("../crates/server/tests/fixtures/notification.jsonl", import.meta.url);
writeFileSync(`${target.pathname}.tmp`, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(`${target.pathname}.tmp`, target.pathname);
console.log(`${rows.length} notification rows`);
