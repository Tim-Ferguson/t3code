// From repo root, Node >=23.6: node rust/tools/generate_provider_install_fixtures.mjs
// Reads the unchanged source CodexInstallation.ts: its official release table through the exported
// resolver, and its archive-entry safety rule and version pattern by evaluating the source text itself.
import fs from "node:fs";
import { resolveCodexReleaseAsset } from "../../apps/server/src/provider/CodexInstallation.ts";

const source = fs.readFileSync("apps/server/src/provider/CodexInstallation.ts", "utf8");
const between = (start, end) => {
  const begin = source.indexOf(start);
  const finish = source.indexOf(end, begin);
  if (begin < 0 || finish < begin) throw Error("Missing original markers: " + start);
  return source.slice(begin + start.length, finish);
};
// `if (<rule>) return yield* installationError("extract", ...)`
const rule = between(
  "      if (\n        entries.length > 10000",
  '\n      )\n        return yield* installationError("extract"',
);
const unsafe = new Function("entries", "types", "return (entries.length > 10000" + rule + ");");
const version = new RegExp(between("Schema.isPattern(/", "/u)"), "u");

const fixtures = [];
for (const platform of ["darwin", "linux", "win32", "freebsd", "sunos"])
  for (const arch of ["arm64", "x64", "ia32", "riscv64"])
    fixtures.push({
      kind: "release",
      platform,
      arch,
      expected: resolveCodexReleaseAsset(platform, arch),
    });
const ok = ["bin/codex", "bin/", "codex-package.json", "codex-path/rg", "a/b/c.txt"];
const lines = (entries, type = "-") =>
  entries.map(
    (entry) => (entry.endsWith("/") ? "d" : type) + "rwxr-xr-x 0 0 0 1 Jan  1 00:00 " + entry,
  );
const cases = [
  ["safe", ok],
  ["empty entry", ["bin/codex", ""]],
  ["parent", ["bin/codex", "../evil"]],
  ["nested parent", ["bin/../../evil"]],
  ["absolute", ["/etc/passwd"]],
  ["dot", ["./bin/codex"]],
  ["dot only", ["./"]],
  ["backslash", ["bin\\codex"]],
  ["colon", ["C:evil"]],
  ["colon in name", ["bin/a:b"]],
  ["double slash", ["bin//codex"]],
  ["trailing slash directory", ["bin/", "bin/codex"]],
];
for (const [label, entries] of cases) {
  fixtures.push({
    kind: "entries",
    label,
    entries,
    types: lines(entries),
    expected: unsafe(entries, lines(entries)),
  });
  fixtures.push({
    kind: "entries",
    label: label + " (symlinks)",
    entries,
    types: lines(entries, "l"),
    expected: unsafe(entries, lines(entries, "l")),
  });
}
fixtures.push({
  kind: "entries",
  label: "mismatched listings",
  entries: ok,
  types: lines(ok.slice(1)),
  expected: unsafe(ok, lines(ok.slice(1))),
});
const many = Array.from({ length: 10001 }, (_, i) => "f" + i);
fixtures.push({
  kind: "entries",
  label: "too many",
  entries: many,
  types: lines(many),
  expected: unsafe(many, lines(many)),
});
const ten = Array.from({ length: 10000 }, (_, i) => "f" + i);
fixtures.push({
  kind: "entries",
  label: "bound",
  entries: ten,
  types: lines(ten),
  expected: unsafe(ten, lines(ten)),
});
for (const text of [
  "0.156.1",
  "1.2.3",
  "1.2.3-beta.1",
  "1.2",
  "v1.2.3",
  "1.2.3-",
  "1.2.3-a_b",
  "01.02.03",
  "1.2.3 ",
  "",
  "../1.2.3",
  "1.2.3/../x",
  "1.2.3-b/../x",
  "١.٢.٣",
])
  fixtures.push({ kind: "version", text, expected: version.test(text) });
fs.writeFileSync(
  "rust/crates/server/tests/fixtures/provider-install.jsonl",
  fixtures.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
console.log(
  fixtures.length +
    " unchanged-source managed Codex installation witnesses; version pattern " +
    version,
);
