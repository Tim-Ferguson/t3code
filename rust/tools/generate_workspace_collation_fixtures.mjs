// Node's actual Intl.Collator/localeCompare reference, with isolated locale env.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
const names = [
  "日本語",
  "Z",
  "á",
  "a.1",
  "É",
  "Ω",
  "a_1",
  "👩‍💻",
  "z",
  "a10",
  "ø",
  "中文",
  "é",
  "a-1",
  "A",
  "ä",
  "e",
  "a2",
  "a",
  "e\u0301",
  "İ",
  "ı",
];
const environments = [
  {},
  { LANG: "de_DE.UTF-8" },
  { LANG: "en_US.UTF-8", LC_ALL: "sv_SE.UTF-8" },
  { LANG: "de_DE.UTF-8", LC_MESSAGES: "tr_TR.UTF-8" },
  { LANG: "C" },
  { LANG: "en_US.UTF-8", LC_COLLATE: "sv_SE.UTF-8" },
];
const base = { ...process.env };
for (const key of ["LANG", "LC_ALL", "LC_MESSAGES", "LC_COLLATE"]) delete base[key];
const fixtures = environments.map((environment) =>
  JSON.parse(
    execFileSync(
      process.execPath,
      [
        "-e",
        `const names=${JSON.stringify(names)};console.log(JSON.stringify({environment:${JSON.stringify(environment)},locale:Intl.Collator().resolvedOptions().locale,input:names,sorted:names.slice().sort((a,b)=>a.localeCompare(b))}))`,
      ],
      { env: { ...base, ...environment }, encoding: "utf8" },
    ),
  ),
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/workspace-collation.json", import.meta.url),
  JSON.stringify(fixtures, null, 2) + "\n",
);
