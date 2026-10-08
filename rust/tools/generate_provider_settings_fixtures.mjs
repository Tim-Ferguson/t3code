// Development-only source oracle. Run with Node24 TypeScript stripping and the
// original dependency checkout. The Rust runtime consumes only generated JSON.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const root = new URL("../../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");
const Schema = await import(new URL("packages/contracts/node_modules/effect/dist/Schema.js", root));
const Equal = await import(new URL("packages/contracts/node_modules/effect/dist/Equal.js", root));
const providerInstance = await import(new URL("packages/contracts/src/providerInstance.ts", root));
const Option = await import(new URL("packages/contracts/node_modules/effect/dist/Option.js", root));
const settings = await import(new URL("packages/contracts/src/settings.ts", root));
const auth = await import(new URL("packages/contracts/src/auth.ts", root));
function between(source, start, end) {
  const a = source.indexOf(start),
    b = source.indexOf(end, a + start.length);
  if (a < 0 || b < 0) throw Error(`Missing source boundary ${start}`);
  return source.slice(a, b);
}
const meta = read("apps/web/src/components/settings/providerDriverMeta.ts");
const driverNames = [
  "CodexSettings",
  "ClaudeSettings",
  "CursorSettings",
  "GrokSettings",
  "OpenCodeSettings",
  "AntigravitySettings",
  "PiSettings",
  "AcpRegistrySettings",
];
const definitions = new Function(
  "ProviderDriverKind",
  ...driverNames,
  stripTypeScriptTypes(
    between(
      meta,
      "const PROVIDER_CLIENT_DEFINITIONS:",
      "const PROVIDER_CLIENT_DEFINITION_BY_VALUE:",
    ),
  ) + "\nreturn PROVIDER_CLIENT_DEFINITIONS;",
)({ make: (value) => value }, ...driverNames.map((name) => settings[name]));
const form = read("apps/web/src/components/settings/ProviderSettingsForm.tsx");
const derive = new Function(
  "Schema",
  "Option",
  stripTypeScriptTypes(
    between(form, "function titleizeFieldKey", "let commandArgumentDraftId").replaceAll(
      "export function",
      "function",
    ),
  ) +
    stripTypeScriptTypes(
      between(
        form,
        "function readProviderConfigString",
        "interface ProviderSettingsFormProps",
      ).replaceAll("export function", "function"),
    ) +
    "\nreturn {derive:deriveProviderSettingsFields,next:nextProviderConfigWithFieldValue};",
)(Schema, Option);
const rows = [],
  fields = {};
for (const definition of definitions) {
  fields[definition.value] = derive.derive(definition, undefined);
  for (const value of [
    undefined,
    null,
    {},
    { source: "local" },
    { source: "registry" },
    { source: " local " },
    { source: 42 },
  ])
    rows.push({
      kind: "fields",
      driver: definition.value,
      config: value ?? null,
      expected: derive.derive(definition, value),
    });
}
fields.acpLocal = derive.derive(
  definitions.find((d) => d.value === "acpRegistry"),
  { source: "local" },
);
const configs = [
  null,
  {},
  [],
  ["first", { unknown: true }],
  false,
  "string",
  { unknown: { nested: [1, 2] }, field: "old" },
  { field: false, retained: 3 },
];
const values = [
  "",
  " ",
  "\ufeff",
  "\u0085",
  "a\u00a0",
  "   untouched  ",
  "\u2028\u3000",
  true,
  false,
];
for (const clearWhenEmpty of ["omit", "persist"])
  for (const defaultBooleanValue of [undefined, false, true])
    for (const config of configs)
      for (const value of values) {
        const field = {
          key: "field",
          control: "text",
          label: "Field",
          clearWhenEmpty,
          ...(defaultBooleanValue !== undefined ? { defaultBooleanValue } : {}),
        };
        rows.push({
          kind: "next",
          config,
          field,
          value,
          expected: derive.next(config, field, value) ?? null,
        });
      }
const accessSource = read("apps/web/src/components/settings/ProviderSettingsPanel.logic.ts");
const selected = new Function(
  stripTypeScriptTypes(
    between(
      accessSource,
      "export function resolveSelectedProviderEnvironmentId",
      "export type ProviderEnvironmentAccess",
    ).replaceAll("export function", "function"),
  ) + "\nreturn resolveSelectedProviderEnvironmentId;",
)();
for (const ids of [[], ["primary"], ["remote", "primary"], ["relay", "ssh"]])
  for (const selection of [null, "primary", "remote", "deleted"])
    for (const primary of [null, "primary", "relay"])
      rows.push({
        kind: "selected",
        ids,
        selected: selection,
        primary,
        expected: selected(
          ids.map((environmentId) => ({ environmentId, label: environmentId })),
          selection,
          primary,
        ),
      });
const access = new Function(
  "AuthProvidersManageScope",
  "sessionGrantsScope",
  stripTypeScriptTypes(
    between(
      accessSource,
      "function resolveSessionOperateAccess",
      "export function classifyProviderEnvironmentAccess",
    ).replaceAll("export function", "function"),
  ) +
    stripTypeScriptTypes(
      accessSource
        .slice(accessSource.indexOf("export function classifyProviderEnvironmentAccess"))
        .replaceAll("export function", "function"),
    ) +
    "\nreturn {operate:resolveSessionOperateAccess, classify:classifyProviderEnvironmentAccess};",
)(auth.AuthProvidersManageScope, auth.sessionGrantsScope);
for (const session of [
  null,
  { authenticated: false },
  { authenticated: true },
  { authenticated: true, scopes: ["providers:manage"] },
  { authenticated: true, scopes: ["orchestration:operate"] },
  { authenticated: true, scopes: ["orchestration:operate"], auth: {} },
  {
    authenticated: true,
    scopes: ["orchestration:operate"],
    auth: { serverUpdateScope: "environment:maintain" },
  },
])
  for (const isPending of [false, true])
    for (const hasError of [false, true]) {
      const input = { session, isPending, hasError };
      rows.push({ kind: "operate", input, expected: access.operate(input) });
    }
for (const connectionPhase of [
  "available",
  "offline",
  "connecting",
  "connected",
  "reconnecting",
  "error",
])
  for (const hasServerConfig of [false, true])
    for (const operateAccess of ["pending", "denied", "granted"]) {
      const input = { connectionPhase, hasServerConfig, operateAccess };
      rows.push({ kind: "access", input, expected: access.classify(input) });
    }
const panel = read("apps/web/src/components/settings/ProviderSettingsPanel.tsx");
const sourceRows = new Function(
  "settings",
  "serverProviders",
  "targetInstanceId",
  "selectedInstanceId",
  "PROVIDER_SETTINGS",
  "DEFAULT_UNIFIED_SETTINGS",
  "defaultInstanceIdForDriver",
  "ProviderDriverKind",
  "Equal",
  "resolveProviderInstanceEnabled",
  stripTypeScriptTypes(
    between(panel, "  const visibleProviderSettings =", "  const textGenerationModelSelection ="),
  ) +
    stripTypeScriptTypes(
      between(panel, "  const instancesByDriver =", "  const updateProviderInstance ="),
    ) +
    "\nreturn {rows,targetInstanceMissing,selectedId:selectedRow?.instanceId??null};",
);
const defaultProviders = settings.DEFAULT_UNIFIED_SETTINGS.providers;
const providerDefs = definitions.map((d) => ({ provider: d.value }));
const base = { providers: defaultProviders, providerInstances: {} };
const instances = [
  {},
  { codex: { driver: "codex", enabled: false, config: { opaque: 1 } } },
  { claudeAgent: { driver: "grok", enabled: true } },
  { work: { driver: "codex", enabled: false }, work2: { driver: "codex", enabled: true } },
  {
    local: { driver: "acpRegistry", config: { source: "local", commandPath: "dsh" } },
    fork: { driver: "forkRuntime", enabled: true },
  },
  { cursor: { driver: "cursor", enabled: false } },
];
const legacy = [
  defaultProviders,
  {},
  { ...defaultProviders, grok: { enabled: false, binaryPath: "custom", opaque: true } },
  { ...defaultProviders, cursor: { enabled: false, binaryPath: "custom" } },
];
for (const providerInstances of instances)
  for (const providers of legacy)
    for (const cursorAvailable of [false, true])
      for (const targetInstanceId of [undefined, "grok", "removed"]) {
        const input = { providers, providerInstances };
        const live = cursorAvailable ? [{ instanceId: "cursor" }] : [];
        const target = targetInstanceId ?? null;
        rows.push({
          kind: "instances",
          settings: input,
          providers: live,
          target,
          selected: target,
          expected: sourceRows(
            input,
            live,
            targetInstanceId,
            target ?? null,
            providerDefs,
            settings.DEFAULT_UNIFIED_SETTINGS,
            providerInstance.defaultInstanceIdForDriver,
            { make: (v) => v },
            Equal,
            settings.resolveProviderInstanceEnabled,
          ),
        });
      }
const dir = new URL("rust/crates/client/src/provider_settings/", root);
mkdirSync(dir, { recursive: true });
writeFileSync(
  new URL("drivers.json", dir),
  JSON.stringify(definitions.map(({ settingsSchema, ...definition }) => definition)) + "\n",
);
writeFileSync(new URL("defaults.json", dir), JSON.stringify(defaultProviders) + "\n");
writeFileSync(new URL("fields.json", dir), JSON.stringify(fields) + "\n");
writeFileSync(
  new URL("rust/crates/client/tests/fixtures/provider-settings.jsonl", root),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`${rows.length} original provider settings witnesses\n`);
