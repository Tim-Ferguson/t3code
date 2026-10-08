// Target proof: shared Rust WASM calls the WebView's actual locale casing API.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const rust = createRequire(import.meta.url)(
  new URL("../target/terminal-node/t3_terminal.js", import.meta.url).pathname,
);
const rows = readFileSync(
  new URL("../crates/client/tests/fixtures/theme-collections.jsonl", import.meta.url),
  "utf8",
)
  .trim()
  .split("\n")
  .map(JSON.parse)
  .filter((row) => row.kind === "labels");
for (const row of rows) {
  assert.deepEqual(
    JSON.parse(rust.appearance_collection_labels(JSON.stringify(row.input), row.locale)),
    row.expected,
    `${row.locale}: ${JSON.stringify(row.input)}`,
  );
}
const defaultLocale = Intl.DateTimeFormat().resolvedOptions().locale;
const defaultRows = rows.filter((row) => defaultLocale.startsWith(row.locale));
for (const row of defaultRows)
  assert.deepEqual(
    JSON.parse(rust.appearance_collection_labels(JSON.stringify(row.input))),
    row.expected,
    `default locale: ${defaultLocale}`,
  );
console.log(
  JSON.stringify({
    localeWitnesses: rows.length,
    defaultLocale,
    defaultWitnesses: defaultRows.length,
  }),
);
