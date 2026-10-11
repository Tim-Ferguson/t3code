// Development-only witnesses of `buildPiRpcLaunch` (the arguments and environment a `pi --mode rpc` is started with), in the
// unattended mode text generation uses: extensions and tools off, whatever the user's launch arguments select.
import { writeFileSync } from "node:fs";
import { buildPiRpcLaunch } from "../../apps/server/src/orchestration-v2/Adapters/piT3McpInjection.ts";

const environment = {
  PATH: "/usr/bin",
  T3_MCP_URL: "http://stale",
  T3_MCP_BEARER_TOKEN: "stale",
  KEEP: "1",
};
const extensionPath = "/cache/pi-t3-mcp-extension.ts";
const launchArgSets = [
  [],
  ["--approve"],
  ["--extension", "/mine/ext.ts", "--thinking", "low"],
  ["-e", "/mine/ext.ts", "-e=other.ts", "--extension=third.ts"],
  ["--tools", "read,bash", "--model", "x/y"],
  [
    "-t",
    "read",
    "-xt",
    "bash",
    "--no-builtin-tools",
    "-nbt",
    "-nt",
    "--no-tools",
    "--tools=a",
    "-t=b",
    "--exclude-tools=c",
    "-xt=d",
  ],
  ["--extension", extensionPath],
  ["--extension"],
  ["--tools"],
  ["--model", "provider/model", "--verbose"],
];
const rows = [];
for (const launchArgs of launchArgSets)
  for (const ephemeral of [true, false])
    for (const disableExtensions of [undefined, true, false])
      for (const disableTools of [undefined, true, false])
        for (const withExtension of [false, true]) {
          const input = {
            launchArgs,
            environment,
            mcpSession: withExtension
              ? { endpoint: "http://127.0.0.1:1/mcp", authorizationHeader: "Bearer secret" }
              : undefined,
            extensionPath: withExtension ? extensionPath : undefined,
            ephemeral,
            ...(disableExtensions === undefined ? {} : { disableExtensions }),
            ...(disableTools === undefined ? {} : { disableTools }),
            runtimeMode: "auto",
          };
          const launch = buildPiRpcLaunch(input);
          rows.push({
            launchArgs,
            ephemeral,
            disableExtensions: disableExtensions ?? null,
            disableTools: disableTools ?? null,
            withExtension,
            args: launch.args,
            env: launch.env,
          });
        }
writeFileSync(
  new URL("../crates/server/tests/fixtures/pi-rpc-launch.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ rows: rows.length }));
