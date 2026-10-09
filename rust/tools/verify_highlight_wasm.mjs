// Actual Rust WASM tokenizer + upstream scanner dependency; no browser UI reads.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const root = resolve(import.meta.dirname, "..");
const require = createRequire(import.meta.url);
const wasm = require(
  resolve(
    process.env.T3_HIGHLIGHT_ORACLE_DIR ?? resolve(root, "target/highlight-oracle-node"),
    "t3_highlight_wasm_oracle.js",
  ),
);
const fixtures = resolve(root, "crates/highlight/tests/fixtures");
const rows = async (name) =>
  (await readFile(resolve(fixtures, name), "utf8")).trimEnd().split("\n").map(JSON.parse);
const oracle = await wasm.load(await readFile(resolve(fixtures, "grammars.json"), "utf8"));
let scanner = 0,
  lines = 0,
  prefixes = 0,
  transitions = 0,
  colored = 0,
  rejected = 0;
for (const row of await rows("scanner.jsonl")) {
  assert.deepEqual(
    JSON.parse(oracle.scanner(JSON.stringify(row))),
    row.expected,
    `scanner ${++scanner}`,
  );
}
// These are intentional safe constructor errors, not claimed source parity.
const malformed = JSON.parse(
  await readFile(resolve(fixtures, "invalid-observations.json"), "utf8"),
);
for (let round = 0; round < 20; round++)
  for (const witness of malformed.observations) {
    for (const patterns of [
      [witness.pattern],
      [[118, 97, 108, 105, 100], witness.pattern],
      [witness.pattern, [118, 97, 108, 105, 100]],
    ]) {
      assert.equal(
        JSON.parse(oracle.scanner(JSON.stringify({ kind: "reject", input: { patterns } }))),
        true,
      );
      rejected++;
    }
    assert.deepEqual(
      JSON.parse(
        oracle.scanner(
          JSON.stringify({
            kind: "match",
            input: {
              patterns: [[118, 97, 108, 105, 100]],
              text: [118, 97, 108, 105, 100],
              start: 0,
              options: 0,
            },
          }),
        ),
      ),
      { index: 0, captureIndices: [{ start: 0, end: 5, length: 5 }] },
    );
  }
for (const row of await rows("tokenizer.jsonl")) {
  const stream = oracle.tokenizer(row.scope, "");
  const actual = JSON.parse(stream.document(row.text));
  assert.deepEqual(
    actual,
    row.lines.map((line) => ({ tokens: line.tokens, state: line.state })),
    row.scope,
  );
  lines += actual.length;
  stream.free();
}
for (const row of await rows("streaming.jsonl")) {
  const stream = oracle.tokenizer(row.scope, "");
  for (const prefix of row.prefixes) {
    assert.deepEqual(
      JSON.parse(stream.document(prefix.prefix)),
      prefix.lines.map((line) => ({ tokens: line.tokens, state: line.state })),
      `prefix ${row.scope} ${prefixes++}`,
    );
  }
  stream.free();
}
for (const row of await rows("transitions.jsonl")) {
  const stream = oracle.tokenizer(row.scope, "");
  for (const version of row.versions) {
    assert.deepEqual(
      JSON.parse(stream.document(version.text)),
      version.lines.map((line) => ({ tokens: line.tokens, state: line.state })),
      `transition ${row.name} ${transitions++}`,
    );
  }
  stream.free();
}
const themes = JSON.parse(await readFile(resolve(fixtures, "themes.json"), "utf8"));
for (const row of await rows("colored.jsonl")) {
  const stream = oracle.tokenizer(
    row.scope,
    JSON.stringify(themes.find((theme) => theme.name === row.theme)),
  );
  assert.deepEqual(
    JSON.parse(stream.document(row.text)).map((line) => line.tokens),
    row.lines.map((line) => line.tokens),
    `theme ${row.theme} ${row.scope}`,
  );
  colored++;
  stream.free();
}
oracle.free();
assert.deepEqual(
  { scanner, lines, prefixes, transitions, colored, rejected },
  { scanner: 2364, lines: 54, prefixes: 178, transitions: 17, colored: 14, rejected: 240 },
);
console.log(
  JSON.stringify({
    scanner,
    lines,
    prefixes,
    transitions,
    colored,
    safeMalformedRejections: rejected,
  }),
);
