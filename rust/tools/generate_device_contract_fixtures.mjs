import { execFileSync } from "node:child_process";
// Development-only Effect oracle. Rust runtime/tests never execute TypeScript.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const Schema = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
const rust = readFileSync(root + "/rust/crates/contracts/src/device.rs", "utf8");
const rustNames = new Set(
  [
    ...rust.matchAll(/pub (?:struct|enum|type) (\w+)|(?:vocabulary|protocol_union)!\s*\{\s*(\w+)/g),
  ].map((match) => match[1] ?? match[2]),
);
rustNames.add("DeviceHostId");
rustNames.add("DeviceId");
const strictObjectTypes = rustNames;
const fixtures = [];
// Reuse the bounded mutation cases from the original broad oracle without
// changing its owned dispatcher or fixture corpus.
const generator = readFileSync(root + "/rust/tools/generate_contract_fixtures.mjs", "utf8");
const helpers = generator
  .slice(generator.indexOf("function seed("), generator.indexOf("const skipped = []"))
  .replace(
    "const all = s.anyOf ?? s.oneOf;",
    "const all = s.anyOf ?? s.oneOf; if(all.some(x=>x.properties?._tag?.enum?.includes('None')))return {_tag:'None'};",
  );
const { seed, codecCases } = new Function(
  "Schema",
  "fixtures",
  "strictObjectTypes",
  helpers + "\nreturn {seed,codecCases};",
)(Schema, fixtures, strictObjectTypes);
const moduleDevice = await import(pathToFileURL(root + "/packages/contracts/src/device.ts"));
const mapping = {},
  skipped = [];
for (const file of ["device"]) {
  const module = await import(pathToFileURL(root + "/packages/contracts/src/" + file + ".ts"));
  for (const [name, schema] of Object.entries(module)) {
    if (!rustNames.has(name) || !Schema.isSchema(schema)) continue;
    const doc = Schema.toJsonSchemaDocument(schema);
    if (codecCases(name, schema, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
    else skipped.push(name);
  }
}
function witness(name, input, label) {
  const codec = Schema.toCodecJson(moduleDevice[name]);
  let decoded;
  try {
    decoded = Schema.decodeUnknownSync(codec)(input);
  } catch {
    fixtures.push({ schema: name, label, input, valid: false, decoded_valid: false });
    return;
  }
  try {
    fixtures.push({
      schema: name,
      label,
      input,
      valid: true,
      decoded_valid: true,
      output: Schema.encodeUnknownSync(codec)(decoded),
    });
  } catch {
    fixtures.push({ schema: name, label, input, valid: false, decoded_valid: true });
  }
}
for (const name of ["DeviceHostId", "DeviceId"]) {
  const schema = moduleDevice[name],
    limit = name === "DeviceHostId" ? 128 : 256;
  const doc = Schema.toJsonSchemaDocument(schema);
  const cases = [
    "",
    " ",
    " local ",
    "\uFEFFalias\u00A0",
    "\u0085",
    "a".repeat(limit),
    "a".repeat(limit + 1),
    "😀".repeat(limit / 2),
    "😀".repeat(limit / 2) + "x",
    false,
    [],
    null,
  ];
  for (const input of cases) witness(name, input, "direct scalar boundary");
}
const recordDoc = Schema.toJsonSchemaDocument(moduleDevice.DeviceServiceState);
for (const hostStatuses of [
  { " a ": { status: "idle" }, a: { status: "failed" } },
  { "\uFEFFlocal\u00A0": { status: "ready" } },
  { ["😀".repeat(65)]: { status: "idle" } },
  { "": { status: "idle" } },
]) {
  const value = seed(recordDoc.schema, recordDoc.definitions);
  value.hostStatuses = hostStatuses;
  witness("DeviceServiceState", value, "normalized host record keys");
}
const unique = [...new Map(fixtures.map((f) => [JSON.stringify([f.schema, f.input]), f])).values()];
writeFileSync(
  root + "/rust/crates/contracts/tests/fixtures/device.jsonl.tmp",
  unique.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
renameSync(
  root + "/rust/crates/contracts/tests/fixtures/device.jsonl.tmp",
  root + "/rust/crates/contracts/tests/fixtures/device.jsonl",
);
const testPath = root + "/rust/crates/contracts/tests/device.rs";
let test = readFileSync(testPath, "utf8");
const begin = test.indexOf("// BEGIN RESOURCE DISPATCH") + "// BEGIN RESOURCE DISPATCH".length;
const end = test.indexOf("// END RESOURCE DISPATCH", begin);
if (begin < 0 || end < 0) throw Error("Missing resource oracle dispatcher markers");
test =
  test.slice(0, begin) +
  "\n" +
  Object.keys(mapping)
    .map((name) => `"${name}"=>roundtrip::<${name}>(fixture.input.clone()),`)
    .join("\n") +
  "\n" +
  test.slice(end);
writeFileSync(testPath, test);
execFileSync("rustfmt", ["--edition", "2024", testPath]);
console.log(JSON.stringify({ cases: unique.length, codecs: Object.keys(mapping).length, skipped }));
