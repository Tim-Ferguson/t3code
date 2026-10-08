// Original react-markdown's remark parser + GFM and ChatMarkdown fence helpers.
// Development oracle only; runtime parsing and components are Rust.
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire, stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";
const web = createRequire(new URL("../../apps/web/package.json", import.meta.url));
const markdown = createRequire(web.resolve("react-markdown"));
const { unified } = await import(pathToFileURL(web.resolve("unified")));
const { default: parse } = await import(pathToFileURL(markdown.resolve("remark-parse")));
const { default: gfm } = await import(pathToFileURL(markdown.resolve("remark-gfm")));
const original = readFileSync(
  new URL("../../apps/web/src/components/ChatMarkdown.tsx", import.meta.url),
  "utf8",
);
function extract(start, end) {
  const a = original.indexOf(start),
    b = original.indexOf(end, a);
  if (a < 0 || b < 0) throw Error("Missing original " + start);
  return stripTypeScriptTypes(original.slice(a, b));
}
const constant = original.match(/const CODE_FENCE_LANGUAGE_REGEX = .*;/)?.[0];
if (!constant) throw Error("Missing language regex");
const language = new Function(
  "value",
  constant +
    extract("function extractFenceLanguage(", "const FENCE_TITLE_ATTR_REGEX") +
    "return extractFenceLanguage(value)",
);
const title = new Function(
  "value",
  extract("const FENCE_TITLE_ATTR_REGEX", "function extractPreCodeMeta(") +
    "return extractFenceTitle(value)",
);
const rows = [];
const infos = [
  "",
  "rust",
  "gitignore",
  "GITIGNORE",
  "rust src/main.rs",
  'ts title="a b.ts"',
  "sh\tfilename=a.sh",
  "js\u0085title=x.js",
  "js\uFEFFtitle=x.js",
  "js \uFEFFtitle=x.js",
  "js\u00A0title=x.js",
  "rust title=🦀.rs",
  "text file=a.md",
  "rust title=",
  'rust title=""',
  "ts a\\.ts",
  "ts &quot;x&quot;",
  "ts title=&quot;a.ts&quot;",
];
for (const info of infos)
  for (const prefix of ["", "> ", "- ", "   "])
    for (const ending of ["", "\n```", "\n```\n\nstream"]) {
      const source =
        prefix +
        "```" +
        info +
        "\n" +
        (prefix === "- " ? "  " : prefix) +
        'const value = "😀";' +
        ending.replaceAll("\n", "\n" + (prefix === "- " ? "  " : prefix));
      const tree = unified().use(parse).use(gfm).parse(source);
      const expected = [];
      function visit(node) {
        if (node.type === "code")
          expected.push({
            language: language(node.lang ? "language-" + node.lang : undefined),
            title: title(node.meta?.trim()),
            code: node.value + "\n",
          });
        node.children?.forEach(visit);
      }
      visit(tree);
      rows.push({ source, expected });
    }
for (const source of [
  "| Left | Center | Right | None |\n|:--|:--:|--:|--|\n| a | b | c | d |",
  "| a | b |\n|---|---|\n| **a** | `b` |",
]) {
  const table = unified()
    .use(parse)
    .use(gfm)
    .parse(source)
    .children.find((node) => node.type === "table");
  if (!table) throw Error("Original table missing");
  rows.push({ source, align: table.align });
}
writeFileSync(
  new URL("../crates/ui/tests/fixtures/markdown-documents.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original parsed markdown code/table witnesses`);
