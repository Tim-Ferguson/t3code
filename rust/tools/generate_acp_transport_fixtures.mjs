// Development oracle: Node24 with the pinned original checkout/dependencies.
// node rust/tools/generate_acp_transport_fixtures.mjs
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as RpcSerialization from "../../packages/effect-acp/node_modules/effect/dist/rpc/RpcSerialization.js";
const source = fs.readFileSync(
  new URL("../../packages/effect-acp/src/protocol.ts", import.meta.url),
  "utf8",
);
const begin = source.indexOf("const makeStrictNdJsonRpcParser =");
const end = source.indexOf("const MAX_BUFFERED_RAW_NOTIFICATIONS", begin);
if (begin < 0 || end < 0) throw Error("Missing pinned strict ACP parser markers");
const makeParser = new Function(
  "RpcSerialization",
  stripTypeScriptTypes(source.slice(begin, end)) + "\nreturn makeStrictNdJsonRpcParser;",
)(RpcSerialization);
const encoder = new TextEncoder();
const bom = [0xef, 0xbb, 0xbf];
const inputs = [
  [
    "initial_bom_whole",
    [Uint8Array.from([...bom, ...encoder.encode('{"jsonrpc":"2.0","method":"x/bom"}\n')])],
  ],
  [
    "initial_bom_split",
    [
      Uint8Array.of(bom[0]),
      Uint8Array.of(bom[1]),
      Uint8Array.from([bom[2], ...encoder.encode('{"jsonrpc":"2.0","method":"x/bom"}\n')]),
    ],
  ],
  [
    "later_bom_is_not_stripped",
    [
      encoder.encode('{"jsonrpc":"2.0","method":"x/first"}\n'),
      Uint8Array.from([...bom, ...encoder.encode('{"jsonrpc":"2.0","method":"x/bom"}\n')]),
    ],
  ],
  [
    "initial_bom_then_later_bom",
    [
      Uint8Array.from([...bom, ...encoder.encode('{"jsonrpc":"2.0","method":"x/first"}\n')]),
      Uint8Array.from([...bom, ...encoder.encode('{"jsonrpc":"2.0","method":"x/bom"}\n')]),
    ],
  ],

  ["fragmented", ['{"jsonrpc":"2.0","method":"x/n","params":{"text":"caf', 'é"}}\n']],
  [
    "split_utf8",
    Array.from(
      encoder.encode('{"jsonrpc":"2.0","method":"x/n","params":{"text":"café 😀"}}\n'),
      (byte) => Uint8Array.of(byte),
    ),
  ],
  [
    "same_chunk",
    ['{"jsonrpc":"2.0","method":"x/a"}\n{"jsonrpc":"2.0","id":0,"result":{"ok":true}}\n'],
  ],
  [
    "error_presence",
    [
      '{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"no"}}\n{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"no","data":null}}\n',
    ],
  ],
  [
    "batch",
    [
      '[{"jsonrpc":"2.0","id":"0","method":"x/r","params":null},false,3,{"jsonrpc":"2.0","method":"x/n"}]\n',
    ],
  ],
  ["scalar", ['null\nfalse\n3\n"ignored"\n']],
  [
    "headers_trace",
    [
      '{"jsonrpc":"2.0","id":7,"method":"x/r","params":{},"headers":[["a","b"]],"traceId":"t","spanId":"s","sampled":false}\n',
    ],
  ],
  [
    "effect_frames",
    [
      '{"jsonrpc":"2.0","method":"@effect/rpc/Ping"}\n{"jsonrpc":"2.0","method":"@effect/rpc/Ack","params":{"requestId":0}}\n{"jsonrpc":"2.0","id":3,"chunk":true,"result":[1,2]}\n',
    ],
  ],
  [
    "private_cause",
    [
      '{"jsonrpc":"2.0","id":4,"error":{"_tag":"Cause","code":0,"message":"private","data":[{"_tag":"Die","defect":{"name":"Error","message":"private"}}]}}\n',
    ],
  ],
  [
    "defect",
    [
      '{"jsonrpc":"2.0","id":-32603,"error":{"_tag":"Defect","code":1,"message":"A defect occurred","data":{"private":true}}}\n',
    ],
  ],
  ["malformed", ['{"secret":"private-token"\n']],
  ["valid_then_malformed", ['{"jsonrpc":"2.0","method":"x/valid"}\n{"secret":"private-token"\n']],
  ["unfinished", ['{"jsonrpc":"2.0","method":"x/ignored"}']],
  ["empty_line", ["\n"]],
];
const cases = [];
for (const [name, chunks] of inputs) {
  const parser = makeParser();
  const encoded = chunks.map((c) => (typeof c === "string" ? encoder.encode(c) : c));
  const observations = [];
  for (const chunk of encoded) {
    try {
      observations.push({ success: true, messages: parser.decode(chunk) });
    } catch {
      observations.push({ success: false });
      break;
    }
  }
  cases.push({ kind: "decode", name, chunks: encoded.map((c) => Array.from(c)), observations });
}
for (const message of [
  { _tag: "Request", id: 1, tag: "x/raw", payload: { text: "café" }, headers: [] },
  {
    _tag: "Request",
    id: 4294967296,
    tag: "initialize",
    payload: { protocolVersion: 2 },
    headers: [],
  },
  { _tag: "Exit", requestId: 0, exit: { _tag: "Success", value: { done: true } } },
  {
    _tag: "Exit",
    requestId: "0",
    exit: {
      _tag: "Failure",
      cause: [{ _tag: "Fail", error: { code: -32000, message: "Denied", data: null } }],
    },
  },
  {
    _tag: "Exit",
    requestId: "private",
    exit: {
      _tag: "Failure",
      cause: [{ _tag: "Die", defect: { name: "Error", message: "private" } }],
    },
  },
]) {
  const encoded = makeParser().encode(message);
  cases.push({ kind: "encode", message, wire: JSON.parse(encoded), raw: encoded });
}
fs.writeFileSync(
  new URL("../crates/acp/tests/fixtures/transport.jsonl", import.meta.url),
  cases.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${cases.length} original ACP transport cases`);
