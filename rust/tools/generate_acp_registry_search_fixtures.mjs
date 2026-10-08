// Node >=24.13.1: node rust/tools/generate_acp_registry_search_fixtures.mjs
// Executes unchanged private relevance/comparison helpers; fixtures need no TS runtime.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
const start = source.indexOf("function compareText("),
  end = source.indexOf("export const makeAcpRegistryCatalog", start);
if (start < 0 || end < start) throw Error("Missing original rank/comparison functions");
const [rank, compare] = new Function(
  stripTypeScriptTypes(source.slice(start, end)) + ";return [searchRank,compareText];",
)();
const rows = [];
const agents = [
  {
    id: "fast-agent",
    name: "Fast Agent",
    description: "A coding assistant",
    authors: ["Alpha Tools", "Beta Team"],
  },
  { id: "tools-cli", name: "Smart Code CLI", description: "Flexible reasoning with Alpha engines" },
  { id: "unicode", name: "ΟΔΟΣ ΣΟΦΟΣ", description: "İstanbul Kelvin longſ" },
  { id: "emoji", name: "😀 Agent", description: "\u0085A tool\ufeff", authors: ["Mixed Unicode"] },
];
const queries = [
  "",
  " ",
  "\ufeff",
  "fast-agent",
  "Fast Agent",
  " FAST ",
  "fa ag",
  "as gen",
  "alpha",
  "coding",
  "alpha coding",
  "fast beta",
  "missing",
  " smart cli",
  " smart\u0085cli",
  "smart\ufeffcli",
  "smart\u00a0cli",
  "agent\u2028tool",
  "οδός",
  "οδοσ",
  "ΣΟΦΟΣ",
  "İstanbul",
  "KELVIN",
  "longſ",
  "😀",
  "toolscli",
  "cli",
  "engine code",
];
for (const agent of agents)
  for (const query of queries)
    rows.push({ operation: "rank", agent, query, output: rank(agent, query) ?? null });
for (const left of ["a", "A", "α", "ΟΔΟΣ", "😀", "\ue000", "aa", ""])
  for (const right of ["a", "A", "α", "Σ", "😀", "\ue000", "aa", ""])
    rows.push({ operation: "compare", left, right, output: compare(left, right) });
writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-search.jsonl",
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(`${rows.length} original registry rank/UTF16 comparison witnesses`);
