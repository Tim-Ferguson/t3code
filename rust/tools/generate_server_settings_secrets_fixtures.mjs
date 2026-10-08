// Source witnesses for private secret decisions and persisted JSONC decoding.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as SchemaTransformation from "../../packages/contracts/node_modules/effect/dist/SchemaTransformation.js";
import {
  ServerSettings,
  ServerSettingsError,
  ResponseStreamingMode,
  ProjectSettingsOverrides,
} from "../../packages/contracts/src/settings.ts";
import { ProjectId } from "../../packages/contracts/src/baseSchemas.ts";
import {
  ProviderInstanceId,
  ProviderDriverKind,
} from "../../packages/contracts/src/providerInstance.ts";
import {
  DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER,
  DEFAULT_MODEL_BY_PROVIDER,
  DEFAULT_TEXT_GENERATION_MODEL,
} from "../../packages/contracts/src/model.ts";
import { isModelSelectionProviderEnabled } from "../../packages/shared/src/serverSettings.ts";
import { resolveProviderInstanceEnabled } from "../../packages/contracts/src/settings.ts";
import { fromLenientJson } from "../../packages/shared/src/schemaJson.ts";
const source = readFileSync(
  new URL("../../apps/server/src/serverSettings.ts", import.meta.url),
  "utf8",
);
const extract = (from, to) => source.slice(source.indexOf(from), source.indexOf(to));
const helpers = extract(
  "function providerEnvironmentSecretName(",
  "export function redactServerSettingsForClient",
);
const persistence = extract(
  "  const persistProviderEnvironmentSecrets =",
  "  const rollbackProviderEnvironmentSecretWrites =",
);
const materialization = extract(
  "  const materializeProviderEnvironmentSecrets =",
  "  const materializeChanges =",
);
const factory = new Function(
  "Effect",
  "Option",
  "ProviderInstanceId",
  "secretStore",
  "ServerSettingsError",
  stripTypeScriptTypes(
    'const textEncoder=new TextEncoder();const textDecoder=new TextDecoder();const settingsPath="fixture";' +
      helpers +
      persistence +
      materialization,
    { mode: "strip" },
  ) +
    ";return {plan:persistProviderEnvironmentSecrets,materialize:materializeProviderEnvironmentSecrets};",
);
const codec = Schema.toCodecJson(ServerSettings),
  decode = Schema.decodeUnknownSync(codec),
  wire = Schema.encodeSync(codec),
  rows = [];
const env = (value, sensitive = true, valueRedacted) => ({
  name: "KEY",
  value,
  sensitive,
  ...(valueRedacted === undefined ? {} : { valueRedacted }),
});
const setups = [
  {},
  ...["plain", "", "••••••"].map((value) => ({
    bitbucket: { accessToken: value, apiToken: value },
    github: { tokens: { "GitHub.COM": value } },
  })),
  { providerInstances: { fixture: { driver: "codex", environment: [env("old"), env("last")] } } },
  { providerInstances: { fixture: { driver: "codex", environment: [env("old", false)] } } },
];
const nexts = [
  {},
  ...["plain", "", "••••••"].map((value) => ({
    bitbucket: { accessToken: value, apiToken: value },
    github: { tokens: { "GitHub.COM": value, "New.HOST": value } },
  })),
  ...["plain", "", "••••••"].flatMap((value) =>
    [undefined, false, true].flatMap((redacted) =>
      [false, true].map((sensitive) => ({
        providerInstances: {
          fixture: { driver: "codex", environment: [env(value, sensitive, redacted)] },
        },
      })),
    ),
  ),
  { providerInstances: { fixture: { driver: "codex", environment: [env("first"), env("last")] } } },
  {
    providerInstances: {
      fixture: {
        driver: "codex",
        environment: [{ name: "UTF8_KEY", value: "值😀", sensitive: true }],
      },
    },
  },
];
for (const rawCurrent of setups)
  for (const rawNext of nexts) {
    const current = decode(rawCurrent),
      next = decode(rawNext);
    const fn = factory(Effect, Option, ProviderInstanceId, null, ServerSettingsError);
    const result = Effect.runSync(fn.plan(current, next));
    rows.push({
      op: "plan",
      current: wire(current),
      next: wire(next),
      settings: wire(result.settings),
      changes: result.changes.map((change) =>
        change.kind === "write" ? { ...change, value: Array.from(change.value) } : change,
      ),
    });
  }
for (const bytes of [
  undefined,
  [],
  [239, 187, 191, 104, 105, 255],
  Array.from(new TextEncoder().encode("  token  ")),
  Array.from(new TextEncoder().encode("值😀")),
]) {
  const input = decode({
    providerInstances: { fixture: { driver: "codex", environment: [env("", true, true)] } },
    bitbucket: { accessToken: "••••••", apiToken: "••••••" },
    github: { tokens: { "GitHub.COM": "••••••" } },
  });
  const fn = factory(
    Effect,
    Option,
    ProviderInstanceId,
    {
      get: () =>
        Effect.succeed(bytes === undefined ? Option.none() : Option.some(Uint8Array.from(bytes))),
    },
    ServerSettingsError,
  );
  rows.push({
    op: "materialize",
    input: wire(input),
    ...(bytes === undefined ? {} : { bytes }),
    output: wire(Effect.runSync(fn.materialize(input))),
  });
}
const jsonSchemaCode = extract(
  "const PersistedResponseStreamingMode =",
  "const PersistedOptionalProviderSettings =",
);
const jsonDecoder = new Function(
  "Schema",
  "SchemaTransformation",
  "Effect",
  "fromLenientJson",
  "ServerSettings",
  "ResponseStreamingMode",
  "ProjectId",
  "ProjectSettingsOverrides",
  stripTypeScriptTypes(jsonSchemaCode, { mode: "strip" }) +
    ";return Schema.decodeUnknownSync(ServerSettingsJson);",
)(
  Schema,
  SchemaTransformation,
  Effect,
  fromLenientJson,
  ServerSettings,
  ResponseStreamingMode,
  ProjectId,
  ProjectSettingsOverrides,
);
for (const input of [
  "{}",
  '{/* valid block */"responseStreamingMode":"token",}',
  '{// valid line\n"responseStreamingMode":"token",}',
  '{"projectSettingsOverrides":{"fixture":{"responseStreamingMode":"token",},},}',
  '{/* retained // inside quoted JSON */"responseStreamingMode":"token",}',
  '{"worktreesDirectory":"//a/*b*/",}',
  "{/* unfinished",
  '{"deviceHosts":[],\uFEFF}',
  '{"deviceHosts":[],\u0085}',
  '{"responseStreamingMode":null}',
  "null",
]) {
  try {
    rows.push({ op: "decode", input, valid: true, output: wire(jsonDecoder(input)) });
  } catch {
    rows.push({ op: "decode", input, valid: false });
  }
}
const fallbackCode = extract(
  "function selectionSupportsTextGeneration(",
  "// Values under these keys are compared",
);
const fallback = new Function(
  "isModelSelectionProviderEnabled",
  "resolveProviderInstanceEnabled",
  "ProviderInstanceId",
  "ProviderDriverKind",
  "DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER",
  "DEFAULT_MODEL_BY_PROVIDER",
  "DEFAULT_TEXT_GENERATION_MODEL",
  stripTypeScriptTypes('const ACP_REGISTRY_DRIVER="acpRegistry";' + fallbackCode, {
    mode: "strip",
  }) + ";return resolveTextGenerationProvider;",
)(
  isModelSelectionProviderEnabled,
  resolveProviderInstanceEnabled,
  ProviderInstanceId,
  ProviderDriverKind,
  DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER,
  DEFAULT_MODEL_BY_PROVIDER,
  DEFAULT_TEXT_GENERATION_MODEL,
);
for (const input of [
  {},
  { providers: { codex: { enabled: false }, claudeAgent: { enabled: true } } },
  {
    providerInstances: { codex: { driver: "codex", enabled: false } },
    providers: { claudeAgent: { enabled: true } },
  },
  {
    providerInstances: { custom: { driver: "acpRegistry" } },
    textGenerationModelSelection: { instanceId: "custom", model: "default" },
  },
  ...["codex", "claudeAgent", "cursor", "grok", "pi", "opencode", "antigravity"].map((driver) => ({
    providers: Object.fromEntries(
      ["codex", "claudeAgent", "cursor", "grok", "pi", "opencode", "antigravity"].map((key) => [
        key,
        { enabled: key === driver },
      ]),
    ),
    textGenerationModelSelection: {
      instanceId: "missing",
      model: "unused",
      options: [{ id: "reasoningEffort", value: "high" }],
    },
  })),
]) {
  const settings = decode(input);
  rows.push({ op: "fallback", input: wire(settings), output: wire(fallback(settings)) });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/server-settings-secrets.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
