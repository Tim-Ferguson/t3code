// Runs the unchanged provider event logger (apps/server/src/provider/EventNdjsonLogger.ts) over fixed events and directories and records
// what it filters, bounds, writes, rotates and deletes, for the Rust port (`provider_event_log.rs`). Timestamps are masked.
//   node rust/tools/generate_provider_event_log_fixtures.mjs          writes crates/server/tests/fixtures/provider-event-log.jsonl
//   node rust/tools/generate_provider_event_log_fixtures.mjs --check  compares with the file
import * as NodeFSP from "node:fs/promises";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  boundProviderEventForLogging,
  makeEventNdjsonLogStore,
  shouldPersistProviderEvent,
} from "../../apps/server/src/provider/EventNdjsonLogger.ts";

const root = fileURLToPath(new URL("../../", import.meta.url));
const Effect = await import(pathToFileURL(root + "apps/server/node_modules/effect/dist/Effect.js"));

const target = new URL("../crates/server/tests/fixtures/provider-event-log.jsonl", import.meta.url);
const scratch = await NodeFSP.mkdtemp(NodePath.join(NodeOS.tmpdir(), "t3-event-log-"));
const rows = [];
const mask = (text) => text.replace(/^\[[^\]]+\]/gmu, "[T]");

const long = (n) => "x".repeat(n);
const deep = (n) => (n === 0 ? { leaf: true } : { next: deep(n - 1) });
const events = [
  [
    "a plain frame",
    {
      provider: "codex",
      kind: "protocol",
      event: { method: "thread/started", params: { threadId: "t" } },
    },
  ],
  ["a canonical delta", { type: "content.delta", delta: "hi" }],
  ["a canonical event", { type: "turn.started", turnId: "u" }],
  ["a raw frame", { event: { stage: "raw", payload: "{}" } }],
  [
    "a decoded delta",
    { event: { stage: "decoded", payload: { method: "item/agentMessage/delta", params: {} } } },
  ],
  [
    "a decoded request",
    { event: { stage: "decoded", payload: { method: "thread/start", payload: { a: 1 } } } },
  ],
  ["a claude stream delta", { method: "claude/stream_event/content_block_delta/text_delta" }],
  ["a claude stream event", { type: "stream_event", event: { type: "content_block_delta" } }],
  ["a claude stream start", { type: "stream_event", event: { type: "message_start" } }],
  ["an opencode part delta", { type: "message.part.delta" }],
  [
    "an acp chunk",
    { method: "session/update", payload: { update: { sessionUpdate: "agent_message_chunk" } } },
  ],
  [
    "an acp tool call",
    { method: "session/update", payload: { update: { sessionUpdate: "tool_call" } } },
  ],
  ["an acp update without an update", { method: "session/update", payload: { nope: 1 } }],
  [
    "an opencode text part",
    { type: "message.part.updated", payload: { properties: { part: { type: "text" } } } },
  ],
  [
    "an opencode running tool",
    {
      type: "message.part.updated",
      payload: { properties: { part: { type: "tool", state: { status: "running" } } } },
    },
  ],
  [
    "an opencode finished tool",
    {
      type: "message.part.updated",
      payload: { properties: { part: { type: "tool", state: { status: "completed" } } } },
    },
  ],
  [
    "an opencode tool without state",
    { type: "message.part.updated", payload: { properties: { part: { type: "tool" } } } },
  ],
  ["a string", "just text"],
  ["null", null],
  ["an array", [1, 2, 3]],
  ["a long string", { provider: "claude", event: { message: long(70000), status: "ok" } }],
  ["a string just past the character bound", { event: { a: long(65530), b: long(10) } }],
  ["too deep", { provider: "claude", kind: "protocol", event: deep(20) }],
  ["deep enough", { provider: "claude", event: deep(14) }],
  [
    "many fields",
    {
      provider: "x",
      payload: Object.fromEntries(Array.from({ length: 1100 }, (_, index) => [`k${index}`, index])),
    },
  ],
  ["a long array", { provider: "x", payload: Array.from({ length: 1100 }, (_, index) => index) }],
  [
    "summary fields",
    {
      provider: "codex",
      protocol: "p",
      kind: "k",
      direction: "incoming",
      stage: "decoded",
      id: 7,
      threadId: "t",
      error: {
        message: long(2000),
        code: 3,
        items: [1, 2],
        nested: {
          nested: { nested: { nested: { nested: { nested: { nested: { type: "deep" } } } } } },
        },
      },
      payload: long(80000),
      params: { content: [1, 2, 3], turn: { id: "x" } },
      unrelated: "dropped",
    },
  ],
  ["emoji count as two units", { event: { text: "😀".repeat(33000), status: "x" } }],
];
rows.push({ kind: "events", events });
for (const stream of ["native", "canonical", "orchestration"]) {
  for (const [index, [name, event]] of events.entries()) {
    const bounded = boundProviderEventForLogging(event) ?? null;
    rows.push({
      kind: "filter",
      stream,
      index,
      name,
      persist: shouldPersistProviderEvent(stream, event),
      // The event itself when it fits; saying so keeps the large ones from being written out again.
      bounded: bounded === event ? "same" : bounded,
    });
  }
}

// ---- what is written -----------------------------------------------------------------------

async function run(effect) {
  return Effect.runPromise(effect);
}
async function tree(directory) {
  const files = {};
  for (const name of (await NodeFSP.readdir(directory)).sort()) {
    files[name] = mask(await NodeFSP.readFile(NodePath.join(directory, name), "utf8"));
  }
  return files;
}

{
  const directory = await NodeFSP.mkdtemp(NodePath.join(scratch, "write-"));
  const store = await run(
    makeEventNdjsonLogStore(NodePath.join(directory, "events.log"), { batchWindowMs: 0 }),
  );
  const writes = [
    ["native", events[0][1], "thread-1"],
    ["native", events[1][1], "thread-1"],
    ["canonical", events[2][1], "thread-1"],
    ["native", events[4][1], "thread-1"],
    ["native", events[5][1], "Thread/ÄÖ 2"],
    ["native", events[20][1], "thread-1"],
    ["native", events[22][1], null],
    ["native", events[27][1], "pending"],
    ["orchestration", { type: "content.delta", note: "kept" }, "  --  "],
    ["canonical", events[17][1], "Thread-1"],
  ];
  for (const [stream, event, threadId] of writes) {
    await run(store.logger(stream).write(event, threadId));
  }
  await run(store.close());
  rows.push({
    kind: "write",
    name: "records to per-thread files",
    writes: writes.map(([stream, event, threadId]) => ({
      stream,
      event:
        events.findIndex(([, candidate]) => candidate === event) >= 0
          ? events.findIndex(([, candidate]) => candidate === event)
          : event,
      threadId,
    })),
    files: await tree(directory),
  });
}

{
  const directory = await NodeFSP.mkdtemp(NodePath.join(scratch, "rotate-"));
  const options = { batchWindowMs: 0, maxBytes: 400, maxFiles: 2 };
  const store = await run(makeEventNdjsonLogStore(NodePath.join(directory, "events.log"), options));
  const writes = Array.from({ length: 9 }, (_, index) => [
    "native",
    { provider: "codex", kind: "protocol", event: { method: "thread/start", n: index } },
    "thread-1",
  ]);
  for (const [stream, event, threadId] of writes)
    await run(store.logger(stream).write(event, threadId));
  await run(store.close());
  rows.push({
    kind: "rotate",
    name: "files rotate by size and keep the newest",
    options,
    writes: writes.map(([stream, event, threadId]) => ({
      stream,
      event:
        events.findIndex(([, candidate]) => candidate === event) >= 0
          ? events.findIndex(([, candidate]) => candidate === event)
          : event,
      threadId,
    })),
    files: await tree(directory),
  });
}

{
  const directory = await NodeFSP.mkdtemp(NodePath.join(scratch, "batch-"));
  // A window of 2 records: the third write finds two buffered and flushes the three together.
  const options = { batchWindowMs: 60000, maxBufferedRecords: 3 };
  const store = await run(makeEventNdjsonLogStore(NodePath.join(directory, "events.log"), options));
  const logger = store.logger("native");
  const events3 = [1, 2, 3, 4].map((n) => ({ provider: "codex", event: { n } }));
  for (const event of events3.slice(0, 2)) await run(logger.write(event, "t"));
  const before = await tree(directory);
  await run(logger.write(events3[2], "t"));
  const after = await tree(directory);
  await run(logger.write(events3[3], "t"));
  const pending = await tree(directory);
  await run(store.close());
  rows.push({
    kind: "batch",
    name: "records wait for the window, the count or close",
    options,
    events: events3,
    before,
    after,
    pending,
    closed: await tree(directory),
  });
}

// ---- retention -----------------------------------------------------------------------------

const DAY = 24 * 60 * 60 * 1000;
async function retention(name, options, files) {
  const directory = await NodeFSP.mkdtemp(NodePath.join(scratch, "retain-"));
  const now = Date.now();
  for (const file of files) {
    const path = NodePath.join(directory, file.name);
    await NodeFSP.writeFile(path, file.text ?? "x".repeat(file.size));
    const mtime = (now - file.ageDays * DAY) / 1000;
    await NodeFSP.utimes(path, mtime, mtime);
  }
  const store = await run(makeEventNdjsonLogStore(NodePath.join(directory, "events.log"), options));
  await run(store.close());
  rows.push({
    kind: "retention",
    name,
    options,
    files,
    kept: (await NodeFSP.readdir(directory)).sort(),
  });
}
const header = "[2026-01-01T00:00:00.000Z] NTIVE: {}\n";
await retention("files past the age go, the rest stay", { maxAgeMs: 14 * DAY }, [
  { name: "events.a.log", size: 10, ageDays: 20 },
  { name: "events.b.log", size: 10, ageDays: 1 },
  { name: "events.b.log.1", size: 10, ageDays: 30 },
  { name: "other.txt", size: 10, ageDays: 90 },
  { name: "notes.log", size: 10, ageDays: 90 },
  { name: "foreign.log", text: header, ageDays: 90 },
  { name: "foreign.log.2", text: header, ageDays: 1 },
]);
await retention("the oldest go until the total fits", { maxTotalBytes: 25 }, [
  { name: "events.a.log", size: 10, ageDays: 5 },
  { name: "events.b.log", size: 10, ageDays: 4 },
  { name: "events.c.log", size: 10, ageDays: 3 },
  { name: "events.d.log", size: 10, ageDays: 2 },
  { name: "unrelated.log", size: 1000, ageDays: 1 },
]);
await retention("equal ages go in name order", { maxTotalBytes: 15, maxAgeMs: 100 * DAY }, [
  { name: "events.b.log", size: 10, ageDays: 5 },
  { name: "events.a.log", size: 10, ageDays: 5 },
]);

// ---- options -------------------------------------------------------------------------------

for (const [option, value] of [
  ["maxBytes", 0],
  ["maxFiles", 0],
  ["batchWindowMs", -1],
  ["maxTotalBytes", 0],
  ["maxAgeMs", 0],
  ["retentionCheckIntervalMs", 0],
  ["maxBufferedBytes", 0],
  ["maxBufferedRecords", 1.5],
]) {
  const path = NodePath.join(scratch, "options", "events.log");
  const result = await Effect.runPromiseExit(makeEventNdjsonLogStore(path, { [option]: value }));
  const failure =
    result._tag === "Failure"
      ? (result.cause.reasons?.[0]?.error ?? result.cause.failures?.[0])
      : undefined;
  rows.push({
    kind: "option",
    option,
    value,
    message: failure?.message?.replaceAll(scratch, "<scratch>") ?? null,
  });
}

await NodeFSP.rm(scratch, { recursive: true, force: true });
const output = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
if (process.argv.includes("--check")) {
  const current = await NodeFSP.readFile(target, "utf8");
  if (current !== output) {
    console.error(
      "provider-event-log.jsonl is out of date; run rust/tools/generate_provider_event_log_fixtures.mjs",
    );
    process.exit(1);
  }
} else {
  await NodeFSP.writeFile(target, output);
  console.log(`wrote ${rows.length} rows (${output.length} bytes)`);
}
