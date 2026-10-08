// Source codec oracle for client-local settings. From repository root:
// node >=24.13.1 rust/tools/generate_client_settings_fixtures.mjs
// Rust tests use the compressed artifact and need no Node/TypeScript runtime.
import { fileURLToPath, pathToFileURL } from "node:url";
import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const S = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
const sources = await Promise.all(
  ["settings", "preview", "browserProfile"].map(
    (file) => import(pathToFileURL(root + "/packages/contracts/src/" + file + ".ts")),
  ),
);
const rust = new Set(
  [
    ...readFileSync(root + "/rust/crates/contracts/src/client_settings.rs", "utf8").matchAll(
      /pub (?:enum|struct|type) (\w+)/g,
    ),
  ].map((m) => m[1]),
);
const cases = [],
  mapped = new Set();
function test(name, s, input, label) {
  const codec = S.toCodecJson(s);
  try {
    const decoded = S.decodeUnknownSync(codec)(input);
    const output = S.encodeUnknownSync(codec)(decoded);
    cases.push({ schema: name, label, input, valid: true, output });
  } catch {
    cases.push({ schema: name, label, input, valid: false });
  }
}
function seed(s, defs = {}) {
  if (s.$ref) return seed(defs[s.$ref.split("/").pop()], defs);
  if (s.enum) return s.enum[0];
  if (s.anyOf) return seed(s.anyOf.find((x) => x.type !== "null") ?? s.anyOf[0], defs);
  if (s.type === "string") return "sample";
  if (s.type === "integer" || s.type === "number") return s.minimum ?? 1;
  if (s.type === "boolean") return false;
  if (s.type === "array") return [];
  if (s.type === "object")
    return Object.fromEntries((s.required ?? []).map((k) => [k, seed(s.properties[k], defs)]));
  throw new Error("Unseeded schema " + JSON.stringify(s));
}
for (const module of sources)
  for (const [name, s] of Object.entries(module)) {
    if (!rust.has(name) || !S.isSchema(s)) continue;
    const doc = S.toJsonSchemaDocument(s),
      initial = seed(doc.schema, doc.definitions);
    mapped.add(name);
    test(name, s, initial, "baseline");
    for (const input of [null, false, 42, "", [], {}, " sample ", 1.0, 1.5, 9007199254740992])
      test(name, s, input, "scalar boundary");
    const variants = doc.schema.anyOf ?? [doc.schema];
    for (const variant of variants) {
      const baseline = seed(variant, doc.definitions);
      test(name, s, baseline, "union baseline");
      if (variant.enum) for (const value of variant.enum) test(name, s, value, "literal member");
      for (const [key, schema] of Object.entries(variant.properties ?? {})) {
        const missing = { ...baseline };
        delete missing[key];
        test(name, s, missing, "missing " + key);
        for (const v of [
          null,
          true,
          false,
          42,
          "",
          [],
          {},
          " sample ",
          seed(schema, doc.definitions),
        ])
          test(name, s, { ...baseline, [key]: v }, "field " + key);
      }
    }
  }
const settings = sources[0];
for (const [name, s] of [
  ["ClientSettingsSchema", settings.ClientSettingsSchema],
  ["ClientSettingsPatch", settings.ClientSettingsPatch],
]) {
  const baseline = { ...settings.DEFAULT_CLIENT_SETTINGS };
  delete baseline.dismissedProviderUpdateNotificationKeys;
  if (name === "ClientSettingsSchema") baseline.dismissedProviderUpdateNotificationKeys = [];
  test(name, s, baseline, "complete source defaults");
  for (const [key, value] of Object.entries(baseline))
    for (const v of [null, 42, false, "", {}, [], value])
      test(name, s, { ...baseline, [key]: v }, "complete field " + key);
  for (const input of [
    { favorites: [{ provider: " codex ", model: " gpt-5 " }] },
    { favorites: [{ provider: "Invalid ID", model: "gpt-5" }] },
    { favorites: [["codex", "gpt-5"]] },
    { browserProfiles: [["id", "name", "persistent"]] },
    { providerModelPreferences: { codex: [[], []] } },
    { snapShotShortcut: ["a", false, false, true, false, false] },
    { providerModelPreferences: { codex: { hiddenModels: null } } },
    { providerModelPreferences: { " bad id ": {} } },
    { providerModelPreferences: { " codex ": { hiddenModels: [""], modelOrder: [" gpt-5 "] } } },
    { loadBalancingWeights: { " sample ": 20 } },
    { loadBalancingWeights: { " ": 20 } },
    { browserDefaultViewport: { _tag: "freeform", width: 3840, height: 3840 } },
    {
      browserDefaultViewport: { _tag: "preset", width: 3840, height: 2160, presetId: "iphone-se" },
    },
    { browserProfiles: [{ id: "id\u0080", name: "Browser", kind: "persistent" }] },
    { browserProfiles: [{ id: "id", name: "😀".repeat(25), kind: "persistent" }] },
    {
      snapShotShortcut: {
        key: " a ",
        metaKey: false,
        ctrlKey: false,
        shiftKey: false,
        altKey: false,
        modKey: false,
      },
    },
    {
      snapShotShortcut: {
        key: " a ",
        metaKey: false,
        ctrlKey: false,
        shiftKey: false,
        altKey: false,
        modKey: true,
      },
    },
    { confirmQuit: true },
    { confirmQuit: false },
    { fontFamilySans: "😀".repeat(101) },
  ])
    test(name, s, input, "nested/transformed boundary");
}
const unique = [...new Map(cases.map((c) => [JSON.stringify([c.schema, c.input]), c])).values()];
writeFileSync(
  root + "/rust/crates/contracts/tests/fixtures/client-settings.jsonl.gz",
  gzipSync(unique.map((c) => JSON.stringify(c)).join("\n") + "\n", { level: 9 }),
);
const testPath = root + "/rust/crates/contracts/tests/client_settings.rs";
let content = readFileSync(testPath, "utf8");
const beginMarker = "        // BEGIN SOURCE CODEC DISPATCH\n",
  endMarker = "        // END SOURCE CODEC DISPATCH";
const raw = content.indexOf(beginMarker),
  end = content.indexOf(endMarker);
if (raw < 0 || end < raw) throw new Error("Missing or unordered codec dispatch markers");
const dispatch =
  [...mapped]
    .sort()
    .map((name) => `        "${name}" => roundtrip::<${name}>(input),`)
    .join("\n") + "\n";
content = content.slice(0, raw + beginMarker.length) + dispatch + content.slice(end);
writeFileSync(testPath, content);
console.log(`${unique.length} source JSON codec cases across ${mapped.size} schemas`);
