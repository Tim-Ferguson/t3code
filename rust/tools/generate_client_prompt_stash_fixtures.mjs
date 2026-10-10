// Runs the original stash menu's row text over many entries, for `crates/client/tests/prompt_stash_parity.rs`.
//   stashEntrySnippet   apps/web/src/components/chat/ComposerStashMenu.tsx
// (cut out unchanged; assistant quotes are not part of this client, so their rewriting is stubbed away).
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const source = readFileSync(
  new URL("../../apps/web/src/components/chat/ComposerStashMenu.tsx", import.meta.url),
  "utf8",
);
const from = source.indexOf("const SNIPPET_MAX_CHARS");
const to = source.indexOf("/**\n * Attached banner", from);
if (from < 0 || to < 0) throw new Error("cannot find the snippet function");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-stash-"));
const file = NodePath.join(scratch, "stash.mjs");
writeFileSync(
  file,
  `const assistantCitationsToPlainText = (text) => text;\n${stripTypeScriptTypes(source.slice(from, to))}\nexport { stashEntrySnippet };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const prompts = [
  "",
  "   ",
  "\n\t\n",
  "fix the login bug",
  "  fix   the\nlogin\tbug  ",
  "x".repeat(89),
  "x".repeat(90),
  "x".repeat(91),
  "word ".repeat(40),
  "héllo wörld \u{1F600} ".repeat(12),
  "a b c",
];
const rows = [];
for (const prompt of prompts)
  for (const images of [0, 1, 3])
    for (const files of [0, 1, 2]) {
      const entry = {
        prompt,
        attachments: Array.from({ length: images }, () => ({})),
        droppedImageNames: [],
        files: Array.from({ length: files }, () => ({})),
      };
      rows.push({ prompt, images, files, snippet: original.stashEntrySnippet(entry) });
    }
writeFileSync(
  new URL("../crates/client/tests/fixtures/prompt-stash.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
