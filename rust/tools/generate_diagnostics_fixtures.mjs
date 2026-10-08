import { execFileSync } from "node:child_process";
// Development-only Effect oracle. Rust runtime/tests never execute TypeScript.
import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const Schema = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
const rust =
  readFileSync(root + "/rust/crates/contracts/src/diagnostics.rs", "utf8") +
  readFileSync(root + "/rust/crates/contracts/src/resource_discovery.rs", "utf8");
const rustNames = new Set(
  [
    ...rust.matchAll(/pub (?:struct|enum|type) (\w+)|(?:vocabulary|protocol_union)!\s*\{\s*(\w+)/g),
  ].map((match) => match[1] ?? match[2]),
);
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
const mapping = {},
  skipped = [];
for (const file of ["server"]) {
  const module = await import(pathToFileURL(root + "/packages/contracts/src/" + file + ".ts"));
  for (const [name, schema] of Object.entries(module)) {
    if (!rustNames.has(name) || !Schema.isSchema(schema)) continue;
    const doc = Schema.toJsonSchemaDocument(schema);
    if (codecCases(name, schema, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
    else skipped.push(name);
  }
}
const unique = [...new Map(fixtures.map((f) => [JSON.stringify([f.schema, f.input]), f])).values()];
writeFileSync(
  root + "/rust/crates/contracts/tests/fixtures/diagnostics.jsonl",
  unique.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
const testPath = root + "/rust/crates/contracts/tests/diagnostics.rs";
let test = readFileSync(testPath, "utf8");
const begin = test.indexOf("// BEGIN RESOURCE DISPATCH") + "// BEGIN RESOURCE DISPATCH".length;
const end = test.indexOf("// END RESOURCE DISPATCH", begin);
if (begin < 0 || end < 0) throw Error("Missing resource oracle dispatcher markers");
test =
  test.slice(0, begin) +
  "\n" +
  Object.keys(mapping)
    .map((name) => `"${name}"=>roundtrip::<${name}>(fixture.input),`)
    .join("\n") +
  "\n" +
  test.slice(end);
writeFileSync(testPath, test);
execFileSync("rustfmt", ["--edition", "2024", testPath]);
console.log(JSON.stringify({ cases: unique.length, codecs: Object.keys(mapping).length, skipped }));
