// Development oracle for RpcAuthorization's decoded settings-update scopes.
// Run with Node24 and the original dependency checkout.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const root = new URL("../../", import.meta.url);
const Schema = await import(new URL("packages/contracts/node_modules/effect/dist/Schema.js", root));
const settings = await import(new URL("packages/contracts/src/settings.ts", root));
const provider = await import(new URL("packages/contracts/src/providerInstance.ts", root));
const source = readFileSync(new URL("apps/server/src/auth/RpcAuthorization.ts", root), "utf8");
const start = source.indexOf("const SettingsUpdate =");
const end = source.indexOf("const requiredScopesForRpcCall =", start);
if (start < 0 || end < 0) throw Error("Missing original settings authorization boundary");
const resolve = new Function(
  "Schema",
  "ServerSettingsPatch",
  "ProviderInstanceMutation",
  "requiredScopesForServerSettingsPatch",
  "AuthProvidersManageScope",
  stripTypeScriptTypes(source.slice(start, end)) + "\nreturn requiredScopesForSettingsUpdate;",
)(
  Schema,
  settings.ServerSettingsPatch,
  provider.ProviderInstanceMutation,
  settings.requiredScopesForServerSettingsPatch,
  "providers:manage",
);
const patches = [
  {},
  { providers: {} },
  { providerInstances: {} },
  { usageLimitSources: {} },
  { enableProviderUpdateChecks: true },
  { defaultRuntimeMode: "full-access" },
  { providers: {}, enableProviderUpdateChecks: false },
  { providerInstances: {}, defaultRuntimeMode: "approval-required" },
  { unknown: "stripped" },
  { enableProviderUpdateChecks: "invalid" },
  null,
];
const mutations = [
  undefined,
  { operation: "remove", instanceId: "work_local" },
  {
    operation: "upsert",
    instanceId: "work_local",
    instance: { driver: "acpRegistry", enabled: false, config: { source: "local" } },
  },
  { operation: "create", instanceId: "new_local", instance: { driver: "acpRegistry" } },
  null,
  { operation: "invalid", instanceId: "work_local" },
];
const rows = patches.flatMap((patch) =>
  mutations.map((providerInstanceMutation) => {
    const input = {
      patch,
      ...(providerInstanceMutation !== undefined ? { providerInstanceMutation } : {}),
    };
    try {
      return { input, expected: resolve(input) };
    } catch (error) {
      if (error?.name !== "SchemaError") throw error;
      return { input, rejected: true };
    }
  }),
);
writeFileSync(
  new URL("rust/crates/ui/src/settings_rpc_scope_fixtures.jsonl", root),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`${rows.length} original settings RPC authorization witnesses\n`);
