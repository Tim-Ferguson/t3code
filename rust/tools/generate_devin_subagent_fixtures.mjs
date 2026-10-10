// Devin's subagent extraction (`extractDevinSubagentUpdate`) over tool calls, computed by the unchanged
// TypeScript for the Rust port to reproduce.
import { writeFileSync } from "node:fs";
import { extractDevinSubagentUpdate } from "../../apps/server/src/orchestration-v2/Adapters/DevinAcp.ts";

const tool = (meta) => ({ toolCallId: "t1", title: "Tool", status: "inProgress", data: { meta } });
const cases = [
  tool(undefined),
  tool({}),
  tool({
    "cognition.ai/subagent_started": {
      agentId: "a1",
      task: "Do it",
      title: "Titled",
      model: "swe-1",
    },
  }),
  tool({ "cognition.ai/subagent_started": { agentId: "a1" } }),
  tool({ "cognition.ai/subagent_started": { agentId: 7, task: "x" } }),
  tool({ "cognition.ai/subagent_completed": { agentId: "a1", success: true, summary: "Done" } }),
  tool({ "cognition.ai/subagent_completed": { agentId: "a1", success: false, summary: "Broke" } }),
  tool({ "cognition.ai/subagent_completed": { agentId: "a1" } }),
  tool({
    "cognition.ai/subagent_started": { agentId: "a2", task: "ignored for status", title: "T" },
    "cognition.ai/subagent_completed": { agentId: "a3", success: true, summary: "S" },
  }),
  { toolCallId: "t2", data: { meta: [] } },
];
const rows = cases.map((input) => ({ input, update: extractDevinSubagentUpdate(input) ?? null }));
writeFileSync(
  new URL("../crates/server/tests/fixtures/devin-subagent.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} devin cases`);
