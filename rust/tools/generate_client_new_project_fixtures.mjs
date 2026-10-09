// Runs the original helpers a client uses to show where a project started from just a name will
// go (`newProjectFolderName`, `getNewProjectPathPreview`) and records what they produce. The Rust
// tests in t3-client replay the rows and require identical output.
import { writeFileSync } from "node:fs";
import { newProjectFolderName } from "../../packages/shared/src/path.ts";
import { getNewProjectPathPreview } from "../../packages/client-runtime/src/operations/projects.ts";

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
  "áb",
  "́",
  "é́",
  "ᴬᴮ",
  "①②",
  "Ⅻ",
  "Αθήνα",
  "Привет мир",
  "שלום עולם",
  "مرحبا بالعالم",
  "हिन्दी नाм",
  "한국어 이름",
  "1 2 3",
  "9lives",
  "-a-",
  "a--b",
  "a_b",
  "a.b",
  "a b",
];
const roots = [
  "/home/me/.t3/projects",
  "/home/me/.t3/projects/",
  "C:\\Users\\me\\.t3\\projects",
  "C:\\Users\\me\\.t3\\projects\\",
  "C:/Users/me/projects",
  "\\\\server\\share\\projects",
  "relative/projects",
  "relative\\projects",
  "~",
  "",
  "  /spaced/root  ",
];
const rows = [];
for (const name of names) rows.push({ type: "folder", name, folder: newProjectFolderName(name) });
for (const root of roots)
  for (const name of ["Pinball Stats", "", "CON", "Ünï"])
    rows.push({ type: "preview", root, name, path: getNewProjectPathPreview(root, name) });
writeFileSync(
  new URL("../crates/client/tests/fixtures/new-project.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows`);
