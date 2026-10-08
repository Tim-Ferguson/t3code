// Load migration decisions from unchanged original serverSettings.ts.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import { ServerSettings } from "../../packages/contracts/src/settings.ts";
import { ModelSelection } from "../../packages/contracts/src/modelSelection.ts";
import { ProjectScript } from "../../packages/contracts/src/project.ts";
import { fromLenientJson } from "../../packages/shared/src/schemaJson.ts";
import { deriveLegacyProjectOverrides } from "../../packages/shared/src/serverSettings.ts";
const source = readFileSync(
  new URL("../../apps/server/src/serverSettings.ts", import.meta.url),
  "utf8",
);
const extract = (from, to) => source.slice(source.indexOf(from), source.indexOf(to));
const code =
  extract("function restoreUsedProviders(", "const ACP_REGISTRY_DRIVER") +
  extract("const decodeProjectScriptsJson =", "const make = Effect.gen");
const helpers = new Function(
  "Schema",
  "Option",
  "ModelSelection",
  "ProjectScript",
  "deriveLegacyProjectOverrides",
  stripTypeScriptTypes(code, { mode: "strip" }) +
    ";return {restore:restoreUsedProviders,fold:foldLegacyProjectSettings};",
)(Schema, Option, ModelSelection, ProjectScript, deriveLegacyProjectOverrides);
const codec = Schema.toCodecJson(ServerSettings),
  decode = Schema.decodeUnknownSync(codec),
  wire = Schema.encodeSync(codec),
  rows = [];
const histories = [
  [],
  ...["cursor", "grok", "opencode"].map((providerName) => [
    { providerName, providerInstanceId: null },
  ]),
  [
    { providerName: "cursor", providerInstanceId: "work" },
    { providerName: "opencode", providerInstanceId: "custom" },
    { providerName: "grok", providerInstanceId: "grok" },
  ],
];
const setups = [
  {},
  ...["cursor", "grok", "opencode"].flatMap((driver) =>
    [true, false].map((enabled) => ({ providers: { [driver]: { enabled } } })),
  ),
  ...["cursor", "grok", "opencode"].flatMap((driver) =>
    [undefined, true, false].map((enabled) => ({
      providerInstances: {
        work: { driver, ...(enabled === undefined ? {} : { enabled }), config: { enabled: false } },
      },
    })),
  ),
  {
    providerInstances: {
      custom: { driver: "opencode" },
      work: { driver: "cursor" },
      grok: { driver: "grok" },
    },
  },
];
for (const persisted of setups)
  for (const history of histories) {
    const input = decode(persisted);
    rows.push({
      op: "restore",
      input: wire(input),
      persisted,
      history,
      output: wire(helpers.restore(input, persisted, history)),
    });
  }
const script = {
  id: "check",
  name: "Check",
  command: "npm test",
  icon: "play",
  runOnWorktreeCreate: false,
};
const model = JSON.stringify({ instanceId: "codex", model: "gpt-5.5" });
const projectRows = [
  [],
  [
    {
      projectId: "legacy",
      defaultModelSelection: model,
      defaultThreadEnvMode: "worktree",
      autoPull: 1,
      scripts: JSON.stringify([script]),
    },
    {
      projectId: "scripted",
      defaultModelSelection: null,
      defaultThreadEnvMode: null,
      autoPull: 0,
      scripts: JSON.stringify([script]),
    },
  ],
  ...[
    null,
    "null",
    "{}",
    "[]",
    "false",
    "broken",
    '{"instanceId":"codex","model":"gpt-5.5","options":{"reasoningEffort":"high"}}',
  ].map((defaultModelSelection) => [
    {
      projectId: "legacy",
      defaultModelSelection,
      defaultThreadEnvMode: "unsupported",
      autoPull: 2,
      scripts: "broken",
    },
  ]),
  ...[
    null,
    "null",
    "{}",
    "[[]]",
    "[]",
    "broken",
    JSON.stringify([script]),
    JSON.stringify([Object.values(script)]),
    ...["runOnSettle", "async", "previewUrl", "autoOpenPreview"].map((key) =>
      JSON.stringify([{ ...script, [key]: null }]),
    ),
  ].map((scripts) => [
    {
      projectId: "legacy",
      defaultModelSelection: null,
      defaultThreadEnvMode: "local",
      autoPull: 0,
      scripts: scripts ?? "null",
    },
  ]),
];
for (const raw of [
  {},
  {
    projectAgentBrowserAccessOverrides: { legacy: false },
    projectAutoPullOverrides: { scripted: true },
    projectScriptOverrides: { legacy: null },
  },
  {
    projectSettingsOverrides: {
      legacy: { defaultAutoPull: false, defaultProjectScripts: [], defaultModelSelection: null },
    },
  },
  { projectSettingsFolded: true },
  { projectScriptOverrides: { legacy: [script] } },
])
  for (const projects of projectRows) {
    const input = decode(raw);
    rows.push({
      op: "fold",
      input: wire(input),
      projects,
      output: wire(helpers.fold(input, projects)),
    });
  }
// Independently decoded metadata survives full settings failure; unrelated
// values are deliberately ignored by this exact original schema.
const persistedCode = extract(
  "const PersistedOptionalProviderSettings =",
  "function restoreUsedProviders(",
);
const metadataDecoder = new Function(
  "Schema",
  "fromLenientJson",
  stripTypeScriptTypes(persistedCode, { mode: "strip" }) +
    ";return decodePersistedOptionalProviderSettingsJsonExit;",
)(Schema, fromLenientJson);
for (const input of [
  null,
  [],
  false,
  0,
  "text",
  {},
  { defaultAutoPull: "invalid", providers: { cursor: { enabled: false } } },
  ...[null, [], false, 0].map((providers) => ({ providers })),
  ...[
    null,
    [],
    false,
    0,
    {},
    { enabled: null },
    { enabled: 0 },
    { enabled: "false" },
    { enabled: false },
    { enabled: true },
    { enabled: false, unknown: "ignored" },
  ].map((cursor) => ({ providers: { cursor } })),
  { providers: { codex: null } },
  { providers: { grok: { enabled: false }, opencode: { enabled: true } } },
]) {
  const raw = JSON.stringify(input),
    result = metadataDecoder(raw);
  rows.push({
    op: "metadata",
    raw,
    accepted: result._tag === "Success",
    ...(result._tag === "Success" ? { output: result.value } : {}),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/settings-migrations.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
