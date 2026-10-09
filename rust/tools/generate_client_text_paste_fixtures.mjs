// Runs the original's pasted-text rules over many inputs, for the Rust tests in t3-client
// (`src/text_paste.rs`). The original's file is converted to a plain module unchanged (types
// stripped) and driven directly:
//   packages/client-runtime/src/textPaste.ts   pastedTextDisposition, nextPastedTextFileName,
//                                              isPasteAsTextShortcut
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const source = readFileSync(
  new URL("../../packages/client-runtime/src/textPaste.ts", import.meta.url),
  "utf8",
);
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-text-paste-"));
const file = NodePath.join(scratch, "textPaste.mjs");
writeFileSync(file, stripTypeScriptTypes(source));
const paste = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const T = paste.PASTED_TEXT_ATTACHMENT_THRESHOLD_BYTES;
const rows = [];
const texts = [
  "",
  "short",
  "a".repeat(T - 1),
  "a".repeat(T),
  "a".repeat(T + 1),
  // Multi-byte text: the byte count passes the threshold well before the character count does.
  "é".repeat(T / 2 - 1),
  "é".repeat(T / 2),
  "😀".repeat(T / 4 - 1),
  "😀".repeat(T / 4),
  "日本語".repeat(Math.ceil(T / 9)),
];
for (const text of texts)
  for (const canAttach of [true, false])
    for (const bypassAutoAttachment of [undefined, true, false])
      for (const wouldExceedInputLimit of [undefined, true, false])
        rows.push({
          fn: "disposition",
          input: { text, canAttach, bypassAutoAttachment, wouldExceedInputLimit },
          output: paste.pastedTextDisposition({
            text,
            canAttach,
            bypassAutoAttachment,
            wouldExceedInputLimit,
          }),
        });
const names = [
  [],
  ["pasted-text.txt"],
  ["PASTED-TEXT.TXT"],
  ["pasted-text.txt", "pasted-text-2.txt"],
  ["pasted-text.txt", "pasted-text-3.txt"],
  ["pasted-text-2.txt"],
  ["a.png", "pasted-text.txt", "Pasted-Text-2.txt", "pasted-text-3.txt"],
];
for (const existing of names)
  rows.push({ fn: "name", input: existing, output: paste.nextPastedTextFileName(existing) });
for (const key of ["v", "V", "x"])
  for (const shiftKey of [true, false])
    for (const metaKey of [true, false])
      for (const ctrlKey of [true, false])
        for (const altKey of [true, false])
          for (const mac of [true, false])
            rows.push({
              fn: "shortcut",
              input: { key, shiftKey, metaKey, ctrlKey, altKey, mac },
              output: paste.isPasteAsTextShortcut({ key, shiftKey, metaKey, ctrlKey, altKey }, mac),
            });
writeFileSync(
  new URL("../crates/client/tests/fixtures/text-paste.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
