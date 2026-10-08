// Development oracle extracts the unchanged private daemon-file schema.
import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const Schema = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Schema.js")
);
const source = readFileSync(root + "apps/server/src/device/LocalDeviceHost.ts", "utf8");
const expression = source.match(
  /const AgentDeviceDaemonFile = (Schema\.Struct\([\s\S]*?\n\}\));/,
)[1];
const schema = Function("Schema", `return ${expression}`)(Schema);
const decode = Schema.decodeUnknownSync(Schema.fromJsonString(schema));
const values = [
  null,
  [],
  {},
  ...[12345, 12345.0, -1, 0, 1.5, 9007199254740991, 9007199254740992].map((httpPort) => ({
    httpPort,
    token: "",
  })),
];
for (const key of ["pid", "version"])
  for (const value of [null, true, "", "0.21.12", 1, 1.0, 1.5, 9007199254740992])
    values.push({ httpPort: 12345, token: "isolated token", [key]: value });
values.push({ httpPort: 12345, token: "isolated token", pid: 1, version: "0.21.12" });
const rows = values.map((input) => {
  try {
    return { input, accepted: true, output: decode(JSON.stringify(input)) };
  } catch {
    return { input, accepted: false };
  }
});
writeFileSync(
  root + "rust/crates/server/tests/fixtures/device-daemon.jsonl",
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
