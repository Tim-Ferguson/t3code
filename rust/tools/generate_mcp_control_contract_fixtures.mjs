// Execute unchanged source JSON codecs. No TypeScript is used by native tests.
import { readFileSync, writeFileSync } from "node:fs";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as control from "../../packages/contracts/src/orchestratorMcp.ts";
import * as metadata from "../../packages/contracts/src/threadMetadataMcp.ts";
const rust = readFileSync(
  new URL("../crates/contracts/src/mcp_control.rs", import.meta.url),
  "utf8",
);
const rustNames = new Set([...rust.matchAll(/pub (?:struct|enum|type) (\w+)/g)].map((m) => m[1]));
const fixtures = [],
  strictObjectTypes = rustNames;
const source = readFileSync(new URL("./generate_contract_fixtures.mjs", import.meta.url), "utf8");
const helpers = source.slice(
  source.indexOf("function seed("),
  source.indexOf("const skipped = []"),
);
const { seed, codecCases } = new Function(
  "Schema",
  "fixtures",
  "strictObjectTypes",
  helpers + ";return {seed,codecCases};",
)(Schema, fixtures, strictObjectTypes);
const names = [];
for (const module of [control, metadata])
  for (const [name, schema] of Object.entries(module)) {
    if (!rustNames.has(name) || !Schema.isSchema(schema)) continue;
    const doc = Schema.toJsonSchemaDocument(schema);
    const value =
      name === "ThreadMetadataMcpUpdateInput"
        ? { action: "rename", title: "New title" }
        : seed(doc.schema, doc.definitions);
    if (codecCases(name, schema, doc, value)) names.push(name);
  }
for (const [name, values] of [
  [
    "OrchestratorMcpThreadSendInput",
    [
      { threadId: " x ", message: "\uFEFFhello\u00A0" },
      { threadId: "x", message: "😀".repeat(60000) },
      { threadId: "x", message: "😀".repeat(60000) + "a" },
    ],
  ],
  [
    "ThreadMetadataMcpUpdateInput",
    [
      { action: "rename", title: "new" },
      {
        action: "rename",
        title: "x",
        pullRequest: { repository: "r", number: 1, url: "https://a" },
      },
      {
        action: "link_pull_request",
        pullRequest: { repository: "r", number: 1, url: "http:example.org" },
      },
      { action: "link_pull_request", pullRequest: { repository: "r", number: 1, url: "ftp://a" } },
      { action: "regenerate_title" },
      { action: "unlink_pull_request" },
    ],
  ],
]) {
  const codec = Schema.toCodecJson({ ...control, ...metadata }[name]);
  for (const input of values) {
    try {
      const decoded = Schema.decodeUnknownSync(codec)(input);
      fixtures.push({
        schema: name,
        label: "direct boundary",
        input,
        valid: true,
        decoded_valid: true,
        output: Schema.encodeUnknownSync(codec)(decoded),
      });
    } catch {
      fixtures.push({
        schema: name,
        label: "direct boundary",
        input,
        valid: false,
        decoded_valid: false,
      });
    }
  }
}
writeFileSync(
  new URL("../crates/contracts/tests/fixtures/mcp-control.jsonl", import.meta.url),
  fixtures.map(JSON.stringify).join("\n") + "\n",
);
let test = readFileSync(new URL("../crates/contracts/tests/device.rs", import.meta.url), "utf8");
const begin = test.indexOf("// BEGIN RESOURCE DISPATCH"),
  end = test.indexOf("// END RESOURCE DISPATCH");
test =
  test.slice(0, begin) +
  names
    .sort()
    .map((name) => `"${name}" => roundtrip::<${name}>(fixture.input.clone()),`)
    .join("\n") +
  test.slice(end + "// END RESOURCE DISPATCH".length);
test = test
  .replace(
    "device_codecs_match_original_effect_json_oracle",
    "cooperative_mcp_codecs_match_original_effect_json_oracle",
  )
  .replace("fixtures/device.jsonl", "fixtures/mcp-control.jsonl")
  .replace(".lines().enumerate()", ".split('\\n').filter(|line| !line.is_empty()).enumerate()");
writeFileSync(new URL("../crates/contracts/tests/mcp_control.rs", import.meta.url), test);
console.log(JSON.stringify({ schemas: names.length, cases: fixtures.length }));
