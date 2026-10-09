import { readFile, writeFile } from "node:fs/promises";
import { gunzipSync } from "node:zlib";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
const root = resolve(import.meta.dirname, "../..");
const deps = resolve(root, "node_modules/.pnpm");
const { Registry, INITIAL } = await import(
  pathToFileURL(
    resolve(
      deps,
      "@shikijs+vscode-textmate@10.0.2/node_modules/@shikijs/vscode-textmate/dist/index.js",
    ),
  )
);
const { createOnigurumaEngine } = await import(
  pathToFileURL(
    resolve(
      deps,
      "@shikijs+engine-oniguruma@4.2.0/node_modules/@shikijs/engine-oniguruma/dist/index.mjs",
    ),
  )
);
const { wasmBinary } = await import(
  pathToFileURL(
    resolve(
      deps,
      "@shikijs+engine-oniguruma@4.2.0/node_modules/@shikijs/engine-oniguruma/dist/wasm-inlined.mjs",
    ),
  )
);
const engine = await createOnigurumaEngine({ data: wasmBinary });
const onigLib = {
  createOnigScanner: (p) => engine.createScanner(p),
  createOnigString: (s) => engine.createString(s),
};
const index = JSON.parse(
  await readFile(resolve(root, "rust/crates/highlight/assets/catalog.json"), "utf8"),
);
const pack = await readFile(resolve(root, "rust/crates/highlight/assets/grammars.pack"));
const map = new Map(
  Object.entries(index.scopes).map(([scope, r]) => [
    scope,
    JSON.parse(gunzipSync(pack.subarray(r.offset, r.offset + r.length))),
  ]),
);
const rows = [];
for (const [scope, raw] of map) {
  const language = raw.name;
  if (index.languages[language]?.scope !== scope) throw new Error(`missing ${language}`);
  const registry = new Registry({ onigLib, loadGrammar: (s) => map.get(s) });
  const grammar = registry.loadGrammar(scope);
  let state = INITIAL;
  const text = 'let value = "hello 😀"; // comment\nvalue + 42';
  const lines = text.split("\n").map((line) => {
    const result = grammar.tokenizeLine(line, state);
    state = result.ruleStack;
    const frames = [];
    for (let frame = state; frame; frame = frame.parent)
      frames.unshift({
        names: frame.nameScopesList.getScopeNames(),
        content: frame.contentNameScopesList.getScopeNames(),
        end:
          frame.endRule === null
            ? null
            : Array.from(frame.endRule, (ch) => ch).flatMap((ch) =>
                Array.from({ length: ch.length }, (_, i) => ch.charCodeAt(i)),
              ),
        beganAtEol: frame.beginRuleCapturedEOL,
      });
    return { line, tokens: result.tokens, state: frames };
  });
  rows.push({ language, scope, text, lines });
  registry.dispose();
}
await writeFile(
  resolve(root, "rust/crates/highlight/tests/fixtures/catalog.jsonl"),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    languages: rows.length,
    lines: rows.reduce((n, row) => n + row.lines.length, 0),
  }),
);
