// Development oracle for `server.getTraceDiagnostics`: writes rotated trace files of every shape
// the original's reader must survive (valid spans, failures, interruptions, logs of each level,
// malformed lines, records missing a field) into a throwaway directory, runs the original
// `TraceDiagnostics.read` over it and records the encoded result. The Rust reader replays the
// same files and must produce the same JSON. Run: node rust/tools/generate_trace_diagnostics_fixtures.mjs
import { mkdtempSync, realpathSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { ServerTraceDiagnosticsResult } from "../../packages/contracts/src/index.ts";
import * as TraceDiagnostics from "../../apps/server/src/diagnostics/TraceDiagnostics.ts";

const base = realpathSync(mkdtempSync(join(realpathSync(tmpdir()), "t3-trace-")));
const nano = (ms) => `${BigInt(ms) * 1_000_000n}`;
const T = Date.UTC(2026, 9, 1, 12, 0, 0);
const span = (name, startOffset, durationMs, extra = {}) => ({
  name,
  traceId: `trace-${name}`,
  spanId: `span-${name}-${startOffset}`,
  startTimeUnixNano: nano(T + startOffset),
  endTimeUnixNano: nano(T + startOffset + Math.round(durationMs)),
  durationMs,
  exit: { _tag: "Success" },
  events: [],
  ...extra,
});
const log = (level, message, offset) => ({
  name: message,
  timeUnixNano: nano(T + offset),
  attributes: { "effect.logLevel": level },
});
const files = {
  "server.trace.ndjson.2": [
    span("ws.request", 0, 12),
    span("ws.request", 20, 1500),
    span("db.query", 30, 3.5),
    JSON.stringify(span("db.query", 40, 8)),
    "{not json",
    "[]",
    "null",
    "42",
    JSON.stringify({ name: "no-trace", spanId: "s", durationMs: 1, endTimeUnixNano: nano(T) }),
    JSON.stringify({
      name: "blank",
      traceId: " ",
      spanId: "s",
      durationMs: 1,
      endTimeUnixNano: nano(T),
    }),
    JSON.stringify({
      name: "bad-end",
      traceId: "t",
      spanId: "s",
      durationMs: 1,
      endTimeUnixNano: "soon",
    }),
    JSON.stringify({
      name: "bad-duration",
      traceId: "t",
      spanId: "s",
      durationMs: "5",
      endTimeUnixNano: nano(T),
    }),
    "",
    "   ",
  ],
  "server.trace.ndjson.1": [
    span("ws.request", 100, 30, { exit: { _tag: "Failure", cause: "  boom  " } }),
    span("ws.request", 110, 40, { exit: { _tag: "Failure", cause: "boom" } }),
    span("ws.request", 120, 50, { exit: { _tag: "Failure" } }),
    span("ws.request", 130, 60, { exit: { _tag: "Failure", cause: "   " } }),
    span("git.status", 140, 2500, { exit: { _tag: "Interrupted" } }),
    span("git.status", 150, 999.9),
    span("git.status", 160, 1000),
    span("provider.turn", 170, 20, {
      events: [
        log("Info", "hello", 171),
        log("Warning", "careful", 172),
        log("Error", "bad thing", 173),
        log("FATAL", "dead", 174),
        log("warn", "lower", 175),
        log("Debug", "d", 176),
        { name: "no level", attributes: {} },
        { name: " ", attributes: { "effect.logLevel": "Error" } },
        { name: "no time", attributes: { "effect.logLevel": "Error" } },
        "x",
        null,
        { name: "blank level", attributes: { "effect.logLevel": "  " } },
      ],
    }),
    span("late", 180, 5, { startTimeUnixNano: undefined }),
    span("early", -5000, 5),
  ],
  "server.trace.ndjson": [
    ...Array.from({ length: 14 }, (_, index) =>
      span(`bulk-${index}`, 200 + index, 100 + index * 7),
    ),
    ...Array.from({ length: 25 }, (_, index) =>
      span("many", 300 + index, 10 + (index % 5), {
        exit: index % 2 ? { _tag: "Failure", cause: `cause ${index % 3}` } : { _tag: "Success" },
        events: [log("Warning", `warn ${index}`, 300 + index)],
      }),
    ),
    span("tie-a", 400, 50),
    span("tie-b", 410, 50),
    span("tie-c", 420, 50),
  ],
};
const written = [];
for (const [name, lines] of Object.entries(files)) {
  const text =
    lines
      .map((line) => (typeof line === "string" ? line : JSON.stringify(line)))
      .join(name.endsWith(".1") ? "\r\n" : "\n") + "\n";
  writeFileSync(join(base, name), text);
  written.push({ name, hex: Buffer.from(text).toString("hex") });
}
const encode = Schema.encodeSync(ServerTraceDiagnosticsResult);
const readAt = DateTime.makeUnsafe(Date.UTC(2026, 9, 9, 0, 0, 0));
const read = (maxFiles, traceFilePath = join(base, "server.trace.ndjson")) =>
  Effect.runPromise(
    TraceDiagnostics.readTraceDiagnostics({ traceFilePath, maxFiles, readAt }).pipe(
      Effect.provide(TraceDiagnostics.layer),
      Effect.provide(NodeServices.layer),
    ),
  ).then((result) => JSON.parse(JSON.stringify(encode(result)).split(base).join("{base}")));
const cases = [
  { name: "all-files", maxFiles: 10, file: "server.trace.ndjson", result: await read(10) },
  { name: "backups-skipped", maxFiles: 0, file: "server.trace.ndjson", result: await read(0) },
  { name: "one-backup", maxFiles: 1, file: "server.trace.ndjson", result: await read(1) },
  {
    name: "missing",
    maxFiles: 3,
    file: "absent.ndjson",
    result: await read(3, join(base, "absent.ndjson")),
  },
];
const target = new URL("../crates/server/tests/fixtures/trace-diagnostics.json", import.meta.url);
writeFileSync(new URL(target.href + ".tmp"), JSON.stringify({ files: written, cases }, null, 1));
renameSync(new URL(target.href + ".tmp"), target);
rmSync(base, { recursive: true, force: true });
console.log(JSON.stringify({ files: written.length, cases: cases.length }));
