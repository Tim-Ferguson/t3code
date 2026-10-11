// Runs the unchanged `coalesceLiveToolUpdatedEvents` (apps/server/src/orchestration-v2/ThreadLiveEventCoalescer.ts) over seeded
// sequences of thread events and records which sequence numbers survive, for the Rust port to replay
// (crates/server/src/thread_live_coalescer.rs).
import { writeFileSync } from "node:fs";
import { coalesceLiveToolUpdatedEvents } from "../../apps/server/src/orchestration-v2/ThreadLiveEventCoalescer.ts";

let seed = 20260906;
const random = (limit) => {
  seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
  return (seed >>> 8) % limit;
};
const pick = (values) => values[random(values.length)];

const rows = [];
for (let index = 0; index < 400; index += 1) {
  const events = [];
  const length = 1 + random(20);
  for (let sequence = 1; sequence <= length; sequence += 1) {
    const type =
      random(8) === 0
        ? pick(["run.updated", "message.updated", "node.updated"])
        : "turn-item.updated";
    const event = {
      type,
      threadId: pick(["t", "t", "t", "t2"]),
      ...(random(6) === 0 ? {} : { runId: pick(["r", "r", "r2"]) }),
      payload: {
        id: pick(["a", "a", "b"]),
        type: pick([
          "command_execution",
          "command_execution",
          "command_execution",
          "file_change",
          "file_search",
          "web_search",
          "dynamic_tool",
          "assistant_message",
          "reasoning",
        ]),
        status: pick(["running", "running", "running", "completed", "failed"]),
      },
    };
    events.push({ kind: "event", sequence, event });
  }
  rows.push({
    events,
    survivors: coalesceLiveToolUpdatedEvents(events).map((item) => item.sequence),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/thread-live-coalescing.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} sequences`);
