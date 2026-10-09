// Runs the original's timeline row derivation over many threads and records the rows it makes, for
// the Rust tests in t3-client (`tests/timeline_rows_parity.rs`). The original's own files are
// converted to plain modules unchanged (types stripped, imports rewired) and driven directly:
//   apps/web/src/session-logic.ts                              work entries from turn items
//   apps/web/src/components/chat/MessagesTimeline.logic.ts     grouping, folds, the live row
//   packages/client-runtime/src/work-log/presentation.ts       tool actions, summaries, failures
//   packages/client-runtime/src/work-log/toolPresentation.ts   tool surface, icon and source
//   packages/shared/src/toolActivity.ts                        tool classification and labels
// What is stood in for, and so is not covered: T3 MCP tool names (`resolveT3McpToolDefinition`
// answers nothing), the shell parser behind `commandDisplayText` / `commandProgramName` (see the
// stubs below), published HTML pages and MCP apps, and the worktree-setup card.
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};

// ---- the files and how their imports are answered -----------------------------------------------
const files = {
  "session-logic": "apps/web/src/session-logic.ts",
  "messages-timeline-logic": "apps/web/src/components/chat/MessagesTimeline.logic.ts",
  presentation: "packages/client-runtime/src/work-log/presentation.ts",
  "tool-presentation": "packages/client-runtime/src/work-log/toolPresentation.ts",
  "tool-activity": "packages/shared/src/toolActivity.ts",
};
const siblings = new Map([
  ["../../session-logic", "session-logic"],
  ["@t3tools/client-runtime/work-log/presentation", "presentation"],
  ["@t3tools/client-runtime/work-log/tool-presentation", "tool-presentation"],
  ["@t3tools/shared/toolActivity", "tool-activity"],
]);

const toolOutput = read("packages/shared/src/toolOutput.ts");
const timing = read("packages/shared/src/orchestrationTiming.ts");
const stubs = {
  // Pieces of other files, unchanged.
  toolOutputIndicatesFailure: undefined,
  formatDuration: undefined,
  // Not covered, see above.
  resolveT3McpToolDefinition: () => null,
  resolveT3McpToolPresentation: () => null,
  resolveT3McpToolId: () => null,
  summarizeT3ToolCalls: () => ({ label: "", failedCount: 0 }),
  t3ToolResultIndicatesFailure: () => false,
  compactDynamicToolOutput: () => undefined,
  htmlRenderFromToolItem: () => undefined,
  mcpAppFromToolItem: () => undefined,
  worktreeSetupAgentStarted: () => false,
  // Paths are shown as the call gave them, with separators made forward slashes (no workspace root is known to the rows).
  formatWorkspaceRelativePath: (path) =>
    path.replaceAll("\\", "/").replace(/^\/([A-Za-z]:)/u, "$1"),
  // The Rust port shows a command whole and names it by its first word.
  commandDisplayText: (command) =>
    command
      .trim()
      .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/gu, (c) =>
        c === "\u007f" ? "␡" : String.fromCharCode(0x2400 + c.charCodeAt(0)),
      ),
  commandProgramName: (command) => {
    const word = command.trim().split(/\s+/u)[0];
    const name = word
      ?.split(/[\\/]/u)
      .at(-1)
      ?.replace(/\.exe$/u, "");
    return name ? name : null;
  },
  isLiveSubagentTurnItem: (item) =>
    item.type === "subagent" && ["pending", "running", "waiting"].includes(item.status),
  turnItemIsWorkspacePreparation: (item) =>
    (item.type === "command_execution" && item.input === "Preparing workspace") ||
    (item.type === "error" &&
      item.status === "cancelled" &&
      item.failure.code === "workspace_preparation_failed"),
  turnItemDetailRevision: (item) => item.updatedAt,
  isToolLifecycleItemType: (value) =>
    [
      "command_execution",
      "file_change",
      "mcp_tool_call",
      "dynamic_tool_call",
      "collab_agent_tool_call",
      "web_search",
      "image_view",
    ].includes(value),
  isWorkspaceImagePreviewPath: (path) =>
    [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"].some((e) =>
      path.toLowerCase().endsWith(e),
    ),
  RunId: { make: (value) => value },
  DateTime: { formatIso: (value) => value },
  Equal: { equals: (a, b) => a === b },
  shallow: (a, b) => a === b,
  deriveRevertTurnCountByUserMessageId: () => new Map(),
  timelineEntryIsPersistentResourceCard: undefined,
};
const plain = (text) => stripTypeScriptTypes(text.replace("export ", ""));
stubs.toolOutputIndicatesFailure = new Function(
  `${plain(cut(toolOutput, "export function toolOutputIndicatesFailure", "/** An image a tool returned"))}; return toolOutputIndicatesFailure;`,
)();
stubs.formatDuration = new Function(
  `${plain(cut(timing, "export function formatDuration", "function isLatestRunSettled"))}; return formatDuration;`,
)();
const usage = read("packages/shared/src/usageFormat.ts");
stubs.formatTokens = new Function(
  `${plain(cut(usage, "export function formatTokens", "function trim("))}; ${plain(cut(usage, "function trim(", "\n}\n") + "\n}\n")}; return formatTokens;`,
)();
globalThis.__stubs = new Proxy(stubs, {
  get: (target, key) =>
    key in target
      ? target[key]
      : () => {
          throw new Error(`stub ${String(key)} was used`);
        },
});

const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-timeline-rows-"));
function convert(name) {
  let source = read(files[name]);
  const out = [];
  // Imports and re-exports: types go, siblings are rewired, everything else comes from the stubs.
  source = source.replace(
    /^export\s+(type\s+)?\{([^}]*)\}\s+from\s+"([^"]+)";?\s*$/gm,
    (_all, isType, names, specifier) => {
      if (isType) return "";
      const items = names
        .split(",")
        .map((item) => item.trim())
        .filter((item) => item && !item.startsWith("type "));
      if (siblings.has(specifier))
        return `export { ${items.join(", ")} } from "./${siblings.get(specifier)}.mjs";`;
      return items
        .map(
          (item) =>
            `const __re_${item} = globalThis.__stubs.${item}; export { __re_${item} as ${item} };`,
        )
        .join("\n");
    },
  );
  source = source.replace(/^import\s+type\s+[\s\S]*?\s+from\s+"[^"]+";?\s*$/gm, "");
  source = source.replace(
    /^import\s+(\*\s+as\s+(\w+)|\{([\s\S]*?)\})\s+from\s+"([^"]+)";?\s*$/gm,
    (_all, _what, namespace, names, specifier) => {
      if (namespace) return `const ${namespace} = globalThis.__stubs.${namespace};`;
      const items = names
        .split(",")
        .map((item) => item.trim())
        .filter((item) => item && !item.startsWith("type "));
      if (items.length === 0) return "";
      if (siblings.has(specifier))
        return `import { ${items.join(", ")} } from "./${siblings.get(specifier)}.mjs";`;
      return `const { ${items.map((item) => item.replace(/\s+as\s+/u, ": ")).join(", ")} } = globalThis.__stubs;`;
    },
  );
  void out;
  writeFileSync(NodePath.join(scratch, `${name}.mjs`), stripTypeScriptTypes(source));
}
for (const name of Object.keys(files)) convert(name);
const logic = await import(
  pathToFileURL(NodePath.join(scratch, "messages-timeline-logic.mjs")).href
);
const session = await import(pathToFileURL(NodePath.join(scratch, "session-logic.mjs")).href);
const presentation = await import(pathToFileURL(NodePath.join(scratch, "presentation.mjs")).href);
rmSync(scratch, { recursive: true, force: true });

// ---- building threads -----------------------------------------------------------------------------
let seed = 20261009;
const random = () => {
  seed = (seed * 1664525 + 1013904223) >>> 0;
  return seed / 2 ** 32;
};
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (probability) => random() < probability;

const clock = (index) => new Date(Date.UTC(2026, 9, 1, 10, 0, 0) + index * 7_000).toISOString();

function projectedRow(item) {
  return { item, sourceThreadId: "thread-1", sourceItemId: item.id };
}

/** A thread of turn items in the shapes the server writes them. */
function buildThread(spec) {
  const items = [];
  let tick = 0;
  const base = (type, run, extra = {}) => {
    const id = `${type}-${items.length}`;
    tick += 1;
    return {
      id,
      type,
      runId: run ?? null,
      nodeId: run ? `node-${run}` : null,
      parentItemId: null,
      status: "completed",
      startedAt: clock(tick),
      updatedAt: clock(tick + 1),
      ...extra,
    };
  };
  for (const step of spec) {
    const run = step.run === undefined ? "run-1" : step.run;
    const make = (type, extra) =>
      items.push(
        base(type, run, {
          ...extra,
          ...(step.status ? { status: step.status } : {}),
          ...(step.attemptNode ? { nodeId: step.attemptNode } : {}),
        }),
      );
    switch (step.t) {
      case "user":
        make("user_message", {
          messageId: `m-${items.length}`,
          text: step.text ?? "Please do the thing",
          inputIntent: step.intent ?? "turn_start",
          createdBy: "user",
          creationSource: "client",
        });
        break;
      case "assistant":
        make("assistant_message", {
          messageId: `m-${items.length}`,
          text: step.text ?? "Done.",
          streaming: step.streaming ?? false,
        });
        break;
      case "reasoning":
        make("reasoning", { text: step.text ?? "", ...(step.title ? { title: step.title } : {}) });
        break;
      case "command":
        make("command_execution", {
          input: step.input ?? "npm test",
          ...(step.exit !== undefined ? { exitCode: step.exit } : {}),
          ...(step.failureOutput ? { outputIndicatesFailure: true } : {}),
          ...(step.surface ? { toolSurface: step.surface } : {}),
          ...(step.source ? { toolSource: step.source } : {}),
          ...(step.icon ? { toolIcon: step.icon } : {}),
        });
        break;
      case "edit":
        make("file_change", {
          fileName: step.file ?? "src/a.ts",
          changes: step.changes ?? [{ path: step.file ?? "src/a.ts", operation: "update" }],
        });
        break;
      case "search":
        make("file_search", { pattern: step.pattern ?? "TODO" });
        break;
      case "web":
        make("web_search", { patterns: step.patterns ?? ["rust dioxus"], results: [] });
        break;
      case "tool":
        make("dynamic_tool", {
          toolName: step.name ?? "Read",
          input: step.input ?? { file_path: "src/b.ts" },
          output: null,
          ...(step.title ? { title: step.title } : {}),
        });
        break;
      case "approval":
        make("approval_request", { requestKind: "command", prompt: "Run rm?" });
        break;
      case "error":
        make("error", {
          failure: { class: step.cls ?? "provider", message: "Boom", code: "x" },
          ...(step.retry ? { retry: step.retry } : {}),
          ...(step.parent ? { parentItemId: step.parent } : {}),
        });
        break;
      case "notice":
        make("system_notice", { message: "Heads up", level: "info" });
        break;
      case "notification":
        make("notification", { summary: "Background task finished" });
        break;
      case "compaction":
        make("compaction", { beforeTokenCount: 100000, afterTokenCount: 20000, summary: "kept" });
        break;
      case "plan":
        make("proposed_plan", { planId: `plan-${items.length}`, markdown: "# Plan\n\nDo it." });
        break;
      case "subagent":
        make("subagent", {
          subagentId: step.sub ?? `sub-${items.length}`,
          origin: "tool",
          driver: "codex",
          providerInstanceId: "codex",
          providerTurnId: step.turn ?? "turn-1",
          prompt: "Look",
          result: null,
        });
        break;
      case "thread_created":
        make("thread_created", { createdThreadId: "t2" });
        break;
      case "interrupt":
        make("run_interrupt_result", { outcome: "interrupted" });
        break;
      case "todo":
        make("todo_list", { items: [] });
        break;
      case "checkpoint":
        make("checkpoint", { files: [], checkpointId: `c-${items.length}` });
        break;
      default:
        throw new Error(`unknown step ${step.t}`);
    }
  }
  return items.map(projectedRow);
}

const normalize = (rows, entries, latestRunning) => {
  const kindOf = (id) => entries.find((e) => e.id === id);
  void kindOf;
  const out = [];
  const log = (row) =>
    row.kind === "work" ||
    row.kind === "work-toggle" ||
    row.kind === "work-live" ||
    row.kind === "thinking";
  for (const row of rows) {
    const continues = row.continuesWorkLog === true;
    switch (row.kind) {
      case "work":
        if (row.isExpandedToolGroup)
          out.push({
            kind: "members",
            group: row.id.replace(/:details$/u, ""),
            entries: row.groupedEntries.map((e) => e.id),
          });
        else if (row.displayLabel !== undefined)
          out.push({
            kind: "single",
            entry: row.groupedEntries[0].id,
            label: row.displayLabel,
            continues,
          });
        else out.push({ kind: "entry", id: row.id });
        break;
      case "work-toggle":
        out.push({
          kind: "toggle",
          id: row.id,
          group: row.groupId,
          count: row.hiddenCount,
          summary: row.summary,
          summaryKind: row.summaryKind,
          surface: row.toolSurface ?? null,
          hasFailure: row.hasFailure,
          expanded: row.expanded,
          continues,
        });
        break;
      case "work-live":
        out.push({
          kind: "live",
          id: row.id,
          group: row.groupId,
          entry: row.entry.id,
          label: logic.liveWorkEntryLabel(row.entry, undefined, row.active),
          count: row.groupedEntries.length,
          expanded: row.expanded,
          active: row.active,
          thought: row.thought?.id ?? null,
          surface: null,
          continues,
        });
        break;
      case "thinking":
        out.push({
          kind: "thinking",
          group: row.groupId ?? null,
          expanded: row.expanded === true,
          continues,
        });
        break;
      case "working":
        out.push({ kind: "working", since: row.createdAt });
        break;
      case "turn-fold":
        out.push({ kind: "turn-fold", run: row.runId, label: row.label, expanded: row.expanded });
        break;
      case "attempt-fold":
        out.push({
          kind: "attempt-fold",
          run: row.runId,
          attempt: row.attemptId,
          label: row.label,
          expanded: row.expanded,
        });
        break;
      case "context-compaction":
        out.push({ kind: "compaction", id: row.id, label: row.label, active: row.active });
        break;
      case "event":
        if (row.subagents)
          out.push({ kind: "subagents", ids: row.subagents.map((s) => s.item.id) });
        else if (row.projectedItem.item.type === "subagent")
          out.push({ kind: "subagents", ids: [row.projectedItem.item.id] });
        else out.push({ kind: "entry", id: row.id });
        break;
      case "assistant-meta":
        break;
      default:
        out.push({ kind: "entry", id: row.id });
    }
    void log;
  }
  void latestRunning;
  return out;
};

const rows = [];
function scenario(name, thread, options) {
  const {
    attempts = [],
    nodes = [],
    latestRun = null,
    runningRunId = null,
    expandedRuns = [],
    expandedAttempts = [],
    expandedGroups = [],
    isWorking = false,
    runlessWorkActive = false,
    activeTurnStartedAt = null,
  } = options;
  const entries = session.deriveTimelineEntriesFromVisibleTurnItems({
    visibleTurnItems: thread,
    optimisticMessages: [],
    attempts,
    nodes,
    plans: [],
  });
  const derived = logic.deriveMessagesTimelineRows({
    timelineEntries: entries,
    latestRun,
    runningRunId,
    expandedRunIds: new Set(expandedRuns),
    expandedAttemptIds: new Set(expandedAttempts),
    expandedWorkGroupIds: new Set(expandedGroups),
    isWorking,
    runlessWorkActive,
    activeTurnStartedAt,
    turnDiffSummaries: [],
    supportsConversationRollback: false,
  });
  rows.push({
    name,
    input: {
      items: thread,
      attempts,
      nodes,
      latestRun,
      runningRunId,
      expandedRuns,
      expandedAttempts,
      expandedGroups,
      isWorking,
      runlessWorkActive,
      activeTurnStartedAt,
    },
    entries: entries.map((entry) => ({ id: entry.id, kind: entry.kind })),
    rows: normalize(derived, entries),
    // The summary of every contiguous run of work entries, as the Rust summary function reads it.
  });
}

const settledRun = (id, startedAt = clock(1), completedAt = clock(40)) => ({
  runId: id,
  status: "completed",
  startedAt,
  completedAt,
});
const runningRun = (id) => ({
  runId: id,
  status: "running",
  startedAt: clock(1),
  completedAt: null,
});

// ---- named threads --------------------------------------------------------------------------------
const turn = [
  { t: "user" },
  { t: "reasoning", text: "Looking at the layout" },
  { t: "command", input: "ls -la" },
  { t: "command", input: "cat src/a.ts" },
  { t: "tool", name: "Read", input: { file_path: "src/a.ts" } },
  { t: "edit", file: "src/a.ts" },
  { t: "edit", file: "src/b.ts" },
  { t: "command", input: "npm test" },
  { t: "assistant", text: "All done." },
];
scenario("a settled turn folds its work behind one line", buildThread(turn), {
  latestRun: settledRun("run-1"),
});
scenario("the fold opens", buildThread(turn), {
  latestRun: settledRun("run-1"),
  expandedRuns: ["run-1"],
});
scenario("the group inside an opened fold opens", buildThread(turn), {
  latestRun: settledRun("run-1"),
  expandedRuns: ["run-1"],
  expandedGroups: ["work-group:reasoning-1"],
});
scenario("a running turn keeps its work and shows the live row", buildThread(turn.slice(0, 5)), {
  latestRun: runningRun("run-1"),
  runningRunId: "run-1",
  isWorking: true,
  activeTurnStartedAt: clock(1),
});
scenario(
  "a running turn with a command in progress",
  buildThread([{ t: "user" }, { t: "command", status: "running", input: "cargo build --release" }]),
  {
    latestRun: runningRun("run-1"),
    runningRunId: "run-1",
    isWorking: true,
    activeTurnStartedAt: clock(1),
  },
);
scenario("a running turn with nothing to show yet", buildThread([{ t: "user" }]), {
  latestRun: runningRun("run-1"),
  runningRunId: "run-1",
  isWorking: true,
  activeTurnStartedAt: clock(1),
});
scenario(
  "a running turn whose last call failed",
  buildThread([
    { t: "user" },
    { t: "command", input: "false", exit: 1 },
    { t: "command", input: "ls", status: "failed" },
  ]),
  { latestRun: runningRun("run-1"), runningRunId: "run-1", isWorking: true },
);
scenario(
  "a steer keeps one response",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "assistant", text: "partial", streaming: true },
    { t: "user", intent: "steer", text: "also this" },
    { t: "command", input: "git status" },
  ]),
  { latestRun: runningRun("run-1"), runningRunId: "run-1", isWorking: true },
);
scenario(
  "an interrupted turn says you stopped",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "command", input: "npm run build" },
    { t: "interrupt" },
    { t: "assistant", text: "ok" },
  ]),
  {
    latestRun: {
      runId: "run-1",
      status: "interrupted",
      startedAt: clock(1),
      completedAt: clock(30),
    },
  },
);
scenario(
  "a failed turn stays open",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "command" },
    { t: "error", status: "failed" },
  ]),
  { latestRun: { runId: "run-1", status: "failed", startedAt: clock(1), completedAt: clock(20) } },
);
scenario(
  "two turns fold separately",
  buildThread([
    { t: "user", run: "run-1" },
    { t: "command", run: "run-1" },
    { t: "edit", run: "run-1" },
    { t: "assistant", run: "run-1" },
    { t: "user", run: "run-2" },
    { t: "command", run: "run-2" },
    { t: "tool", run: "run-2" },
    { t: "assistant", run: "run-2" },
  ]),
  { latestRun: settledRun("run-2") },
);
scenario(
  "a lone command is one row",
  buildThread([{ t: "user" }, { t: "command" }, { t: "assistant" }]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "a group of one kind is summarised by count",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "command" },
    { t: "command" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "thoughts alone read as thought",
  buildThread([
    { t: "user" },
    { t: "reasoning", text: "one" },
    { t: "reasoning", text: "two" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "a browser surface and an integration source",
  buildThread([
    { t: "user" },
    {
      t: "command",
      surface: "browser",
      source: { key: "chrome", name: "Chrome", kind: "browser" },
      icon: {
        _tag: "website",
        pageUrl: "https://example.com/",
        faviconUrl: "https://example.com/f.ico",
      },
    },
    { t: "command", source: { key: "gh", name: "GitHub", kind: "integration" } },
    { t: "command" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "subagents launched together form one row",
  buildThread([
    { t: "user" },
    { t: "subagent", status: "running" },
    { t: "subagent", status: "completed" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1") },
);
scenario(
  "a live subagent stays visible after its turn settles",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "subagent", status: "running" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1") },
);
scenario(
  "context compaction between work",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "compaction" },
    { t: "command" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "a warning and a notification stand alone",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "notice" },
    { t: "command" },
    { t: "assistant" },
    { t: "notification", run: null },
    { t: "assistant", run: "run-2" },
  ]),
  { latestRun: settledRun("run-1"), expandedRuns: ["run-1"] },
);
scenario(
  "a plan and a checkpoint are never grouped",
  buildThread([
    { t: "user" },
    { t: "command" },
    { t: "plan" },
    { t: "checkpoint" },
    { t: "todo" },
    { t: "assistant" },
  ]),
  { latestRun: settledRun("run-1") },
);
scenario(
  "a superseded attempt folds its provider output",
  buildThread([
    { t: "user" },
    { t: "command", attemptNode: "node-a" },
    { t: "assistant", text: "partial", attemptNode: "node-a" },
    { t: "user", intent: "steer" },
    { t: "command", input: "git log", attemptNode: "node-b" },
    { t: "assistant", text: "fin", attemptNode: "node-b" },
  ]),
  {
    attempts: [
      {
        id: "att-a",
        runId: "run-1",
        rootNodeId: "node-a",
        status: "superseded",
        attemptOrdinal: 1,
      },
      { id: "att-b", runId: "run-1", rootNodeId: "node-b", status: "completed", attemptOrdinal: 2 },
    ],
    nodes: [
      { id: "node-a", rootNodeId: "node-a", parentNodeId: null },
      { id: "node-b", rootNodeId: "node-b", parentNodeId: null },
    ],
    latestRun: settledRun("run-1"),
  },
);
scenario(
  "an attempt fold opens",
  buildThread([
    { t: "user" },
    { t: "command", attemptNode: "node-a" },
    { t: "assistant", text: "partial", attemptNode: "node-a" },
    { t: "assistant", text: "fin", attemptNode: "node-b" },
  ]),
  {
    attempts: [
      {
        id: "att-a",
        runId: "run-1",
        rootNodeId: "node-a",
        status: "superseded",
        attemptOrdinal: 1,
      },
      { id: "att-b", runId: "run-1", rootNodeId: "node-b", status: "completed", attemptOrdinal: 2 },
    ],
    nodes: [
      { id: "node-a", rootNodeId: "node-a", parentNodeId: null },
      { id: "node-b", rootNodeId: "node-b", parentNodeId: null },
    ],
    latestRun: settledRun("run-1"),
    expandedAttempts: ["att-a"],
  },
);
scenario(
  "work with no run in a provider-native subagent thread",
  buildThread([
    { t: "user", run: null },
    { t: "command", run: null },
    { t: "command", run: null },
    { t: "assistant", run: null },
  ]),
  { latestRun: null },
);
scenario(
  "runless work still running",
  buildThread([
    { t: "user", run: null },
    { t: "command", run: null, status: "running" },
  ]),
  { latestRun: null, isWorking: true, runlessWorkActive: true },
);

// ---- generated threads ------------------------------------------------------------------------------
const toolSteps = () =>
  pick([
    () => ({
      t: "command",
      input: pick(["ls", "npm test", "git status", "cat a.ts", "rg foo", "/bin/bash -lc 'make'"]),
      ...(chance(0.15) ? { exit: 1 } : {}),
      ...(chance(0.1) ? { failureOutput: true } : {}),
      ...(chance(0.1) ? { status: "failed" } : {}),
    }),
    () => ({
      t: "edit",
      file: pick(["a.ts", "b.ts", "c.ts"]),
      ...(chance(0.2) ? { changes: [{ path: "a.ts" }, { path: "b.ts" }] } : {}),
    }),
    () => ({ t: "search", pattern: pick(["TODO", "fn main"]) }),
    () => ({ t: "web" }),
    () => ({
      t: "tool",
      name: pick(["Read", "Grep", "Glob", "Bash", "mcp__db__find", "Skill", "cua_repl.js"]),
      input: pick([
        { file_path: "x.ts" },
        { pattern: "foo", path: "/repo/src" },
        { skill: "review" },
        { title: "Open page" },
        {},
      ]),
    }),
    () => ({
      t: "reasoning",
      text: pick(["", "Considering options", "**Plan the change**\nthen do it"]),
    }),
    () => ({ t: "approval" }),
    () => ({ t: "thread_created" }),
    () => ({
      t: "command",
      input: "sleep 1",
      status: pick(["running", "waiting", "pending", "cancelled", "interrupted"]),
    }),
    () => ({
      t: "command",
      input: "make",
      surface: pick(["browser", "computer"]),
      source: {
        key: pick(["a", "b"]),
        name: pick(["Chrome", "Slack"]),
        kind: pick(["browser", "integration", "computer"]),
      },
    }),
  ])();
const otherSteps = () =>
  pick([
    () => ({ t: "notice" }),
    () => ({ t: "compaction" }),
    () => ({ t: "plan" }),
    () => ({ t: "error", status: pick(["failed", "completed"]) }),
    () => ({
      t: "error",
      status: "running",
      retry: { attempt: 1, maxAttempts: 3, retryDelayMs: 1500 },
    }),
    () => ({
      t: "subagent",
      status: pick(["running", "completed", "waiting"]),
      turn: pick(["turn-1", "turn-2"]),
    }),
    () => ({ t: "interrupt" }),
    () => ({ t: "todo" }),
    () => ({ t: "checkpoint" }),
  ])();
for (let index = 0; index < 400; index += 1) {
  const turns = 1 + Math.floor(random() * 3);
  const spec = [];
  const attempts = [];
  const nodes = [];
  for (let turnIndex = 1; turnIndex <= turns; turnIndex += 1) {
    const run = `run-${turnIndex}`;
    spec.push({ t: "user", run });
    const superseded = chance(0.15);
    const nodeA = `node-${run}-a`;
    const nodeB = `node-${run}-b`;
    if (superseded) {
      attempts.push(
        {
          id: `att-${run}-a`,
          runId: run,
          rootNodeId: nodeA,
          status: "superseded",
          attemptOrdinal: 1,
        },
        {
          id: `att-${run}-b`,
          runId: run,
          rootNodeId: nodeB,
          status: "completed",
          attemptOrdinal: 2,
        },
      );
      nodes.push(
        { id: nodeA, rootNodeId: nodeA, parentNodeId: null },
        { id: nodeB, rootNodeId: nodeB, parentNodeId: null },
      );
    }
    const count = Math.floor(random() * 9);
    for (let step = 0; step < count; step += 1) {
      const next = chance(0.8) ? toolSteps() : otherSteps();
      spec.push({
        ...next,
        run: chance(0.03) ? null : run,
        ...(superseded ? { attemptNode: step < count / 2 ? nodeA : nodeB } : {}),
      });
      if (chance(0.06))
        spec.push({
          t: "assistant",
          run,
          text: "note",
          ...(superseded ? { attemptNode: nodeA } : {}),
        });
    }
    if (chance(0.85))
      spec.push({
        t: "assistant",
        run,
        streaming: chance(0.1),
        ...(superseded ? { attemptNode: nodeB } : {}),
      });
    if (chance(0.1)) spec.push({ t: "user", run, intent: "steer" });
    if (chance(0.1)) spec.push({ t: "notification", run: null });
  }
  const thread = buildThread(spec);
  const lastRun = `run-${turns}`;
  const live = chance(0.4);
  const status = live
    ? pick(["running", "running", "waiting", "starting"])
    : pick(["completed", "completed", "failed", "interrupted", "cancelled"]);
  const latestRun = chance(0.05)
    ? null
    : {
        runId: lastRun,
        status,
        startedAt: chance(0.9) ? clock(1) : null,
        completedAt: live ? null : clock(60),
      };
  const itemIds = thread.map((row) => row.item.id);
  const groupIds = itemIds.map((id) => `work-group:${id}`);
  const subset = (list) => list.filter(() => chance(0.15));
  scenario(`generated ${index}`, thread, {
    attempts,
    nodes,
    latestRun,
    runningRunId: live && latestRun && chance(0.9) ? lastRun : null,
    isWorking: live,
    runlessWorkActive: chance(0.1),
    activeTurnStartedAt: live ? clock(1) : null,
    expandedRuns: subset(Array.from({ length: turns }, (_, i) => `run-${i + 1}`)),
    expandedAttempts: subset(attempts.map((a) => a.id)),
    expandedGroups: subset(groupIds),
  });
}

// ---- tool presentation on its own -------------------------------------------------------------------
const items = rows.flatMap((row) =>
  row.input.items
    .map((r) => r.item)
    .filter((item) => !["user_message", "assistant_message"].includes(item.type)),
);
const seen = new Set();
const presentations = [];
for (const item of items) {
  const entry = session.deriveTimelineEntriesFromVisibleTurnItems({
    visibleTurnItems: [projectedRow(item)],
    optimisticMessages: [],
  })[0];
  if (entry?.kind !== "work") continue;
  const work = entry.entry;
  const key = JSON.stringify([
    item.type,
    item.status,
    item.input,
    item.toolName,
    item.title,
    item.exitCode,
    item.pattern,
    item.changes?.length,
    item.toolSurface,
    item.retry,
    item.failure?.class,
  ]);
  if (seen.has(key)) continue;
  seen.add(key);
  presentations.push({
    item,
    label: logic.workEntryDisplayLabel(work, undefined),
    live: logic.liveWorkEntryLabel(work, undefined, true),
    settled: logic.liveWorkEntryLabel(work, undefined, false),
    single: logic.workEntryDisplayLabel === undefined ? null : null,
    action: presentation.toolGroupAction(work),
    summary: presentation.summarizeToolGroup([work]).summary,
    failure: presentation.workEntryDisplayIndicatesToolFailure(work),
    visible: logic.workEntryIsVisibleInGroup(work, false),
    visibleInActive: logic.workEntryIsVisibleInGroup(work, true),
  });
}

const lines = [
  ...rows.map((row) => JSON.stringify({ type: "thread", ...row })),
  ...presentations.map((row) => JSON.stringify({ type: "entry", ...row })),
];
writeFileSync(
  new URL("../crates/client/tests/fixtures/timeline-rows.jsonl", import.meta.url),
  `${lines.join("\n")}\n`,
);
console.log(`wrote ${rows.length} threads and ${presentations.length} entries`);
void mkdirSync;
