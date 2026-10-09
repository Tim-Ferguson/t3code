// Runs the original's pin-order keys over fixed arrangements, for `tests/pin_order_parity.rs`.
//   pinOrderKeyBetween, generateSpreadPinOrderKeys, planPinnedReorder   packages/client-runtime/src/state/threadSort.ts
import { writeFileSync } from "node:fs";
import {
  generateSpreadPinOrderKeys,
  pinOrderKeyBetween,
  planPinnedReorder,
} from "../../packages/client-runtime/src/state/threadSort.ts";

const rows = [];
const keys = [
  null,
  "a",
  "b",
  "m",
  "n",
  "z",
  "zz",
  "mm",
  "mn",
  "an",
  "ab",
  "mz",
  "zy",
  "aa",
  "bn",
  "nm",
];
for (const before of keys) {
  for (const after of keys) {
    rows.push({ type: "between", before, after, output: pinOrderKeyBetween(before, after) });
  }
}
for (const count of [0, 1, 2, 3, 5, 10, 26, 100, 700]) {
  rows.push({ type: "spread", count, output: generateSpreadPinOrderKeys(count) });
}
const arrangements = [
  { ids: ["a", "b", "c"], keys: { a: "g", b: "m", c: "t" }, moved: "a" },
  { ids: ["b", "a", "c"], keys: { a: "g", b: "m", c: "t" }, moved: "a" },
  { ids: ["b", "c", "a"], keys: { a: "g", b: "m", c: "t" }, moved: "a" },
  { ids: ["c", "a", "b"], keys: { a: "g", b: "m", c: "t" }, moved: "c" },
  { ids: ["a", "b", "c"], keys: { a: null, b: null, c: null }, moved: "c" },
  { ids: ["b", "a", "c"], keys: { a: "g", b: null, c: "t" }, moved: "a" },
  { ids: ["a", "b"], keys: { a: "m", b: "n", hidden: "g" }, moved: "b" },
  { ids: ["a"], keys: { a: "m" }, moved: "a" },
  { ids: ["a", "b", "c"], keys: { a: "g", b: "g", c: "t" }, moved: "b" },
  { ids: ["x", "a", "b"], keys: { a: "g", b: "m", hidden: "d" }, moved: "x" },
  { ids: ["a", "b"], keys: { a: "m", b: "n" }, moved: "missing" },
  { ids: ["a", "b", "c"], keys: { a: "m", b: "n", c: "o", hidden: "mm" }, moved: "c" },
];
for (const arrangement of arrangements) {
  const keysById = new Map(Object.entries(arrangement.keys));
  rows.push({
    type: "plan",
    ...arrangement,
    output: planPinnedReorder({
      orderedIds: arrangement.ids,
      keysById,
      movedId: arrangement.moved,
    }),
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/pin-order.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
