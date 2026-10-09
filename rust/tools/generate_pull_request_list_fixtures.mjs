// Runs the continuation-cursor helpers of PullRequestService.ts, which are module-private, by
// slicing them out of the original source. For crates/server/src/pull_request_list.rs.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/server/src/pullRequest/PullRequestService.ts", import.meta.url),
  "utf8",
);
const block = (name) => {
  const start = source.indexOf(`function ${name}(`),
    end = source.indexOf("\n}\n", start);
  if (start < 0 || end < 0) throw new Error(`Missing ${name}`);
  return source.slice(start, end + 3);
};
const pattern = source.slice(
  source.indexOf("const LIST_CURSOR_PATTERN"),
  source.indexOf(";", source.indexOf("const LIST_CURSOR_PATTERN")) + 1,
);
const code = stripTypeScriptTypes(
  `${pattern}\n${block("parseListCursor")}\n${block("listCursorAt")}\nreturn {parseListCursor,listCursorAt};`
    .replace(/^/, "function __wrap(){")
    .concat("\n}"),
  { mode: "strip" },
);
const { parseListCursor, listCursorAt } = new Function(`${code};return __wrap();`)();
const rows = [];
for (const raw of [
  "2026-01-05T00:00:00Z|1|7",
  "2026-01-05T00:00:00.123456789+02:00|12|1,2,3",
  "2026-01-05T00:00:00Z|0|",
  "2026-01-05T00:00:00Z|1234567890|1",
  "2026-01-05T00:00:00Z|1",
  "nonsense",
  "",
  "2026-01-05 00:00:00Z|1|7",
  "2026-01-05T00:00:00Z|1|7,",
  "2026-01-05T00:00:00Z|1|x",
])
  rows.push({ type: "parse", raw, result: parseListCursor(raw) ?? null });
const fetched = (updated) => updated.map((updatedAt, index) => ({ number: index + 1, updatedAt }));
for (const previous of [
  undefined,
  { updatedBefore: "2026-01-05T00:00:00Z", delivered: 3, seenAt: [7, 8] },
  { updatedBefore: "2026-01-04T00:00:00Z", delivered: 5, seenAt: [1] },
])
  for (const boundary of ["2026-01-05T00:00:00Z", "2026-01-03T00:00:00Z"])
    for (const rowsAt of [
      [],
      ["2026-01-05T00:00:00Z"],
      ["2026-01-05T00:00:00Z", "2026-01-03T00:00:00Z", "2026-01-03T00:00:00Z"],
    ])
      for (const delivered of [0, 2])
        rows.push({
          type: "at",
          previous: previous ?? null,
          boundary,
          fetched: fetched(rowsAt),
          delivered,
          result: listCursorAt(previous, boundary, fetched(rowsAt), delivered),
        });
const target = new URL(
    "../crates/server/tests/fixtures/pull-request-list-cursors.jsonl",
    import.meta.url,
  ),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
