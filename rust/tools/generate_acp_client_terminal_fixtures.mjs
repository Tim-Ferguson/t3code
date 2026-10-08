// Development oracle, unchanged original source. Run with Node 24:
// node rust/tools/generate_acp_client_terminal_fixtures.mjs
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { stripTypeScriptTypes } from "node:module";
import vm from "node:vm";
import * as NodeBuffer from "node:buffer";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const source = fs.readFileSync(
  path.join(root, "apps/server/src/provider/acp/AcpClientTerminals.ts"),
  "utf8",
);
const begin = source.indexOf("function trimOutputStart(");
const end = source.indexOf("function terminalExitSignal(", begin);
if (begin < 0 || end < begin) throw new Error("Original output helper markers missing");
const context = vm.createContext({ NodeBuffer });
vm.runInContext(stripTypeScriptTypes(source.slice(begin, end)), context);
const fixtures = [];
const samples = [
  [],
  [65],
  [...Buffer.from("prefix😀TAIL\n")],
  [...Buffer.from("\uFEFFBOM")],
  [0x80, 0xbf, 65],
  [0xff, 0xc0, 0xaf, 65],
  [0xe2, 0x82],
  [0xf0, 0x9f, 0x98, 0x80],
  [0xed, 0xa0, 0x80],
  [0, 10, 13, 127, 255],
];
for (const bytes of samples)
  for (const limit of [0, 1, 2, 3, 4, 5, 7, 16, 64])
    for (const split of [...new Set([0, 1, 2, Math.floor(bytes.length / 2), bytes.length])].filter(
      (n) => n <= bytes.length,
    ))
      for (const trim of [0, 1, 3, 64]) {
        const chunks = [bytes.slice(0, split), bytes.slice(split)];
        const state = { chunks: [], bytes: 0, truncated: false, limit };
        for (const chunk of chunks) context.appendOutput(state, new Uint8Array(chunk));
        context.trimOutputStart(state, trim);
        fixtures.push({
          input: { chunks, limit, trim },
          output: {
            text: context.readOutput(state),
            bytes: state.bytes,
            truncated: state.truncated,
          },
        });
      }
const target = path.join(
  root,
  "rust/crates/server/tests/fixtures/acp-client-terminal-buffer.jsonl",
);
fs.writeFileSync(target, fixtures.map((row) => JSON.stringify(row)).join("\n") + "\n");
console.log(`${fixtures.length} original terminal output witnesses`);
