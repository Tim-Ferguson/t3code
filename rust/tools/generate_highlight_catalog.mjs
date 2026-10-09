// Data-only upstream grammar/theme assets; the application tokenizer is Rust.
import assert from "node:assert/strict";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { gzipSync } from "node:zlib";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
const root = resolve(import.meta.dirname, "../..");
const folder = resolve(
  root,
  "node_modules/.pnpm/@shikijs+langs@4.2.0/node_modules/@shikijs/langs/dist",
);
const { languageNames } = await import(pathToFileURL(resolve(folder, "index.mjs")));
const grammars = new Map(),
  languages = {};
for (const name of languageNames) {
  const { default: records } = await import(pathToFileURL(resolve(folder, `${name}.mjs`)));
  const main = records.find((record) => record.name === name);
  assert.ok(main, `missing primary grammar ${name}`);
  const entry = { scope: main.scopeName, grammars: records.map((record) => record.scopeName) };
  for (const alias of [name, ...(main.aliases ?? [])]) languages[alias] = entry;
  for (const record of records) {
    const text = JSON.stringify(record);
    if (grammars.has(record.scopeName))
      assert.equal(grammars.get(record.scopeName), text, `conflicting scope ${record.scopeName}`);
    grammars.set(record.scopeName, text);
  }
}
const chunks = [],
  scopes = {};
let offset = 0;
for (const [scope, text] of [...grammars].sort(([a], [b]) => a.localeCompare(b, "en"))) {
  const bytes = Buffer.from(text),
    compressed = gzipSync(bytes, { level: 9, mtime: 0 });
  scopes[scope] = {
    offset,
    length: compressed.length,
    bytes: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
  chunks.push(compressed);
  offset += compressed.length;
}
const out = resolve(root, "rust/crates/highlight/assets");
await mkdir(out, { recursive: true });
const pack = Buffer.concat(chunks);
await writeFile(resolve(out, "grammars.pack"), pack);
await writeFile(
  resolve(out, "catalog.json"),
  JSON.stringify({
    version: 1,
    source: "@shikijs/langs@4.2.0",
    sha256: createHash("sha256").update(pack).digest("hex"),
    languages,
    scopes,
  }) + "\n",
);
const themes = [];
const { normalizeTheme } = await import(
  pathToFileURL(
    resolve(
      root,
      "node_modules/.pnpm/@shikijs+core@4.2.0/node_modules/@shikijs/core/dist/index.mjs",
    ),
  )
);
for (const name of ["pierre-light", "pierre-dark"])
  themes.push(
    normalizeTheme(
      JSON.parse(
        await readFile(
          resolve(
            root,
            `node_modules/.pnpm/@pierre+theme@2.0.0/node_modules/@pierre/theme/themes/${name}.json`,
          ),
          "utf8",
        ),
      ),
    ),
  );
await writeFile(resolve(out, "themes.json"), JSON.stringify(themes) + "\n");
console.log(
  JSON.stringify({
    languages: languageNames.length,
    aliases: Object.keys(languages).length,
    grammars: grammars.size,
    compressedBytes: pack.length,
    uncompressedBytes: [...grammars.values()].reduce((n, s) => n + Buffer.byteLength(s), 0),
  }),
);
