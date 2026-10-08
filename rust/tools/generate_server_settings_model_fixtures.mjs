// Pure decisions against unchanged shared settings helpers; no production I/O.
import { readFileSync, writeFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { stripTypeScriptTypes } from "node:module";
import * as Equal from "../../packages/contracts/node_modules/effect/dist/Equal.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import {
  DEFAULT_SERVER_SETTINGS,
  ServerSettings,
  ServerSettingsPatch,
} from "../../packages/contracts/src/settings.ts";
import {
  applyServerSettingsPatch,
  deriveLegacyProjectOverrides,
} from "../../packages/shared/src/serverSettings.ts";
const wire = Schema.encodeSync(Schema.toCodecJson(ServerSettings)),
  decode = Schema.decodeUnknownSync(Schema.toCodecJson(ServerSettingsPatch));
const previous = gunzipSync(
  readFileSync(
    new URL("../crates/contracts/tests/fixtures/expanded-codecs.jsonl.gz", import.meta.url),
  ),
)
  .toString()
  .trim()
  .split("\n")
  .map((row) => JSON.parse(row));
const patches = previous
  .filter((row) => row.schema === "ServerSettingsPatch" && row.decoded_valid)
  .map((row) => row.input);
patches.push(
  ...[
    { deviceHosts: [] },
    { defaultModelSelection: null },
    { sourceControlWriterModelSelection: null },
    { worktreeCleanup: null },
    { defaultProjectScripts: [] },
    { github: { hosts: {}, tokens: { "github.com": "" } } },
    { providerInstances: {} },
    { textGenerationModelSelection: { options: [] } },
    {
      backgroundActivity: {
        profile: "custom",
        baseProfile: "balanced",
        overrides: { idleClientTtl: 1 },
      },
    },
    { automaticGitFetchInterval: 0 },
    { backgroundActivityProfile: "performance", providerHealthRefreshInterval: 5 },
    {
      projectSettingsOverrides: { fixture: { defaultAutoPull: true } },
      projectAutoPullOverrides: { fixture: false },
    },
    { projectScriptOverrides: { fixture: null } },
    { worktreesDirectory: "/tmp/new" },
  ],
);
const setups = [
  [],
  [
    {
      textGenerationModelSelection: {
        options: [
          { id: "reasoningEffort", value: "high" },
          { id: "fastMode", value: true },
        ],
      },
    },
    {
      projectSettingsOverrides: {
        fixture: {
          enableAgentBrowserAccess: true,
          defaultAutoPull: true,
          defaultProjectScripts: [],
        },
      },
    },
    { worktreesDirectory: "/tmp/old" },
  ],
  [
    {
      backgroundActivity: {
        profile: "custom",
        baseProfile: "battery-saver",
        overrides: { idleClientTtl: 120000 },
      },
    },
    { worktreeCleanup: { mode: "custom", rules: { worktreeOnMerge: false } } },
  ],
];
const rows = [];
for (const setup of setups) {
  let current = DEFAULT_SERVER_SETTINGS;
  for (const patch of setup) current = applyServerSettingsPatch(current, decode(patch));
  for (const patch of patches) {
    try {
      const decoded = decode(patch);
      const input = wire(current);
      let output;
      try {
        output = wire(applyServerSettingsPatch(current, decoded));
      } catch {
        rows.push({ op: "patch", input, patch, valid: false });
        continue;
      }
      rows.push({ op: "patch", input, patch, valid: true, output });
    } catch {
      /*schema rejects unsupported seed*/
    }
  }
}
const service = readFileSync(
  new URL("../../apps/server/src/serverSettings.ts", import.meta.url),
  "utf8",
);
const foldCode = service.slice(
  service.indexOf("const foldProviderInstanceEnabledFlags ="),
  service.indexOf("const normalizeServerSettings ="),
);
const sparseCode = service.slice(
  service.indexOf("const ATOMIC_SETTINGS_KEYS:"),
  service.indexOf("const decodeProjectScriptsJson ="),
);
const helpers = new Function(
  "DEFAULT_SERVER_SETTINGS",
  "Equal",
  stripTypeScriptTypes(foldCode + sparseCode, { mode: "strip" }) +
    ";return {fold:foldProviderInstanceEnabledFlags,sparse:settings=>stripDefaultServerSettings(settings,PERSISTED_SERVER_SETTINGS_DEFAULTS)??{}};",
)(DEFAULT_SERVER_SETTINGS, Equal);
const normalizeInputs = [];
for (const outer of [undefined, false, true])
  for (const inner of [undefined, false, true, "false", null]) {
    const settings = wire(DEFAULT_SERVER_SETTINGS);
    settings.providerInstances = {
      fixture: {
        driver: "codex",
        ...(outer === undefined ? {} : { enabled: outer }),
        ...(inner === undefined ? {} : { config: { enabled: inner, unknown: "preserved" } }),
      },
    };
    settings.projectSettingsOverrides = { fixture: { defaultAutoPull: true } };
    normalizeInputs.push(settings);
  }
for (const raw of normalizeInputs) {
  const input = Schema.decodeUnknownSync(Schema.toCodecJson(ServerSettings))(raw);
  let output = helpers.fold(input);
  output = { ...output, ...deriveLegacyProjectOverrides(output) };
  rows.push({ op: "normalize", input: wire(input), output: wire(output) });
}
for (const setup of setups) {
  let input = DEFAULT_SERVER_SETTINGS;
  rows.push({ op: "sparse", input: wire(input), output: wire(helpers.sparse(input)) });
  for (const patch of setup) {
    input = applyServerSettingsPatch(input, decode(patch));
    rows.push({ op: "sparse", input: wire(input), output: wire(helpers.sparse(input)) });
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/server-settings-model.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length, patches: patches.length }));
