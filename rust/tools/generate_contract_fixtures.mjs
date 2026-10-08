// Development-only reference generator. Requires the original Node dependencies.
// Rust runtime and Rust tests consume the checked-in JSON and do not execute TS.
// From repository root, after installing original dependencies:
//   node --version  # requires Node >=24.13.1 with built-in TypeScript stripping
//   node rust/tools/generate_contract_fixtures.mjs
//   cargo fmt --manifest-path rust/Cargo.toml --all
// No separate TypeScript loader is required; original relative imports use .ts.
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const Schema = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
import { readFileSync, writeFileSync } from "node:fs";
const fixtures = [],
  mapping = {};
const files = [
  "auth",
  "providerInstance",
  "model",
  "threadPullRequest",
  "orchestrationV2",
  "provider",
  "environment",
  "server",
  "providerUsageLimits",
  "acpRegistry",
  "settings",
  "device",
  "project",
  "keybindings",
  "editor",
];
const rustNames = new Set(
  [
    "auth",
    "provider",
    "thread_command",
    "orchestration",
    "server_config",
    "settings",
    "api_config",
  ].flatMap((f) =>
    [
      ...readFileSync(root + "/rust/crates/contracts/src/" + f + ".rs", "utf8").matchAll(
        /pub (?:struct|enum|type) (\w+)/g,
      ),
    ].map((m) => m[1]),
  ),
);
function seed(s, defs, key = "", depth = 0) {
  if (depth > 20) return null;
  if (s.$ref) return seed(defs[s.$ref.split("/").pop()], defs, key, depth + 1);
  if (/ModelSelection$/.test(key) || key === "modelSelection")
    return { instanceId: "codex", model: "gpt-6-astra" };
  if (key === "mimeType") return "image/png";
  if (key === "canvas" || key === "accent") return "#123abc";
  if (key === "autoCompactWindow") return "300000";
  if (s.const !== undefined) return s.const;
  if (s.enum) return s.enum[0];
  if (s.anyOf || s.oneOf) {
    const all = s.anyOf ?? s.oneOf;
    return seed(all.find((x) => x.type !== "null") ?? all[0], defs, key, depth + 1);
  }
  if (s.allOf) return seed(s.allOf[0], defs, key, depth + 1);
  if (s.type === "object") {
    const o = {};
    for (const p of s.required ?? []) o[p] = seed(s.properties[p], defs, p, depth + 1);
    return o;
  }
  if (s.type === "array") return [];
  if (s.type === "boolean") return false;
  if (s.type === "number" || s.type === "integer") return Math.max(1, s.minimum ?? 0);
  if (key === "modelSelection") return { instanceId: "codex", model: "gpt-6-astra" };
  if (key === "providerInstanceHistory") return [];
  if (s.type === "string") {
    if (/At$|Until$/.test(key)) return "2026-10-07T13:04:05Z";
    return "sample";
  }
  return {};
}
function codecCases(name, schema, schemaDoc, initial) {
  const codec = Schema.toCodecJson(schema),
    clone = (x) => JSON.parse(JSON.stringify(x));
  const test = (input, label) => {
    let decoded;
    try {
      decoded = Schema.decodeUnknownSync(codec)(input);
    } catch {
      fixtures.push({ schema: name, label, input, valid: false, decoded_valid: false });
      return false;
    }
    try {
      const output = Schema.encodeUnknownSync(codec)(decoded);
      fixtures.push({ schema: name, label, input, valid: true, decoded_valid: true, output });
      return true;
    } catch {
      fixtures.push({ schema: name, label, input, valid: false, decoded_valid: true });
      return false;
    }
  };
  if (!test(initial, "baseline")) {
    fixtures.pop();
    return false;
  }
  test({ ...initial, ignoredFutureField: { hello: true } }, "unknown field");
  if (schemaDoc.schema.type === "array") {
    const member = seed(schemaDoc.schema.items, schemaDoc.definitions);
    test([member], "array member");
    test([member, null, { future: true }], "mixed array");
  }
  const variants = schemaDoc.schema.anyOf ?? [schemaDoc.schema];
  for (const variant of variants) {
    const candidate = seed(variant, schemaDoc.definitions);
    test(candidate, "union variant");
    for (const [key, s] of Object.entries(variant.properties ?? {})) {
      const missing = clone(candidate);
      delete missing[key];
      test(missing, "missing " + key);
      for (const [label, v] of [
        ["null", null],
        ["number", 42],
        ["value", seed(s, schemaDoc.definitions, key)],
        ["empty", ""],
        ["boolean", false],
      ])
        test({ ...clone(candidate), [key]: v }, label + " " + key);
      const present = seed(s, schemaDoc.definitions, key);
      if (s.type === "string")
        test({ ...clone(candidate), [key]: " sample " }, "trim boundary " + key);
      if (s.type === "array") {
        const member = seed(s.items, schemaDoc.definitions, key);
        test({ ...clone(candidate), [key]: [member] }, "array member " + key);
        test(
          { ...clone(candidate), [key]: [member, null, { future: true }] },
          "mixed array " + key,
        );
      }
      if (typeof present === "number")
        for (const value of [-1, 0, 1.5, 9007199254740991, 9007199254740992])
          test({ ...clone(candidate), [key]: value }, "numeric boundary " + key);
      if (/At$|Until$/.test(key))
        for (const value of [
          "2026-10-07",
          "2026-10-07T13:04:05.123456Z",
          "2026-10-07T15:04:05+02:00",
          "not-a-date",
        ])
          test({ ...clone(candidate), [key]: value }, "date boundary " + key);
      if (present && typeof present === "object" && !Array.isArray(present)) {
        for (const nested of Object.keys(present)) {
          for (const value of [null, 42, ""])
            test(
              { ...clone(candidate), [key]: { ...clone(present), [nested]: value } },
              "nested " + key + "." + nested,
            );
          const missing = clone(present);
          delete missing[nested];
          test({ ...clone(candidate), [key]: missing }, "nested missing " + key + "." + nested);
        }
      }
    }
  }
  return true;
}
const skipped = [];
for (const file of files) {
  const mod = await import(pathToFileURL(root + "/packages/contracts/src/" + file + ".ts"));
  for (const [name, s] of Object.entries(mod)) {
    if (!rustNames.has(name) || !Schema.isSchema(s)) continue;
    const doc = Schema.toJsonSchemaDocument(s);
    let initial = seed(doc.schema, doc.definitions);
    if (name === "LimitRecoveryUpdate")
      initial = { runId: "run", resetAt: "time", autoResume: false };
    if (codecCases(name, s, doc, initial)) mapping[name] = name;
    else skipped.push(file + "." + name);
  }
}
const orch = await import(pathToFileURL(root + "/packages/contracts/src/orchestrationV2.ts"));
const mapped = {
  ProviderRef: "OrchestrationV2ProviderRef",
  ProviderThreadNativeMetadata: "OrchestrationV2ProviderThreadNativeMetadata",
  LimitRecovery: "OrchestrationV2LimitRecovery",
  LimitRecoveryUpdate: "OrchestrationV2LimitRecoveryUpdate",
  PendingBackgroundTask: "OrchestrationV2PendingBackgroundTask",
  ThreadForkSourcePoint: "OrchestrationV2ThreadForkSourcePoint",
};
for (const [rust, source] of Object.entries(mapped)) {
  const s = orch[source];
  if (!s) continue;
  const doc = Schema.toJsonSchemaDocument(s);
  let initial = seed(doc.schema, doc.definitions);
  if (rust === "LimitRecoveryUpdate")
    initial = { runId: "run", resetAt: "time", autoResume: false };
  if (codecCases(rust, s, doc, initial)) mapping[rust] = rust;
  else skipped.push(source);
}
const doc = Schema.toJsonSchemaDocument(orch.OrchestrationV2Command);
const tags = [
  ...readFileSync(root + "/rust/crates/contracts/src/thread_command.rs", "utf8").matchAll(
    /serde\(rename = "(thread\.[^"]+)"/g,
  ),
].map((m) => m[1]);
for (const variant of doc.schema.anyOf) {
  const tag = variant.properties?.type?.const ?? variant.properties?.type?.enum?.[0];
  if (!tags.includes(tag)) continue;
  let initial = seed(variant, doc.definitions);
  initial.modelSelection =
    initial.modelSelection === undefined
      ? undefined
      : { instanceId: "codex", model: "gpt-6-astra" };
  if (
    codecCases(
      "ThreadCommand",
      orch.OrchestrationV2Command,
      { schema: variant, definitions: doc.definitions },
      JSON.parse(JSON.stringify(initial)),
    )
  )
    mapping.ThreadCommand = "ThreadCommand";
  else skipped.push(tag);
}
const unique = [...new Map(fixtures.map((f) => [JSON.stringify([f.schema, f.input]), f])).values()];
writeFileSync(
  root + "/rust/crates/contracts/tests/fixtures/expanded-codecs.jsonl",
  unique.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
const testPath = root + "/rust/crates/contracts/tests/original_codec_parity.rs";
let rustTest = readFileSync(testPath, "utf8");
const markerIndex = rustTest.indexOf("// BEGIN ORIGINAL CODEC DISPATCH");
if (markerIndex < 0) throw new Error("Missing Rust fixture dispatcher beginning marker");
const begin = markerIndex + "// BEGIN ORIGINAL CODEC DISPATCH".length;
const end = rustTest.indexOf("// END ORIGINAL CODEC DISPATCH", begin);
if (begin < 0 || end < 0) throw new Error("Missing Rust fixture dispatcher markers");
rustTest =
  rustTest.slice(0, begin) +
  "\n" +
  Object.entries(mapping)
    .map(
      ([name, type]) =>
        `            "${name}" => checked_roundtrip::<${type}>(fixture.input.clone()),\n`,
    )
    .join("") +
  "            " +
  rustTest.slice(end);
writeFileSync(testPath, rustTest);
console.log(
  JSON.stringify(
    { fixtures: unique.length, codecs: Object.keys(mapping).length, skipped },
    null,
    2,
  ),
);

// Runtime default artifact: generated once from source, loaded by pure Rust.
const { DEFAULT_RESOLVED_KEYBINDINGS } = await import(
  pathToFileURL(root + "/packages/shared/src/keybindings.ts")
);
const { ResolvedKeybindingsConfig } = await import(
  pathToFileURL(root + "/packages/contracts/src/keybindings.ts")
);
writeFileSync(
  root + "/rust/crates/contracts/assets/default-keybindings.json",
  JSON.stringify(
    Schema.encodeUnknownSync(Schema.toCodecJson(ResolvedKeybindingsConfig))(
      DEFAULT_RESOLVED_KEYBINDINGS,
    ),
    null,
    2,
  ) + "\n",
);
