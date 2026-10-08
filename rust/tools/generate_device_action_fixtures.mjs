// Development-only oracle executes unchanged DeviceActions; no device tools run.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const actions = await import(pathToFileURL(root + "apps/server/src/device/DeviceActions.ts"));
const contracts = await import(pathToFileURL(root + "packages/contracts/src/device.ts"));
const Schema = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Schema.js")
);
const Effect = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Effect.js")
);
const inputs = [];
function add(type, fields = {}) {
  inputs.push({ type, deviceId: "fixture-id", ...fields });
}
for (const value of ["light", "dark"]) add("setAppearance", { value });
for (const value of ["small", "default", "large", "extra-large"]) add("setTextSize", { value });
for (const setting of [
  "reduceMotion",
  "increaseContrast",
  "reduceTransparency",
  "showBorders",
  "voiceOver",
  "networkEnabled",
])
  for (const value of [true, false]) add("setToggle", { setting, value });
for (const value of ["clear", "tinted"]) add("setLiquidGlass", { value });
for (const value of ["none", "grayscale", "red-green", "green-red", "blue-yellow"])
  add("setColorFilter", { value });
for (const value of ["portrait", "landscape_left", "portrait_upside_down", "landscape_right"])
  for (const deviceId of ["fixture-id", "emulator-5554"])
    add("setOrientation", { value, deviceId });
for (const latitude of [0, -0, -12.5, 1e-7, 1e21])
  add("setLocation", { latitude, longitude: -0.0000001 });
add("clearLocation");
add("shake");
for (const permission of [
  "camera",
  "microphone",
  "photos",
  "contacts",
  "calendar",
  "reminders",
  "location",
  "notifications",
  "motion",
  "media-library",
  "faceid",
])
  for (const decision of ["grant", "revoke", "reset"])
    add("setPermission", { permission, decision, appId: "fixture.app" });
for (const url of ["https://example.test/path?q=a&b=x", "fixture://quoted/path"])
  add("openUrl", { url });
for (const type of ["launchApp", "terminateApp"]) add(type, { appId: "fixture.app" });
for (const payload of [
  "hello",
  { aps: { alert: "hello" } },
  [],
  null,
  42,
  { z: 1, 10: -0, 2: 1e-7, "01": 1e21, a: { z: 2, 4294967295: 3, 4294967294: -0, 0: 4 } },
  { z: 1, a: 2 },
  { aps: { z: 1, 20: 2, 3: 3, "03": 4 }, a: -0 },
  { a: 1e-6, b: 1e20, c: 1e21, d: 1e-7 },
])
  add("sendPush", { appId: "fixture.app", payload });
const fixtures = [];
for (const input of inputs) {
  let decoded;
  try {
    decoded = Schema.decodeUnknownSync(Schema.toCodecJson(contracts.DeviceActionInput))(input);
  } catch {
    continue;
  }
  for (const platform of ["ios", "android"])
    for (const helpersPresent of [true, false])
      for (const code of [0, 7]) {
        const helpers = {
          nodePath: "/fixture/node",
          serveSimAxSettings: helpersPresent ? "/fixture/ax" : null,
          serveSimCli: helpersPresent ? "/fixture/cli" : null,
        };
        const calls = [];
        const ready = {
          nodePath: helpers.nodePath,
          helpers,
          run: (command, args, options) => {
            calls.push({
              command,
              args,
              ...(options?.stdin === undefined ? {} : { stdin: options.stdin }),
            });
            return Effect.succeed({ code, stdout: "fixture stdout", stderr: "fixture stderr" });
          },
        };
        const outcome = await Effect.runPromise(
          Effect.match(actions.runDeviceAction(ready, platform, decoded), {
            onSuccess: () => ({ ok: true }),
            onFailure: (error) => ({
              ok: false,
              error: Schema.encodeUnknownSync(Schema.toCodecJson(contracts.DeviceError))(error),
            }),
          }),
        );
        fixtures.push({
          input: Schema.encodeUnknownSync(Schema.toCodecJson(contracts.DeviceActionInput))(decoded),
          platform,
          helpers,
          code,
          supported: actions.supportsAction(platform, decoded),
          calls,
          ...outcome,
        });
      }
}
const destination = root + "rust/crates/server/tests/fixtures/device-actions.jsonl";
writeFileSync(destination + ".tmp", fixtures.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(destination + ".tmp", destination);
console.log(JSON.stringify({ cases: fixtures.length }));
