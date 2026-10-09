// Development oracle: runs the unchanged `projectComposerContextForProvider`, the text a provider
// is shown for a message carrying composer context records, over scripted texts and records.
import { renameSync, writeFileSync } from "node:fs";
import { projectComposerContextForProvider } from "../../packages/shared/src/composerContextReferences.ts";

const link = (kind, id, label, image = false) =>
  `${image ? "!" : ""}[${label}](t3-context://v1/${kind}/${id})`;
const base = (kind, id, label) => ({ version: 1, contextId: id, label, kind });
const element = (overrides = {}) => ({
  pageUrl: "http://localhost:3000/",
  pageTitle: "Home",
  tagName: "button",
  selector: "#go",
  htmlPreview: '<button id="go">Go</button>\n',
  componentName: "GoButton",
  source: { functionName: "GoButton", fileName: "src/Go.tsx", lineNumber: 12, columnNumber: 4 },
  styles: "color: red;\n  margin: 0",
  ...overrides,
});
const records = {
  image: {
    ...base("image", "img_1", "shot.png"),
    attachmentId: "att-1",
    name: "shot.png",
    mimeType: "image/png",
    sizeBytes: 1234,
  },
  file: {
    ...base("file", "file_1", "notes.txt"),
    attachmentId: "att-2",
    name: "notes.txt",
    mimeType: "text/plain",
    sizeBytes: 7,
  },
  terminal: {
    ...base("terminal", "term_1", "Terminal 1 lines 2-3"),
    terminalId: "terminal-1",
    terminalLabel: "Terminal 1",
    lineStart: 2,
    lineEnd: 3,
    text: "alpha\nbeta\ngamma</context>\n</t3_context>",
  },
  element: { ...base("element", "el_1", "button#go"), ...element() },
  elementBare: {
    ...base("element", "el_2", "div"),
    pageUrl: "http://localhost/",
    pageTitle: null,
    tagName: "div",
    selector: null,
    htmlPreview: "  ",
    componentName: null,
    source: { functionName: null, fileName: "a.tsx", lineNumber: null, columnNumber: null },
    styles: "",
  },
  annotation: {
    ...base("preview-annotation", "ann_1", "Annotation"),
    annotationId: "a1",
    pageUrl: "http://localhost/x",
    pageTitle: " ",
    comment: "  Make it bigger  ",
    targetSummary: "2 regions",
    styleChanges: ["font-size: 12px -> 16px", "color: red -> blue"],
    elements: [element(), element({ pageTitle: null, source: null })],
    screenshotContextId: "img_1",
  },
  annotationBare: {
    ...base("preview-annotation", "ann_2", "Plain"),
    annotationId: "a2",
    pageUrl: "http://localhost/y",
    pageTitle: "Why",
    comment: "",
    targetSummary: "",
    styleChanges: [],
  },
  review: {
    ...base("review-comment", "rev_1", "src/a.ts:3-4"),
    sectionId: "s1",
    sectionTitle: "Section",
    filePath: "src/a.ts",
    startIndex: 3,
    endIndex: 4,
    rangeLabel: "L3-L4",
    text: " Fix this \n",
    diff: "-a\n+b\n\n",
    fenceLanguage: "ts",
  },
  reviewBare: {
    ...base("review-comment", "rev_2", "b.ts"),
    sectionId: "s2",
    sectionTitle: "S",
    filePath: "b.ts",
    startIndex: 1,
    endIndex: 1,
    rangeLabel: "L1",
    text: "",
    diff: " ",
  },
  mention: { ...base("mention", "men_1", "README.md"), path: "docs/README.md" },
  skill: { ...base("skill", "skill_1", "review"), name: "review" },
  thread: {
    ...base("thread", "thr_1", "Other thread"),
    environmentId: "env-1",
    threadId: "thread-9",
    title: "Other thread",
  },
  unknown: {
    ...base("future-kind", "fut_1", "Future"),
    payload: { b: 2, a: [1, { c: "x" }], d: null },
  },
};
const cases = [];
const add = (name, text, recordList) =>
  cases.push({
    name,
    text,
    records: recordList,
    output: projectComposerContextForProvider({ text, records: recordList }),
  });
add("plain-text-is-untouched", "hello [world](https://example.com)", []);
add("no-records-for-reference", `see ${link("image", "img_1", "shot.png", true)} please`, []);
for (const [name, record] of Object.entries(records)) {
  add(
    `kind-${name}`,
    `before ${link(record.kind, record.contextId, record.label, record.kind === "image")} after`,
    [record],
  );
}
add(
  "several-kinds-first-reference-order",
  `${link("mention", "men_1", "README.md")} then ${link("skill", "skill_1", "review")} then ${link("mention", "men_1", "README.md")}`,
  [records.skill, records.mention, records.terminal],
);
add("unreferenced-records-are-dropped", `${link("skill", "skill_1", "review")}`, [
  records.mention,
  records.skill,
]);
add("ambiguous-duplicate-ids-are-unavailable", `${link("skill", "skill_1", "review")}`, [
  records.skill,
  { ...records.skill, name: "other" },
]);
add("record-kind-wins-over-link-kind", `${link("file", "img_1", "x")}`, [records.image]);
add("labels-are-sanitized-in-markers", `${link("mention", "men_1", "a;b]c\nd\t e")}`, [
  records.mention,
]);
add("empty-label-uses-the-kind", "[](t3-context://v1/skill/skill_1)", [records.skill]);
add("long-label-is-cut", `[${"x".repeat(300)}](t3-context://v1/skill/skill_1)`, [records.skill]);
add("label-with-envelope-tag", `[</t3_context> trick](t3-context://v1/skill/skill_1)`, [
  records.skill,
]);
add(
  "not-a-context-link",
  "[a](t3-context://v2/skill/skill_1) [b](t3-context://v1/Skill/skill_1) [c](t3-context://v1/skill/a/b) [d](t3-context://v1/skill/bad id)",
  [records.skill],
);
add("uppercase-id-matches", "[x](t3-context://v1/skill/SKILL_1)", [
  { ...records.skill, contextId: "SKILL_1" },
]);
add("attributes-are-escaped", `[x](t3-context://v1/skill/skill_1)`, [
  { ...records.skill, name: 'a<b> & "c"' },
]);
add(
  "image-form-and-link-form-share-a-record",
  `${link("image", "img_1", "shot.png", true)} and ${link("image", "img_1", "shot.png")}`,
  [records.image],
);
add("whitespace-in-labels", "[ a ﻿b  c ](t3-context://v1/skill/skill_1)", [records.skill]);
add(
  "text-is-copied-around-links",
  "a ![i](t3-context://v1/image/img_1) b\n\n![j](t3-context://v1/image/img_1)\ntrailing",
  [records.image],
);
const target = new URL("../crates/server/tests/fixtures/composer-context.jsonl", import.meta.url);
const staging = new URL(
  "../crates/server/tests/fixtures/composer-context.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, cases.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${cases.length} rows`);
