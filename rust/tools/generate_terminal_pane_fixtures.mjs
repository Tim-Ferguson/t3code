// Unchanged original transition helpers; no application TypeScript runtime.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/web/src/terminalUiStateStore.ts", import.meta.url),
  "utf8",
);
const first = source.indexOf("function normalizeTerminalIds("),
  last = source.indexOf("export function selectThreadTerminalUiState(");
if (first < 0 || last < 0) throw Error("terminal helper boundary changed");
const functions = [
  "normalizeThreadTerminalUiState",
  "setThreadTerminalOpen",
  "setThreadTerminalHeight",
  "splitThreadTerminal",
  "newThreadTerminal",
  "setThreadActiveTerminal",
  "closeThreadTerminal",
  "reconcileThreadTerminalSessionIds",
];
const helpers = new Function(
  'const DEFAULT_THREAD_TERMINAL_HEIGHT=280,DEFAULT_THREAD_TERMINAL_ID="term-1",MAX_TERMINALS_PER_GROUP=4;' +
    stripTypeScriptTypes(source.slice(first, last)) +
    `;return {${functions.join(",")}};`,
)();
const seed = {
  terminalOpen: false,
  terminalHeight: 280,
  terminalIds: [],
  activeTerminalId: "",
  terminalGroups: [],
  activeTerminalGroupId: "",
};
const rows = [];
const initial = [
  seed,
  {
    ...seed,
    terminalOpen: true,
    terminalHeight: -1,
    terminalIds: [" a ", "a", "\ufeffb\ufeff", "\u0085", ""],
    activeTerminalId: "missing",
    terminalGroups: [
      { id: " shared ", terminalIds: [" a ", "missing"], splitDirection: "vertical" },
      { id: "shared", terminalIds: ["a", "b"] },
      { id: "", terminalIds: ["\u0085"] },
      { id: "ignored", terminalIds: [] },
    ],
    activeTerminalGroupId: "shared",
  },
  {
    ...seed,
    terminalIds: ["a", "b", "c", "d", "e"],
    activeTerminalId: "d",
    terminalGroups: [
      { id: "g", terminalIds: ["a", "b", "c", "d"] },
      { id: "h", terminalIds: ["e"] },
    ],
    activeTerminalGroupId: "g",
  },
];
const operations = [
  { type: "normalize" },
  { type: "open", value: true },
  { type: "height", value: 450 },
  { type: "new", id: "a" },
  { type: "split", id: "b", direction: "horizontal" },
  { type: "split", id: "c", direction: "vertical" },
  { type: "split", id: "d", direction: "horizontal" },
  { type: "split", id: "e", direction: "vertical" },
  { type: "new", id: "e" },
  { type: "split", id: "e", direction: "vertical" },
  { type: "active", id: "a" },
  { type: "close", id: "b" },
  { type: "close", id: "unknown" },
  { type: "open", value: false },
  { type: "reconcile", ids: ["e", "a", "server"] },
  { type: "active", id: "server" },
  { type: "close", id: "server" },
  { type: "close", id: "e" },
  { type: "close", id: "a" },
  { type: "open", value: true },
];
function run(state, op) {
  switch (op.type) {
    case "normalize":
      return helpers.normalizeThreadTerminalUiState(state);
    case "open":
      return helpers.setThreadTerminalOpen(state, op.value);
    case "height":
      return helpers.setThreadTerminalHeight(state, op.value);
    case "new":
      return helpers.newThreadTerminal(state, op.id);
    case "split":
      return helpers.splitThreadTerminal(state, op.id, op.direction);
    case "active":
      return helpers.setThreadActiveTerminal(state, op.id);
    case "close":
      return helpers.closeThreadTerminal(state, op.id);
    case "reconcile":
      return helpers.reconcileThreadTerminalSessionIds(state, op.ids);
    default:
      throw Error(op.type);
  }
}
for (const state of initial) {
  let current = state;
  rows.push({
    kind: "sequence",
    state,
    operations,
    expected: operations.map((op) => (current = run(current, op))),
  });
}
for (const state of initial)
  for (const type of ["new", "split", "active", "close"])
    for (const id of ["a", "e", "term-1", "missing", " ", "\ufeff", " raw ", "\u0085"])
      for (const direction of ["horizontal", "vertical"]) {
        const operation = { type, id, direction };
        rows.push({ kind: "single", state, operation, expected: run(state, operation) });
      }
for (const state of initial)
  for (const ids of [[], ["a"], ["e", "a", "server"], [" raw ", "raw", "\ufeffb\ufeff", "b", ""]]) {
    const operation = { type: "reconcile", ids };
    rows.push({ kind: "single", state, operation, expected: run(state, operation) });
  }
writeFileSync(
  new URL("../crates/client/tests/fixtures/terminal-panes.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original pane witnesses`);
const labelsSource = readFileSync(
  new URL("../../packages/shared/src/terminalLabels.ts", import.meta.url),
  "utf8",
)
  .replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "")
  .replace(/^export /gm, "");
const labels = new Function(
  stripTypeScriptTypes(labelsSource) +
    ";return {getTerminalLabel,resolveTerminalSessionLabel,nextTerminalId};",
)();
const labelsRows = [];
const ids = [
  "term-1",
  "terminal-001",
  "TERM-2",
  "term-3-abcdef01-abcd-abcd-abcd-abcdefabcdef",
  "term-3-extra",
  "term-١",
  "raw",
  "\ufeffterm-1",
  "term-01",
  "term-99",
];
for (const id of ids)
  for (const summary of [null, "", "  Shell  ", "\ufeffname\ufeff", "\u0085", "\ufeff"])
    labelsRows.push({
      kind: "label",
      id,
      summary,
      expected: labels.resolveTerminalSessionLabel(
        id,
        summary === null ? null : { label: summary },
      ),
    });
for (const ids of [
  [],
  ["term-1"],
  ["term-01"],
  ["term-1", "TERM-2", "terminal-3"],
  ["raw", "term-2", "term-1-abcdef01-abcd-abcd-abcd-abcdefabcdef"],
])
  for (const suffix of [null, "", "00000000-abcd-abcd-abcd-000000000000"])
    labelsRows.push({
      kind: "next",
      ids,
      suffix,
      expected: labels.nextTerminalId(ids, suffix === null ? undefined : suffix),
    });
writeFileSync(
  new URL("../crates/client/tests/fixtures/terminal-labels.jsonl", import.meta.url),
  labelsRows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
