// Original RPC codec witnesses. Production Rust does not execute this generator.
import { readFileSync, writeFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import { WsSubscribeServerConfigRpc, WsRpcGroup } from "../../packages/contracts/src/rpc.ts";
import { ServerConfigStreamEvent } from "../../packages/contracts/src/server.ts";
import { ServerSettingsError } from "../../packages/contracts/src/settings.ts";
const expanded = gunzipSync(
  readFileSync(
    new URL("../crates/contracts/tests/fixtures/expanded-codecs.jsonl.gz", import.meta.url),
  ),
)
  .toString()
  .trim()
  .split("\n")
  .map(JSON.parse);
const config = expanded.find((row) => row.schema === "ServerConfig" && row.decoded_valid).input;
const schemas = {
  SubscribeServerConfigInput: WsSubscribeServerConfigRpc.payloadSchema,
  UpdateServerSettingsInput: WsRpcGroup.requests.get("server.updateSettings").payloadSchema,
  GetServerSettingsInput: Schema.Struct({}),
  ServerConfigStreamEvent,
  ServerSettingsError,
};
const seeds = {
  SubscribeServerConfigInput: [
    {},
    { environmentThemes: true, usageLimitSources: false, usageLimitsCommand: true },
    { environmentThemes: null, usageLimitSources: null, usageLimitsCommand: null },
  ],
  UpdateServerSettingsInput: [
    { patch: {} },
    {
      patch: { providers: { codex: { enabled: false } } },
      providerInstanceMutation: {
        operation: "create",
        instanceId: "fixture",
        instance: { driver: "codex" },
      },
    },
    { patch: {}, providerInstanceMutation: { operation: "remove", instanceId: "fixture" } },
  ],
  GetServerSettingsInput: [{}],
  ServerConfigStreamEvent: [
    { type: "snapshot", version: 1, config },
    { type: "settingsUpdated", version: 1, payload: { settings: {} } },
    { type: "providerStatuses", version: 1, payload: { providers: [] } },
    { type: "keybindingsUpdated", version: 1, payload: { keybindings: [], issues: [] } },
    { type: "environmentThemesUpdated", version: 1, payload: { themes: [] } },
    { type: "usageLimitSourcesUpdated", version: 1, payload: { sources: [] } },
  ],
  ServerSettingsError: [
    { _tag: "ServerSettingsError", settingsPath: "fixture", operation: "write-file" },
    {
      _tag: "ServerSettingsError",
      settingsPath: "fixture",
      operation: "read-secret",
      providerInstanceId: "fixture",
      environmentVariable: "KEY",
    },
  ],
};
const rows = [];
for (const [name, schema] of Object.entries(schemas)) {
  const codec = Schema.toCodecJson(schema),
    decode = Schema.decodeUnknownSync(codec),
    encode = Schema.encodeSync(codec);
  const inputs = [null, [], false, 0, "", {}, { extra: "ignored" }, ...seeds[name]];
  for (const seed of seeds[name])
    for (const key of Object.keys(seed)) {
      const removed = { ...seed };
      delete removed[key];
      inputs.push(removed);
      for (const value of [null, {}, [], false, "wrong", 0, 2])
        inputs.push({ ...seed, [key]: value });
    }
  for (const input of inputs) {
    try {
      rows.push({ schema: name, input, valid: true, output: encode(decode(input)) });
    } catch {
      rows.push({ schema: name, input, valid: false });
    }
  }
}
writeFileSync(
  new URL("../crates/contracts/tests/fixtures/settings-rpc.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length, schemas: Object.keys(schemas).length }));
