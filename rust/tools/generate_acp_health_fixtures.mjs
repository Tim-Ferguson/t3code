// Development oracle: Node24 with the unchanged source checkout dependencies.
// PATH=/tmp/node-v24.13.1-darwin-arm64/bin:$PATH node rust/tools/generate_acp_health_fixtures.mjs
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import {
  officialAcpRegistryIconUrlForAgentId,
  resolveOfficialAcpRegistryIconUrl,
} from "../../packages/contracts/src/acpRegistry.ts";
import { AcpRegistrySettings as Settings } from "../../packages/contracts/src/settings.ts";
import { createModelCapabilities } from "../../packages/shared/src/model.ts";
import { providerModelsFromSettings } from "../../apps/server/src/provider/providerSnapshot.ts";
import {
  acpRegistryProbeFailure,
  normalizeAcpRegistryAuthMethods,
} from "../../apps/server/src/provider/acp/AcpRegistryProbe.ts";
import * as Errors from "../../packages/effect-acp/src/errors.ts";
function extract(start, end) {
  const text = fs.readFileSync(
    new URL("../../apps/server/src/provider/Drivers/AcpRegistryDriver.ts", import.meta.url),
    "utf8",
  );
  const begin = text.indexOf(start),
    finish = text.indexOf(end, begin);
  if (begin < 0 || finish < 0) throw Error(`Missing ${start}`);
  return stripTypeScriptTypes(text.slice(begin, finish)).replaceAll("export function", "function");
}
const checked = new Function(
  "Effect",
  "createModelCapabilities",
  "providerModelsFromSettings",
  "officialAcpRegistryIconUrlForAgentId",
  "resolveOfficialAcpRegistryIconUrl",
  `const DRIVER_KIND='acpRegistry';const EMPTY_CAPABILITIES=createModelCapabilities({optionDescriptors:[]});${extract("function modelsFromDiscovery(", "export function acpRegistrySnapshotReadiness")}${extract("export function acpRegistrySnapshotReadiness(", "interface SnapshotIdentity")}${extract("function baseSnapshot(", "export function applyAcpRegistryAvailableCommands")}${extract("export function buildCheckedAcpRegistrySnapshot(", "export const checkAcpRegistryProviderStatus")}return buildCheckedAcpRegistrySnapshot;`,
)(
  Effect,
  createModelCapabilities,
  providerModelsFromSettings,
  officialAcpRegistryIconUrlForAgentId,
  resolveOfficialAcpRegistryIconUrl,
);
const rows = [];
const add = (operation, input, output) => rows.push({ operation, input, output });
const identity = {
  instanceId: "fixture",
  displayName: "Fixture agent",
  accentColor: "#123456",
  continuationKey: "acpRegistry:instance:fixture",
  checkedAt: "2026-10-08T00:00:00.000Z",
};
const management = {
  canList: true,
  canLoad: true,
  canResume: false,
  canLogout: true,
  canDelete: false,
  canConfigureProviders: false,
};
const methods = [
  { id: "agent", name: "Browser login", description: null, type: "agent" },
  {
    id: "terminal",
    name: "Terminal",
    description: null,
    type: "terminal",
    command: "fixture --login",
  },
  {
    id: "env",
    name: "Token",
    description: null,
    type: "env_var",
    envVarNames: ["TOKEN", "API_KEY"],
  },
];
const inspections = [
  { status: "ready", version: "1", documentationUrl: "https://example.test/setup" },
  { status: "unconfigured" },
  { status: "not_found", agentId: "fixture" },
  { status: "unsupported", agentId: "fixture", version: "1" },
  {
    status: "missing_runner",
    agentId: "fixture",
    version: "1",
    distribution: "npx",
    runner: "npm",
  },
  { status: "missing_runner", version: null, distribution: "local" },
  { status: "unprepared", agentId: "fixture", version: "1" },
  { status: "failed", message: "Could not inspect fixture" },
];
for (const enabled of [true, false])
  for (const source of ["local", "registry"])
    for (const inspection of inspections)
      for (const kind of ["none", "success", "authentication", "other"]) {
        const settings = Schema.decodeSync(Settings)({
          source,
          enabled,
          agentId: "fixture",
          customModels: ["custom"],
        });
        const input = { ...identity, settings, inspection };
        if (kind === "success")
          input.probe = {
            probe: {
              models: [{ id: "fixture-model", name: "Fixture model", description: null }],
              currentModelId: "fixture-model",
              configOptions: [],
              authMethods: methods,
              sessionManagement: management,
              icon: null,
            },
            slashCommands: [{ name: "fixture" }],
            skills: [],
          };
        if (kind === "authentication")
          input.probeError = {
            reason: "authentication_failed",
            message: "The ACP agent could not complete authentication.",
            authMethods: methods,
            authAction: {
              elicitationId: "consent",
              url: "https://example.test/login",
              message: "Login",
            },
          };
        if (kind === "other")
          input.probeError = {
            reason: "probe_failed",
            message: "The ACP agent could not create a test session.",
          };
        add("checked", input, checked(input));
      }
for (const method of ["agent", "terminal", "env", "missing"]) {
  const input = {
    ...identity,
    settings: Schema.decodeSync(Settings)({ source: "local", authMethodId: method }),
    inspection: inspections[0],
    probeError: { reason: "authentication_failed", message: "Login failed", authMethods: methods },
  };
  add("checked", input, checked(input));
}
for (const code of [-32000, -32603])
  for (const detail of [
    "authentication required",
    "Credentials rejected",
    "login failure",
    "log in",
    "log-in",
    "log\ufeffin",
    "log\u0085in",
    "relogin",
    "élogin",
    "Klogin",
    "ſlogin",
    "authenticate!",
    "unrelated failure",
  ]) {
    const input = { code, detail };
    const error = Errors.AcpRequestError.fromProtocolError(
      { code, message: detail },
      { method: "session/new", requestId: 1 },
    );
    const observed = acpRegistryProbeFailure(error);
    add("failure", input, { reason: observed.reason, message: observed.message });
  }
for (const methods of [
  [],
  [{ id: " agent ", name: "ignored" }],
  [{ id: "agent", name: "  Browser  ", description: " description " }],
  [
    {
      id: "terminal",
      type: "terminal",
      name: "CLI",
      args: ["--login", "with spaces", "a'b"],
      env: { TOKEN: "with spaces" },
    },
  ],
  [
    {
      id: "env",
      type: "env_var",
      name: " Token ",
      vars: [{ name: "TOKEN" }, { name: " BAD " }, { name: "API_KEY" }],
      link: "HTTP://Example.test",
    },
  ],
]) {
  const input = {
    initialize: { authMethods: methods },
    command: "/fixture/agent",
    args: ["--flag", ""],
  };
  add(
    "methods",
    input,
    normalizeAcpRegistryAuthMethods(methods, { command: input.command, args: input.args }),
  );
}
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-health.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`Generated ${rows.length} original ACP health projection witnesses`);
