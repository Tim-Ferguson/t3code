// Splits the original's `delegated_task_status` Codex recording, which `OrchestratorMcpToolkit.integration.test.ts`
// replays, for `tests/codex_delegated_replay.rs`.
// The original serves the parent and its child from one app-server session; the Rust server runs one app-server
// process per thread, so the recording is split by the label prefix into the parent's frames and the child's,
// each with the shared `initialize`, and the child runs on a second provider instance that serves its half. The original runs it under a runtime-policy override that
// leaves `cwd` and `model` out of `thread/start`; the Rust client always sends them, as it does for every other
// recording, so the two keys are added to the recorded `thread/start` frames. Nothing else is changed.
import { readFileSync, writeFileSync } from "node:fs";

const source = new URL(
  "../../apps/server/src/orchestration-v2/testkit/fixtures/delegated_task_status/codex_transcript.ndjson",
  import.meta.url,
);
const lines = readFileSync(source, "utf8")
  .split("\n")
  .filter((line) => line.trim() !== "")
  .map((line) => JSON.parse(line));
for (const entry of lines) {
  if (entry.type === "expect_outbound" && entry.frame.method === "thread/start") {
    entry.frame.params = { ...entry.frame.params, cwd: "<workspace>", model: "gpt-5.4" };
  }
}
const shared = (entry) => !/^(parent|child)\./.test(entry.label ?? "");
for (const half of ["parent", "child"]) {
  const frames = lines.filter(
    (entry) =>
      entry.type === "transcript_start" || shared(entry) || entry.label.startsWith(`${half}.`),
  );
  writeFileSync(
    new URL(
      `../crates/server/tests/fixtures/codex-delegated-task-status.${half}.ndjson`,
      import.meta.url,
    ),
    frames.map((entry) => JSON.stringify(entry).replaceAll("gpt-5.4", "fixture-model")).join("\n") +
      "\n",
  );
}
console.log(JSON.stringify({ frames: lines.length }));
