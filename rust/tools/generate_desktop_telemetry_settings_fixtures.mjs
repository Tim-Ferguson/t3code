import { writeFileSync } from "node:fs";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Duration from "../../packages/contracts/node_modules/effect/dist/Duration.js";
import { ServerSettings } from "../../packages/contracts/src/settings.ts";
import { resolveServerBackgroundActivitySettings } from "../../packages/shared/src/backgroundActivitySettings.ts";
const cases = [];
function run(input) {
  const settings = Schema.decodeUnknownSync(ServerSettings)(input);
  const resolved = resolveServerBackgroundActivitySettings(settings);
  cases.push({
    input,
    active: Math.max(1, Math.round(Duration.toMillis(resolved.hostPowerMonitorActiveInterval))),
    idle: Math.max(1, Math.round(Duration.toMillis(resolved.hostPowerMonitorIdleInterval))),
  });
}
run({});
for (const profile of ["balanced", "performance", "battery-saver"]) {
  run({ backgroundActivityProfile: profile });
  run({ backgroundActivity: { profile, overrides: { hostPowerMonitorActiveInterval: 1 } } });
  for (const baseProfile of [undefined, "balanced", "performance", "battery-saver"]) {
    run({
      backgroundActivity: {
        profile: "custom",
        ...(baseProfile ? { baseProfile } : {}),
        overrides: {},
      },
    });
    for (const value of [0, 1, 1.5, 30000, 9007199254740991, 9007199254740992])
      run({
        backgroundActivity: {
          profile: "custom",
          ...(baseProfile ? { baseProfile } : {}),
          overrides: { hostPowerMonitorActiveInterval: value, hostPowerMonitorIdleInterval: value },
        },
      });
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/desktop-telemetry-settings.jsonl", import.meta.url),
  cases.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
console.log(`${cases.length} source settings witnesses`);
