// Runs `folderWords` of ManagedProjectFolders.ts, the part of a Scratch thread's folder name that
// comes from its first message, cut out of that file verbatim because the file exports none of it.
import { mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const source = readFileSync(
  new URL("../../apps/server/src/project/ManagedProjectFolders.ts", import.meta.url),
  "utf8",
);
const from = source.indexOf("const folderWords = ");
const to = source.indexOf("// Tailwind 600 shades", from);
if (from < 0 || to < 0) throw new Error("ManagedProjectFolders.ts changed near folderWords");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-thread-folder-fixtures-"));
const module = NodePath.join(scratch, "words.ts");
writeFileSync(module, `${source.slice(from, to)}\nexport { folderWords };\n`);
const { folderWords } = await import(pathToFileURL(module).href);
rmSync(scratch, { recursive: true, force: true });

const texts = [
  "Convert these PNGs to WebP, please!",
  "",
  "   ",
  "!!!",
  "Fix the bug",
  "one two three four five six seven",
  "UPPER lower MiXeD",
  "a-b_c.d/e",
  `${"word ".repeat(10_000)}../../etc`,
  "x".repeat(200),
  `${"ab ".repeat(20)}`,
  "日本語のプロジェクト",
  "Ünïcödé Café",
  "İstanbul ΣΊΣΥΦΟΣ",
  "ß-test",
  "1 2 3 4 5 6",
  "../../etc/passwd",
  "a".repeat(47) + " b",
  "a".repeat(48) + " b",
  "a".repeat(46) + " b c",
  "😀 Emoji Party",
  "tab\tseparated\nlines",
  "ﬁnal ﬂight",
  "ǅ",
  "Ⅻ ①②",
];
const rows = texts.map((text) => ({ text, words: folderWords(text) }));
const target = new URL("../crates/server/tests/fixtures/thread-folders.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
