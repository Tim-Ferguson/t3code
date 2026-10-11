// Runs the unchanged transcript reader (apps/server/src/usage/usageTranscriptReader.ts) and the unchanged scan cache
// (usageScanCache.ts) over fixed inputs and records what they make of them for the Rust port (`usage_tests.rs`). Nothing reads a
// real transcript directory.
//   node rust/tools/generate_usage_scan_cache_fixtures.mjs          writes crates/server/tests/fixtures/usage-scan-cache.jsonl
//   node rust/tools/generate_usage_scan_cache_fixtures.mjs --check  compares with the file
import * as NodeFSP from "node:fs/promises";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";

import { readTranscriptRecords } from "../../apps/server/src/usage/usageTranscriptReader.ts";
import {
  decodeScanCache,
  makeScanCacheWriter,
  pruneScanCache,
} from "../../apps/server/src/usage/usageScanCache.ts";

const target = new URL("../crates/server/tests/fixtures/usage-scan-cache.jsonl", import.meta.url);
const scratch = await NodeFSP.mkdtemp(NodePath.join(NodeOS.tmpdir(), "t3-usage-scan-cache-"));
const rows = [];

const claude = (id, at, tokens, extra = {}) =>
  JSON.stringify({
    type: "assistant",
    timestamp: at,
    requestId: `req_${id}`,
    sessionId: "session-claude",
    message: {
      id: `msg_${id}`,
      model: "claude-sonnet-4-6",
      usage: { input_tokens: tokens, output_tokens: 5 },
    },
    ...extra,
  });
const codexMeta = (id, at, extra = {}) =>
  JSON.stringify({ type: "session_meta", timestamp: at, payload: { id, ...extra } });
const codexTurn = (at, model) =>
  JSON.stringify({ type: "turn_context", timestamp: at, payload: { model } });
const codexTier = (at, tier) =>
  JSON.stringify({
    type: "event_msg",
    timestamp: at,
    payload: { type: "thread_settings_applied", thread_settings: { service_tier: tier } },
  });
const codexCount = (at, input, output) =>
  JSON.stringify({
    type: "event_msg",
    timestamp: at,
    payload: {
      type: "token_count",
      info: {
        last_token_usage: { input_tokens: input, cached_input_tokens: 0, output_tokens: output },
      },
    },
  });

const view = (result) =>
  result === null
    ? null
    : {
        records: result.records,
        tailRecords: result.tailRecords,
        position: result.position,
        resumed: result.resumed,
      };

/**
 * `versions` are successive contents of one file. Each is parsed whole; each is also parsed resuming from the position of the full
 * parse of version `resumeFrom` (default: the one before), which the Rust reader is given verbatim.
 */
async function parseScenario(name, provider, versions) {
  const path = NodePath.join(scratch, `${rows.length}.jsonl`);
  const out = [];
  const fulls = [];
  for (const [index, version] of versions.entries()) {
    const text = typeof version === "string" ? version : version.text;
    await NodeFSP.writeFile(path, text);
    const full = await readTranscriptRecords(path, provider);
    fulls.push(full);
    const resumeFromIndex =
      typeof version === "string" ? index - 1 : (version.resumeFrom ?? index - 1);
    const resumeFrom = resumeFromIndex >= 0 ? fulls[resumeFromIndex]?.position : undefined;
    const resumed = resumeFrom ? await readTranscriptRecords(path, provider, resumeFrom) : null;
    out.push({
      text,
      full: view(full),
      resumeFrom: resumeFrom ?? null,
      resumedParse: view(resumed),
    });
  }
  rows.push({ kind: "parse", name, provider, versions: out });
}

await parseScenario(
  "claude: duplicates, blank and unrelated lines, CRLF, unterminated tail, then growth",
  "claude",
  [
    [
      claude(1, "2026-10-08T10:00:00.000Z", 10),
      claude(1, "2026-10-08T10:00:01.000Z", 10),
      "",
      JSON.stringify({ type: "user", message: { content: "no usage here" } }),
      claude(2, "2026-10-08T10:05:00.000Z", 20, { costUSD: 0.25 }) + "\r",
      'not json but "usage" appears',
      claude(3, "2026-10-08T10:06:00.000Z", 30, { sessionId: "session-é" }),
    ].join("\n") +
      "\n" +
      claude(4, "2026-10-08T10:07:00.000Z", 40),
    // The tail was finished and more written: the tail record is re-read once, never twice.
    [
      claude(1, "2026-10-08T10:00:00.000Z", 10),
      claude(1, "2026-10-08T10:00:01.000Z", 10),
      "",
      JSON.stringify({ type: "user", message: { content: "no usage here" } }),
      claude(2, "2026-10-08T10:05:00.000Z", 20, { costUSD: 0.25 }) + "\r",
      'not json but "usage" appears',
      claude(3, "2026-10-08T10:06:00.000Z", 30, { sessionId: "session-é" }),
      claude(4, "2026-10-08T10:07:00.000Z", 40),
      claude(5, "2026-10-08T10:08:00.000Z", 50),
    ].join("\n") + "\n",
    // Rewritten, not grown: the guard bytes no longer match, so the parse starts again.
    {
      text:
        claude(9, "2026-10-09T10:00:00.000Z", 90) +
        "\n" +
        claude(10, "2026-10-09T10:01:00.000Z", 100) +
        "\n" +
        claude(11, "2026-10-09T10:02:00.000Z", 110) +
        "\n" +
        claude(12, "2026-10-09T10:03:00.000Z", 120) +
        "\n",
      resumeFrom: 1,
    },
    "",
    "\n\n",
  ],
);

const codexRollout = [
  codexMeta("rollout-a", "2026-10-08T09:00:00.000Z"),
  codexTurn("2026-10-08T09:00:01.000Z", "gpt-5"),
  codexTier("2026-10-08T09:00:02.000Z", "priority"),
  codexCount("2026-10-08T09:00:03.000Z", 100, 10),
  codexCount("2026-10-08T09:00:03.500Z", 100, 10),
  codexCount("2026-10-08T09:01:00.000Z", 200, 20),
];
await parseScenario(
  "codex: reducer state carried across a resume, a tier change in the appended bytes",
  "codex",
  [
    codexRollout.join("\n") + "\n" + codexCount("2026-10-08T09:02:00.000Z", 300, 30),
    codexRollout.join("\n") +
      "\n" +
      [
        codexCount("2026-10-08T09:02:00.000Z", 300, 30),
        codexTurn("2026-10-08T09:02:30.000Z", "gpt-5-mini"),
        codexTier("2026-10-08T09:02:31.000Z", "ultrafast"),
        codexCount("2026-10-08T09:03:00.000Z", 400, 40),
      ].join("\n") +
      "\n",
    codexRollout.join("\n") +
      "\n" +
      [
        codexCount("2026-10-08T09:02:00.000Z", 300, 30),
        codexTurn("2026-10-08T09:02:30.000Z", "gpt-5-mini"),
        codexTier("2026-10-08T09:02:31.000Z", "ultrafast"),
        codexCount("2026-10-08T09:03:00.000Z", 400, 40),
        codexCount("2026-10-08T09:04:00.000Z", 500, 50),
      ].join("\n") +
      "\n",
  ],
);
await parseScenario("codex: a fork suppresses the history it copies, across a resume", "codex", [
  [
    codexMeta("fork-1", "2026-10-08T10:00:00.000Z", { forked_from_id: "parent" }),
    codexTurn("2026-10-08T10:00:00.100Z", "gpt-5"),
    codexCount("2026-10-08T10:00:00.200Z", 100, 10),
    codexCount("2026-10-08T10:00:00.300Z", 150, 15),
  ].join("\n") + "\n",
  [
    codexMeta("fork-1", "2026-10-08T10:00:00.000Z", { forked_from_id: "parent" }),
    codexTurn("2026-10-08T10:00:00.100Z", "gpt-5"),
    codexCount("2026-10-08T10:00:00.200Z", 100, 10),
    codexCount("2026-10-08T10:00:00.300Z", 150, 15),
    codexCount("2026-10-08T10:00:10.000Z", 200, 20),
    codexCount("2026-10-08T10:00:20.000Z", 250, 25),
  ].join("\n") + "\n",
]);
await parseScenario("codex: a resume position without reducer state is not resumed", "codex", [
  codexRollout.join("\n") + "\n",
]);

// ---- the cache document ------------------------------------------------------------------------

const usage = (model, sessionId, at, uncached, extra = {}) => ({
  provider: "claude",
  timestampMs: at,
  model,
  sessionId,
  totals: {
    uncachedInputTokens: uncached,
    cachedInputTokens: 1,
    cacheCreationTokens: 2,
    outputTokens: 3,
    reasoningTokens: 0,
  },
  reportedCostUsd: null,
  speed: "standard",
  dedupeKey: null,
  ...extra,
});
const position = (
  offset,
  guardLength = Math.min(64, offset),
  guardHash = 1234,
  codexState = null,
) => ({
  resumeOffset: offset,
  guardLength,
  guardHash,
  codexState,
});
const codexState = {
  model: "gpt-5",
  speed: "fast",
  sessionId: "rollout-a",
  lastUsageSignature: '{"input_tokens":1}',
  sawSessionMeta: true,
  suppressingForkCopies: true,
  forkCopyAnchorMs: 1790000000000,
};
const entries = [
  {
    path: "/home/u/.claude/projects/a/one.jsonl",
    size: 4096,
    mtimeMs: 1790000000123.4567,
    provider: "claude",
    records: [
      usage("claude-sonnet-4-6", "s1", 1790000000000, 10, { dedupeKey: "msg_1:req_1" }),
      usage("claude-sonnet-4-6", "s1", 1790000001000, 11, { reportedCostUsd: 0.5, speed: "fast" }),
      usage("claude-opus-4-1", "", 1790000002000, 12),
    ],
    tailRecords: [usage("claude-opus-4-1", "s2", 1790000003000, 13, { reportedCostUsd: 2 })],
    position: position(4000),
  },
  {
    path: '/home/u/.codex/sessions/2026/roll out "quoted".jsonl',
    size: 100,
    mtimeMs: 1790000005000,
    provider: "codex",
    records: [
      usage("gpt-5", "rollout-a", 1790000004000, 14, { provider: "codex", speed: "ultrafast" }),
    ],
    tailRecords: [],
    position: position(100, 64, 4294967295, codexState),
  },
  {
    path: "/home/u/.claude/projects/a/empty.jsonl",
    size: 0,
    mtimeMs: 1790000006000,
    provider: "claude",
    records: [],
    tailRecords: [],
    position: position(0, 0, 0),
  },
];
{
  const cache = new Map(entries.map(({ path, ...entry }) => [path, entry]));
  const write = makeScanCacheWriter();
  const extra = {
    sources: {
      "claude\0/home/u/.claude/projects": {
        dir: "/home/u/.claude/projects",
        volumeId: "16777232:42",
      },
    },
  };
  const first = write(cache, extra);
  // A second write after one entry is replaced and one is added: the memoised entries keep their intern indexes.
  const replaced = new Map(cache);
  replaced.set("/home/u/.claude/projects/a/empty.jsonl", {
    size: 10,
    mtimeMs: 1790000009000,
    provider: "claude",
    records: [usage("claude-haiku-4-5", "s3", 1790000008000, 15)],
    tailRecords: [],
    position: position(10),
  });
  replaced.set("/home/u/.claude/projects/b/new.jsonl", {
    size: 20,
    mtimeMs: 1790000010000,
    provider: "claude",
    records: [usage("claude-sonnet-4-6", "s4", 1790000009000, 16)],
    tailRecords: [],
    position: position(20),
  });
  const second = write(replaced, extra);
  rows.push({
    kind: "write",
    name: "entries, then a replacement and an addition",
    entries,
    replaced: [...replaced].map(([path, entry]) => ({ path, ...entry })),
    extra,
    first,
    second,
  });
}

const decoded = (document) =>
  [...decodeScanCache(document)].map(([path, entry]) => ({ path, ...entry }));
const document = JSON.parse(rows.at(-1).second);
const withFile = (name, change) => {
  const copy = structuredClone(document);
  const first = Object.keys(copy.files)[0];
  change(copy, copy.files[first], first);
  return [name, copy];
};
const decodeCases = [
  ["a written cache", document],
  ["a version from the future", { ...document, version: 6 }],
  ["a version before speed was cached", { ...document, version: 3 }],
  ["no files", { ...document, files: undefined }],
  ["not an object", [1, 2]],
  ["a model table with a number", { ...document, models: [...document.models.slice(1), 5] }],
  withFile("a short row drops the entry", (_, file) => file.r.push([1, 2, 3])),
  withFile("a model index out of range drops the entry", (_, file) => (file.r[0][1] = 999)),
  withFile("a non-finite token count drops the entry", (_, file) => (file.r[0][3] = "x")),
  withFile("a speed index out of range drops the entry", (_, file) => (file.r[0][10] = 7)),
  withFile("a session index out of range reads as empty", (_, file) => (file.r[0][2] = 999)),
  withFile("a guard longer than the window drops the entry", (_, file) => (file.gl = 65)),
  withFile("a guard longer than the offset drops the entry", (_, file) => {
    file.o = 3;
    file.gl = 10;
  }),
  withFile("a negative offset drops the entry", (_, file) => (file.o = -1)),
  withFile("a fractional offset drops the entry", (_, file) => (file.o = 1.5)),
  withFile("an unknown provider drops the entry", (_, file) => (file.p = "cursor")),
  withFile("a grok entry is kept", (_, file) => (file.p = "grok")),
  withFile("tail rows that do not decode drop the entry", (_, file) => (file.t = [[]])),
  [
    "a codex state that is not an object",
    (() => {
      const copy = structuredClone(document);
      copy.files[Object.keys(copy.files).find((path) => path.includes("roll"))].cs = "bad";
      return copy;
    })(),
  ],
  [
    "a codex state with a bad speed",
    (() => {
      const copy = structuredClone(document);
      copy.files[Object.keys(copy.files).find((path) => path.includes("roll"))].cs.speed = "warp";
      return copy;
    })(),
  ],
  [
    "a codex state without a signature",
    (() => {
      const copy = structuredClone(document);
      copy.files[
        Object.keys(copy.files).find((path) => path.includes("roll"))
      ].cs.lastUsageSignature = null;
      return copy;
    })(),
  ],
  ["a v4 codex entry keeps its records but cannot resume", { ...document, version: 4 }],
];
for (const [name, doc] of decodeCases) {
  rows.push({
    kind: "decode",
    name,
    document: JSON.parse(JSON.stringify(doc)),
    decoded: decoded(JSON.parse(JSON.stringify(doc))),
  });
}

{
  const cache = new Map(entries.map(({ path, ...entry }) => [path, entry]));
  const removed = pruneScanCache(cache, 1790000005000);
  rows.push({
    kind: "prune",
    name: "entries last written before the cutoff go",
    entries,
    cutoff: 1790000005000,
    removed,
    kept: [...cache.keys()],
  });
}

await NodeFSP.rm(scratch, { recursive: true, force: true });
const output = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
if (process.argv.includes("--check")) {
  const current = await NodeFSP.readFile(target, "utf8");
  if (current !== output) {
    console.error(
      "usage-scan-cache.jsonl is out of date; run rust/tools/generate_usage_scan_cache_fixtures.mjs",
    );
    process.exit(1);
  }
} else {
  await NodeFSP.writeFile(target, output);
  console.log(`wrote ${rows.length} rows (${output.length} bytes)`);
}
