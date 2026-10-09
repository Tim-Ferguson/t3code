// Runs the original's decoding of a WebSocket RPC payload over cases derived from the payload
// schema in packages/contracts/src/rpc.ts, for the methods the Rust server decodes by hand, and
// records whether it accepts. `tests/fixtures/rpc-decode-cases.jsonl` is the witness
// `rpc_decode_tests.rs` checks the port's decoding against: the port must refuse what the original
// refuses (a call it carries out that the original would have refused).
import { writeFileSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { WsRpcGroup } from "../../packages/contracts/src/rpc.ts";
import { casesFor } from "./schema_cases.mjs";

export const methods = [
  "vcs.listRefs",
  "projects.createNew",
  "scheduledTasks.upsert",
  "pullRequests.comment",
  "provider.install.start",
  "orchestration.searchThreads",
  "git.runStackedAction",
  "server.upsertKeybinding",
  "attachments.createUploadUrl",
  "sourceControl.cloneRepository",
];

const compress = (value) => {
  if (typeof value === "string") {
    const unit = [...value][0];
    const count = unit ? [...value].length : 0;
    if (count >= 64 && unit.repeat(count) === value) return { $repeat: [unit, count] };
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

const rows = [];
for (const method of methods) {
  const rpc = WsRpcGroup.requests.get(method);
  if (!rpc) throw new Error(`no such method ${method}`);
  const decode = Schema.decodeUnknownExit(rpc.payloadSchema);
  for (const input of casesFor(rpc.payloadSchema.ast)) {
    const exit = decode(input);
    rows.push({ method, input: compress(input), accepted: exit._tag === "Success" });
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/rpc-decode-cases.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
const per = {};
for (const row of rows) {
  per[row.method] ??= { cases: 0, accepted: 0 };
  per[row.method].cases += 1;
  per[row.method].accepted += row.accepted ? 1 : 0;
}
console.log(JSON.stringify(per));
