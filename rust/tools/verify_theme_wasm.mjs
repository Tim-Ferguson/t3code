// Actual Rust WASM target math compared with unchanged original theme/Culori witnesses.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const wasm = require("../target/theme-oracle-node/t3_theme_wasm_oracle.js");
const rows = (name) =>
  readFileSync(
    new URL(`../crates/client/tests/fixtures/theme-${name}.jsonl`, import.meta.url),
    "utf8",
  )
    .trimEnd()
    .split("\n")
    .map((line) => JSON.parse(line));
let exact = 0,
  thrown = 0,
  vivid = 0,
  families = 0,
  editors = 0,
  vscode = 0;
const failures = [];
for (const [i, row] of rows("colors").entries()) {
  const actual = JSON.parse(wasm.color(JSON.stringify(row.input)));
  try {
    if (row.originalError) {
      assert.deepEqual(actual, { canonical: null, hex: null });
      thrown++;
    } else {
      assert.equal(actual.canonical, row.canonical);
      assert.equal(actual.hex, row.hex);
      exact++;
    }
  } catch (error) {
    failures.push({
      kind: "color",
      i,
      input: row.input,
      actual,
      expected: { canonical: row.canonical, hex: row.hex },
      message: error.message,
    });
  }
}
for (const [i, row] of rows("vivid").entries()) {
  const actual = JSON.parse(wasm.vivid(JSON.stringify(row)));
  try {
    assert.deepEqual(actual, row.expected);
    vivid++;
  } catch (error) {
    failures.push({
      kind: "vivid",
      i,
      input: { appearance: row.appearance, background: row.background, accent: row.accent },
      actual,
      expected: row.expected,
    });
  }
}
for (const [i, row] of rows("families").entries()) {
  const actual = JSON.parse(wasm.family(JSON.stringify(row)));
  try {
    assert.deepEqual(actual, row.expected);
    families++;
  } catch (error) {
    failures.push({
      kind: "family",
      i,
      input: { appearance: row.appearance, role: row.role, color: row.color },
      actual,
      expected: row.expected,
    });
  }
}
const [header, ...editorRows] = rows("editor");
for (const [i, row] of editorRows.entries()) {
  const input = {
    initial: header.dictionary[row.initial],
    draft: { ...row.draft, colors: header.dictionary[row.draft.colors] },
  };
  const actual = JSON.parse(wasm.editor(JSON.stringify(input)));
  const expected = row.error
    ? { target: row.target, error: row.error }
    : {
        target: row.target,
        saved: header.dictionary[row.saved],
        created: row.context.created,
        mergedAppearance: row.context.mergedAppearance ?? null,
      };
  try {
    assert.deepEqual(actual, expected);
    editors++;
  } catch (error) {
    failures.push({ kind: "editor", i, actual, expected });
  }
}
const [vscodeHeader, ...vscodeRows] = readFileSync(
  new URL("../crates/client/tests/fixtures/vscode-themes.jsonl", import.meta.url),
  "utf8",
)
  .trimEnd()
  .split("\n")
  .map((line) => JSON.parse(line));
for (const [i, row] of vscodeRows.entries()) {
  const actual = JSON.parse(
    wasm.vscode(JSON.stringify({ ...row, input: vscodeHeader.dictionary[row.input] })),
  );
  const expected = row.error
    ? { error: row.error }
    : { value: vscodeHeader.dictionary[row.expected] };
  if (row.kind === "import") expected.isFile = row.isFile;
  try {
    assert.deepEqual(actual, expected);
    vscode++;
  } catch (error) {
    failures.push({ kind: "vscode", i, actual, expected });
  }
}
const [packageHeader, ...packageRows] = gunzipSync(
  readFileSync(new URL("../crates/client/tests/fixtures/openvsx-themes.jsonl.gz", import.meta.url)),
)
  .toString()
  .trimEnd()
  .split("\n")
  .map((line) => JSON.parse(line));
let packages = 0;
for (const [i, row] of packageRows.entries()) {
  const actual = JSON.parse(
    wasm.openvsx(JSON.stringify({ ...row, input: packageHeader.dictionary[row.input] })),
  );
  const expected = row.error
    ? { error: row.error }
    : { value: packageHeader.dictionary[row.expected] };
  try {
    assert.deepEqual(actual, expected);
    packages++;
  } catch (error) {
    failures.push({ kind: "openvsx", i, actual, expected });
  }
}
assert.equal(failures.length, 0, JSON.stringify(failures.slice(0, 10), null, 2));
console.log(
  JSON.stringify({
    exactColors: exact,
    originalParserThrowsRejected: thrown,
    vivid,
    families,
    editors,
    vscode,
    packages,
  }),
);
