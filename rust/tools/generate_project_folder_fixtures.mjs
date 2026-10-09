// Runs the original managed-project-folder helpers and records what they produce. The Rust tests
// replay the rows and require identical output. `newProjectFolderName` is imported from the shared
// package; the helpers private to ManagedProjectFolders.ts are cut out of that file verbatim (their
// text, not a copy) and loaded as a module of their own, because the file exports none of them.
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";
import { newProjectFolderName } from "../../packages/shared/src/path.ts";

const source = readFileSync(
  new URL("../../apps/server/src/project/ManagedProjectFolders.ts", import.meta.url),
  "utf8",
);
const cut = (start, end) => {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const snippets = [
  cut("// Tailwind 600 shades", "const MAX_NAMED_FOLDER_ATTEMPTS"),
  cut("function escapeXml", "const make = Effect.gen"),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-folder-fixtures-"));
const module = NodePath.join(scratch, "helpers.ts");
writeFileSync(
  module,
  `${snippets}\nexport { namedProjectIconSvg, namedProjectReadme, describeCommitFailure };\n`,
);
const { namedProjectIconSvg, namedProjectReadme, describeCommitFailure } = await import(
  pathToFileURL(module).href
);
rmSync(scratch, { recursive: true, force: true });

const names = [
  "Pinball Stats",
  "pinball stats",
  "  Spaced   Out  ",
  "",
  "   ",
  "---",
  "A",
  "ab",
  "Ünïcödé Café",
  "Ｆｕｌｌｗｉｄｔｈ ABC",
  "ﬁnal ﬂight",
  "日本語のプロジェクト",
  "日本語 project",
  "😀 Emoji Party",
  "CON",
  "con",
  "Aux.txt",
  "nul",
  "COM1",
  "lpt9",
  "com0",
  "prn!",
  "x".repeat(64),
  "x".repeat(65),
  `${"a".repeat(63)}-b`,
  `${"a".repeat(64)}-b`,
  "Hello, World!",
  "../../etc/passwd",
  "a/b\\c",
  "My App v2.0",
  "İstanbul",
  "ΣΊΣΥΦΟΣ",
  "ß-test",
  "Ñandú",
  "²³ squared",
  "℃ celsius",
  'Tom & Jerry <3 "quotes"',
  "x y z w",
  "  leading",
  "trailing  ",
  "Zażółć gęślą jaźń",
  "ǅ",
  "áb",
  "́",
  "é́",
  "ᴬᴮ",
  "①②",
  "Ⅻ",
  "Αθήνα",
  "Привет мир",
  "שלום עולם",
  "مرحبا بالعالم",
  "हिन्दी नाम",
  "한국어 이름",
  "1 2 3",
  "9lives",
  "-a-",
  "a--b",
  "a_b",
  "a.b",
  "a b",
];
const rows = [];
for (const name of names) {
  rows.push({
    type: "slug",
    name,
    slug: newProjectFolderName(name),
    svg: namedProjectIconSvg(name),
    readme: namedProjectReadme(name),
  });
}
const stderrs = [
  "",
  "\n\n",
  "error: gpg failed to sign the data",
  'Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  git config --global user.email "you@example.com"\n',
  "fatal: unable to auto-detect email address (got 'x@y.(none)')",
  "fatal: empty ident name (for <>) not allowed",
  "Committer IDENTITY UNKNOWN",
  "no name was given and auto-detection is disabled",
  "No Email was given",
  "line one\n  line two  \n\n",
  "  \t padded last line \t \n",
  "a\r\nb\r\n",
  " nbsp ",
  "hint: something\nerror: last\nhint: trailing\n",
];
for (const stderr of stderrs)
  rows.push({ type: "commit-failure", stderr, message: describeCommitFailure(stderr) });

const target = new URL("../crates/server/tests/fixtures/project-folders.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
