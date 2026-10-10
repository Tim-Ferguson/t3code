// Runs the original's `#` pull request menu over many lists and queries, for
// `crates/client/tests/pull_request_mention_parity.rs`: which pull requests it offers in what order
// (the part of `ChatComposer.tsx` that filters and ranks them), and the context chip picking one
// attaches. Functions are cut out of the original sources unchanged.
//   filterComposerPullRequestMatches                    packages/shared/src/composerPullRequestMatches.ts
//   matchesPullRequestQuery, scorePullRequestMatch,
//   rankPullRequestMatches                              apps/web/src/components/pullRequest/pullRequestList.logic.ts
//   bounded, boundedField, pullRequestContextComment,
//   buildPullRequestReferenceContext                    apps/web/src/components/pullRequest/pullRequestDetail.logic.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const shared = read("packages/shared/src/composerPullRequestMatches.ts");
const list = read("apps/web/src/components/pullRequest/pullRequestList.logic.ts");
const detail = read("apps/web/src/components/pullRequest/pullRequestDetail.logic.ts");
const body = [
  shared,
  cut(
    list,
    "export function matchesPullRequestQuery",
    "/**\n * The server returns the involvement superset",
  ),
  cut(list, "export function scorePullRequestMatch", "/**\n * The default review queue"),
  cut(
    detail,
    "const FINDING_LIMIT",
    "/**\n * A review thread as the composer's own annotation context",
  ),
  cut(
    detail,
    "function pullRequestContextComment",
    "/** What the agent is asked to do with a question",
  ),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-pr-mention-"));
const file = NodePath.join(scratch, "mention.mjs");
writeFileSync(
  file,
  `${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { filterComposerPullRequestMatches, matchesPullRequestQuery, rankPullRequestMatches, buildPullRequestReferenceContext };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

let seed = 20261012;
const random = () => (seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32;
const pick = (items) => items[Math.floor(random() * items.length)];
const chance = (p) => random() < p;
const words = [
  "wizard",
  "welcome",
  "fix",
  "login",
  "bug",
  "cache",
  "Refactor",
  "parser",
  "docs",
  "ui",
  "drag",
  "sidebar",
  "retry",
];
const at = (minutes) => new Date(Date.UTC(2026, 9, 1, 8, 0, 0) + minutes * 60_000).toISOString();
const makeEntry = (number) => ({
  number,
  projectId: chance(0.85) ? "p1" : "p2",
  repository: chance(0.9) ? "acme/web" : "Acme/Other",
  host: pick(["github.com", "github.com", "gitlab.com"]),
  title: Array.from({ length: 1 + Math.floor(random() * 4) }, () => pick(words)).join(" "),
  url: `https://github.com/acme/web/pull/${number}`,
  headBranch: `${pick(["feat", "fix"])}/${pick(words).toLowerCase()}-${number}`,
  baseBranch: "main",
  state: pick(["open", "open", "merged", "closed"]),
  isDraft: chance(0.2),
  updatedAt: at(Math.floor(random() * 3000)),
  author: chance(0.8) ? { login: pick(["ana", "ben", "cy"]) } : null,
});
const rows = [];
for (let index = 0; index < 120; index += 1) {
  const entries = Array.from({ length: Math.floor(random() * 20) }, () =>
    makeEntry(1 + Math.floor(random() * 140)),
  );
  const providers = [
    { host: "github.com", searchesOnHost: chance(0.5) },
    { host: "gitlab.com", searchesOnHost: chance(0.5) },
  ];
  const exact = chance(0.2) ? [makeEntry(7)] : [];
  const query = pick([
    "",
    "",
    "7",
    "1",
    "12",
    String(entries[0]?.number ?? 3),
    "wizard",
    "wiz",
    "login fix",
    "feat",
    "ana",
    "acme",
    "nomatch",
    "Refactor",
    "drag-sidebar",
  ]);
  const projectId = "p1";
  const repository = "acme/web";
  const matches = /^\d*$/u.test(query)
    ? original.filterComposerPullRequestMatches({
        entries: [...exact, ...entries],
        projectId,
        repository,
        query,
        limit: 12,
      })
    : original
        .rankPullRequestMatches(
          entries.filter((entry) => {
            if (
              entry.projectId !== projectId ||
              entry.repository.trim().toLowerCase() !== repository.trim().toLowerCase()
            )
              return false;
            const provider = providers.find((candidate) => candidate.host === entry.host);
            return (
              provider?.searchesOnHost === true || original.matchesPullRequestQuery(entry, query)
            );
          }),
          query,
        )
        .slice(0, 12);
  rows.push({
    type: "matches",
    entries,
    providers,
    exact,
    query,
    projectId,
    repository,
    output: matches.map((entry) => `${entry.projectId}:${entry.repository}:${entry.number}`),
  });
}
for (const entry of [
  makeEntry(7),
  { ...makeEntry(9), title: "  A title\nwith   lines\tand tabs  " },
  { ...makeEntry(10), title: "x".repeat(1200), url: "https://example.com/" + "y".repeat(1100) },
  { ...makeEntry(11), isDraft: true, state: "open", headBranch: " spaced / branch " },
]) {
  rows.push({
    type: "reference",
    input: {
      number: entry.number,
      title: entry.title,
      url: entry.url,
      headBranch: entry.headBranch,
      baseBranch: entry.baseBranch,
      state: entry.state,
      isDraft: entry.isDraft,
    },
    output: original.buildPullRequestReferenceContext(entry),
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/pull-request-mention.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
