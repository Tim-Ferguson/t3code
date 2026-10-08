// Run: PATH=/tmp/node-v24.13.1-darwin-arm64/bin:$PATH node rust/tools/generate_acp_registry_fixtures.mjs
import { readFileSync, writeFileSync } from "node:fs";
const source = readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
const start = source.indexOf("const EXACT_RUNNER_VERSION =");
const end = source.indexOf("const NpxPackage =", start);
if (start < 0 || end < start) throw new Error("Original package regexp markers missing");
const [npx, uvx] = new Function(
  `${source.slice(start, end)};return [EXACT_NPX_PACKAGE,EXACT_UVX_PACKAGE];`,
)();
const names = [
  "pkg",
  "Pkg",
  "K",
  "ſ",
  "İ",
  "ı",
  "@scope/name",
  "@scope\u0085/name",
  "@scope\ufeff/name",
  "@scope/name\u0085",
  "@scope/name\ufeff",
  "@scope//name",
  "@scope/name/child",
  "@scope/name;command",
  "",
  "../pkg",
];
const versions = [
  "1.2.3",
  "v1.2.3",
  "V1.2.3",
  "1.2",
  "1.2.3-alpha.1+build",
  "latest",
  "1.2.3\n",
  "1.2.3\r",
  "1.2.3\r\n",
  "1.2.3\u2028",
  "1.2.3 ",
];
const rows = [];
for (const npm of [true, false])
  for (const name of names)
    for (const version of versions)
      for (const separator of npm ? ["@"] : ["@", "=="]) {
        const input = `${name}${separator}${version}`;
        rows.push({ input, npx: npm, ok: input.length <= 256 && (npm ? npx : uvx).test(input) });
      }
for (const code of [
  9, 10, 11, 12, 13, 32, 0x85, 0xa0, 0x1680, 0x2000, 0x200a, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
  0xfeff,
]) {
  const input = `@scope${String.fromCodePoint(code)}/name@1.2.3`;
  rows.push({ input, npx: true, ok: npx.test(input) });
}
writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-packages.jsonl",
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original pinned-package witnesses`);
