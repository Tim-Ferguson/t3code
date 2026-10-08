// Node >=24.13.1: node rust/tools/generate_acp_registry_windows_fixtures.mjs
// Executes unchanged shared resolveSpawnCommand, then intercepts Node's actual
// normalized child_process spawn options. No Windows process is executed.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as NodePath from "node:path";
import * as ChildProcess from "node:child_process";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import {
  resolveSpawnCommand,
  SpawnExecutableResolution,
  CommandResolutionCache,
  mergePathEntries,
} from "../../packages/shared/src/shell.ts";
import {
  HostProcessPlatform,
  HostProcessEnvironment,
} from "../../packages/shared/src/hostProcess.ts";
const source = readFileSync("packages/shared/src/shell.ts", "utf8");
function between(source, begin, end) {
  const start = source.indexOf(begin),
    finish = source.indexOf(end, start);
  if (start < 0 || finish < start) throw Error(`Missing source ${begin}`);
  return source.slice(start, finish);
}
const candidates = new Function(
  stripTypeScriptTypes(between(source, "function resolveWindowsPathExtensions(", "export const ")) +
    "; return [resolveWindowsPathExtensions,resolveCommandCandidates];",
)();
const registry = readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
const preferredPath = new Function(
  "mergePathEntries",
  stripTypeScriptTypes(
    between(registry, "function readEnvironmentPath(", "/**\n * Directories exposing"),
  ) + "; return withPreferredPath;",
)(mergePathEntries);
const rows = [];
const environments = [
  { "=C:": "C:\\work", Path: "second", pAtH: "third", PATH: "first", K: "kelvin", k: "latin" },
  {},
  { PATH: "C:\\bin", PATHEXT: ".COM;.EXE;.BAT;.CMD" },
  { Path: "C:\\other", PATH: "C:\\first", path: "C:\\last", pathext: ".INVALID" },
  { PATH: "", PATHEXT: " cmd ; .ExE;.cmd;\ufeffBAT;\u0085" },
  { PATHEXT: " ; \ufeff " },
];
for (const environment of environments)
  for (const command of [
    "agent",
    "node.Exe",
    "agent.cmd",
    "agent.bat",
    "agent.ps1",
    ".cmd",
    "..cmd",
    "C:\\tools\\agent.CMD",
    "C:/tools/foo.exe",
    "agent.",
    "foo.cmd\n",
    "foo.cmd/",
    "C:\\bin\\agent.bat\\",
  ]) {
    rows.push({
      operation: "candidates",
      environment,
      command,
      output: candidates[1](command, "win32", candidates[0](environment), NodePath.win32.extname),
    });
  }
for (const environment of [...environments, { pAtH: "C:\\fallback;C:\\same", Path: "ignored" }])
  for (const windows of [true, false])
    for (const directory of [
      "C:\\preferred",
      "/first",
      " \ufefffirst ; second ",
      "C:\\bin",
      "quote path",
      "",
      " \ufeff ",
    ]) {
      rows.push({
        operation: "path",
        environment,
        windows,
        directory,
        output: preferredPath(environment, directory, windows ? "win32" : "linux"),
      });
    }
const descriptor = Object.getOwnPropertyDescriptor(process, "platform");
const oldSpawn = ChildProcess.ChildProcess.prototype.spawn;
const oldComspec = process.env.comspec;
let captured;
try {
  Object.defineProperty(process, "platform", { value: "win32" });
  ChildProcess.ChildProcess.prototype.spawn = function (options) {
    captured = options;
    return 0;
  };
  for (const environment of environments)
    for (const resolved of [
      null,
      "C:\\Program Files\\npm & tools\\agent.cmd",
      "C:\\bin\\agent.BAT",
      "C:\\bin\\agent.exe",
    ])
      for (const command of ["agent", "missing & calc", "literal.cmd"])
        for (const args of [
          [],
          ["run", "value & calc", "%PATH%", 'quote"value'],
          ["", "trailing\\", 'slash\\"quote', "[]()^`<>&|;, *?\t\n😀"],
        ])
          for (const comspec of [
            null,
            "C:\\Windows\\System32\\cmd.exe",
            "custom-shell.exe",
            "cmd.exe\n",
            "cmd.exe\r",
            "cmd.exe\r\n",
            "C:/cmd.exe",
          ]) {
            const resolvedCommand = await Effect.runPromise(
              resolveSpawnCommand(command, args, { env: environment }).pipe(
                Effect.provideService(HostProcessPlatform, "win32"),
                Effect.provideService(HostProcessEnvironment, {}),
                Effect.provideService(CommandResolutionCache, new Map()),
                Effect.provideService(SpawnExecutableResolution, () => resolved ?? undefined),
              ),
            );
            if (comspec === null) delete process.env.comspec;
            else process.env.comspec = comspec;
            ChildProcess.spawn(resolvedCommand.command, resolvedCommand.args, {
              shell: resolvedCommand.shell,
              env: environment,
            });
            rows.push({
              operation: "plan",
              environment,
              command,
              args,
              resolved,
              comspec,
              output: {
                command: captured.file,
                args: captured.args.slice(1),
                verbatim: captured.windowsVerbatimArguments,
                envPairs: captured.envPairs,
              },
            });
          }
} finally {
  Object.defineProperty(process, "platform", descriptor);
  ChildProcess.ChildProcess.prototype.spawn = oldSpawn;
  if (oldComspec === undefined) delete process.env.comspec;
  else process.env.comspec = oldComspec;
}
writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-windows-spawn.jsonl",
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(
  `${rows.length} original Windows candidates/PATH/spawn wire witnesses (no Windows processes executed)`,
);
