// Runs the original's T3 MCP tool naming and summaries over many names and tool results, for the
// Rust tests in t3-client (`tests/t3_tools_parity.rs`), and writes the tool inventory the Rust
// code reads (`crates/client/src/t3_mcp_tools.json`). The original's own files are converted to
// plain modules unchanged (types stripped) and driven directly:
//   packages/shared/src/t3McpToolPresentation.ts    names, labels, icons, summary actions
//   packages/client-runtime/src/t3ToolSummary.ts    group summaries and result failure
//   packages/shared/src/changeRequestUrl.ts         the pull request number in a link
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-t3-tools-"));
const load = async (name, source) => {
  const file = NodePath.join(scratch, `${name}.mjs`);
  writeFileSync(file, stripTypeScriptTypes(source));
  return import(pathToFileURL(file).href);
};
const presentation = await load(
  "presentation",
  read("packages/shared/src/t3McpToolPresentation.ts"),
);
const summary = await load("summary", read("packages/client-runtime/src/t3ToolSummary.ts"));
const changeRequest = await load(
  "change-request",
  read("packages/shared/src/changeRequestUrl.ts").replace(
    /^import \{ canonicalRepositoryKey \} from "\.\/sourceControl\.ts";$/mu,
    "const canonicalRepositoryKey = (key) => key;",
  ),
);
rmSync(scratch, { recursive: true, force: true });

// ---- the inventory -----------------------------------------------------------------------------
const ids = [...presentation.T3_MCP_TOOL_NAMES];
const inventory = ids.map((id) => {
  const definition = presentation.resolveT3McpToolDefinition(id);
  return {
    id,
    displayName: definition.displayName,
    labels: definition.labels,
    icon: definition.icon,
    summaryAction: definition.summaryAction,
  };
});
writeFileSync(
  new URL("../crates/client/src/t3_mcp_tools.json", import.meta.url),
  `${JSON.stringify(inventory, null, 2)}\n`,
);

// ---- names ---------------------------------------------------------------------------------------
let seed = 20261010;
const random = () => {
  seed = (seed * 1664525 + 1013904223) >>> 0;
  return seed / 2 ** 32;
};
const pick = (list) => list[Math.floor(random() * list.length)];
const rows = [];
const names = new Set([
  "",
  "   ",
  "Read",
  "bash",
  "t3-code",
  "t3-code.",
  "mcp__t3-code__",
  "mcp__other__html_render",
]);
for (const id of ids) {
  for (const spelling of [
    id,
    `mcp__t3-code__${id}`,
    `mcp__T3_Code__${id}`,
    `mcp__t3code__${id}`,
    `t3-code.${id}`,
    `t3_code:${id}`,
    `t3code/${id}`,
    `t3-code · ${id}`,
    `t3-code_${id}`,
    `mcp_t3-code__${id}`,
    `mcp-t3-code-${id}`,
    `t3-code-thread_abc_${id}`,
    `t3-code-thread-abc_${id}`,
    `${id} complete`,
    `mcp__t3-code__${id} Completed `,
    `mcp__other__${id}`,
    `other.${id}`,
    `${id}x`,
    `t3-code.${id}x`,
  ])
    names.add(spelling);
}
for (const name of names) {
  const definition = presentation.resolveT3McpToolDefinition(name);
  rows.push({
    fn: "resolve",
    input: name,
    output: {
      id: presentation.resolveT3McpToolId(name),
      displayName: definition?.displayName ?? null,
      summaryAction: presentation.resolveT3McpToolSummaryAction(name),
    },
  });
}

// ---- results and summaries ---------------------------------------------------------------------------
const word = () => pick(["alpha", "beta", "gamma", "delta"]);
const envelope = (data, form) => {
  switch (form) {
    case 0:
      return data;
    case 1:
      return JSON.stringify(data);
    case 2:
      return { structuredContent: data };
    case 3:
      return { content: [{ type: "text", text: JSON.stringify(data) }] };
    case 4:
      return [{ type: "text", text: JSON.stringify(data) }];
    case 5:
      return { isError: true, content: [{ type: "text", text: JSON.stringify(data) }] };
    case 6:
      return { _tag: "ToolFailure", ...data };
    case 7:
      return { error: "nope", ...data };
    case 8:
      return { content: null };
    case 9:
      return [{ text: { text: JSON.stringify(data) } }, { text: "not json" }];
    case 10:
      return "plain text";
    default:
      return undefined;
  }
};
const actions = [...new Set(inventory.map((tool) => tool.summaryAction))];
for (let index = 0; index < 700; index += 1) {
  const action = pick(actions);
  const count = 1 + Math.floor(random() * 4);
  const calls = Array.from({ length: count }, () => {
    const data = {
      ...(random() < 0.6 ? { threadId: `thread-${word()}` } : {}),
      ...(random() < 0.4 ? { messageId: `m-${word()}` } : {}),
      ...(random() < 0.4 ? { taskId: `task-${word()}` } : {}),
      ...(random() < 0.3 ? { scheduledTaskId: `s-${word()}` } : {}),
      ...(random() < 0.3 ? { queuedRunId: `q-${word()}` } : {}),
      ...(random() < 0.3 ? { requestId: `r-${word()}` } : {}),
      ...(random() < 0.3 ? { attachmentId: `a-${word()}` } : {}),
      ...(random() < 0.3 ? { worktreePath: `/w/${word()}` } : {}),
      ...(random() < 0.3 ? { cwd: `/c/${word()}` } : {}),
      ...(random() < 0.3 ? { id: `p-${word()}` } : {}),
      ...(random() < 0.3 ? { projectId: `p-${word()}` } : {}),
      ...(random() < 0.2 ? { thread: { threadId: `thread-${word()}` } } : {}),
      ...(random() < 0.3
        ? {
            threads: Array.from({ length: 1 + Math.floor(random() * 3) }, () =>
              random() < 0.2 ? { status: "rolled_back" } : { threadId: `thread-${word()}` },
            ),
          }
        : {}),
    };
    const input = {
      ...(random() < 0.5 ? { threadId: `thread-${word()}` } : {}),
      ...(random() < 0.3 ? { taskId: `task-${word()}` } : {}),
      ...(random() < 0.3 ? { queuedRunId: `q-${word()}` } : {}),
      ...(random() < 0.4
        ? {
            attachments: Array.from({ length: Math.floor(random() * 3) }, () => ({ name: word() })),
          }
        : {}),
    };
    const wrapInput = random() < 0.15;
    return {
      input: wrapInput ? { toolName: "x", args: input } : input,
      output: envelope(data, Math.floor(random() * 12)),
      outcome: pick(["completed", "completed", "failed", "unfinished"]),
    };
  });
  const result = summary.summarizeT3ToolCalls(action, calls);
  rows.push({ fn: "summarize", input: { action, calls }, output: result });
  for (const call of calls)
    rows.push({
      fn: "failure",
      input: call.output ?? null,
      output: summary.t3ToolResultIndicatesFailure(call.output),
    });
}

// ---- pull request links ---------------------------------------------------------------------------------
for (const url of [
  "https://github.com/o/r/pull/12",
  "https://github.com/o/r/pull/12/files",
  "https://github.com/o/r/issues/12",
  "https://ghe.example.com/o/r/pull/3",
  "https://github.example.com/o/r/pull/3",
  "https://example.com/o/r/pull/3",
  "https://git.example.com/o/r/pulls/9",
  "https://gitlab.com/g/s/r/-/merge_requests/4",
  "https://selfhosted.example/g/r/-/merge_requests/4",
  "https://bitbucket.org/w/r/pull-requests/8",
  "https://dev.azure.com/org/proj/_git/repo/pullrequest/5",
  "https://org.visualstudio.com/proj/_git/repo/pullrequest/5",
  "https://github.com/o/r/pull/0",
  "https://github.com/o/r/pull/99999999999999999999",
  "javascript:alert(1)",
  "not a url",
  "",
])
  rows.push({
    fn: "changeRequestNumber",
    input: url,
    output: changeRequest.parseChangeRequestUrl(url)?.number ?? null,
  });

writeFileSync(
  new URL("../crates/client/tests/fixtures/t3-tools.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows and ${inventory.length} tools`);
