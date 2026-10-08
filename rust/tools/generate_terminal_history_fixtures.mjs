// Execute only the pure history/filter functions extracted from original Manager.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/server/src/terminal/Manager.ts", import.meta.url),
  "utf8",
);
const start = source.indexOf("export class BoundedTerminalHistory");
const end = source.indexOf("function legacySafeThreadId", start);
const pure =
  "const DEFAULT_HISTORY_BYTE_LIMIT=8*1024*1024;const MAX_HISTORY_CHUNK_LENGTH=16*1024;\n" +
  source
    .slice(start, end)
    .replace(
      "function sanitizeTerminalHistoryChunk(",
      "export function sanitizeTerminalHistoryChunk(",
    );
const { BoundedTerminalHistory, sanitizeTerminalHistoryChunk } = await import(
  "data:text/javascript;base64," +
    Buffer.from(stripTypeScriptTypes(pure, { mode: "strip" })).toString("base64")
);
const sequences = [
  "\x1b[6n",
  "\x1b[12;3R",
  "\x1b[>0;1c",
  "\x1b[?2004$p",
  "\x1b[?2004;1$y",
  "\x1b[!p",
  "\x1b[>q",
  "\x1b[2 q",
  "\x1b[?u",
  "\x1b[u",
  "\x1bP$qm\x1b\\",
  "\x1bP1$r0m\x1b\\",
  "\x1bP+q544e\x07",
  "\x1bPqkeep\x1b\\",
  "\x1b]10;?\x07",
  "\x1b]11;rgb:00/00\x9c",
  "\x1b]0;title\x07",
  "\x1b^message\x1b\\",
  "\x1b_kitty\x1b\\",
  "\x1b(0",
  "\x1b",
  "\x1b[",
  "\x1b]0;unfinished",
  "\x1b[38;2;255;0;0m",
  "\x9b?2004$p",
  "\x90+qcap\x9c",
  "\x9d12;?\x9c",
  "\x9d0;title\x9c",
];
const filters = [];
for (const sequence of sequences) {
  const input = "前😀" + sequence + "tail";
  const chars = Array.from(input);
  for (let i = 0; i <= chars.length; i++) {
    const chunks = [chars.slice(0, i).join(""), chars.slice(i).join("")];
    let pending = "",
      outputs = [];
    for (const chunk of chunks) {
      const result = sanitizeTerminalHistoryChunk(pending, chunk);
      outputs.push(result.visibleText);
      pending = result.pendingControlSequence;
    }
    filters.push({ chunks, outputs, pending });
  }
}
const histories = [];
for (const lines of [0, 1, 2, 3, 20])
  for (const bytes of [0, 1, 3, 4, 5, 17, 32, 128]) {
    const chunks = ["one\n", "two", "\n前😀", "\nlast\n", "é", "\n", "end"];
    const history = new BoundedTerminalHistory(lines, "", bytes);
    const values = [];
    for (const chunk of chunks) {
      history.append(chunk);
      values.push(history.value());
    }
    histories.push({ lines, bytes, chunks, values });
  }
for (const initial of [
  "a".repeat(17000) + "\n" + "😀".repeat(2000) + "\nlast",
  "line\n".repeat(4000),
]) {
  const chunks = [initial, "tail\n"];
  const history = new BoundedTerminalHistory(20, "", 2048);
  const values = [];
  for (const chunk of chunks) {
    history.append(chunk);
    values.push(history.value());
  }
  histories.push({ lines: 20, bytes: 2048, chunks, values });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/terminal-history.json", import.meta.url),
  JSON.stringify({ filters, histories }) + "\n",
);
console.log(`${filters.length}control fixtures, ${histories.length}history fixtures`);
