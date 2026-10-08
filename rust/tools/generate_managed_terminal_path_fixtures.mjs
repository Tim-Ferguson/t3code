// Execute unchanged shell merge and the terminal manager's actual append block.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const shell = readFileSync(new URL("../../packages/shared/src/shell.ts", import.meta.url), "utf8");
const manager = readFileSync(
  new URL("../../apps/server/src/terminal/Manager.ts", import.meta.url),
  "utf8",
);
const merge = shell.slice(
  shell.indexOf("export function mergePathEntries("),
  shell.indexOf("function envCaptureStart("),
);
const start = manager.indexOf(
  'const delimiter = platform === "win32" ? ";" : ":";',
  manager.indexOf("// Append (never prepend)"),
);
const end = manager.indexOf("\n              }", start);
const block = manager.slice(start, end);
const append = new Function(
  "terminalEnv",
  "managedDirectories",
  "platform",
  stripTypeScriptTypes(merge, { mode: "strip" }).replace("export function ", "function ") +
    "\nif(managedDirectories.length>0){" +
    block +
    "} return terminalEnv;",
);
const fixtures = [];
for (const platform of ["darwin", "linux", "win32"]) {
  const delimiter = platform === "win32" ? ";" : ":";
  for (const entries of [
    [],
    ["/managed"],
    [" /managed ", "/managed", "/second"],
    ["\uFEFF/managed\u00A0", "\u0085/literal\u0085"],
    ["", " "],
    ["A", "a"],
  ]) {
    for (const base of [
      {},
      { PATH: "/provider" + delimiter + "/system" },
      { PATH: " /provider " + delimiter + delimiter + "/managed " + delimiter + "/provider" },
      { Path: "/first", PATH: "/second", path: "/third" },
      { path: "\uFEFF/a\u00A0" },
      { PATH: " " },
      { PATH: "A" + delimiter + "a" },
    ]) {
      const env = structuredClone(base);
      fixtures.push({
        platform,
        env: base,
        directories: entries,
        result: append(env, entries, platform),
      });
    }
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/managed-terminal-path.jsonl", import.meta.url),
  fixtures.map((x) => JSON.stringify(x)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: fixtures.length }));
