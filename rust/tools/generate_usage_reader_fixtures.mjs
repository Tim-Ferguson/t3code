// Runs the unchanged OpenCode and Antigravity usage readers (apps/server/src/usage/opencodeUsageReader.ts,
// antigravityUsageReader.ts) over stores built here, and records the stores' files with what the readers made of them.
// The Rust test (tests/usage_readers_flow.rs) rebuilds the files in a temporary directory and compares. Nothing reads a real
// home directory. The scenarios are those of the original's own tests (usageTranscriptReader.test.ts) plus the edges they share.
//   node rust/tools/generate_usage_reader_fixtures.mjs          writes crates/server/tests/fixtures/usage-readers.jsonl
//   node rust/tools/generate_usage_reader_fixtures.mjs --check  compares with the file
import * as NodeFSP from "node:fs/promises";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeSqlite from "node:sqlite";

import { readAntigravityUsage } from "../../apps/server/src/usage/antigravityUsageReader.ts";
import { readOpenCodeUsage } from "../../apps/server/src/usage/opencodeUsageReader.ts";

const target = new URL("../crates/server/tests/fixtures/usage-readers.jsonl", import.meta.url);

function protoNumber(field, value) {
  const varint = (number) => {
    const bytes = [];
    do {
      const byte = number % 128;
      number = Math.floor(number / 128);
      bytes.push(byte + (number > 0 ? 128 : 0));
    } while (number > 0);
    return bytes;
  };
  return [...varint(field * 8), ...varint(value)];
}
function protoBytes(field, bytes) {
  const encoded = protoNumber(field, bytes.length);
  encoded[0] = encoded[0] + 2;
  return [...encoded, ...bytes];
}
function protoText(field, value) {
  return protoBytes(field, [...Buffer.from(value)]);
}

/** A scenario's files: `files` maps a relative path to text or bytes, `mtimes` to the modification time the reader sees. */
async function build(root, files, mtimes) {
  for (const [relative, content] of Object.entries(files)) {
    const path = NodePath.join(root, relative);
    await NodeFSP.mkdir(NodePath.dirname(path), { recursive: true });
    await NodeFSP.writeFile(path, content);
  }
  for (const [relative, ms] of Object.entries(mtimes ?? {})) {
    await NodeFSP.utimes(NodePath.join(root, relative), ms / 1000, ms / 1000);
  }
}

/** A SQLite file made by running `statements` against a fresh database, as its bytes. */
async function database(directory, name, run) {
  const path = NodePath.join(directory, name);
  const db = new NodeSqlite.DatabaseSync(path);
  try {
    run(db);
  } finally {
    db.close();
  }
  return new Uint8Array(await NodeFSP.readFile(path));
}

const relative = (root, path) => NodePath.relative(root, path).split(NodePath.sep).join("/");
const record = (r) => ({
  provider: r.provider,
  timestampMs: r.timestampMs,
  model: r.model,
  sessionId: r.sessionId,
  totals: r.totals,
  reportedCostUsd: r.reportedCostUsd,
  speed: r.speed,
  dedupeKey: r.dedupeKey,
});
const byPath = (a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0);

const rows = [];
const scratch = await NodeFSP.mkdtemp(NodePath.join(NodeOS.tmpdir(), "usage-reader-fixtures-"));

async function scenario(kind, name, files, mtimes, calls) {
  const root = await NodeFSP.mkdtemp(NodePath.join(scratch, "case-"));
  await build(root, files, mtimes);
  const results = [];
  for (const call of calls) {
    const roots = call.roots.map((r) => NodePath.join(root, r));
    if (kind === "opencode") {
      const result = await readOpenCodeUsage(roots[0], call.sinceMs);
      results.push({
        ...call,
        result: {
          files: result.files
            .map((f) => ({ path: relative(root, f.path), records: f.records.map(record) }))
            .sort(byPath),
          missing: result.missing,
          error: result.error,
        },
      });
    } else {
      const result = await readAntigravityUsage(roots, call.sinceMs);
      results.push({
        ...call,
        result: {
          files: result.files.map((f) => ({
            root: relative(root, f.root),
            path: relative(root, f.path),
            records: f.records.map(record),
          })),
          errors: result.errors.map((e) => relative(root, e)),
        },
      });
    }
  }
  rows.push({
    kind,
    name,
    files: Object.fromEntries(
      Object.entries(files).map(([path, content]) => [
        path,
        typeof content === "string"
          ? { text: content }
          : { base64: Buffer.from(content).toString("base64") },
      ]),
    ),
    mtimes: mtimes ?? {},
    calls: results,
  });
}

// ---- OpenCode ------------------------------------------------------------------------------------------------
const message = {
  id: "msg-1",
  sessionID: "session-1",
  role: "assistant",
  modelID: "claude-sonnet-4-5",
  time: { created: 1780000000000 },
  cost: 0.25,
  tokens: { input: 100, output: 20, reasoning: 5, cache: { read: 30, write: 10 } },
};
const migrated = await database(scratch, "migrated.db", (db) => {
  db.exec("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)");
  const insert = db.prepare("INSERT INTO message VALUES (?, ?, ?)");
  insert.run("msg-1", "session-1", JSON.stringify(message));
  insert.run(
    "msg-2",
    "session-1",
    JSON.stringify({ ...message, id: "msg-2", time: { created: 1780000001000 } }),
  );
});
await scenario(
  "opencode",
  "a migrated store counts each message once",
  {
    "opencode.db": migrated,
    "storage/message/session-1/msg-1.json": JSON.stringify(message),
    "storage/message/session-1/msg-9.json": JSON.stringify({
      ...message,
      id: "msg-9",
      time: { created: 1780000002000 },
    }),
  },
  {},
  [
    { roots: ["."], sinceMs: 0 },
    { roots: ["."], sinceMs: 1780000001000 },
    { roots: ["."], sinceMs: 1790000000000 },
  ],
);
const timed = await database(scratch, "timed.db", (db) => {
  db.exec(
    "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_created INTEGER); CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, data TEXT, time_created INTEGER)",
  );
  const insert = db.prepare("INSERT INTO message VALUES (?, ?, ?, ?)");
  // The row's own time stands in for a message without one; a message of the user, or without tokens or a model, is not usage.
  const bare = {
    id: "a1",
    role: "assistant",
    model: { id: "gpt-5" },
    tokens: { input: 7, output: 3 },
    cost: 0,
  };
  insert.run("a1", "s1", JSON.stringify(bare), 1780000005000);
  insert.run("a2", "s1", JSON.stringify({ ...bare, id: "a2", role: "user" }), 1780000005000);
  insert.run("a3", "s1", JSON.stringify({ ...bare, id: "a3", tokens: {} }), 1780000005000);
  insert.run("a4", "s1", JSON.stringify({ ...bare, id: "a4", model: undefined }), 1780000005000);
  insert.run("a5", "s1", "not json", 1780000005000);
  insert.run(
    "a6",
    "s1",
    JSON.stringify({ ...bare, id: "a6", time: { created: "soon" } }),
    1780000005000,
  );
  insert.run(
    "a7",
    "s1",
    JSON.stringify({
      ...bare,
      id: "a7",
      time: { created: null },
      tokens: { input: 2.9, output: -4, reasoning: 1e400 },
    }),
    1780000006000,
  );
  insert.run("old", "s1", JSON.stringify({ ...bare, id: "old" }), 1);
  const session = db.prepare("INSERT INTO session_message VALUES (?, ?, ?, ?, ?)");
  session.run(
    "b1",
    "s2",
    "assistant",
    JSON.stringify({ ...bare, id: "b1", modelID: "claude-opus-4-6" }),
    1780000007000,
  );
  session.run("b2", "s2", "user", JSON.stringify({ ...bare, id: "b2" }), 1780000007000);
});
await scenario(
  "opencode",
  "rows carry their own time and only assistant messages with tokens count",
  { "opencode.db": timed, "opencode-extra_1.db": timed, "other.db": timed },
  {},
  [{ roots: ["."], sinceMs: 1780000000000 }],
);
await scenario("opencode", "nothing there is nothing missing", {}, {}, [
  { roots: ["absent"], sinceMs: 0 },
]);
await scenario(
  "opencode",
  "legacy json alone",
  {
    "storage/message/s/m1.json": JSON.stringify({ ...message, id: undefined }),
    "storage/message/s/m2.json": "{",
  },
  {},
  [{ roots: ["."], sinceMs: 0 }],
);
await scenario(
  "opencode",
  "a database without messages is an error",
  {
    "opencode.db": await database(scratch, "empty.db", (db) => db.exec("CREATE TABLE other (x)")),
    "opencode-bad.db": new Uint8Array([1, 2, 3]),
  },
  {},
  [{ roots: ["."], sinceMs: 0 }],
);

// ---- Antigravity ---------------------------------------------------------------------------------------------
const stamp = protoNumber(1, 1780000000);
const usage = [
  ...protoNumber(2, 100),
  ...protoNumber(3, 40),
  ...protoNumber(4, 5),
  ...protoNumber(5, 20),
  ...protoNumber(9, 10),
  ...protoText(11, "response-1"),
];
const retry = [
  ...protoNumber(1, 1026),
  ...protoNumber(2, 12),
  ...protoNumber(3, 3),
  ...protoText(11, "retry-1"),
];
const generation = protoBytes(1, [
  ...protoBytes(4, usage),
  ...protoText(19, "Gemini 3 Pro"),
  ...protoBytes(9, protoBytes(4, stamp)),
]);
const step = [
  ...protoBytes(9, usage),
  ...protoBytes(8, stamp),
  ...protoBytes(28, protoBytes(2, retry)),
];
await scenario(
  "antigravity",
  "generation and step usage are one record, and a retry keeps its own model",
  {
    "session-1.db": await database(scratch, "a.db", (db) => {
      db.exec(
        "CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)",
      );
      db.prepare("INSERT INTO gen_metadata VALUES (?, ?)").run(0, new Uint8Array(generation));
      db.prepare("INSERT INTO steps VALUES (?, ?)").run(0, new Uint8Array(step));
    }),
  },
  {},
  [
    { roots: ["."], sinceMs: 0 },
    { roots: ["."], sinceMs: 1780000000001 },
  ],
);
await scenario(
  "antigravity",
  "each model-less step takes its generation's model",
  {
    "model-switch.db": await database(scratch, "b.db", (db) => {
      db.exec(
        "CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)",
      );
      const g = db.prepare("INSERT INTO gen_metadata VALUES (?, ?)");
      const s = db.prepare("INSERT INTO steps VALUES (?, ?)");
      for (const [idx, name] of [
        "Gemini 3 Pro",
        "Claude Opus 4.6",
        "Claude 4.5 Sonnet (Thinking)",
        "Mystery Model",
      ].entries()) {
        g.run(idx, new Uint8Array(protoBytes(1, protoText(19, name))));
        s.run(idx, new Uint8Array(protoBytes(9, protoNumber(2, 10 + idx))));
      }
      s.run(9, new Uint8Array(protoBytes(9, [...protoNumber(1, 99999), ...protoNumber(2, 1)])));
    }),
  },
  { "model-switch.db": 1780000000000 },
  [{ roots: ["."], sinceMs: 0 }],
);
await scenario(
  "antigravity",
  "aliases that bridge separate step records merge them",
  {
    "bridge.db": await database(scratch, "c.db", (db) => {
      db.exec(
        "CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)",
      );
      const s = db.prepare("INSERT INTO steps VALUES (?, ?)");
      s.run(
        0,
        new Uint8Array(protoBytes(9, [...protoNumber(2, 100), ...protoText(11, "response")])),
      );
      s.run(
        1,
        new Uint8Array(protoBytes(9, [...protoNumber(3, 40), ...protoText(12, "provider")])),
      );
      db.prepare("INSERT INTO gen_metadata VALUES (?, ?)").run(
        0,
        new Uint8Array(
          protoBytes(1, [
            ...protoText(19, "Gemini 3 Pro"),
            ...protoBytes(4, [
              ...protoNumber(2, 50),
              ...protoNumber(5, 20),
              ...protoText(11, "response"),
              ...protoText(12, "provider"),
            ]),
          ]),
        ),
      );
    }),
  },
  { "bridge.db": 1780000000000 },
  [{ roots: ["."], sinceMs: 0 }],
);
const aliased = async (index) =>
  database(scratch, `alias-${index}.db`, (db) => {
    db.exec("CREATE TABLE steps (idx INTEGER, metadata BLOB)");
    for (const identity of [7, 12]) {
      const u = [
        ...protoNumber(1, 246),
        ...protoNumber(2, index === 0 ? 100 : 150),
        ...protoText(11, `response-${index}-${identity}`),
        ...protoText(identity, `shared-${identity}`),
      ];
      db.prepare("INSERT INTO steps VALUES (?, ?)").run(identity, new Uint8Array(protoBytes(9, u)));
    }
  });
await scenario(
  "antigravity",
  "aliases merge across roots and keep the first root's ownership",
  { "first/session-0.db": await aliased(0), "second/session-1.db": await aliased(1) },
  { "first/session-0.db": 1780000000000, "second/session-1.db": 1780000000000 },
  [{ roots: ["first", "second"], sinceMs: 0 }],
);
const fallback = async (kind) =>
  database(scratch, `${kind}.db`, (db) => {
    db.exec(
      "CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)",
    );
    if (kind === "trajectory") {
      db.exec("CREATE TABLE trajectory_metadata_blob (data BLOB)");
      db.prepare("INSERT INTO trajectory_metadata_blob VALUES (?)").run(
        new Uint8Array(protoBytes(2, protoNumber(1, 1780000200))),
      );
    }
    for (const [index, seconds] of [1780000000, 1780000200].entries()) {
      const u = [...protoNumber(2, 10), ...protoText(11, `${kind}-${index}`)];
      db.prepare("INSERT INTO steps VALUES (?, ?)").run(index, new Uint8Array(protoBytes(9, u)));
      db.prepare("INSERT INTO gen_metadata VALUES (?, ?)").run(
        index,
        new Uint8Array(
          protoBytes(1, [
            ...protoBytes(4, u),
            ...protoBytes(9, protoBytes(4, protoNumber(1, seconds))),
          ]),
        ),
      );
    }
  });
await scenario(
  "antigravity",
  "fallback timestamps are upgraded before the window applies",
  { "mtime.db": await fallback("mtime"), "trajectory.db": await fallback("trajectory") },
  { "mtime.db": 1780000000000, "trajectory.db": 1780000000000 },
  [{ roots: ["."], sinceMs: 1780000100000 }],
);
const stepOnly = await database(scratch, "steps.db", (db) => {
  db.exec("CREATE TABLE steps (idx INTEGER, metadata BLOB)");
  const u = [...protoNumber(1, 246), ...protoNumber(2, 10), ...protoNumber(3, 5)];
  db.prepare("INSERT INTO steps VALUES (?, ?)").run(
    0,
    new Uint8Array([...protoBytes(9, u), ...protoBytes(8, protoNumber(1, 1780000000))]),
  );
});
const largeUnused = await database(scratch, "large.db", (db) => {
  db.exec("CREATE TABLE steps (idx INTEGER, metadata BLOB)");
  const unusedField = [...protoNumber(99, 0).slice(0, -1), ...Array(9).fill(0xff), 0x01];
  const u = [...protoNumber(1, 246), ...protoNumber(2, 10), ...unusedField];
  db.prepare("INSERT INTO steps VALUES (?, ?)").run(0, new Uint8Array(protoBytes(9, u)));
});
await scenario(
  "antigravity",
  "step-only stores are read, a malformed database is reported, a large unused varint is skipped",
  {
    "steps.db": stepOnly,
    "broken.db": "not a sqlite database",
    "nested/large.db": largeUnused,
    "nested/readme.txt": "ignored",
    "empty.db": await database(scratch, "none.db", (db) => db.exec("CREATE TABLE unrelated (x)")),
  },
  { "steps.db": 1780000000000, "nested/large.db": 1780000000000 },
  [{ roots: ["."], sinceMs: 0 }],
);
await scenario("antigravity", "a root that is not there", {}, {}, [
  { roots: ["absent"], sinceMs: 0 },
]);

await NodeFSP.rm(scratch, { recursive: true, force: true });
const output = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
if (process.argv.includes("--check")) {
  const current = await NodeFSP.readFile(target, "utf8");
  if (current !== output) {
    console.error(
      "usage-readers.jsonl is out of date; run rust/tools/generate_usage_reader_fixtures.mjs",
    );
    process.exit(1);
  }
} else {
  await NodeFSP.writeFile(target, output);
  console.log(`wrote ${rows.length} scenarios (${output.length} bytes)`);
}
