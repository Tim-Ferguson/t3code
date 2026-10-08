import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/web/src/components/ThreadTerminalDrawer.tsx", import.meta.url),
  "utf8",
);
const clamp = source.slice(
  source.indexOf("function maxDrawerHeight()"),
  source.indexOf("function writeSystemMessage("),
);
const exit = source.slice(
  source.indexOf("export function shouldHandleTerminalExit("),
  source.indexOf("interface TerminalViewportProps"),
);
if (
  !clamp.startsWith("function maxDrawerHeight()") ||
  !exit.startsWith("export function shouldHandleTerminalExit(")
)
  throw Error("Drawer helper boundary changed");
const rows = [];
for (const viewport of [null, 120, 240, 480, 721, 900, 1440]) {
  const helper = new Function(
    "window",
    "const MIN_DRAWER_HEIGHT=180,MAX_DRAWER_HEIGHT_RATIO=.75,DEFAULT_THREAD_TERMINAL_HEIGHT=280;" +
      stripTypeScriptTypes(clamp) +
      ";return clampDrawerHeight;",
  )(viewport === null ? undefined : { innerHeight: viewport });
  for (const height of [-300, 0, 179.49, 179.5, 180, 280.4, 280.5, 540.5, 900, 1e6])
    rows.push({ kind: "height", viewport, height, expected: helper(height) });
}
const should = new Function(
  stripTypeScriptTypes(exit).replace("export ", "") + ";return shouldHandleTerminalExit;",
)();
for (const current of ["starting", "running", "closed", "exited", "error"])
  for (const synchronized of ["starting", "running", "closed", "exited", "error"])
    for (const handled of [false, true])
      for (const version of [0, 1, 99])
        rows.push({
          kind: "exit",
          current,
          synchronized,
          handled,
          version,
          expected: should(current, synchronized, handled, version),
        });
writeFileSync(
  new URL("../crates/client/tests/fixtures/terminal-drawer.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original drawer witnesses`);
