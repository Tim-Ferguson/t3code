// Original Node path joins and unchanged LocalDeviceHost helper environment.
import path from "node:path";
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/server/src/device/LocalDeviceHost.ts", import.meta.url),
  "utf8",
);
const block = source.slice(
  source.indexOf("const deviceHostEnvironment ="),
  source.indexOf("const hubEnvironment ="),
);
const environment = new Function(
  stripTypeScriptTypes(block, { mode: "strip" }) + ";return deviceHostEnvironment;",
)();
const fixtures = [];
for (const platform of ["darwin", "linux", "win32"]) {
  const adapter = platform === "win32" ? path.win32 : path.posix;
  for (const root of [
    "",
    ".",
    "./sdk",
    "sdk/../sdk",
    "/sdk//one/../",
    "/sdk/",
    "../../sdk",
    "C:\\sdk\\..\\sdk",
    "C:sdk",
    "\\\\server\\share\\sdk",
    "\\\\server",
    "/",
    "C:\\",
    "//server//share",
  ]) {
    for (const parts of [
      ["platform-tools"],
      ["emulator"],
      ["cmdline-tools", "latest", "bin", "avdmanager"],
      [],
    ]) {
      fixtures.push({ type: "join", platform, root, parts, result: adapter.join(root, ...parts) });
    }
    for (const env of [
      { PATH: "/system" },
      { Path: "/case-path" },
      { PATH: "", Path: "/ignored" },
      {},
    ]) {
      fixtures.push({
        type: "environment",
        platform,
        root,
        env,
        result: environment(env, root, platform, adapter),
      });
    }
  }
  fixtures.push({
    type: "environment",
    platform,
    root: null,
    env: { PATH: "unchanged" },
    result: environment({ PATH: "unchanged" }, null, platform, adapter),
  });
}
const target = new URL("../crates/server/tests/fixtures/device-platform.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, fixtures.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: fixtures.length }));
