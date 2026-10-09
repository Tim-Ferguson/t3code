// Runs the original Bitbucket pull request schema and normalization over rows and pages of
// rows, and records the provider-neutral result (or that decoding failed). No network.
import { writeFileSync, renameSync } from "node:fs";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  BitbucketPullRequestListSchema,
  BitbucketPullRequestSchema,
  normalizeBitbucketPullRequestRecord,
} from "../../apps/server/src/sourceControl/bitbucketPullRequests.ts";

const toJson = (record) => ({
  provider: "bitbucket",
  ...record,
  updatedAt: Option.isSome(record.updatedAt)
    ? { _tag: "Some", value: DateTime.formatIso(record.updatedAt.value) }
    : { _tag: "None" },
});
const decodeOne = Schema.decodeUnknownResult(Schema.toCodecJson(BitbucketPullRequestSchema));
const decodePage = Schema.decodeUnknownResult(Schema.toCodecJson(BitbucketPullRequestListSchema));
const pr = (o = {}) => ({
  id: 42,
  title: " Add it ",
  state: "OPEN",
  updated_on: "2026-01-02T00:00:00.000Z",
  links: { html: { href: " https://bitbucket.org/w/r/pull-requests/42 " } },
  source: {
    branch: { name: "feature/x" },
    repository: { full_name: "octocat/r", workspace: { slug: "octocat" } },
  },
  destination: {
    branch: { name: "main" },
    repository: { full_name: "w/r", workspace: { slug: "w" } },
  },
  ...o,
});
const singles = [
  pr(),
  pr({ state: "MERGED" }),
  pr({ state: "declined" }),
  pr({ state: "SUPERSEDED" }),
  pr({ state: null }),
  pr({ state: "weird" }),
  pr({ draft: true }),
  pr({ draft: false }),
  pr({ updated_on: null }),
  pr({ updated_on: "bad" }),
  pr({ updated_on: undefined }),
  pr({ source: { branch: { name: "f" } } }),
  pr({ source: { branch: { name: "f" }, repository: null } }),
  pr({ source: { branch: { name: "f" }, repository: { full_name: "w/r" } } }),
  pr({ source: { branch: { name: "f" }, repository: { full_name: "solo" } } }),
  pr({
    source: {
      branch: { name: "f" },
      repository: { full_name: "fork/r", workspace: { slug: "slugged" } },
    },
  }),
  pr({ source: { branch: { name: "f" }, repository: { workspace: { slug: " ws " } } } }),
  pr({ source: { branch: { name: "f" }, repository: { full_name: "", workspace: null } } }),
  pr({ source: { branch: { name: " " } } }),
  pr({ source: {} }),
  pr({ destination: { branch: { name: "main" } } }),
  pr({ id: 0 }),
  pr({ id: "42" }),
  pr({ title: "" }),
  pr({ links: {} }),
  pr({ links: { html: { href: "" } } }),
  5,
  null,
  [],
];
const rows = [];
for (const raw of singles) {
  const decoded = decodeOne(JSON.parse(JSON.stringify(raw ?? null)));
  rows.push({
    type: "pr",
    raw: JSON.parse(JSON.stringify(raw ?? null)),
    result:
      decoded._tag === "Success"
        ? { ok: toJson(normalizeBitbucketPullRequestRecord(decoded.success)) }
        : { error: true },
  });
}
for (const page of [
  { values: [pr(), pr({ id: 7 })] },
  { values: [] },
  { values: [pr(), { bad: 1 }] },
  {},
  { values: "x" },
  { values: [pr()], next: "https://x" },
  { values: [pr()], next: "" },
]) {
  const decoded = decodePage(page);
  rows.push({
    type: "page",
    raw: page,
    result:
      decoded._tag === "Success"
        ? { ok: decoded.success.values.map((v) => toJson(normalizeBitbucketPullRequestRecord(v))) }
        : { error: true },
  });
}
const target = new URL(
  "../crates/server/tests/fixtures/source-control-bitbucket.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
