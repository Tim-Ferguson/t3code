// Development-only oracle. Node >=24.13.1 and unchanged original dependencies:
// node rust/tools/generate_acp_registry_archive_fixtures.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Stream from "../../apps/server/node_modules/effect/dist/Stream.js";
import { collectUint8StreamText } from "../../apps/server/src/stream/collectUint8StreamText.ts";
const source = readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
const begin = source.indexOf("function normalizeRegistryCommandPath(");
const end = source.indexOf("function compareText(", begin);
if (begin < 0 || end < begin) throw new Error("Original archive helper markers missing");
const helpers = new Function(
  stripTypeScriptTypes(source.slice(begin, end)) +
    ";return {normalizeRegistryCommandPath, validateArchiveEntries, archiveKind, archiveFileName};",
)();
const rows = [];
const paths = [
  "",
  " ",
  ".",
  "./",
  "../",
  "..",
  "/agent",
  "//agent",
  "C:agent",
  "c:/agent",
  "1:agent",
  "agent",
  "./agent",
  "././agent",
  "./bin//agent",
  "bin/agent",
  "bin/../agent",
  "bin/./agent",
  "bin\\agent",
  "\\agent",
  "bin\\..\\agent",
  "../outside",
  "bin/..outside",
  " bin/agent ",
  "\ufeffbin/agent\ufeff",
  "\u0085bin/agent\u0085",
  "bin/agent\n",
  "bin/agent\r",
  "bin/agent\u2028",
  "工具/agent",
  "..%2Foutside",
  "bin/%2e%2e/agent",
  "bin/agent\u0000",
];
for (const input of paths)
  rows.push({
    operation: "path",
    input,
    output: helpers.normalizeRegistryCommandPath(input) ?? null,
  });
for (const left of paths)
  for (const right of ["", "agent", "./", "../outside", "/outside"])
    for (const separator of ["\n", "\r\n", "\r", "\u2028"])
      rows.push({
        operation: "entries",
        input: left + separator + right,
        output: helpers.validateArchiveEntries(left + separator + right),
      });
for (const suffix of [
  "agent",
  "agent.TAR.GZ",
  "agent.tar.bz2",
  "agent.tgz",
  "agent.TBZ2",
  "agent.ZIP",
  "agent.zip/",
  "agent%2ezip",
  "agent.zip?download=.tar.gz",
  "agent.bin#x.zip",
]) {
  const input = "https://example.test/" + suffix;
  const output = helpers.archiveKind(input);
  rows.push({ operation: "kind", input, output, fileName: helpers.archiveFileName(output) });
}
for (const chunks of [
  [],
  [[]],
  [[97, 98]],
  [[97, 98], []],
  [[97, 98], [99]],
  [[102, 128, 111]],
  [[239, 187, 191, 102]],
  [[240], [159], [152], [128]],
  [[239, 191, 189]],
  [[97], [226, 130], [172], [98]],
])
  for (const limit of [0, 1, 2, 3, 4, 5, 20]) {
    const output = await Effect.runPromise(
      collectUint8StreamText({
        stream: Stream.fromIterable(chunks.map((chunk) => new Uint8Array(chunk))),
        maxBytes: limit,
      }),
    );
    rows.push({ operation: "collect", chunks, limit, output });
  }
writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-archives.jsonl",
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original archive/path/byte-collector witnesses`);
