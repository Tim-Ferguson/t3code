// Development oracle only: execute the pinned original bindings, never production UI code.
import assert from "node:assert/strict";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "../..");
const enginePath = resolve(
  root,
  "node_modules/.pnpm/@shikijs+engine-oniguruma@4.2.0/node_modules/@shikijs/engine-oniguruma/dist/index.mjs",
);
const textmatePath = resolve(
  root,
  "node_modules/.pnpm/@shikijs+vscode-textmate@10.0.2/node_modules/@shikijs/vscode-textmate/dist/index.js",
);
const engineSource = await readFile(enginePath, "utf8");
const textmateSource = await readFile(textmatePath, "utf8");
assert(!/^import /m.test(engineSource));
assert(!/^import /m.test(textmateSource));
const wasmModule = await import(pathToFileURL(resolve(enginePath, "../wasm-inlined.mjs")));
const wasm = wasmModule.wasmBinary;
assert.equal(
  createHash("sha256").update(wasm).digest("hex"),
  "fd885c2d12e5951e59d761ebd4a006e06254b1491fd6f530c92b69fb4d8d77d9",
);
const sourceModule = (source, exports) =>
  import(
    `data:text/javascript;base64,${Buffer.from(`${source}\nexport { ${exports} };`).toString("base64")}`
  );
const original = await sourceModule(engineSource, "UtfString, onigBinding");
const { UtfString } = original;
const engine = await original.createOnigurumaEngine({ data: wasm });
const { RegExpSource } = await sourceModule(textmateSource, "RegExpSource");
const units = (s) => Array.from({ length: s.length }, (_, i) => s.charCodeAt(i));
const rows = [];
function find(scanner, patterns, text, start, options) {
  return scanner.findNextMatchSync(text, start, options);
}
const put = (kind, input, expected) => rows.push({ kind, input, expected });

const strings = [
  "",
  "abc",
  "é",
  "A😀B",
  "😀😀",
  "a\ud800b",
  "\udc00",
  "\ud800\ud800\udc00",
  "\u0085\ufeff",
  "Σ İ 🇺🇸",
  "x".repeat(1001),
  "é".repeat(501),
];
for (const text of strings) {
  const utf = new UtfString(text);
  const string = engine.createString(text);
  try {
    put(
      "input",
      { units: units(text) },
      {
        bytes: [...utf.utf8Value],
        toByte: Array.from({ length: text.length + 2 }, (_, i) =>
          string.convertUtf16OffsetToUtf8(i),
        ),
        toUnit: Array.from({ length: utf.utf8Length + 2 }, (_, i) =>
          string.convertUtf8OffsetToUtf16(i),
        ),
      },
    );
  } finally {
    string.dispose();
  }
}
const patterns = [
  "",
  "\\Afoo\\Gbar\\z",
  "\\\\A\\\\z",
  "\\1-\\12-\\0",
  "\\\\1",
  "\\99999999999999999999",
  "[\\A]",
  "😀\\z",
  "x\\",
  "\\z\\z",
  "\\A\\G",
  "\\٢",
];
const captures = [
  { start: 0, end: 3 },
  { start: 1, end: 5 },
  { start: 7, end: 2 },
  { start: 500, end: 900 },
];
const line = ".a😀- #\u0085\ufeff";
for (const pattern of patterns) {
  for (const replacement of [null, "\\G\\A\\2", "plain", "\\z"]) {
    const source = new RegExpSource(pattern, 7);
    if (replacement !== null) source.setSource(replacement);
    put(
      "regex",
      {
        pattern: units(pattern),
        replacement: replacement === null ? null : units(replacement),
        line: units(line),
        captures,
      },
      {
        source: units(source.source),
        hasAnchor: source.hasAnchor,
        hasBackreferences: source.hasBackReferences,
        anchors: [false, true].flatMap((a) =>
          [false, true].map((g) => units(source.resolveAnchors(a, g))),
        ),
        backreferences: units(source.resolveBackReferences(line, captures)),
      },
    );
  }
}
const scanners = [
  ["foo", "bar"],
  ["a", "ab"],
  ["ab", "a"],
  ["(a)?(b)"],
  ["\\Afoo", "\\Gfoo", "foo\\z"],
  ["(?<=a)b", "b"],
  ["(?i:Σ|İ|é)"],
  ["\\p{L}+", "\\d+"],
  ["😀", "."],
  ["(a)(?<named>b)?"],
  ["^.*$"],
  ["\\bword\\b"],
  ["a*", "b"],
  [],
];
for (const patterns of scanners) {
  const scanner = engine.createScanner(patterns);
  try {
    for (const text of [
      "",
      "foo bar",
      "ab",
      "b",
      "a\nb",
      "a😀b",
      "a\ud800b",
      "\udc00",
      "Σİé 12",
      "word",
      "é".repeat(501) + "foo",
      "x".repeat(1001) + "ab",
    ]) {
      for (const start of [...new Set([0, 1, text.length].filter((i) => i <= text.length))]) {
        for (const options of [0, 1, 2, 4, 7]) {
          put(
            "match",
            { patterns: patterns.map(units), text: units(text), start, options },
            find(scanner, patterns, text, start, options),
          );
        }
      }
    }
  } finally {
    scanner.dispose();
  }
}
for (const pattern of ["", "$", ".", "\\G"]) {
  const scanner = engine.createScanner([pattern]);
  try {
    for (const text of ["", "abc", "😀", "\ud800"]) {
      for (const start of [text.length + 1, 1000000, 4294967296, 4294967297]) {
        put(
          "match",
          { patterns: [units(pattern)], text: units(text), start, options: 0 },
          find(scanner, [pattern], text, start, 0),
        );
      }
    }
  } finally {
    scanner.dispose();
  }
}
const observations = [];
for (const pattern of ["[", "(", "(?invalid)", "\\k<missing>"]) {
  const scanner = engine.createScanner([pattern]);
  try {
    observations.push({
      pattern: units(pattern),
      constructorRejected: false,
      firstFind: scanner.findNextMatchSync("abc", 0, 0),
    });
  } finally {
    scanner.dispose();
  }
}
const output = resolve(root, "rust/crates/highlight/tests/fixtures/scanner.jsonl");
await mkdir(resolve(output, ".."), { recursive: true });
await writeFile(output, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
await writeFile(
  resolve(output, "../invalid-observations.json"),
  JSON.stringify(
    { upstreamNullDereference: "vscode-oniguruma1.7.0/src/onig.cc159-161", observations },
    null,
    2,
  ) + "\n",
);
console.log(
  JSON.stringify({
    rows: rows.length,
    engine: "4.2.0",
    textmate: "10.0.2",
    sourceHashes: [engineSource, textmateSource].map((s) =>
      createHash("sha256").update(s).digest("hex"),
    ),
  }),
);
