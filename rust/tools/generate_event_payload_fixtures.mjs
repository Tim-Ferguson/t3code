// Event payloads as the original writes them: values its own schemas generate, encoded the way its event store encodes them.
//
// `OrchestrationV2DomainEventJson` is the codec of `orchestration_events.payload_json` for a thread event, and
// `ApplicationProjectEvent` the one for a project event. Every member of each union (and every member of a union a payload is,
// `OrchestrationV2TurnItem` being one) is sampled on its own, at growing sizes, so optional fields appear in some samples. The
// Rust server must carry every field of every sample: `crates/server/tests/event_payload_roundtrip.rs` decodes each payload with
// the typed codecs, reduces it and compares what is kept with what the original wrote.
//
// Deterministic (seeded). `--check` regenerates in memory and fails when the committed file differs.
//   node rust/tools/generate_event_payload_fixtures.mjs [--check]
import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync, gunzipSync } from "node:zlib";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Arbitrary from "../../apps/server/node_modules/effect/dist/Arbitrary.js";
import { OrchestrationV2DomainEventJson } from "../../packages/contracts/src/orchestrationV2.ts";
import { ApplicationProjectEvent } from "../../packages/contracts/src/applicationEvent.ts";

const out = new URL("../crates/server/tests/fixtures/event-payloads.jsonl.gz", import.meta.url);
const hash = (text) => {
  let h = 2166136261;
  for (const c of text) h = Math.imul(h ^ c.charCodeAt(0), 16777619) >>> 0;
  return h;
};
const jsonOf = (value) => JSON.parse(JSON.stringify(value));
// The arbitrary DateTime reaches years the original never wrote ("+039926-08-10..."); a timestamp outside 1971..2099 is replaced
// by one inside, derived from the original so equal times stay equal and the order of two is kept.
const STAMP = /^([+-]?\d{4,6})-(\d\d)-(\d\d)T(\d\d:\d\d:\d\d)(\.\d+)?Z$/;
const realistic = (value) => {
  if (typeof value === "string") {
    const found = STAMP.exec(value);
    if (found && !(Number(found[1]) > 1970 && Number(found[1]) < 2100 && found[1].length === 4)) {
      const year = 1971 + (Math.abs(Number(found[1])) % 120);
      const day = Math.min(Number(found[3]), 28);
      return `${year > 2099 ? year - 100 : year}-${found[2]}-${String(day).padStart(2, "0")}T${found[4]}${found[5] ?? ".000"}Z`;
    }
    // A lone surrogate is not text a JSON reader outside JavaScript accepts.
    return value.toWellFormed();
  }
  if (Array.isArray(value)) return value.map(realistic);
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key.toWellFormed(), realistic(item)]),
    );
  return value;
};
const members = (schema) => (Array.isArray(schema?.members) ? schema.members : [schema]);
const literals = (schema) => {
  const ast = schema.ast;
  if (ast?._tag === "Literal") return [ast.literal];
  if (ast?._tag === "Union") return ast.types.map((type) => type.literal);
  return [];
};
const SIZE_LIMIT = 30000;
const PER_MEMBER = 14;
const SIZES = [16, 24, 36, 52, 80, 120, 200, 300];

// Values the sampler's arbitrary cannot reach: nested optional fields hide behind a size budget. These are built from the schema's
// type side and then encoded as the event store encodes: every object carries all of its optional fields, a union takes the member
// a rotating counter names, and a leaf is a value its own arbitrary makes. A value a composite check refuses is discarded by
// decoding it.
const CANDIDATES = [
  "image/png",
  "image/jpeg",
  "text/plain",
  "application/json",
  "https://example.com/a.png",
  "/tmp/x",
  "a",
  "abc",
  "x1",
  "0",
  "2026-01-02T03:04:05.000Z",
  "main",
  "feature/x",
  "owner/repo",
  "github.com",
  "abc123def4567890abc123def4567890abc12345",
  "cmd",
  "name",
  "web",
  "ok",
  0,
  1,
  2,
  3,
  100,
  1024,
];
const leafArbitraries = new Map();
const leaf = (ast, seed) => {
  let arbitrary = leafArbitraries.get(ast);
  if (!arbitrary) {
    arbitrary = Arbitrary.schema(Schema.make(ast));
    leafArbitraries.set(ast, arbitrary);
  }
  try {
    const [value] = Effect.runSync(
      Arbitrary.sampleEffect(arbitrary, { count: 1, size: 6, maxDiscards: 3000, seed }),
    );
    return value;
  } catch (error) {
    // A leaf whose checks the arbitrary cannot meet by chance (a mime type, a pattern): the first of these it accepts.
    const accepts = Schema.is(Schema.make(ast));
    const found = CANDIDATES.filter(accepts);
    if (found.length === 0) throw error;
    return found[seed % found.length];
  }
};
const maximal = (ast, state, depth = 0) => {
  switch (ast._tag) {
    case "Objects": {
      const object = {};
      for (const property of ast.propertySignatures) {
        const optional = property.type.context?.isOptional === true;
        if (optional && state.sparse && state.next() % 3 === 0) continue;
        const value = maximal(property.type, state, depth + 1);
        if (value !== undefined) object[property.name] = value;
      }
      for (const index of ast.indexSignatures) {
        const key = leaf(index.parameter, state.next());
        object[String(key)] = maximal(index.type, state, depth + 1);
      }
      return object;
    }
    case "Arrays": {
      const items = ast.elements.map((element) => maximal(element, state, depth + 1));
      if (ast.rest.length > 0)
        for (let n = 0; n < 1 + (state.next() % 2); n++)
          items.push(maximal(ast.rest[0], state, depth + 1));
      return items;
    }
    case "Union": {
      const types = ast.types.filter((type) => type._tag !== "Undefined");
      if (types.length === 0) return undefined;
      const pick = types[state.next() % types.length];
      return depth > 40 ? leaf(pick, state.next()) : maximal(pick, state, depth + 1);
    }
    case "Suspend":
      return depth > 12 ? null : maximal(ast.thunk(), state, depth + 1);
    case "Undefined":
      return undefined;
    default:
      return leaf(ast, state.next());
  }
};
const counter = (seed, sparse) => {
  let n = hash(seed);
  return { sparse, next: () => (n = (Math.imul(n, 1103515245) + 12345) >>> 0) >>> 8 };
};

const rows = [];
const emit = (aggregate, member, seedName) => {
  const types = literals(member.fields.type);
  const payloads = member.fields.payload;
  // A payload that is itself a union is split, so each variant is sampled.
  const variants = members(payloads);
  for (const [index, variant] of variants.entries()) {
    const event = Schema.Struct({ ...member.fields, payload: variant });
    let arbitrary;
    try {
      arbitrary = Arbitrary.schema(event);
    } catch (error) {
      console.error(`cannot generate ${types.join(",")} variant ${index}: ${error.message}`);
      continue;
    }
    const encode = Schema.encodeUnknownSync(event);
    const decode = Schema.decodeUnknownSync(event);
    // A payload schema shared by several event types is sampled enough for each type to appear several times.
    const wanted = Math.max(PER_MEMBER, types.length * 3);
    for (let attempt = 0; attempt < SIZES.length; attempt++) {
      let kept = 0;
      let values;
      try {
        values = Effect.runSync(
          Arbitrary.sampleEffect(arbitrary, {
            count: Math.ceil(wanted / 6),
            // An object takes as many optional fields as its size allows beyond its required ones: sizes run well above a payload's
            // required field count, or no optional field would ever appear.
            size: SIZES[attempt],
            maxDiscards: 20000,
            seed: hash(`${seedName}:${types.join(",")}:${index}`) + attempt,
          }),
        );
      } catch {
        continue;
      }
      for (const value of values) {
        let wire;
        try {
          wire = realistic(jsonOf(encode(value)));
        } catch {
          continue;
        }
        const text = JSON.stringify(wire.payload);
        if (text.length > SIZE_LIMIT) continue;
        // The type is one of the member's literals, rotated, so every type of a shared payload schema appears.
        const type = types[(kept + attempt + rows.length) % types.length];
        // What the original reads the payload as and writes back (decoding defaults filled in): its projection rows are this.
        let canonical;
        try {
          canonical = jsonOf(encode(decode(wire))).payload;
        } catch {
          continue;
        }
        rows.push({ aggregate, type, payload: wire.payload, canonical });
        kept++;
        if (kept >= Math.ceil(wanted / 6)) break;
      }
    }
    // The maximal values: the same encoding and decoding as above.
    const eventAst = Schema.toType(event).ast;
    const wantedMaximal = Math.max(30, types.length * 3);
    let made = 0;
    for (let attempt = 0; attempt < wantedMaximal * 6 && made < wantedMaximal; attempt++) {
      let wire;
      try {
        wire = realistic(
          jsonOf(
            encode(
              maximal(
                eventAst,
                counter(`${seedName}:${types.join(",")}:${index}:${attempt}`, attempt % 3 === 2),
              ),
            ),
          ),
        );
        decode(wire);
      } catch (error) {
        if (process.env.DEBUG_MAXIMAL)
          console.error(
            types[0],
            (error?.message ?? JSON.stringify(error) ?? String(error))
              .split("\n")
              .slice(0, 3)
              .join(" | "),
          );
        continue;
      }
      const text = JSON.stringify(wire.payload);
      if (text.length > SIZE_LIMIT) continue;
      let canonical;
      try {
        canonical = jsonOf(encode(decode(wire))).payload;
      } catch {
        continue;
      }
      rows.push({ aggregate, type: types[made % types.length], payload: wire.payload, canonical });
      made++;
    }
    if (!rows.some((row) => types.includes(row.type) && row.aggregate === aggregate))
      console.error(`no sample for ${types.join(",")} variant ${index}`);
  }
};

for (const member of OrchestrationV2DomainEventJson.members) emit("thread", member, "thread");
for (const member of ApplicationProjectEvent.members) emit("project", member, "project");

// Coverage: the key paths of each payload schema (its encoded side, as written) against the paths the samples carry.
const schemaPaths = (ast, at, out, seen = new Set()) => {
  const encoded = ast.encoding ? ast.encoding[ast.encoding.length - 1].to : ast;
  switch (encoded._tag) {
    case "Union":
      for (const type of encoded.types) schemaPaths(type, at, out, seen);
      break;
    case "Objects":
      for (const property of encoded.propertySignatures) {
        const path = `${at}.${String(property.name)}`;
        out.add(path);
        schemaPaths(property.type, path, out, seen);
      }
      for (const index of encoded.indexSignatures) schemaPaths(index.type, `${at}.*`, out, seen);
      break;
    case "Arrays":
      for (const element of [...encoded.elements, ...encoded.rest])
        schemaPaths(element, `${at}[]`, out, seen);
      break;
    case "Suspend":
      if (!seen.has(encoded)) schemaPaths(encoded.thunk(), at, out, new Set([...seen, encoded]));
      break;
    default:
      break;
  }
};
const valuePaths = (value, at, out) => {
  if (Array.isArray(value)) for (const item of value) valuePaths(item, `${at}[]`, out);
  else if (value && typeof value === "object")
    for (const [key, item] of Object.entries(value)) {
      out.add(`${at}.${key}`);
      valuePaths(item, `${at}.${key}`, out);
      valuePaths(item, `${at}.*`, out);
    }
};
if (process.argv.includes("--coverage")) {
  const expected = new Map();
  const note = (aggregate, member) => {
    for (const type of literals(member.fields.type)) {
      const set = expected.get(`${aggregate} ${type}`) ?? new Set();
      for (const variant of members(member.fields.payload)) schemaPaths(variant.ast, "", set);
      expected.set(`${aggregate} ${type}`, set);
    }
  };
  for (const member of OrchestrationV2DomainEventJson.members) note("thread", member);
  for (const member of ApplicationProjectEvent.members) note("project", member);
  for (const [key, paths] of expected) {
    const [aggregate, type] = key.split(" ");
    const seen = new Set();
    for (const row of rows)
      if (row.aggregate === aggregate && row.type === type) valuePaths(row.payload, "", seen);
    const missing = [...paths].filter((path) => !seen.has(path));
    console.log(
      `${key}: ${paths.size} paths, ${missing.length} not in any sample${missing.length ? ": " + missing.slice(0, 12).join(" ") : ""}`,
    );
  }
}
const text = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
if (process.argv.includes("--check")) {
  const committed = gunzipSync(readFileSync(out)).toString("utf8");
  if (committed !== text) {
    console.error("event-payloads.jsonl.gz is not what the original's schemas generate now");
    process.exit(1);
  }
  console.log(`event-payloads.jsonl.gz is current (${rows.length} payloads)`);
} else {
  writeFileSync(out, gzipSync(text, { level: 9, mtime: 0 }));
  const counts = {};
  for (const row of rows)
    counts[`${row.aggregate} ${row.type}`] = (counts[`${row.aggregate} ${row.type}`] ?? 0) + 1;
  console.log(`${rows.length} payloads, ${Object.keys(counts).length} event types`);
}
