// Development-only source oracle: execute original helpers without initializing services.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { DeviceToolUnavailableError } from "../../packages/contracts/src/device.ts";
const source = readFileSync(
  new URL("../../apps/server/src/mcp/toolkits/device/handlers.ts", import.meta.url),
  "utf8",
);
const helpers = source.slice(
  source.indexOf("export function agentDeviceTargetArgs"),
  source.indexOf("const requireDeviceAccess"),
);
const pick = source.slice(source.indexOf("const pickDevice ="), source.indexOf("const toolError"));
const png = source.slice(
  source.indexOf("export function pngDimensions"),
  source.indexOf("const { device_screenshot"),
);
const { targetArgs, quickStart, pickDevice, pngDimensions } = new Function(
  "Effect",
  "DeviceToolUnavailableError",
  "LOCAL_DEVICE_HOST_ID",
  stripTypeScriptTypes((helpers + pick + png).replaceAll("export function", "function"), {
    mode: "strip",
  }) +
    ";return {targetArgs:agentDeviceTargetArgs,quickStart:agentDeviceQuickStart,pickDevice,pngDimensions};",
)(Effect, DeviceToolUnavailableError, "local");
const rows = [];
const device = (id, platform = "ios", hostId = "local", booted = false) => ({
  hostId,
  id,
  platform,
  name: `Device ${id}`,
  version: "OS 1",
  booted,
  physical: false,
});
for (const platform of ["ios", "android"])
  for (const id of ["simple", "quote ' space", "日本語 😀", "--flag"])
    for (const command of [
      "agent-device",
      "/path space/'quoted/agent-device",
      "",
      "C:\\Program Files\\agent-device.cmd",
    ]) {
      const d = device(id, platform);
      const args = [
        ...targetArgs(d),
        "--config",
        "/host config/'quoted.json",
        "--session",
        "t3-session",
      ];
      rows.push({
        type: "guidance",
        device: d,
        args,
        command,
        targetArgs: targetArgs(d),
        result: quickStart(d, args, command),
      });
    }
for (const devices of [
  [],
  [device("one")],
  [device("one"), device("two", "ios", "local", true)],
  [device("one"), device("android", "android")],
  [device("remote", "ios", "remote", true)],
])
  for (const input of [
    {},
    { deviceId: "one" },
    { deviceId: "missing" },
    { hostId: "remote" },
    { platform: "ios" },
    { platform: "android" },
    { hostId: "remote", deviceId: "remote" },
    { hostId: "remote", platform: "android" },
  ]) {
    const result = await Effect.runPromise(
      Effect.match(pickDevice(devices, input), {
        onSuccess: (device) => ({ device }),
        onFailure: (error) => ({ error: JSON.parse(JSON.stringify(error)) }),
      }),
    );
    rows.push({ type: "pick", devices, input, result });
  }
for (const width of [0, 1, 0xffffffff])
  for (const height of [0, 1, 0xffffffff])
    for (const length of [0, 23, 24, 25])
      for (const valid of [true, false]) {
        const buffer = Buffer.alloc(length);
        if (length >= 24) {
          buffer.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
          buffer.write("IHDR", 12);
          buffer.writeUInt32BE(width, 16);
          buffer.writeUInt32BE(height, 20);
          if (!valid) buffer[12] = 0;
        }
        rows.push({ type: "png", bytes: [...buffer], result: pngDimensions(buffer) });
      }
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-device.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
