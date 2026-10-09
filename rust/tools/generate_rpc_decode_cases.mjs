// Runs the original's decoding of a WebSocket RPC payload over cases derived from the payload
// schema in packages/contracts/src/rpc.ts, for the methods the Rust server decodes by hand, and
// records whether it accepts. `tests/fixtures/rpc-decode-cases.jsonl` is the witness
// `rpc_decode_tests.rs` checks the port's decoding against: the port must refuse what the original
// refuses (a call it carries out that the original would have refused).
import { readFileSync, writeFileSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as SchemaIssue from "../../apps/server/node_modules/effect/dist/SchemaIssue.js";
import * as Cause from "../../apps/server/node_modules/effect/dist/Cause.js";
import { WsRpcGroup } from "../../packages/contracts/src/rpc.ts";
import { casesFor } from "./schema_cases.mjs";

// Every method of the original's RPC group.
export const methods = [...WsRpcGroup.requests.keys()];

const compress = (value) => {
  if (typeof value === "string") {
    // A long run of one character, perhaps padded by one blank on each side (the boundary of a trim).
    const padded = /^( ?)(.+?)( ?)$/su.exec(value);
    const body = padded ? padded[2] : value;
    const unit = [...body][0];
    const count = unit ? [...body].length : 0;
    if (count >= 64 && unit.repeat(count) === body)
      return padded[1] || padded[3]
        ? { $repeat: [unit, count], $pad: [padded[1], padded[3]] }
        : { $repeat: [unit, count] };
    return value;
  }
  if (Array.isArray(value)) {
    if (
      value.length > 20 &&
      value.every((item) => JSON.stringify(item) === JSON.stringify(value[0]))
    )
      return { $items: [compress(value[0]), value.length] };
    return value.map(compress);
  }
  if (value && typeof value === "object")
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, compress(item)]));
  return value;
};

// Payloads the walk of the schema does not reach by itself: the parts of a schema that only a larger valid value
// exercises (a recursive tree, the records a message carries). Each is tried as it is and with every leaf spoiled.
const treeNode = (children = []) => ({ role: "button", bounds: null, children });
const tree = (root, extra = {}) => ({
  threadId: "x",
  messageId: "x",
  attachments: [
    {
      type: "image",
      name: "x",
      mimeType: "image/png",
      sizeBytes: 1,
      dataUrl: "data:image/png;base64,AAAA",
      source: {
        kind: "snap-shot",
        capturedAt: "x",
        appName: "x",
        windowTitle: "x",
        accessibility: {
          format: "element-tree",
          coordinateSpace: "captured-image",
          imageSize: { width: 1, height: 1 },
          truncated: false,
          root,
          ...extra,
        },
      },
    },
  ],
});
const nest = (depth) => (depth ? treeNode([nest(depth - 1)]) : treeNode());
const message = (records) => ({
  type: "message.dispatch",
  createdBy: "user",
  creationSource: "web",
  commandId: "x",
  threadId: "x",
  messageId: "x",
  text: "x",
  attachments: [],
  dispatchMode: { type: "defer_start" },
  context: { version: 1, records },
});
const base = (contextId, kind, rest) => ({ version: 1, contextId, label: "l", kind, ...rest });
const image = base("c1", "image", {
  attachmentId: "a1",
  name: "n",
  mimeType: "image/png",
  sizeBytes: 1,
});
const terminal = base("c2", "terminal", {
  terminalId: "t",
  terminalLabel: "t",
  lineStart: 1,
  lineEnd: 2,
  text: "x",
});
const unknown = base("c3", "widget", { payload: { a: [1, "b", null], c: { d: true } } });
const seeds = {
  "assets.persistChatAttachments": [
    tree(treeNode()),
    tree(treeNode([treeNode([treeNode()]), treeNode()]), {}),
    tree(nest(12)),
  ],
  "orchestration.dispatchCommand": [
    message([image]),
    message([image, terminal, unknown]),
    message([base("c4", "image", {}), unknown]),
    message([image, { ...terminal, contextId: "c1" }]),
    message([base("c5", "widget", { payload: "x".repeat(70000) })]),
    message([
      base("c6", "image", {
        kind: "image",
        attachmentId: " a ",
        name: " n ",
        mimeType: "image/png",
        sizeBytes: 1,
      }),
    ]),
  ],
};
const spoilers = [null, "", " ", " x ", 0, -1, 1.5, true, false, [], {}, "NaN", "x".repeat(300)];
// Every leaf of the seed spoiled in turn, removed, and an unknown key beside it.
const seedCases = (seed) => {
  const out = [seed];
  const walk = (value, path) => {
    const copyWith = (replacement) => {
      const root = structuredClone(seed);
      if (path.length === 0) return replacement;
      let at = root;
      for (const key of path.slice(0, -1)) at = at[key];
      if (replacement === undefined) {
        if (Array.isArray(at)) at.splice(path[path.length - 1], 1);
        else delete at[path[path.length - 1]];
      } else at[path[path.length - 1]] = replacement;
      return root;
    };
    if (path.length) {
      for (const spoiler of spoilers) out.push(copyWith(spoiler));
      out.push(copyWith(undefined));
    }
    if (value && typeof value === "object") {
      if (!Array.isArray(value)) {
        const root = structuredClone(seed);
        let at = root;
        for (const key of path) at = at[key];
        at.unknown = true;
        out.push(root);
      }
      for (const [key, item] of Object.entries(value))
        walk(item, [...path, Array.isArray(value) ? Number(key) : key]);
    }
  };
  walk(seed, []);
  return out;
};

const rows = [];
for (const method of methods) {
  const rpc = WsRpcGroup.requests.get(method);
  if (!rpc) throw new Error(`no such method ${method}`);
  // The server decodes through the JSON codec, so that is the reference.
  const decode = Schema.decodeUnknownExit(Schema.toCodecJson(rpc.payloadSchema));
  const seen = new Set();
  const inputs = [
    ...casesFor(rpc.payloadSchema.ast, { codec: true }),
    ...(seeds[method] ?? []).flatMap(seedCases),
  ].filter((input) => {
    const key = JSON.stringify(input) ?? "undefined";
    return seen.has(key) ? false : (seen.add(key), true);
  });
  for (const input of inputs) {
    const exit = decode(input);
    if (exit._tag === "Success") rows.push({ method, input: compress(input), accepted: true });
    else {
      // What the server sends: RpcServer formats the issue with the default formatter and sends it as a defect.
      const error = Cause.squash(exit.cause);
      rows.push({
        method,
        input: compress(input),
        accepted: false,
        message: SchemaIssue.defaultFormatter(error.issue),
      });
    }
  }
}
const target = new URL("../crates/server/tests/fixtures/rpc-decode-cases.jsonl", import.meta.url);
// `--check` regenerates in memory and fails when the committed cases are not what the original makes of them.
if (process.argv.includes("--check")) {
  const committed = readFileSync(target, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const same = JSON.stringify(committed) === JSON.stringify(rows);
  console.log(JSON.stringify({ rows: rows.length, same }));
  process.exit(same ? 0 : 1);
}
writeFileSync(target, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
const per = {};
for (const row of rows) {
  per[row.method] ??= { cases: 0, accepted: 0 };
  per[row.method].cases += 1;
  per[row.method].accepted += row.accepted ? 1 : 0;
}
console.log(JSON.stringify(per));
