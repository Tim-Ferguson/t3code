// Cases for the check of what the server sends (`rpc_response.rs`), run through the original's own decoding of a reply.
//
// A valid reply is made the way the original makes one: a value the schema's arbitrary generates, encoded through the exit
// codec `RpcServer` encodes with (or, for a stream chunk, the codec of a non-empty array of its element), which is
// `Schema.toCodecJson(Rpc.exitSchema(rpc))`. Mutations of those replies (a leaf replaced by a wrong value, a key removed, an
// unknown key added, one value swapped for another found elsewhere in the document, an array emptied) are what the check must
// refuse, or accept, exactly as the original's decoder does: `Schema.decodeUnknownExit` on the same codec, with
// `onExcessProperty: "error"` (an encoder never writes a key the schema does not name).
//
// `tests/fixtures/rpc-response-cases.jsonl` is the witness `rpc_response_tests.rs` checks the port against. Methods whose reply
// schemas are the same schema (the same root in `rpc_response_schemas.jsonl`) share cases. `--check` regenerates in memory and
// fails when the committed cases are not what the original makes of them.
import { readFileSync, writeFileSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as SchemaIssue from "../../apps/server/node_modules/effect/dist/SchemaIssue.js";
import * as Cause from "../../apps/server/node_modules/effect/dist/Cause.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Arbitrary from "../../apps/server/node_modules/effect/dist/Arbitrary.js";
import * as Rpc from "../../apps/server/node_modules/effect/dist/rpc/Rpc.js";
import * as RpcSchema from "../../apps/server/node_modules/effect/dist/rpc/RpcSchema.js";
import { WsRpcGroup } from "../../packages/contracts/src/rpc.ts";

const roots = JSON.parse(
  readFileSync(new URL("../crates/server/src/rpc_response_schemas.jsonl", import.meta.url), "utf8"),
).responses;

// Deterministic: a seeded generator, so a regeneration is the same file.
const hash = (text) => {
  let h = 2166136261;
  for (const c of text) h = Math.imul(h ^ c.charCodeAt(0), 16777619) >>> 0;
  return h;
};
const prng = (seed) => () => {
  seed = (seed + 0x6d2b79f5) >>> 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};

const compress = (value) => {
  if (typeof value === "string") {
    const padded = /^( ?)(.+?)( ?)$/su.exec(value);
    const body = padded ? padded[2] : value;
    const unit = [...body][0];
    const count = unit ? [...body].length : 0;
    if (count >= 64 && unit.repeat(count) === body)
      return padded[1] || padded[3]
        ? { $repeat: [unit, count], $pad: [padded[1], padded[3]] }
        : { $repeat: [unit, count] };
    return value;
  }
  if (Array.isArray(value)) {
    if (
      value.length > 20 &&
      value.every((item) => JSON.stringify(item) === JSON.stringify(value[0]))
    )
      return { $items: [compress(value[0]), value.length] };
    return value.map(compress);
  }
  if (value && typeof value === "object")
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, compress(item)]));
  return value;
};

const spoilers = [
  null,
  "",
  " ",
  " x ",
  "x",
  0,
  -1,
  1.5,
  1e21,
  true,
  false,
  [],
  {},
  "NaN",
  "x".repeat(300),
];
const jsonOf = (value) => JSON.parse(JSON.stringify(value));

// Every place in a document, with the path to it.
const places = (value, path = [], out = []) => {
  out.push(path);
  if (Array.isArray(value)) value.forEach((item, index) => places(item, [...path, index], out));
  else if (value && typeof value === "object")
    for (const [key, item] of Object.entries(value)) places(item, [...path, key], out);
  return out;
};
const read = (root, path) => path.reduce((at, key) => at?.[key], root);
const writeAt = (root, path, replacement, remove) => {
  if (path.length === 0) return replacement;
  const copy = structuredClone(root);
  const parent = read(copy, path.slice(0, -1));
  const last = path[path.length - 1];
  if (remove) {
    if (Array.isArray(parent)) parent.splice(last, 1);
    else delete parent[last];
  } else parent[last] = replacement;
  return copy;
};
const mutants = (seed, random, count) => {
  const all = places(seed);
  const leaves = all.filter((path) => {
    const value = read(seed, path);
    return value === null || typeof value !== "object";
  });
  const out = [];
  for (let n = 0; n < count; n++) {
    const path = all[Math.floor(random() * all.length)];
    const value = read(seed, path);
    const roll = random();
    if (path.length === 0 || roll < 0.45)
      out.push(writeAt(seed, path, spoilers[Math.floor(random() * spoilers.length)], false));
    else if (roll < 0.62) out.push(writeAt(seed, path, undefined, true));
    else if (roll < 0.77 && value && typeof value === "object" && !Array.isArray(value))
      out.push(writeAt(seed, path, { ...value, unexpected: 1 }, false));
    else if (roll < 0.9 && leaves.length) {
      // Another value of the document in this place: the right type, the wrong meaning.
      const other = read(seed, leaves[Math.floor(random() * leaves.length)]);
      out.push(writeAt(seed, path, other, false));
    } else if (Array.isArray(value)) out.push(writeAt(seed, path, [], false));
    else out.push(writeAt(seed, path, value === null ? "x" : null, false));
  }
  return out;
};

const sizeOf = (value) => JSON.stringify(value).length;
// Values of a schema, cut to what fits in a fixture.
const samples = (schema, seedName, count, limit) => {
  let arbitrary;
  try {
    arbitrary = Arbitrary.schema(schema);
  } catch {
    return undefined;
  }
  const found = [];
  for (let attempt = 0; attempt < 6 && found.length < count; attempt++) {
    let values;
    try {
      values = Effect.runSync(
        Arbitrary.sampleEffect(arbitrary, {
          count: count * 2,
          size: 1 + attempt,
          maxDiscards: 20000,
          seed: hash(seedName) + attempt,
        }),
      );
    } catch {
      return found.length ? found : undefined;
    }
    for (const value of values) found.push(value);
  }
  return found;
};

const rows = [];
const skipped = [];
const brokenByOriginal = [];
const covered = new Map();
for (const [method, rpc] of WsRpcGroup.requests) {
  const stream = RpcSchema.getStreamSchemas(rpc.successSchema);
  const exitCodec = Schema.toCodecJson(Rpc.exitSchema(rpc));
  const chunkCodec = Option.isSome(stream)
    ? Schema.toCodecJson(Schema.NonEmptyArray(stream.value.success))
    : undefined;
  const parts = [
    ["exit", exitCodec, roots[method].exit],
    ...(chunkCodec ? [["chunk", chunkCodec, roots[method].chunk]] : []),
  ];
  for (const [part, codec, root] of parts) {
    const key = `${part}:${root}`;
    if (covered.has(key)) continue;
    covered.set(key, method);
    const decode = Schema.decodeUnknownExit(codec);
    const encode = Schema.encodeUnknownSync(codec);
    const random = prng(hash(`${method}:${part}`));
    // The decoded values to encode: successes and typed errors, and the failures that are not typed.
    const wires = [];
    const encodeAll = (values, limit = 14000) => {
      for (const value of values) {
        try {
          const wire = jsonOf(encode(value));
          if (sizeOf(wire) <= limit) wires.push(wire);
        } catch {
          // A value the encoder refuses is not a reply.
        }
      }
    };
    if (part === "chunk") {
      const element = samples(stream.value.success, `${method}:chunk`, 14, 12000);
      if (element) encodeAll(element.map((value) => [value]));
      else skipped.push(`${method} chunk`);
    } else {
      if (Option.isNone(stream)) {
        const found = samples(rpc.successSchema, `${method}:success`, 8, 12000);
        if (found) encodeAll(found.map((value) => Exit.succeed(value)));
        else skipped.push(`${method} success`);
      } else encodeAll([Exit.succeed(undefined)]);
      // The failures the exit schema holds: the method's, the stream's, and the middleware's.
      const failures = [
        rpc.errorSchema,
        ...(Option.isSome(stream) ? [stream.value.error] : []),
        ...[...rpc.middlewares].map((middleware) => middleware.error),
      ].filter((failure) => failure.ast._tag !== "Never");
      for (const [position, failure] of failures.entries()) {
        const errors = samples(failure, `${method}:error:${position}`, 3, 4000);
        if (errors?.length) encodeAll(errors.map((error) => Exit.fail(error)));
        else if (errors === undefined) skipped.push(`${method} error ${position}`);
      }
      if (part === "exit")
        encodeAll([
          Exit.die("boom"),
          Exit.failCause(Cause.interrupt(1)),
          Exit.failCause(Cause.interrupt()),
        ]);
    }
    const seen = new Set();
    const inputs = [];
    const add = (input) => {
      const text = JSON.stringify(input) ?? "undefined";
      if (seen.has(text)) return;
      // A lone surrogate is a string no Rust server can send, and the fixture's reader cannot hold.
      if (/\\u[dD][89a-fA-F][0-9a-fA-F]{2}/.test(text)) return;
      seen.add(text);
      inputs.push(input);
    };
    // A reply of each kind first, then mutations of them, within what a schema may take of the fixture.
    let budget = 20000;
    wires.sort((a, b) => sizeOf(a) - sizeOf(b));
    for (const wire of wires.slice(0, 24)) {
      if (budget < 0) break;
      add(wire);
      budget -= sizeOf(wire);
    }
    // More mutations of a larger document, which has more places to go wrong.
    for (const wire of wires.slice(0, 24)) {
      const count = Math.min(8, 1 + Math.floor(sizeOf(wire) / 500));
      for (const mutant of mutants(wire, random, count)) {
        if (budget < 0) break;
        add(mutant);
        budget -= sizeOf(mutant ?? null);
      }
    }
    for (const input of [
      null,
      {},
      [],
      "x",
      { _tag: "Success" },
      { _tag: "Failure" },
      { _tag: "Failure", cause: [] },
    ])
      add(input);
    for (const input of inputs) {
      const exit = decode(input, { onExcessProperty: "error" });
      const ok = exit._tag === "Success";
      const row = { method, part, input: compress(input), accepted: ok };
      if (!ok) row.message = SchemaIssue.defaultFormatter(Cause.squash(exit.cause).issue);
      rows.push(row);
    }
    // The encoder's own output must be a reply the decoder takes.
    for (const wire of wires) {
      if (decode(wire, { onExcessProperty: "error" })._tag !== "Success")
        brokenByOriginal.push(`${method} ${part}`);
    }
  }
}

const target = new URL("../crates/server/tests/fixtures/rpc-response-cases.jsonl", import.meta.url);
if (process.argv.includes("--check")) {
  const committed = readFileSync(target, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const same = JSON.stringify(committed) === JSON.stringify(rows);
  console.log(JSON.stringify({ rows: rows.length, same }));
  process.exit(same ? 0 : 1);
}
const text = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
writeFileSync(target, text);
console.log(
  JSON.stringify({
    rows: rows.length,
    bytes: text.length,
    accepted: rows.filter((row) => row.accepted).length,
    schemas: covered.size,
    skipped,
    brokenByOriginal: [...new Set(brokenByOriginal)],
  }),
);
