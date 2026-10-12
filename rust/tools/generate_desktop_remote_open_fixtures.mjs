// Writes what the original's remote "Open in" links (`buildRemoteOpenUrl`, `REMOTE_CAPABLE_EDITOR_IDS`, `remoteSchemeForEditor`;
// packages/contracts/src/editor.ts, types stripped and imported unchanged) are for many editors, hosts and paths, for
// `crates/desktop/src/remote_open.rs`.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-remote-open-"));
const file = NodePath.join(scratch, "editor.mjs");
const source = read("packages/contracts/src/editor.ts");
// The editor table and the two functions that read it; the schemas around them are not needed.
const table = source.slice(
  source.indexOf("export const EDITORS = ["),
  source.indexOf("] as const satisfies") + "] as const".length,
);
const functions = source.slice(
  source.indexOf("const remoteSchemeOf"),
  source.indexOf("/**\n * SSH hostnames an environment advertises"),
);
if (!table.startsWith("export const EDITORS") || !functions.startsWith("const remoteSchemeOf"))
  throw new Error("cannot find the editor table");
writeFileSync(
  file,
  stripTypeScriptTypes(
    `${table};\ntype EditorId = string; type EditorDefinition = any;\n${functions}`,
  ),
);
const editor = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const hosts = [
  "devbox",
  "devbox.tail1234.ts.net",
  "my-box.local",
  "user name",
  "caf\u00e9",
  "a+b",
  "100.64.0.9",
];
const paths = [
  "/home/me/project",
  "/home/me/my project",
  "/home/me/projet \u00e9t\u00e9/\u65e5\u672c\u8a9e",
  "/srv/a#b?c=d&e/f%g",
  "relative/dir",
  "C:\\Users\\me\\project",
  "C:/Users/me/project",
  "D:\\work\\x y",
  "\\\\server\\share\\project",
  "/C:/looks/like/a/drive",
  "/",
  "/a'b(c)d!e~f*g",
];
const ids = [...editor.EDITORS.map((candidate) => candidate.id), "not-an-editor"];
const rows = [];
for (const id of ids) {
  for (const host of hosts) {
    for (const absolutePath of paths) {
      rows.push({
        editor: id,
        host,
        absolutePath,
        url: editor.buildRemoteOpenUrl({ editor: id, host, absolutePath }) ?? null,
      });
    }
  }
}
const out = new URL("../crates/desktop/tests/fixtures/remote_open_urls.jsonl", import.meta.url);
writeFileSync(out, `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`);
writeFileSync(
  new URL("../crates/desktop/tests/fixtures/remote_capable_editors.json", import.meta.url),
  `${JSON.stringify(editor.REMOTE_CAPABLE_EDITOR_IDS.map((id) => [id, editor.remoteSchemeForEditor(id)]))}\n`,
);
console.log(
  `${rows.length} links, ${editor.REMOTE_CAPABLE_EDITOR_IDS.length} remote capable editors`,
);
