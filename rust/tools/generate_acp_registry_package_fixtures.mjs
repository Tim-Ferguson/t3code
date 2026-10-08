// Node >=24.13.1, unchanged original source/dependencies; no Rust runtime TS dependency.
// node rust/tools/generate_acp_registry_package_fixtures.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
const source = readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
function between(begin, end) {
  const start = source.indexOf(begin),
    finish = source.indexOf(end, start);
  if (start < 0 || finish < start) throw Error(`Missing source ${begin}`);
  return source.slice(start, finish);
}
const setup =
  between("const BoundedArgument =", "const EXACT_RUNNER_VERSION =") +
  between("const NpmPackageManifest =", "const AcpRegistryPackageInstallReceipt =");
const functions =
  between("function compareText(", "function searchRank(") +
  between("interface ExactPackageSpec {", "function readEnvironmentPath(") +
  between("  const npmCommandName =", "  const discoverNpmGlobalPackage =");
const [
  manifest,
  npmCommandName,
  parseNpxPackageSpec,
  parseUvxPackageSpec,
  packageCommandCandidates,
] = new Function(
  "Schema",
  stripTypeScriptTypes(setup + functions) +
    ";return [NpmPackageManifest,npmCommandName,parseNpxPackageSpec,parseUvxPackageSpec,packageCommandCandidates];",
)(Schema);
const rows = [];
for (const name of ["pkg", "@scope/pkg", "fast-agent-acp", "K", "@scope/工具"])
  for (const version of ["1.2.3", "v1.2.3", "V1.2.3", "1.2.3-alpha+build"])
    for (const distribution of ["npx", "uvx"]) {
      const input = name + (distribution === "uvx" ? "==" : "@") + version;
      rows.push({
        operation: "identity",
        distribution,
        input,
        output: (distribution === "npx" ? parseNpxPackageSpec : parseUvxPackageSpec)(input),
      });
    }
for (const id of ["agent", "pkg", "fast-agent"])
  for (const name of ["pkg", "@scope/pkg", "fast-agent-acp", "K", "工具"])
    rows.push({
      operation: "candidates",
      id,
      name,
      output: packageCommandCandidates({ id }, name),
    });
for (const id of ["agent", "pkg"])
  for (const name of ["pkg", "@scope/pkg"])
    for (const bin of [
      "bin/agent",
      "",
      {},
      [],
      null,
      4,
      { pkg: "a" },
      { agent: "a", pkg: "b" },
      { second: "a", first: "a" },
      { first: "a", second: "b" },
      { "../escape": "a" },
      { "../escape": 4, pkg: "bin/pkg" },
      { "../escape": "x".repeat(1025), agent: "bin/agent" },
      { K: "a", ſ: "b" },
      { first: 1 },
      { first: "a".repeat(1025) },
    ]) {
      const input = { name, version: "1.2.3", bin };
      let value;
      try {
        value = Schema.decodeUnknownSync(manifest)(input);
      } catch {
        rows.push({ operation: "npmCommand", id, input, ok: false });
        continue;
      }
      rows.push({
        operation: "npmCommand",
        id,
        input,
        ok: true,
        output: npmCommandName({ id }, name, value) ?? null,
      });
    }
writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-package-policy.jsonl",
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original package identity/candidate/manifest witnesses`);
