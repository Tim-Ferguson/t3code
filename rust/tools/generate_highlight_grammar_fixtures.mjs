// Development oracle. The Rust application ships grammar/theme JSON data only.
import assert from "node:assert/strict";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
const root = resolve(import.meta.dirname, "../..");
const deps = resolve(root, "node_modules/.pnpm");
const original = await import(
  pathToFileURL(
    resolve(
      deps,
      "@shikijs+vscode-textmate@10.0.2/node_modules/@shikijs/vscode-textmate/dist/index.js",
    ),
  )
);
const { normalizeTheme } = await import(
  pathToFileURL(resolve(deps, "@shikijs+core@4.2.0/node_modules/@shikijs/core/dist/index.mjs"))
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
assert.equal(
  createHash("sha256").update(wasmBinary).digest("hex"),
  "fd885c2d12e5951e59d761ebd4a006e06254b1491fd6f530c92b69fb4d8d77d9",
);
const engine = await createOnigurumaEngine({ data: wasmBinary });
const onigLib = {
  createOnigScanner: (p) => engine.createScanner(p),
  createOnigString: (s) => engine.createString(s),
};
const grammars = [];
for (const language of [
  "rust",
  "typescript",
  "tsx",
  "javascript",
  "markdown",
  "html",
  "css",
  "json",
  "yaml",
]) {
  const module = await import(
    pathToFileURL(
      resolve(deps, `@shikijs+langs@4.2.0/node_modules/@shikijs/langs/dist/${language}.mjs`),
    )
  );
  for (const grammar of module.default) {
    if (!grammars.some((g) => g.scopeName === grammar.scopeName)) grammars.push(grammar);
  }
}
grammars.push({
  scopeName: "source.fixture",
  injections: {
    "L:source.fixture - string.quoted": { match: "TODO", name: "keyword.todo" },
    "R:source.fixture": { match: "abc", name: "keyword.late" },
    "L:(source.fixture, source.foreign) meta.parens": {
      match: "[0-9]+",
      name: "constant.injected.number",
    },
  },
  patterns: [
    { include: "#nested" },
    {
      begin: "(\"|')",
      end: "\\1",
      name: "string.quoted",
      beginCaptures: { 1: { name: "punctuation.begin" } },
      endCaptures: { 0: { name: "punctuation.end" } },
      patterns: [{ match: "\\\\.", name: "constant.escape" }],
    },
    {
      begin: "^>",
      while: "^>",
      name: "markup.quote",
      patterns: [{ match: "[0-9]+", name: "constant.number" }],
    },
    {
      match: "(\\[)([^\\]]*)(\\])",
      captures: {
        1: { name: "punctuation.begin" },
        3: { name: "punctuation.end" },
        2: {
          name: "meta.contents",
          patterns: [
            { match: "[0-9]+", name: "constant.number" },
            { match: "[a-z]+", name: "variable" },
          ],
        },
      },
    },
    { include: "source.foreign" },
    { match: "([a-z]+)", name: "entity.${1:/upcase}" },
  ],
  repository: {
    nested: {
      patterns: [
        {
          begin: "\\(",
          end: "\\)",
          name: "meta.parens",
          patterns: [{ include: "#nested" }, { match: "[0-9]+", name: "constant.number" }],
        },
      ],
    },
  },
});
grammars.push({
  scopeName: "source.foreign",
  patterns: [
    { match: "@([a-z]+)", name: "foreign.$1" },
    { begin: "@\\{", end: "\\}", name: "meta.foreign", patterns: [{ include: "$base" }] },
  ],
});
const themes = [];
for (const name of ["pierre-light", "pierre-dark"]) {
  const theme = normalizeTheme(
    JSON.parse(
      await readFile(
        resolve(deps, `@pierre+theme@2.0.0/node_modules/@pierre/theme/themes/${name}.json`),
        "utf8",
      ),
    ),
  );
  themes.push(theme);
}
const map = new Map(grammars.map((g) => [g.scopeName, g]));
const texts = [
  [
    "source.rust",
    'fn main() {\n    let s = r###"raw 😀\ncontinued"###;\n    /* outer /* inner */ end */\n    println!("hi {}", 42);\n}\n',
  ],
  [
    "source.rust",
    "// Σé😀 comment\n#[derive(Debug)]\nstruct Demo<'a> { name: &'a str }\nlet c = '\\n';\n",
  ],
  [
    "source.ts",
    'interface Box<T> { value: T }\nconst x = `hello ${1 + 2}\nnext ${/a+/gi.test("abc")}`;\n/* multi\nline */\nexport const call = async (x: number): Promise<number> => x;\n',
  ],
  [
    "source.tsx",
    "const A = <section title={`value ${n}`}>\n  {/* comment */}<span>Hello 😀</span>\n</section>;\n",
  ],
  [
    "text.html.markdown",
    "# Heading *emphasis*\n\n> quote\n> **strong**\n\n- item\n  nested `code`\n\n```rust\nlet n = 42;\n```\n\n[link](https://example.test)\n",
  ],
  [
    "text.html.markdown",
    '---\ntitle: Demo\n---\n\n<div class="x">hello</div>\n\n| A | B |\n| - | - |\n| 1 | 2 |\n',
  ],
  [
    "source.fixture",
    'TODO abc [one 12] (1 (2)) @word @{base 2}\n"quoted\ncontinued\\n" word\n>1\n>2\nend\n',
  ],
];
const units = (s) => Array.from({ length: s.length }, (_, i) => s.charCodeAt(i));
function stateOf(state) {
  const frames = [];
  while (state) {
    frames.unshift({
      names: state.nameScopesList.getScopeNames(),
      content: state.contentNameScopesList.getScopeNames(),
      end: state.endRule === null ? null : units(state.endRule),
      beganAtEol: state.beginRuleCapturedEOL,
    });
    state = state.parent;
  }
  return frames;
}
const rows = [];
const streaming = [];
for (const [scope, text] of texts) {
  const registry = new original.Registry({ onigLib, loadGrammar: (scope) => map.get(scope) });
  const grammar = await registry.loadGrammar(scope);
  let state = original.INITIAL;
  const lines = [];
  for (const line of text.split("\n")) {
    const result = grammar.tokenizeLine(line, state);
    state = result.ruleStack;
    lines.push({ line, tokens: result.tokens, state: stateOf(state) });
  }
  rows.push({ scope, text, lines });
  registry.dispose();
  const chars = Array.from(text);
  const stops = new Set([0, 1, 2, chars.length]);
  for (let i = 0; i < chars.length; i++)
    if (chars[i] === "\n" || i % 17 === 0) {
      stops.add(i);
      stops.add(i + 1);
    }
  const prefixes = [];
  const streamRegistry = new original.Registry({ onigLib, loadGrammar: (scope) => map.get(scope) });
  const streamGrammar = streamRegistry.loadGrammar(scope);
  for (const stop of [...stops].filter((i) => i <= chars.length).sort((a, b) => a - b)) {
    const prefix = chars.slice(0, stop).join("");
    let state = original.INITIAL;
    const lines = [];
    for (const line of prefix.split("\n")) {
      const result = streamGrammar.tokenizeLine(line, state);
      state = result.ruleStack;
      lines.push({ line, tokens: result.tokens, state: stateOf(state) });
    }
    prefixes.push({ prefix, lines });
  }
  streaming.push({ scope, prefixes });
  streamRegistry.dispose();
}
const { splitLines } = await import(
  pathToFileURL(
    resolve(deps, "@shikijs+primitive@4.2.0/node_modules/@shikijs/primitive/dist/index.mjs"),
  )
);
const transitions = [];
for (const [name, inputs] of [
  [
    "completed edits and shrinking",
    [
      'let a = 1;\nlet b = "x',
      'let a = 1;\nlet b = "xy";\n',
      'let a = 2;\nlet b = "xy";\n',
      "let a",
      "",
      "😀\n",
      "😀\nlet b = 2;\n",
    ],
  ],
  [
    "CRLF split and lone CR",
    [
      "// 😀\r",
      '// 😀\r\nlet s = "x',
      '// 😀\r\nlet s = "x";\r',
      '// 😀\r\nlet s = "x";\r\n',
      "a\rb",
      "a\rb\n",
      "a\r\n",
      "\r",
      "\r\n",
      "\n",
    ],
  ],
]) {
  const registry = new original.Registry({ onigLib, loadGrammar: (scope) => map.get(scope) });
  const grammar = registry.loadGrammar("source.rust");
  const versions = inputs.map((text) => {
    let state = original.INITIAL;
    return {
      text,
      lines: splitLines(text).map(([line]) => {
        const result = grammar.tokenizeLine(line, state);
        state = result.ruleStack;
        return { line, tokens: result.tokens, state: stateOf(state) };
      }),
    };
  });
  transitions.push({ name, scope: "source.rust", versions });
  registry.dispose();
}
const colored = [];
for (const theme of themes) {
  for (const [scope, text] of texts) {
    const registry = new original.Registry({
      onigLib,
      theme,
      loadGrammar: (scope) => map.get(scope),
    });
    const grammar = registry.loadGrammar(scope);
    const palette = registry.getColorMap();
    let state = original.INITIAL;
    const lines = [];
    for (const line of text.split("\n")) {
      const result = grammar.tokenizeLine2(line, state);
      state = result.ruleStack;
      const tokens = [];
      for (let i = 0; i < result.tokens.length; i += 2) {
        const value = result.tokens[i + 1];
        tokens.push({
          start: result.tokens[i],
          fg: palette[original.EncodedTokenMetadata.getForeground(value)],
          bg: palette[original.EncodedTokenMetadata.getBackground(value)],
          font: original.EncodedTokenMetadata.getFontStyle(value),
          type: original.EncodedTokenMetadata.getTokenType(value),
        });
      }
      lines.push({ line, tokens });
    }
    colored.push({ scope, theme: theme.name, text, lines });
    registry.dispose();
  }
}
const output = resolve(root, "rust/crates/highlight/tests/fixtures");
await mkdir(output, { recursive: true });
await writeFile(resolve(output, "grammars.json"), JSON.stringify(grammars) + "\n");
await writeFile(resolve(output, "themes.json"), JSON.stringify(themes) + "\n");
await writeFile(
  resolve(output, "tokenizer.jsonl"),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
await writeFile(
  resolve(output, "streaming.jsonl"),
  streaming.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
await writeFile(
  resolve(output, "transitions.jsonl"),
  transitions.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
await writeFile(
  resolve(output, "colored.jsonl"),
  colored.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    prefixes: streaming.reduce((n, r) => n + r.prefixes.length, 0),
    colored: colored.length,
    documents: rows.length,
    lines: rows.reduce((n, r) => n + r.lines.length, 0),
    grammars: grammars.length,
    themes: themes.length,
  }),
);
