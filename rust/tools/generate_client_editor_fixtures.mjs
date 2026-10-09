// Writes the editor table the Rust client reads (`crates/client/src/editors.json`) from the
// original's own `EDITORS` (packages/contracts/src/editor.ts, types stripped and imported
// unchanged), and the original's choices of a preferred editor and menu label over many inputs
// for `tests/editors_parity.rs`:
//   usePreferredEditor / resolveAndPersistPreferredEditor   apps/web/src/editorPreferences.ts
//   openInEditorMenuLabel                                    apps/web/src/editorLabels.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-editors-"));
const file = NodePath.join(scratch, "editor.mjs");
const source = read("packages/contracts/src/editor.ts");
// Only the table itself; the schemas around it are not needed to read it.
const from = source.indexOf("export const EDITORS = [");
const to = source.indexOf("] as const satisfies", from);
if (from < 0 || to < 0) throw new Error("cannot find EDITORS");
writeFileSync(file, stripTypeScriptTypes(`${source.slice(from, to)}] as const;\n`));
const editor = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const table = editor.EDITORS.map(({ id, label }) => ({ id, label }));
writeFileSync(
  new URL("../crates/client/src/editors.json", import.meta.url),
  `${JSON.stringify(table, null, 2)}\n`,
);

// The original's selection logic, written out here exactly as the two functions read.
const preferred = (available, last) => {
  const ids = new Set(available);
  if (last && ids.has(last)) return last;
  return editor.EDITORS.find((candidate) => ids.has(candidate.id))?.id ?? null;
};
const labels = new Map(editor.EDITORS.map((candidate) => [candidate.id, candidate.label]));
const menuLabel = (id) =>
  id === null || id === "file-manager" ? "Open in editor" : `Open in ${labels.get(id) ?? "Editor"}`;

let seed = 20261012;
const random = () => {
  seed = (seed * 1664525 + 1013904223) >>> 0;
  return seed / 2 ** 32;
};
const ids = editor.EDITORS.map((candidate) => candidate.id);
const rows = [];
for (let index = 0; index < 300; index += 1) {
  const available = ids.filter(() => random() < 0.25);
  if (random() < 0.5) available.sort(() => random() - 0.5);
  const last =
    random() < 0.2
      ? null
      : random() < 0.1
        ? "not-an-editor"
        : ids[Math.floor(random() * ids.length)];
  const chosen = preferred(available, last);
  rows.push({ available, last, preferred: chosen, label: menuLabel(chosen) });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/editors.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${table.length} editors and ${rows.length} choices`);
