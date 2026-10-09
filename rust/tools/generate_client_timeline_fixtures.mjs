// Runs the original's pure timeline helpers over many inputs and records what they answer, for
// the Rust tests in t3-client (`tests/timeline_parity.rs`). The functions are cut out of the
// original sources unchanged (those files import React, effect and contract types that a plain
// script does not need); only `DateTime.formatIso` is stood in for, since the inputs are strings.
//   providerErrorPresentation        apps/web/src/session-logic.ts
//   proposedPlan*                    apps/web/src/proposedPlan.ts
//   formatContextWindowTokens,
//   deriveLatestContextWindowSnapshot apps/web/src/lib/contextWindow.ts
//   formatPercentage                 apps/web/src/components/chat/ContextWindowMeter.tsx
//   formatContextWindowCost,
//   formatContextWindowCompactionMessage apps/web/src/components/chat/ContextWindowMeter.logic.ts
//   contextCompactionLabel, liveThoughtLine packages/client-runtime/src/work-log/presentation.ts
//   formatTokens                     packages/shared/src/usageFormat.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const root = "../../";
const session = read(`${root}apps/web/src/session-logic.ts`);
const plan = read(`${root}apps/web/src/proposedPlan.ts`);
const window_ = read(`${root}apps/web/src/lib/contextWindow.ts`);
const meter = read(`${root}apps/web/src/components/chat/ContextWindowMeter.tsx`);
const meterLogic = read(`${root}apps/web/src/components/chat/ContextWindowMeter.logic.ts`);
const presentation = read(`${root}packages/client-runtime/src/work-log/presentation.ts`);
const usage = read(`${root}packages/shared/src/usageFormat.ts`);

const body = [
  cut(session, "export function providerErrorPresentation(", "function projectedWorkEntry("),
  cut(plan, "export function proposedPlanTitle", "export function downloadPlanAsTextFile"),
  cut(window_, "function asFiniteNumber", "export function formatContextWindowTokens").replace(
    /^type NullableContextWindowUsage[\s\S]*?^};\n/m,
    "",
  ),
  cut(window_, "export function formatContextWindowTokens", null),
  cut(meter, "function formatPercentage", "export function ContextWindowMeter("),
  cut(
    meterLogic,
    "export function formatContextWindowCompactionMessage",
    "/**\n * Whether the footer should hold",
  ),
  cut(meterLogic, "export function formatContextWindowCost", null),
  cut(
    presentation,
    "export function contextCompactionLabel(",
    "export interface WorkLogPresentationEntry",
  ),
  cut(presentation, "export function liveThoughtLine", null),
  cut(usage, "export function formatTokens", "function trim("),
  cut(usage, "function trim(", "\n}\n") + "\n}\n",
].join("\n");
const header = `
const INTEGER = new Intl.NumberFormat("en-US");
const DateTime = { formatIso: (value) => value };
`;
const exportsList = [
  "providerErrorPresentation",
  "proposedPlanTitle",
  "stripDisplayedPlanMarkdown",
  "buildCollapsedProposedPlanPreviewMarkdown",
  "buildProposedPlanMarkdownFilename",
  "buildPlanImplementationPrompt",
  "buildPlanImplementationThreadTitle",
  "resolvePlanFollowUpSubmission",
  "normalizePlanMarkdownForExport",
  "deriveLatestContextWindowSnapshot",
  "formatContextWindowTokens",
  "formatPercentage",
  "formatContextWindowCompactionMessage",
  "formatContextWindowCost",
  "contextCompactionLabel",
  "liveThoughtLine",
  "formatTokens",
];
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-timeline-client-"));
const file = NodePath.join(scratch, "timeline.mjs");
writeFileSync(
  file,
  `${header}\n${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { ${exportsList.join(", ")} };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
const record = (fn, input, output) =>
  rows.push({ fn, input, output: output === undefined ? null : output });

// Provider errors: every status and retry shape, with and without a title.
const failure = (cls, message, extra = {}) => ({ class: cls, message, ...extra });
for (const status of [
  "running",
  "completed",
  "failed",
  "interrupted",
  "cancelled",
  "pending",
  "waiting",
]) {
  for (const cls of ["usage_limit", "provider", "transport"]) {
    for (const title of [undefined, null, "  Custom title  ", ""]) {
      for (const retry of [
        undefined,
        { attempt: 2, maxAttempts: 5, retryDelayMs: null },
        { attempt: 3, maxAttempts: null, retryDelayMs: 1500 },
        { attempt: 1, maxAttempts: 3, retryDelayMs: 250 },
        { attempt: 1, maxAttempts: 3, retryDelayMs: 4000 },
        { attempt: 1, maxAttempts: 3, retryDelayMs: 0 },
      ]) {
        const item = {
          type: "error",
          status,
          failure: failure(cls, "It broke."),
          ...(title === undefined ? {} : { title }),
          ...(retry === undefined ? {} : { retry }),
        };
        record("errorPresentation", item, original.providerErrorPresentation(item));
      }
    }
  }
}

// Proposed plans.
const plans = [
  "# Plan\n\nDo it.",
  "## Summary\n\nShort.",
  "# Title\n\n## Summary\n\nText\n\n- a\n- b",
  "No heading at all",
  "",
  "   \n\n# Spaced\n\nbody",
  "#  Heading with `code` \n\n1. one\n2. two",
  `# Long\n\n${Array.from({ length: 30 }, (_, i) => `- step ${i}`).join("\n")}`,
  `# Long\n\n${Array.from({ length: 12 }, (_, i) => `line ${i}\n`).join("\n")}`,
  "# Only heading",
  "####### seven hashes\ntext",
  "Intro\n\n# Later heading\n",
];
for (const markdown of plans) {
  record("planTitle", markdown, original.proposedPlanTitle(markdown));
  record("planBody", markdown, original.stripDisplayedPlanMarkdown(markdown));
  record(
    "planPreview",
    markdown,
    original.buildCollapsedProposedPlanPreviewMarkdown(markdown, { maxLines: 10 }),
  );
  record("planPreview8", markdown, original.buildCollapsedProposedPlanPreviewMarkdown(markdown));
  record("planFilename", markdown, original.buildProposedPlanMarkdownFilename(markdown));
  record("planImplementationPrompt", markdown, original.buildPlanImplementationPrompt(markdown));
  record("planThreadTitle", markdown, original.buildPlanImplementationThreadTitle(markdown));
  record("planExport", markdown, original.normalizePlanMarkdownForExport(markdown));
  for (const draftText of ["", "  ", "More tests please "]) {
    record(
      "planFollowUp",
      { draftText, planMarkdown: markdown },
      original.resolvePlanFollowUpSubmission({ draftText, planMarkdown: markdown }),
    );
  }
}

// Token formatting.
for (const value of [
  0, 1, 7, 999, 1000, 1234, 9999, 10000, 12345, 99999, 100000, 999999, 1000000, 1250000, 12500000,
  19900000000, 76700000, 804000, 123456789012, 999.6, 1e12, 0.4, -5, -1500,
]) {
  record("formatTokens", value, original.formatTokens(value));
}
for (const value of [
  null,
  0,
  1,
  999,
  1000,
  1049,
  1050,
  9999,
  10000,
  128000,
  200000,
  999999,
  1000000,
  1250000,
  10000000,
  Number.NaN,
  0.5,
]) {
  record(
    "contextTokens",
    Number.isNaN(value) ? "NaN" : value,
    original.formatContextWindowTokens(Number.isNaN(value) ? Number.NaN : value),
  );
}
for (const value of [null, 0, 0.04, 0.5, 5, 9.94, 9.96, 10, 42.4, 42.6, 90, 100, Number.NaN]) {
  record("percentage", Number.isNaN(value) ? "NaN" : value, original.formatPercentage(value));
}
for (const cost of [
  { amount: 0, currency: "USD" },
  { amount: 0.004, currency: "USD" },
  { amount: 0.01, currency: "USD" },
  { amount: 1.239, currency: "EUR" },
  { amount: -0.002, currency: "USD" },
  { amount: 12, currency: "USD" },
]) {
  record("cost", cost, original.formatContextWindowCost(cost));
}
for (const [name, threshold] of [
  ["GPT-5", null],
  [null, null],
  ["Opus", 150000],
  ["Opus", 0],
  [undefined, 1234567],
]) {
  record(
    "compactionMessage",
    { name: name ?? null, threshold },
    original.formatContextWindowCompactionMessage(name ?? undefined, threshold),
  );
}

// Context window snapshots, from live usage, the provider thread's report, or a compaction.
const live = (extra) => ({
  usedTokens: 50000,
  maxTokens: 200000,
  updatedAt: "2026-10-01T10:00:00.000Z",
  ...extra,
});
const thread = (usage) => ({ contextUsage: usage, updatedAt: "2026-10-01T11:00:00.000Z" });
const compaction = (before, after) => ({
  item: {
    type: "compaction",
    beforeTokenCount: before,
    afterTokenCount: after,
    startedAt: "2026-10-01T09:00:00.000Z",
    updatedAt: "2026-10-01T09:30:00.000Z",
  },
});
const snapshots = [
  { entries: [], live: live(), thread: null },
  { entries: [], live: live({ maxTokens: null }), thread: null },
  { entries: [], live: live({ usedTokens: 250000 }), thread: null },
  {
    entries: [],
    live: live({
      usedTokens: -4,
      inputTokens: 10,
      cachedInputTokens: 2,
      outputTokens: 3,
      reasoningOutputTokens: 1,
    }),
    thread: null,
  },
  {
    entries: [],
    live: null,
    thread: thread({
      usedTokens: 9000,
      maxTokens: 100000,
      totalProcessedTokens: 400000,
      cost: { amount: 0.5, currency: "USD" },
      compactsAutomatically: true,
      autoCompactThreshold: 80000,
      toolUses: 3,
      durationMs: 1000,
    }),
  },
  { entries: [], live: null, thread: thread({ usedTokens: 9000 }) },
  { entries: [], live: null, thread: thread({ usedTokens: 9000, maxTokens: 0 }) },
  { entries: [compaction(100000, 20000)], live: null, thread: null },
  {
    entries: [compaction(100000, 20000), compaction(undefined, undefined)],
    live: null,
    thread: null,
  },
  { entries: [compaction(undefined, -1)], live: null, thread: null },
  { entries: [], live: null, thread: null },
  { entries: [compaction(10, 5)], live: live(), thread: thread({ usedTokens: 1 }) },
];
for (const { entries, live: usageLive, thread: providerThread } of snapshots) {
  record(
    "snapshot",
    { entries, live: usageLive, thread: providerThread },
    original.deriveLatestContextWindowSnapshot(entries, usageLive, providerThread),
  );
}

// Compaction labels and the live thought line.
for (const status of [
  "running",
  "completed",
  "failed",
  "pending",
  "waiting",
  "cancelled",
  "interrupted",
]) {
  for (const [before, after] of [
    [undefined, undefined],
    [100000, 20000],
    [1200, 800],
    [5, undefined],
  ]) {
    const item = {
      status,
      ...(before === undefined ? {} : { beforeTokenCount: before }),
      ...(after === undefined ? {} : { afterTokenCount: after }),
    };
    record("compactionLabel", item, original.contextCompactionLabel(item));
  }
}
for (const text of [
  "**Planning the change**\nThen more text.",
  "First sentence. Second sentence.",
  "No punctuation at all",
  "  # Heading\n- item one\n- item two",
  "Look at [the docs](https://example.com) now? Yes.",
  "`code` and **bold** and ~~strike~~ text!",
  'He said "stop." Then left.',
  "",
  "   ",
  "**Only heading**",
  "_italic_ start. next",
  "1. numbered first. then",
  "Wait (really.) ok",
  "A very long thought that never ends and has no stop",
]) {
  record("liveThought", text, original.liveThoughtLine(text));
}

writeFileSync(
  new URL("../crates/client/tests/fixtures/timeline.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows`);
