// Regenerate with Node 24 from the unchanged reference checkout. No TS runtime is
// used by the port; compact repeat descriptors keep large-output regressions small.
import { createHash } from "node:crypto";
import { writeFile } from "node:fs/promises";
import {
  projectTurnItemForWire,
  projectTurnItemForDetail,
  projectContextHandoffForWire,
} from "../../apps/server/src/orchestration-v2/WireProjection.ts";
const repeat = (text, count) => ({ _repeat: [text, count] });
const concat = (...parts) => ({ _concat: parts });
const expand = (value) =>
  value && typeof value === "object"
    ? Array.isArray(value)
      ? value.map(expand)
      : value._repeat
        ? value._repeat[0].repeat(value._repeat[1])
        : value._concat
          ? value._concat.map(expand).join("")
          : Object.fromEntries(Object.entries(value).map(([k, v]) => [k, expand(v)]))
    : value;
const sort = (value) =>
  value && typeof value === "object"
    ? Array.isArray(value)
      ? value.map(sort)
      : Object.fromEntries(
          Object.keys(value)
            .sort()
            .map((key) => [key, sort(value[key])]),
        )
    : value;
const cases = [];
function add(method, input) {
  const expanded = sort(expand(input));
  const fn = {
    wire: projectTurnItemForWire,
    detail: projectTurnItemForDetail,
    handoff: projectContextHandoffForWire,
  }[method];
  const expected = sort(fn(expanded));
  cases.push({
    method,
    input,
    sha256: createHash("sha256").update(JSON.stringify(expected)).digest("hex"),
  });
}
for (const input of [
  concat(" \t\r\n\n  first\t line  \r\n", repeat("x\n", 10000)),
  repeat(" \n", 10000),
  concat(repeat("a", 160), "\n", repeat("x", 20000)),
  concat(repeat("a", 161), "\n", repeat("x", 20000)),
  concat(repeat("a", 159), "\t b", repeat("x", 20000)),
  concat("  café\u00a0\u2003😀 \t\r\n", repeat("x", 20000)),
  concat("\ufeff first\u0085line\n", repeat("x", 20000)),
  repeat('"', 8191),
  repeat('"', 8192),
  { text: repeat("x", 100000) },
])
  add("wire", { type: "dynamic_tool", input });
for (const output of [
  null,
  "",
  "\ufeff",
  "\u0085",
  { ok: true },
  { data: "private-image" },
  { threadId: "thread-1", messageId: "message-1", response: "private" },
  { structuredContent: { threadId: "first" }, content: "ignored" },
  [{ text: '{"threadId":"first"}' }, { text: '{"threadId":"second","isError":true}' }],
  { isError: true, content: repeat("x", 20000) },
  { error: { message: "private" }, structuredContent: { threadId: "id" } },
  { threadId: "wrong", status: "rolled_back", threads: [{ threadId: "known" }, {}] },
  { threads: [] },
  { thread: { threadId: "nested" }, taskId: "task", scheduledTaskId: "scheduled" },
  {
    htmlRender: {
      attachmentId: "page",
      title: "\ufeff Page ",
      height: 4000,
      heights: [
        [500, 100],
        [320, 45],
      ],
    },
  },
  {
    t3McpApp: {
      attachmentId: "app",
      server: "server",
      tool: "tool",
      resourceUri: "ui://page",
      csp: {
        connectDomains: ["https://example.com", "invalid"],
        frameDomains: ["wss://*.example.com:123"],
      },
      permissions: { camera: {}, unknown: {} },
      prefersBorder: false,
    },
  },
])
  add("wire", { type: "dynamic_tool", input: { pr: 42 }, output });
for (const output of [
  "FILE NOT FOUND",
  "No files found",
  "ENOENT",
  "No such file or directory",
  "CommandNotFoundException",
  "command not found",
  "Cannot find path 'a' because it does not exist",
  "The term 'example' is not recognized",
  "<exited with exit code 12>",
  "exited with exit code 2",
  "exit with exit code 8",
  "exit code: 5",
  "exit code 0",
  "exit code 2abc",
  "exit code2",
  "exit code::2",
  "exit code : 2",
  "exit code : : 2",
  "exit with exit code :2abc",
  "exit with exit code 2abc",
  "\ufeff",
  "\u0085",
  "ordinary output",
])
  add("wire", { type: "command_execution", input: "echo", output, exitCode: 0 });
for (const progress of [
  concat(repeat("a", 32767), "😀", repeat("x", 100000)),
  concat(repeat("a", 32765), "\ufffd", repeat("x", 100000)),
  repeat("🧰", 100000),
])
  for (const method of ["wire", "detail"])
    add(method, { type: "subagent", prompt: progress, progress, result: null });
for (const method of ["wire", "detail"]) {
  add(method, { type: "handoff", summary: "private", contextHandoffId: "h" });
  for (const status of ["completed", "failed"])
    add(method, {
      type: "file_change",
      status,
      diffStr: repeat("x", 40000),
      oldStr: "private",
      newStr: "private",
    });
  add(method, {
    type: "command_execution",
    input: repeat("😀", 100000),
    output: repeat("🧰", 100000),
  });
}
for (const mimeType of [
  "image/png",
  "image/jpeg",
  "image/webp",
  "image/gif",
  "image/svg+xml",
  "IMAGE/PNG",
])
  for (const shape of ["mcp", "anthropic"])
    add("detail", {
      type: "dynamic_tool",
      input: {},
      output: {
        content: [
          { type: "text", text: repeat("x", 300000) },
          shape === "mcp"
            ? { type: "image", mimeType, data: "private-image" }
            : {
                type: "image",
                source: { type: "base64", media_type: mimeType, data: "private-image" },
              },
        ],
      },
    });
add("handoff", {
  id: "handoff",
  history: ["private"],
  delivery: { private: true },
  summaryText: "private",
  status: "ready",
});
await writeFile(
  new URL("../crates/server/tests/fixtures/wire-projection-parity.json", import.meta.url),
  JSON.stringify(cases, null, 2) + "\n",
);
console.log(`${cases.length} source wire projection cases`);
