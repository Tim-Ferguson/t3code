// Checkpoint ref naming (checkpointing/Utils.ts) and numstat parsing (checkpointing/Diffs.ts),
// executed from the original sources.
import { writeFileSync, renameSync } from "node:fs";
import { checkpointRefForThreadTurn } from "../../apps/server/src/checkpointing/Utils.ts";
import { parseTurnDiffFilesFromNumstat } from "../../apps/server/src/checkpointing/Diffs.ts";

const rows = [];
const locale = Intl.Collator().resolvedOptions().locale;

const threadIds = [
  "thread-1",
  "",
  "a",
  "ab",
  "abc",
  "abcd",
  "thread-checkpoint-store",
  "00000000-0000-4000-8000-000000000000",
  "café",
  "日本語",
  "😀 emoji",
  "with/slash",
  "with space",
  "???>>>",
  "~~~",
  "x".repeat(300),
  "ÿþ",
  "line\nbreak",
  "tab\there",
  "pad==",
  "+/+/",
];
for (const threadId of threadIds) {
  for (const turnCount of [0, 1, 2, 10, 4294967296]) {
    rows.push({
      type: "ref",
      threadId,
      turnCount,
      ref: checkpointRefForThreadTurn(threadId, turnCount),
    });
  }
}

const numstats = [
  "",
  ["0\t2\tsrc/b.ts", "2\t1\ta.txt", ""].join("\0"),
  [
    "0\t0\t",
    "src/old.ts",
    "src/new.ts",
    "2\t1\t",
    "src/source.ts",
    "src/copied.ts",
    "1\t0\tother.ts",
    "",
  ].join("\0"),
  ["-\t-\timage.png", "0\t0\tempty.txt", ""].join("\0"),
  `3\t2\t\0old\tname\n.txt\0 café\tline\r\nname.txt \0`,
  `1\t0\t café\tline\r\nname.txt \0`,
  // Collation: case, accents, digits, punctuation, CJK.
  [
    "B",
    "a",
    "A",
    "b",
    "á",
    "z",
    "Z",
    "a1",
    "a10",
    "a2",
    "_x",
    "-x",
    ".x",
    "日本",
    "é",
    "e",
    "E",
    "ä",
    "src/B.ts",
    "src/a.ts",
    "Src/c.ts",
  ]
    .map((path, index) => `${index}\t${index + 1}\t${path}`)
    .concat([""])
    .join("\0"),
  // Records that are not counts are skipped; a rename at the end is truncated.
  ["garbage", "1\t1\tkept.txt", "x\t1\tbad.txt", "1\t1\t", "only-source", ""].join("\0"),
  ["1\t1\t", "only-source"].join("\0"),
  ["1\t1\t", "source", "", "2\t2\tafter.txt"].join("\0"),
  "5\t5\tno-trailing-nul.txt",
  ["99999999999\t0\tbig.txt", "007\t010\tpadded.txt", ""].join("\0"),
  ["0\t0\tdup.txt", "1\t1\tdup.txt", ""].join("\0"),
  ["1\t1\t😀.txt", "1\t1\t😁.txt", "1\t1\tz.txt", ""].join("\0"),
];
for (const numstat of numstats) {
  rows.push({ type: "numstat", numstat, locale, files: parseTurnDiffFilesFromNumstat(numstat) });
}

const target = new URL("../crates/vcs/tests/fixtures/checkpoint.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length, locale }));
