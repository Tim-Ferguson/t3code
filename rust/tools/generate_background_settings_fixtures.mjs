// Development-only actual shared background helper witnesses.
import { writeFileSync } from "node:fs";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Duration from "../../packages/contracts/node_modules/effect/dist/Duration.js";
import {
  DEFAULT_SERVER_SETTINGS,
  ServerSettings,
  BackgroundActivitySettings,
} from "../../packages/contracts/src/settings.ts";
import * as Background from "../../packages/shared/src/backgroundActivitySettings.ts";
const settingsCodec = Schema.toCodecJson(ServerSettings),
  backgroundCodec = Schema.toCodecJson(BackgroundActivitySettings);
const wireSettings = Schema.encodeSync(settingsCodec),
  decodeSettings = Schema.decodeUnknownSync(settingsCodec),
  wireBackground = Schema.encodeSync(backgroundCodec),
  decodeBackground = Schema.decodeUnknownSync(backgroundCodec);
const encodeResolved = (value) =>
  Object.fromEntries(
    Object.entries(value).map(([key, value]) => [
      key,
      key.endsWith("Interval") || key.endsWith("Ttl") ? Duration.toMillis(value) : value,
    ]),
  );
const rows = [],
  defaults = wireSettings(DEFAULT_SERVER_SETTINGS);
for (const profile of ["balanced", "performance", "battery-saver"])
  rows.push({
    op: "preset",
    profile,
    output: encodeResolved(Background.getBackgroundActivityPresetSettings(profile)),
  });
const overrides = [
  {},
  ...[
    "automaticGitFetchInterval",
    "providerHealthRefreshInterval",
    "hostPowerMonitorActiveInterval",
    "hostPowerMonitorIdleInterval",
    "idleClientTtl",
  ].flatMap((key) => [0, 1, 0.5, 45000, 60000, 900000].map((value) => ({ [key]: value }))),
  ...[
    "pauseWhenHostLocked",
    "pauseWhenHostLowPower",
    "pauseWhenClientLowPower",
    "pauseWhenOnBattery",
  ].flatMap((key) => [false, true].map((value) => ({ [key]: value }))),
];
for (const profile of ["balanced", "performance", "battery-saver", "custom"])
  for (const baseProfile of [undefined, "balanced", "performance", "battery-saver"])
    for (const override of overrides) {
      const input = {
        schemaVersion: 1,
        profile,
        ...(baseProfile ? { baseProfile } : {}),
        overrides: override,
      };
      const decoded = decodeBackground(input);
      rows.push({
        op: "background",
        input: wireBackground(decoded),
        resolved: encodeResolved(Background.resolveBackgroundActivitySettings(decoded)),
        normalized: wireBackground(Background.normalizeBackgroundActivitySettings(decoded)),
      });
    }
for (const legacy of ["balanced", "performance", "battery-saver"])
  for (const intervals of [
    [defaults.automaticGitFetchInterval, defaults.providerHealthRefreshInterval],
    [0, 0],
    [15000, 60000],
    [60000, 900000],
    [1, 0.5],
  ])
    for (const backgroundActivity of [
      defaults.backgroundActivity,
      {
        schemaVersion: 1,
        profile: "custom",
        baseProfile: "balanced",
        overrides: { idleClientTtl: 1 },
      },
      { schemaVersion: 1, profile: "performance", overrides: {} },
    ]) {
      const input = {
        ...defaults,
        backgroundActivityProfile: legacy,
        automaticGitFetchInterval: intervals[0],
        providerHealthRefreshInterval: intervals[1],
        backgroundActivity,
      };
      const decoded = decodeSettings(input);
      rows.push({
        op: "server",
        input: wireSettings(decoded),
        resolved: encodeResolved(Background.resolveServerBackgroundActivitySettings(decoded)),
        normalized: wireBackground(Background.normalizeServerBackgroundActivitySettings(decoded)),
      });
    }
writeFileSync(
  new URL("../crates/server/tests/fixtures/background-settings.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
