// Development-only original ChatMarkdown fence and command policy; Node24.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/web/src/components/ChatMarkdown.tsx", import.meta.url),
  "utf8",
);
function extract(start, end) {
  const first = source.indexOf(start),
    last = source.indexOf(end, first);
  if (first < 0 || last < 0) throw Error(`Missing original boundary ${start}`);
  return stripTypeScriptTypes(source.slice(first, last));
}
const languageConstant = source.match(/const CODE_FENCE_LANGUAGE_REGEX = .*;/)?.[0];
if (!languageConstant) throw Error("Missing language regex");
const language = new Function(
  "value",
  languageConstant +
    extract("function extractFenceLanguage(", "const FENCE_TITLE_ATTR_REGEX") +
    "return extractFenceLanguage(value)",
);
const title = new Function(
  "value",
  extract("const FENCE_TITLE_ATTR_REGEX", "function extractPreCodeMeta(") +
    "return extractFenceTitle(value)",
);
const closed = new Function(
  "value",
  extract("function isClosedCodeFence(", "type MarkdownAstNode") +
    "return isClosedCodeFence({position:{start:{offset:0},end:{offset:value.length}}},value)",
);
const run = new Function(
  "code",
  "language",
  "isStreaming",
  "onRunShellCommand",
  extract("  const command = code.trim();", "  const handleCopy = useCallback(") + "return canRun",
);
const rows = [];
const languages = [
  undefined,
  "",
  "language-rust",
  "language-gitignore",
  "language-GITIGNORE",
  "language-bash foo",
  "first language-js",
  "language-a\u0085b",
  "\uFEFFlanguage-pwsh",
  "language-ts\uFEFFx",
  " language-text\tlanguage-js",
];
for (const value of languages)
  rows.push({ kind: "language", value: value ?? null, expected: language(value) });
const metadata = [
  undefined,
  "",
  "src/main.rs",
  "title=main.rs",
  'title="a b.rs"',
  "file='index.ts'",
  "FILENAME=a.js",
  "notitle=a.rs",
  ' title="" foo.md',
  "α.rs",
  "@scope/a.ts",
  "a.b.cpp",
  "a.🦀",
  "\uFEFFtitle=BOM.rs",
  "\u0085title=NEL.rs",
  "name foo.txt bar.rs",
  'title="a.rs" title=b.ts',
];
for (const value of metadata)
  rows.push({ kind: "title", value: value ?? null, expected: title(value) });
const fences = [
  "```",
  "```\n",
  "```rs\na\n```",
  "```rs\na\n~~~~",
  "~~~~js\na\n~~~",
  "```rs\na\n`````",
  " ```rs\na\n```",
  "```rs\na\n> ```",
  "```rs\na\n```\r",
  "```rs\na\n```\n",
  "```rs\na\n```x",
  "~~~\nx\n~~~",
  "```\n```\ntext",
];
for (const value of fences) rows.push({ kind: "closed", value, expected: closed(value) });
const commands = [
  "",
  "\n",
  "echo ok\n",
  "echo ok",
  " echo ok \n",
  "echo 😀\n",
  "echo ok\\\n",
  "echo ok\r\n",
  "printf 'one\\ntwo'\n",
  "echo\tgood\n",
  "echo \u202Ebad\n",
  "echo \u200Bbad\n",
  "echo \uFEFFok\n",
  "\uFEFFecho ok\n",
  "echo\u0085bad\n",
  "echo \u0000bad\n",
  "echo ok\nnext\n",
];
for (const code of commands)
  for (const lang of [
    "bash",
    "sh",
    "zsh",
    "fish",
    "shell",
    "powershell",
    "pwsh",
    "BASH",
    "rust",
    "text",
  ])
    for (const streaming of [false, true])
      for (const available of [false, true])
        rows.push({
          kind: "run",
          code,
          language: lang,
          streaming,
          available,
          expected: run(code, lang, streaming, available ? () => {} : undefined),
        });
writeFileSync(
  new URL("../crates/client/tests/fixtures/markdown.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original markdown fence/command witnesses`);
