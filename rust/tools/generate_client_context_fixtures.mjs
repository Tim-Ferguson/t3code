// Runs the original's composer-context record builders over fixed inputs, for
// `tests/composer_context_parity.rs`.
//   sanitizeComposerContextLabel, formatComposerContextReference   packages/shared/src/composerContextReferences.ts
//   toKindScopedComposerContextId                                  apps/web/src/lib/composerContextReferences.ts
//   terminalContextRecord, reviewCommentContextRecord, threadContextRecord, reviewCommentContextLabel
//                                                                  apps/web/src/lib/composerContextRecords.ts
//   formatTerminalContextLabel, normalizeTerminalContextText       apps/web/src/lib/terminalContext.ts
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
const shared = await import("../../packages/shared/src/composerContextReferences.ts");
const records = read("apps/web/src/lib/composerContextRecords.ts");
const terminal = read("apps/web/src/lib/terminalContext.ts");
const idModule = read("apps/web/src/lib/composerContextReferences.ts");
const body = [
  "const COMPOSER_CONTEXT_REVIEW_DIFF_MAX_CHARS = 32000, COMPOSER_CONTEXT_REVIEW_TEXT_MAX_CHARS = 16000;",
  cut(idModule, "const CONTEXT_ID_PATTERN", "export function formatInlineContextReference"),
  cut(
    terminal,
    "export function normalizeTerminalContextText",
    "export function hasTerminalContextText",
  ),
  cut(terminal, "function formatTerminalContextRange", "/** Binds legacy"),
  cut(records, "const TRUNCATION_MARKER", "function previewAnnotationTargetSummary"),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-context-"));
const file = NodePath.join(scratch, "context.mjs");
const header = `import { sanitizeComposerContextLabel } from ${JSON.stringify(pathToFileURL(NodePath.resolve(new URL("../../packages/shared/src/composerContextReferences.ts", import.meta.url).pathname)).href)};\n`;
writeFileSync(
  file,
  `${header}${stripTypeScriptTypes(body.replace(/\bexport /g, "").replace(/type ReviewCommentPresentation = [^;]+;/, ""))}\nexport { toKindScopedComposerContextId, reviewCommentContextLabel, terminalContextRecord, reviewCommentContextRecord, threadContextRecord, formatTerminalContextLabel, normalizeTerminalContextText };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
for (const [label, kind] of [
  ["README.md", "mention"],
  ["a [b] c", "thread"],
  ["  spaced   out\n\ttext ", "terminal"],
  ["", "terminal"],
  ["back\\slash", "thread"],
  ["x".repeat(300), "review-comment"],
  ["[]", "mention"],
  ["tab\tseparated", "skill"],
]) {
  rows.push({
    type: "label",
    label,
    kind,
    output: shared.sanitizeComposerContextLabel(label, kind),
  });
  rows.push({
    type: "reference",
    kind,
    id: "abc_123",
    label,
    output: shared.formatComposerContextReference({ kind, contextId: "abc_123", label }),
  });
}
for (const [kind, id] of [
  ["terminal", "t1"],
  ["review-comment", "comment:src/a.ts:3"],
  ["thread", "thread-a"],
  ["terminal", "has space and ünïcode"],
  ["thread", "x".repeat(200)],
  ["terminal", "terminal_1"],
]) {
  rows.push({ type: "id", kind, id, output: original.toKindScopedComposerContextId(kind, id) });
}
for (const draft of [
  {
    id: "t1",
    terminalId: "default",
    terminalLabel: "Terminal 1",
    lineStart: 3,
    lineEnd: 5,
    text: "ls -la\r\ntotal 8\n\n",
  },
  {
    id: "t2",
    terminalId: "x",
    terminalLabel: "Build",
    lineStart: 7,
    lineEnd: 7,
    text: "\n\nerror: boom\n",
  },
]) {
  rows.push({
    type: "terminal",
    draft,
    label: original.formatTerminalContextLabel(draft),
    record: original.terminalContextRecord(draft),
    text: original.normalizeTerminalContextText(draft.text),
  });
}
const comments = [
  {
    id: "c1",
    sectionId: "turn:1",
    sectionTitle: "Turn 1",
    filePath: "src/lib/a.ts",
    startIndex: 2,
    endIndex: 4,
    rangeLabel: "+12 to +14",
    text: "Rename this",
    diff: "+const a = 1;\n+const b = 2;",
    fenceLanguage: "ts",
  },
  {
    id: "c2",
    sectionId: "turn:1",
    sectionTitle: "Turn 1",
    filePath: "README.md",
    startIndex: 0,
    endIndex: 0,
    rangeLabel: "-3",
    text: "Why removed?",
    diff: "-old line",
  },
  {
    id: "c3",
    sectionId: "pull-request:9",
    sectionTitle: "PR",
    filePath: "PR #9",
    startIndex: 0,
    endIndex: 0,
    rangeLabel: "summary",
    text: "Looks fine",
    diff: "",
  },
  {
    id: "c4",
    sectionId: "turn:2",
    sectionTitle: "Turn 2",
    filePath: "a\\b\\c.rs",
    startIndex: 1,
    endIndex: 1,
    rangeLabel: "+5",
    text: "x".repeat(17000),
    diff: "d".repeat(33000),
    pullRequest: {
      number: 12,
      title: "Fix",
      url: "https://example.com/pr/12",
      headBranch: "f",
      baseBranch: "main",
      state: "open",
      isDraft: false,
    },
  },
];
for (const comment of comments) {
  rows.push({
    type: "review",
    comment,
    label: original.reviewCommentContextLabel(comment),
    record: original.reviewCommentContextRecord(comment),
  });
}
for (const [environmentId, threadId, title] of [
  ["env-1", "thread-a", "Fix the [login] bug"],
  ["env-1", "thread-b", ""],
  ["env-2", "t".repeat(40), "A very long title ".repeat(20)],
]) {
  rows.push({
    type: "thread",
    environmentId,
    threadId,
    title,
    record: original.threadContextRecord({ environmentId, threadId }, title),
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/composer-context.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
