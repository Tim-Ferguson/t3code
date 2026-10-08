// Execute the unchanged logical-I/O service; this is a development oracle.
import { writeFileSync } from "node:fs";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import { ResourceAttributionSnapshot } from "../../packages/contracts/src/resourceTelemetry.ts";
import { make } from "../../apps/server/src/resourceTelemetry/ResourceAttribution.ts";
const cases = [];
const special = (v) =>
  v === "NaN" ? NaN : v === "Infinity" ? Infinity : v === "-Infinity" ? -Infinity : v;
const normalize = (r) => Object.fromEntries(Object.entries(r).map(([k, v]) => [k, special(v)]));
async function run(records) {
  const service = await Effect.runPromise(make());
  for (const record of records) await Effect.runPromise(service.record(normalize(record)));
  const snapshot = await Effect.runPromise(service.snapshot);
  let accepted = true;
  try {
    Schema.encodeUnknownSync(ResourceAttributionSnapshot)(snapshot);
  } catch {
    accepted = false;
  }
  cases.push({ records, entries: snapshot.entries, accepted });
}
await run([]);
for (const value of [
  undefined,
  -5,
  -0.5,
  -0.1,
  0,
  0.49,
  0.5,
  1.5,
  99.6,
  "NaN",
  "Infinity",
  "-Infinity",
  9007199254740991,
  9007199254740992,
]) {
  for (const field of ["logicalReadBytes", "logicalWriteBytes", "count", "durationMs"]) {
    await run([
      {
        component: "sqlite",
        operation: "write",
        ...(value === undefined ? {} : { [field]: value }),
      },
    ]);
  }
}
await run([
  { component: "a\0b", operation: "c", logicalWriteBytes: 2 },
  { component: "a", operation: "b\0c", logicalReadBytes: 4 },
]);
await run([
  { component: "a", operation: "write", logicalWriteBytes: 2 },
  { component: "b", operation: "write", logicalWriteBytes: 2 },
  { component: "c", operation: "write", logicalWriteBytes: 3 },
  { component: "a", operation: "write", logicalWriteBytes: 0.5 },
]);
await run([
  { component: "a", operation: "read", count: 9007199254740991 },
  { component: "a", operation: "read", count: 1 },
]);
await run([{ component: "  label  ", operation: "read" }]);
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-attribution.jsonl", import.meta.url),
  cases.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
console.log(`${cases.length} source attribution witnesses`);
