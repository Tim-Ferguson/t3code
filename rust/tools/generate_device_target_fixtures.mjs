// Development-only oracle executes original stable host and session identities.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Path from "../../apps/server/node_modules/effect/dist/Path.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import {
  agentDeviceConfigPath,
  agentDeviceSession,
} from "../../apps/server/src/device/AgentDeviceTarget.ts";
import { writeFileSync } from "node:fs";
const rows = [];
for (const host of [
  "local",
  "remote",
  "mini",
  "android",
  "quotes '\" $HOME `literal`",
  "👋",
  "e\u0301",
  "é",
  "\uFEFFhost",
  "",
]) {
  rows.push(
    await Effect.runPromise(
      Effect.gen(function* () {
        const path = yield* Path.Path;
        return {
          host,
          config: yield* agentDeviceConfigPath("/fixture/state", host, path),
          session: yield* agentDeviceSession("thread 👋", host, "same-id"),
        };
      }).pipe(Effect.provide(NodeServices.layer)),
    ),
  );
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/device-target.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
