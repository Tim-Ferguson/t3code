// Executes the unchanged exported pure functions of ClaudeAdapterV2 and the MCP tool presentation
// and records witnesses for the Rust port. No provider runs here.
import { writeFileSync } from "node:fs";
import * as Adapter from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";
import { mcpToolPresentation } from "../../apps/server/src/provider/McpToolPresentation.ts";
import { T3_MCP_TOOL_NAMES } from "../../packages/shared/src/t3McpToolPresentation.ts";

writeFileSync(
  new URL("../crates/server/src/claude-t3-mcp-tools.json", import.meta.url),
  JSON.stringify([...T3_MCP_TOOL_NAMES].sort(), null, 1) + "\n",
);

const rows = [];
for (const name of [
  "Bash",
  "bash",
  "Edit",
  "MultiEdit",
  "NotebookEdit",
  "Write",
  "Read",
  "Glob",
  "Grep",
  "LS",
  "WebFetch",
  "web_search",
  "Web Search",
  "Agent",
  "Task",
  "TodoWrite",
  "Monitor",
  "mcp__t3-code__html_render",
  "unknown_tool",
  "",
])
  rows.push({ operation: "classify", name, output: Adapter.classifyClaudeNativeTool(name) });
for (const input of [
  {
    todos: [
      { content: " a ", status: "in_progress" },
      { content: "b", status: "completed" },
      { content: "", status: "pending" },
      { content: "c" },
      5,
    ],
  },
  { todos: "no" },
  {},
  null,
])
  rows.push({ operation: "todos", input, output: Adapter.claudeTodoSteps(input) });
for (const input of [{ plan: "  step one  " }, { plan: "   " }, { plan: 3 }, {}, null])
  rows.push({ operation: "plan", input, output: Adapter.claudeProposedPlan(input) });
for (const input of [
  {
    questions: [
      {
        question: " Which? ",
        header: " H ",
        options: [{ label: " A ", description: " d " }, { label: "" }, { label: "B" }],
        multiSelect: true,
      },
      { question: "", header: "x" },
      { question: "Second" },
      4,
    ],
  },
  { questions: "no" },
  {},
])
  rows.push({ operation: "questions", input, output: Adapter.claudeUserInputQuestions(input) });
for (const answers of [{}, { Which: "A", Many: ["A", "B", 3], None: null, Num: 4 }])
  rows.push({ operation: "answers", answers, output: Adapter.claudeSdkUserInputAnswers(answers) });
for (const decision of ["accept", "acceptForSession", "decline", "cancel"])
  for (const suggestions of [
    undefined,
    [{ type: "addDirectories", directories: ["/a"], destination: "projectSettings" }],
  ])
    rows.push({
      operation: "permission",
      decision,
      suggestions: suggestions ?? null,
      output: Adapter.permissionResultFromDecision({
        toolName: "Bash",
        decision,
        toolInput: { command: "ls" },
        toolUseID: "toolu_1",
        ...(suggestions === undefined ? {} : { suggestions }),
      }),
    });
for (const toolName of [
  "Bash",
  "mcp__linear__create_issue",
  "mcp__t3-code__html_render",
  "mcp__t3-code__not_a_tool",
  "mcp__claude_ai_Gmail__search_threads",
  "mcp__a__b__c",
  "mcp__only",
  "Read",
])
  rows.push({
    operation: "presentation",
    toolName,
    output: mcpToolPresentation({ toolName: toolName.replace(/^mcp__claude_ai_/u, "mcp__") }),
  });
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-adapter-functions.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${T3_MCP_TOOL_NAMES.size} T3 MCP tools, ${rows.length} witnesses`);
