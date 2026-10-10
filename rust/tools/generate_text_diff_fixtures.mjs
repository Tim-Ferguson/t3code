// The unified patch the ACP adapter renders v1 diff content with (`acpToolCallDiffPatch`), computed by the
// unchanged TypeScript for the Rust port to reproduce.
import { writeFileSync } from "node:fs";
import { acpToolCallDiffPatch } from "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts";

const lines = (count, prefix = "line") =>
  Array.from({ length: count }, (_, index) => `${prefix} ${index + 1}`).join("\n");
const edit = (text, changes) => {
  const parts = text.split("\n");
  for (const [index, value] of changes) parts[index] = value;
  return parts.join("\n");
};
const base = lines(40);
const cases = [
  [{ type: "diff", path: "/w/a.txt", oldText: "", newText: "created" }],
  [{ type: "diff", path: "/w/a.txt", newText: "created\nsecond\n" }],
  [{ type: "diff", path: "/w/a.txt", oldText: "same\n", newText: "same\n" }],
  [{ type: "diff", path: "/w/a.txt", oldText: "a\nb\nc\n", newText: "a\nB\nc\n" }],
  [{ type: "diff", path: "/w/a.txt", oldText: "a\nb\nc", newText: "a\nb\nc\n" }],
  [{ type: "diff", path: "/w/a.txt", oldText: "a\r\nb\r\n", newText: "a\r\nc\r\n" }],
  [
    {
      type: "diff",
      path: "/w/a.txt",
      oldText: base,
      newText: edit(base, [
        [2, "changed 3"],
        [30, "changed 31"],
      ]),
    },
  ],
  [
    {
      type: "diff",
      path: "/w/a.txt",
      oldText: base,
      newText: edit(base, [
        [2, "x"],
        [8, "y"],
        [9, "z"],
      ]),
    },
  ],
  [{ type: "diff", path: "/w/a.txt", oldText: base, newText: lines(40, "other") }],
  [{ type: "diff", path: "/w/a.txt", oldText: lines(1500), newText: lines(1500, "different") }],
  [{ type: "diff", path: "/w/a.txt", oldText: "text", newText: "" }],
  [{ type: "diff", path: "/w/a.txt", oldText: "gone\n", newText: "" }],
  [
    { type: "diff", path: "/w/a.txt", oldText: "1\n", newText: "2\n" },
    { type: "diff", path: "/w/b.txt", newText: "new file\n" },
  ],
  [
    {
      type: "diff",
      path: "/w/a.txt",
      patch: { text: "already a patch" },
      oldText: "x",
      newText: "y",
    },
  ],
  [{ type: "content", content: { type: "text", text: "not a diff" } }],
  [{ type: "diff", path: "/w/a.txt", oldText: "ünï\ncödé\n", newText: "ünï\ncödé 2\n" }],
  [{ type: "diff", oldText: "no path", newText: "x" }],
];
const rows = cases.map((content) => ({ content, patch: acpToolCallDiffPatch(content) ?? null }));
writeFileSync(
  new URL("../crates/server/tests/fixtures/text-diff.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} diff cases`);
