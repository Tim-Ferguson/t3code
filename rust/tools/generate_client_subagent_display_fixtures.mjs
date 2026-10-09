// Runs the original's subagent wording helpers over many inputs, for `tests/subagent_display_parity.rs`.
//   subagentGroupSummary, summarizeSubagentStatuses, formatSubagentDisplayTitle,
//   subagentDetailPreview                packages/client-runtime/src/state/subagentDisplay.ts
//   plainDetail, GENERIC_CHILD_END       apps/web/src/components/chat/V2LifecycleRow.tsx
//   deriveSubagentElapsedMs              packages/shared/src/orchestrationTiming.ts
//   formatElapsedSeconds                 apps/web/src/components/chat/AgentElapsed.tsx
//   formatModelSlugName                  packages/shared/src/model.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const display = read("packages/client-runtime/src/state/subagentDisplay.ts");
const lifecycle = read("apps/web/src/components/chat/V2LifecycleRow.tsx");
const timing = read("packages/shared/src/orchestrationTiming.ts");
const elapsed = read("apps/web/src/components/chat/AgentElapsed.tsx");
const model = read("packages/shared/src/model.ts");
const body = [
  cut(display, "export function subagentGroupSummary", "/** Match desktop's model resolution"),
  cut(display, "/** Live work leads with progress", null),
  cut(lifecycle, "/** The server's placeholder when a child ends", "function isoOrNull"),
  cut(timing, "export function deriveSubagentElapsedMs", "type LatestRunTiming"),
  cut(elapsed, "function formatElapsedSeconds", "/** Short form for tight rows"),
  cut(model, "export function formatCodexModelName", "export function normalizeModelSlug"),
].join("\n");
const header = `
const isOrchestrationV2WorkActive = (s) => s === "pending" || s === "running" || s === "waiting";
const isTerminalSubagentStatus = (s) => ["completed", "failed", "cancelled", "interrupted"].includes(s);
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-subagent-display-"));
const file = NodePath.join(scratch, "display.mjs");
writeFileSync(
  file,
  `${header}\n${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { subagentGroupSummary, summarizeSubagentStatuses, formatSubagentDisplayTitle, subagentDetailPreview, plainDetail, GENERIC_CHILD_END, deriveSubagentElapsedMs, formatElapsedSeconds, formatModelSlugName };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
const record = (fn, input, output) =>
  rows.push({ fn, input, output: output === undefined ? null : output });
const statuses = [
  "pending",
  "running",
  "waiting",
  "idle",
  "completed",
  "failed",
  "cancelled",
  "interrupted",
];
const lists = [
  [],
  ["running"],
  ["completed"],
  ["completed", "failed"],
  ["running", "completed", "completed"],
  ["idle", "cancelled", "interrupted", "waiting", "pending", "failed"],
  ["failed", "failed"],
  ...statuses.map((s) => [s, "completed"]),
];
for (const list of lists) {
  record("groupSummary", list, original.subagentGroupSummary(list.map((status) => ({ status }))));
  record("statusSummary", list, original.summarizeSubagentStatuses(list));
}
for (const title of [
  "Review src/math.ts",
  "Subagent: Review it",
  "subagent:   /root/explorer",
  "/root/code_reviewer/",
  "/root/a/b/test_runner",
  "/root/",
  "/root/some thing",
  "/root",
  "Subagent: /root/deep/nest_ed_name",
  "",
  "  /root/x_y  ",
  "/root/__a__",
]) {
  record("displayTitle", title, original.formatSubagentDisplayTitle(title));
}
const details = [
  null,
  undefined,
  "",
  "  ",
  "Looks fine.",
  "- one\n- two\n  * three",
  "See [the docs](https://x.y/z) and `code`.",
  "Child task ended with status completed",
  "child task ended with status Failed: boom",
  "line one\n\n   line   two",
  "x".repeat(300),
  "é".repeat(285),
  `${"a ".repeat(139)}b  `,
];
for (const status of statuses) {
  for (const result of details) {
    for (const progress of [undefined, "Working on it", "  "]) {
      const input = { status, result: result ?? null, progress: progress ?? null };
      record(
        "detailPreview",
        input,
        original.subagentDetailPreview({
          status,
          result: result ?? null,
          progress: progress ?? null,
        }),
      );
      const raw = original.subagentDetailPreview === undefined ? null : null;
      void raw;
      const trimmedResult = (result ?? null)?.trim();
      const trimmedProgress = progress?.trim();
      const settled = ["completed", "failed", "cancelled", "interrupted"].includes(status);
      const rawDetail = settled
        ? trimmedResult || trimmedProgress
        : trimmedProgress || trimmedResult;
      record(
        "rowDetail",
        input,
        rawDetail && !original.GENERIC_CHILD_END.test(rawDetail)
          ? original.plainDetail(rawDetail) || null
          : null,
      );
    }
  }
}
const now = Date.parse("2026-10-01T12:00:00.000Z");
for (const status of statuses) {
  for (const startedAt of [
    null,
    "2026-10-01T11:58:00.000Z",
    "2026-10-01T10:00:00.000Z",
    "2026-10-01T12:00:05.000Z",
  ]) {
    for (const completedAt of [null, "2026-10-01T11:59:00.000Z", "2026-10-01T11:00:00.000Z"]) {
      record(
        "elapsedMs",
        { status, startedAt, completedAt, now },
        original.deriveSubagentElapsedMs({ status, startedAt, completedAt }, now),
      );
    }
  }
}
for (const seconds of [0, 1, 59, 59.9, 60, 61, 125, 3599, 3600, 3661, 7325, 86399, 90000, -5]) {
  record("elapsed", seconds, original.formatElapsedSeconds(seconds));
}
for (const slug of [
  "gpt-5.4",
  "gpt-5.4-codex",
  "openai/gpt-5-mini",
  "GPT-4o",
  "claude-opus-4-5",
  "claude-sonnet-4-5-20250929",
  "claude-sonnet-4-20250514",
  "claude-haiku-4-5[1m]",
  "claude-fable-5",
  "gemini-2.5-pro",
  "grok-4",
  "composer-1",
  "some-model",
  "vendor/claude-opus-4-1",
  "o3",
  "gpt-oss",
]) {
  record("modelName", slug, original.formatModelSlugName(slug));
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/subagent-display.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
