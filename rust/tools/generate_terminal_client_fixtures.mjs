// Execute original shared terminal helpers unchanged; development oracle only.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const root = new URL("../../", import.meta.url);
const source = (name) =>
  readFileSync(new URL(`packages/client-runtime/src/state/${name}.ts`, root), "utf8");
function compile(text, exports, injected = {}) {
  const code = text
    .replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "")
    .replace(/export\s*\{[\s\S]*?\}(?:\s*from\s+"[^"\n]+")?;/g, "")
    .replace(/^export /gm, "");
  return new Function(
    ...Object.keys(injected),
    stripTypeScriptTypes(code) + `\nreturn {${exports.join(",")}};`,
  )(...Object.values(injected));
}
const output = compile(source("terminalOutput"), [
  "EMPTY_TERMINAL_OUTPUT_STATE",
  "INITIAL_TERMINAL_OUTPUT_CURSOR",
  "appendOutput",
  "resetOutput",
  "readTerminalOutputUpdate",
  "terminalOutputText",
  "DEFAULT_MAX_TERMINAL_BUFFER_BYTES",
]);
const sessions = compile(
  source("terminalSession"),
  [
    "EMPTY_TERMINAL_BUFFER_STATE",
    "applyTerminalAttachStreamEvent",
    "combineTerminalSessionState",
    "applyTerminalMetadataStreamEvent",
  ],
  output,
);
const rows = [];
const strings = [
  "",
  "abc",
  "é🙂\r\n",
  "\ufeffBOM",
  "x\ufeff🙂end",
  "\x1b[31mRED\x1b[0m",
  "汉字\r\n",
  "a".repeat(16383) + "🙂\ufeffZ",
  "🙂".repeat(4100),
];
for (const budget of [-1, 0, 1, 2, 3, 4, 5, 8, 16, 16384, 524288])
  for (const data of strings) {
    let state = { ...output.EMPTY_TERMINAL_OUTPUT_STATE, generation: 7 },
      cursor = output.INITIAL_TERMINAL_OUTPUT_CURSOR;
    const operations = [
      { type: "reset", data },
      { type: "append", data: "\ufeff🙂tail" },
      { type: "append", data: "" },
      { type: "append", data: "é\n" },
      { type: "reset", data: "new\r\n" },
      { type: "append", data: "more" },
    ];
    const expected = [];
    for (const operation of operations) {
      state = (operation.type === "reset" ? output.resetOutput : output.appendOutput)(
        state,
        operation.data,
        budget,
      );
      const update = output.readTerminalOutputUpdate(state, cursor);
      cursor = update.cursor;
      expected.push({ state, update, text: output.terminalOutputText(state) });
    }
    rows.push({ kind: "output", budget, operations, expected });
  }
// Compaction, stale readers and reinstalled stream generations are independent of text size.
for (const budget of [64, 524288]) {
  let state = { ...output.EMPTY_TERMINAL_OUTPUT_STATE, generation: 8 };
  for (let i = 0; i < 1026; i++) state = output.appendOutput(state, i % 2 ? "🙂" : "a", budget);
  const cursors = [
    output.INITIAL_TERMINAL_OUTPUT_CURSOR,
    { generation: 8, resetVersion: 0, offset: 0 },
    { generation: 8, resetVersion: 0, offset: state.nextOffset - 2 },
    { generation: 8, resetVersion: 0, offset: state.nextOffset },
    { generation: 7, resetVersion: 0, offset: state.nextOffset },
  ];
  rows.push({
    kind: "read",
    state,
    cursors,
    expected: cursors.map((c) => output.readTerminalOutputUpdate(state, c)),
  });
}
const snapshot = {
  threadId: "thread",
  terminalId: "term-1",
  cwd: "/tmp",
  worktreePath: null,
  status: "running",
  pid: 42,
  exitCode: null,
  exitSignal: null,
  history: "shell🙂\r\n",
  label: "Shell",
  updatedAt: "2026-10-08T12:00:00Z",
};
const base = { threadId: "thread", terminalId: "term-1", sequence: 1 };
const events = [
  { type: "snapshot", snapshot },
  { type: "output", ...base, data: "\x1b[32mhello\x1b[0m" },
  { type: "output", ...base, data: "" },
  { type: "activity", ...base, hasRunningSubprocess: true, label: "build" },
  { type: "exited", ...base, exitCode: 0, exitSignal: null },
  { type: "output", ...base, data: "late" },
  { type: "error", ...base, message: "failed" },
  { type: "cleared", ...base },
  { type: "closed", ...base },
  { type: "output", ...base, data: "new" },
  { type: "snapshot", snapshot },
  { type: "restarted", ...base, snapshot },
];
for (const budget of [0, 1, 8, 524288]) {
  let state = {
    ...sessions.EMPTY_TERMINAL_BUFFER_STATE,
    output: { ...output.EMPTY_TERMINAL_OUTPUT_STATE, generation: 9 },
  };
  const expected = events.map(
    (event) => (state = sessions.applyTerminalAttachStreamEvent(state, event, budget)),
  );
  rows.push({ kind: "session", budget, events, expected });
}
const { history, ...summary } = snapshot;
summary.hasRunningSubprocess = false;
for (const status of ["starting", "running", "exited", "error", "closed"])
  for (const version of [0, 1, 3])
    for (const left of [null, "2026-10-08T12:00:00Z", "2026-10-08T14:00:00+02:00", "invalid"])
      for (const right of [null, "2026-10-08T12:01:00Z", "2026-10-08T11:00:00Z", "invalid"]) {
        const current = {
          ...sessions.EMPTY_TERMINAL_BUFFER_STATE,
          status,
          version,
          updatedAt: right,
        };
        const metadata =
          left === null
            ? null
            : { ...summary, updatedAt: left, hasRunningSubprocess: version === 3 };
        rows.push({
          kind: "combine",
          summary: metadata,
          buffer: current,
          expected: sessions.combineTerminalSessionState(metadata, current),
        });
      }
let metadata = [];
const metadataEvents = [
  { type: "snapshot", terminals: [summary, { ...summary, terminalId: "term-2" }] },
  { type: "upsert", terminal: { ...summary, label: "updated" } },
  { type: "remove", threadId: "another", terminalId: "term-2" },
  { type: "remove", threadId: "thread", terminalId: "term-2" },
  { type: "snapshot", terminals: [] },
];
rows.push({
  kind: "metadata",
  events: metadataEvents,
  expected: metadataEvents.map(
    (event) => (metadata = sessions.applyTerminalMetadataStreamEvent(metadata, event)),
  ),
});
writeFileSync(
  new URL("../crates/client/tests/fixtures/terminal-client.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original terminal client witnesses`);
