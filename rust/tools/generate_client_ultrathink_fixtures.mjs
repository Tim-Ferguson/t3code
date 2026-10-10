// Runs the original's prompt-injected effort helpers over many prompts, for
// `crates/client/tests/ultrathink_parity.rs`.
//   isClaudeUltrathinkPrompt, applyClaudePromptEffortPrefix   packages/shared/src/model.ts
// and the `Ultrathink:` handling of `TraitsPicker.tsx` (`handleSelectChange`, `getSelectedTraits`).
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const model = readFileSync(new URL("../../packages/shared/src/model.ts", import.meta.url), "utf8");
const cut = (start, end) => {
  const from = model.indexOf(start);
  const to = model.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start}`);
  return model.slice(from, to);
};
const body = [
  cut("export function isClaudeUltrathinkPrompt", "/** Compare Codex model families"),
  cut("export function applyClaudePromptEffortPrefix", "\n}\n") + "\n}\n",
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-ultrathink-"));
const file = NodePath.join(scratch, "ultrathink.mjs");
writeFileSync(
  file,
  `${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { isClaudeUltrathinkPrompt, applyClaudePromptEffortPrefix };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const prompts = [
  "",
  "   ",
  "fix it",
  "ultrathink about this",
  "Ultrathink:\nfix it",
  "ULTRATHINK",
  "ultrathinking",
  "pre-ultrathink",
  "my_ultrathink",
  "ultrathink_x",
  "x ultrathink.",
  "Ultrathink:   \n  ultrathink it",
  "ultrathink:\tnow",
  "/deploy now",
  "/deploy",
  "/home/theo/app.ts is broken",
  "/plugin:skill args",
  "/a/b c",
  "  padded  ",
  "Ultrathink: already",
  "ultrathink: lower prefix",
  " nbsp ultrathink",
];
const rows = [];
for (const prompt of prompts) {
  const stripped = prompt.replace(/^Ultrathink:\s*/i, "");
  for (const effort of [null, "high", "ultrathink"])
    rows.push({
      prompt,
      effort,
      is: original.isClaudeUltrathinkPrompt(prompt),
      applied: original.applyClaudePromptEffortPrefix(prompt, effort),
      stripped,
      inBody: original.isClaudeUltrathinkPrompt(stripped),
    });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/ultrathink.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
