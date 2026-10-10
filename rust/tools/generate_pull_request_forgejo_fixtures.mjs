// Runs the original Forgejo pull request provider and records what it produces, for
// crates/server/tests/pull_request_forgejo.rs to replay: ForgejoPullRequestProvider over a scripted
// `ForgejoCli.api`, as the original's own tests drive it. A scenario answers each request by the
// method and a fragment of the path; what is recorded is every request the provider made (the
// repository and host it addressed, its path, method and body) and its result or failure for each
// call. No network is used and no process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as ForgejoCli from "../../apps/server/src/sourceControl/ForgejoCli.ts";
import * as ForgejoPullRequestProvider from "../../apps/server/src/pullRequest/ForgejoPullRequestProvider.ts";

const rows = [];
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );
const json = (value) => JSON.stringify(value);

// ---- payloads -------------------------------------------------------------------------------------
const user = (login, overrides = {}) => ({
  login,
  full_name: login.toUpperCase(),
  avatar_url: `https://img.example/${login}.png`,
  ...overrides,
});
const repo = (name = "acme/web", overrides = {}) => ({
  full_name: name,
  permissions: { push: true, admin: false },
  allow_merge_commits: true,
  allow_squash_merge: true,
  allow_rebase: false,
  allow_rebase_update: true,
  ...overrides,
});
const branch = (ref, sha, repository = repo()) => ({ ref, sha, repo: repository });
const pr = (overrides = {}) => ({
  number: 7,
  title: "Fix it",
  body: "Body text",
  html_url: "https://forgejo.test/acme/web/pulls/7",
  user: user("alice"),
  state: "open",
  merged: false,
  mergeable: true,
  head: branch("feature", "headsha"),
  base: branch("main", "basesha"),
  merge_base: "basesha",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-02T00:00:00Z",
  closed_at: null,
  merged_at: null,
  additions: 3,
  deletions: 1,
  changed_files: 2,
  labels: [
    { id: 1, name: "bug", color: "ff0000", description: "A bug" },
    { id: 2, name: "ui", color: null },
  ],
  requested_reviewers: [user("bob")],
  ...overrides,
});
const comment = (id, overrides = {}) => ({
  id,
  body: `Comment ${id}`,
  user: user("bob"),
  created_at: `2026-01-02T00:0${id % 10}:00Z`,
  html_url: `https://forgejo.test/acme/web/pulls/7#issuecomment-${id}`,
  ...overrides,
});
const review = (id, overrides = {}) => ({
  id,
  body: `Review ${id}`,
  user: user("carol"),
  state: "APPROVED",
  submitted_at: `2026-01-03T00:0${id % 10}:00Z`,
  html_url: `https://forgejo.test/acme/web/pulls/7#issuecomment-${100 + id}`,
  comments_count: 0,
  ...overrides,
});
const inline = (id, overrides = {}) => ({
  ...comment(id),
  path: "src/a.ts",
  position: 4,
  original_position: 4,
  commit_id: "c1",
  original_commit_id: "c0",
  resolver: null,
  ...overrides,
});
const commitRow = (sha, overrides = {}) => ({
  sha,
  author: user("alice"),
  commit: { message: `Subject of ${sha}\n\nMore`, committer: { date: "2026-01-02T03:04:05Z" } },
  parents: [{ sha: "parent0" }],
  stats: { additions: 2, deletions: 1 },
  ...overrides,
});
const status = (context, state, at, overrides = {}) => ({
  context,
  status: state,
  description: `${context} ${state}`,
  target_url: `https://ci.example/${context}`,
  updated_at: at,
  ...overrides,
});
const reactionRow = (content, login) => ({ content, user: login === null ? null : user(login) });

const subject = { repository: "acme/web", host: "forgejo.test", number: 7 };
const listInput = (overrides = {}) => ({
  repository: "acme/web",
  host: "forgejo.test",
  state: "open",
  involvement: "all",
  viewer: "alice",
  limit: 20,
  ...overrides,
});
const P = "repos/acme/web";
const PULL = `${P}/pulls/7`;
const ISSUE = `${P}/issues/7`;
const ok = (value, stderr = 'HTTP/1.1 200\nlink: <x?page=1>; rel="last"\n') => ({
  stdout: typeof value === "string" ? value : json(value),
  stderr,
});
const fail = {
  auth: { error: { reason: "authentication", detail: "Forgejo denied access." } },
  rate: { error: { reason: "rate-limit", detail: "Forgejo API rate limit exceeded." } },
  gone: {
    error: { reason: "not-found", detail: "Forgejo repository or pull request was not found." },
  },
  missing: { error: { reason: "missing-cli", detail: "Install Forgejo CLI." } },
  forbidden: { error: { reason: "forbidden", detail: "Forgejo denied access (403)." } },
  boom: { error: { detail: "Forgejo CLI command failed." } },
};
const baseRoutes = () => [
  {
    match: `${PULL}.diff`,
    ...ok(
      "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
    ),
  },
  { match: `${P}/statuses/headsha`, ...ok([]) },
  { match: PULL, ...ok(pr()) },
  { match: "/user", ...ok(user("carol")) },
  { match: "user", ...ok(user("carol")) },
  { match: P, ...ok(repo()) },
];
const lazy = (routes) => [...routes, ...baseRoutes()];

const scenarios = [
  {
    name: "viewer",
    routes: [{ match: "user", ...ok(user("carol")) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer failures",
    routes: [{ match: "user", ...fail.auth }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unreadable",
    routes: [{ match: "user", ...ok("nope") }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "list",
    routes: [
      {
        match: "state=open&sort=recentupdate&limit=50&page=1",
        ...ok(
          [pr(), pr({ number: 8, title: "WIP: second", draft: undefined, mergeable: false })],
          'link: <x?page=2>; rel="next"\n',
        ),
      },
      { match: "page=2", ...ok([pr({ number: 9 }), null]) },
    ],
    calls: [
      { op: "list", input: listInput() },
      { op: "list", input: listInput({ limit: 2 }) },
      { op: "list", input: listInput({ limit: 3 }) },
    ],
  },
  {
    name: "list states",
    routes: [
      {
        match: "state=closed",
        ...ok([pr({ merged: true, state: "closed", merged_at: "2026-01-04T00:00:00Z" })]),
      },
      { match: "state=all", ...ok([]) },
    ],
    calls: [
      { op: "list", input: listInput({ state: "closed" }) },
      { op: "list", input: listInput({ state: "merged" }) },
      { op: "list", input: listInput({ state: "all", query: "dropped" }) },
    ],
  },
  {
    name: "list carries on from a cursor and learns the page size",
    routes: [
      {
        match: "page=1",
        ...ok([pr({ number: 1 }), pr({ number: 2 }), pr({ number: 3 })], 'link: <x>; rel="next"\n'),
      },
      {
        match: "page=2",
        ...ok([pr({ number: 4 }), pr({ number: 5 }), pr({ number: 6 })], 'link: <x>; rel="next"\n'),
      },
      { match: "page=3", ...ok([pr({ number: 7 })], 'link: <x>; rel="prev"\n') },
    ],
    calls: [
      {
        op: "list",
        input: listInput({
          limit: 2,
          cursor: { updatedBefore: "2026-01-01T00:00:00.000Z", delivered: 4 },
        }),
      },
      {
        op: "list",
        input: listInput({
          limit: 5,
          cursor: { updatedBefore: "2026-01-01T00:00:00.000Z", delivered: 2 },
        }),
      },
      {
        op: "list",
        input: listInput({
          limit: 9,
          cursor: { updatedBefore: "2026-01-01T00:00:00.000Z", delivered: 7 },
        }),
      },
    ],
  },
  {
    name: "list failures",
    routes: [{ match: "pulls", ...fail.rate }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list unreadable",
    routes: [{ match: "pulls", ...ok("{}") }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list with a null body",
    routes: [{ match: "pulls", ...ok("null") }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "summary",
    routes: [{ match: PULL, ...ok(pr()) }],
    calls: [{ op: "summary", input: subject }],
  },
  {
    name: "summary of states",
    routes: [
      {
        match: `${P}/pulls/8`,
        ...ok(
          pr({
            number: 8,
            merged: true,
            state: "closed",
            merged_at: "2026-01-04T00:00:00+02:00",
            closed_at: "2026-01-04T00:00:00Z",
          }),
        ),
      },
      {
        match: `${P}/pulls/9`,
        ...ok(
          pr({
            number: 9,
            state: "closed",
            closed_at: "garbage",
            draft: true,
            user: null,
            requested_reviewers: null,
            labels: null,
          }),
        ),
      },
      {
        match: `${P}/pulls/10`,
        ...ok(pr({ number: 10, title: "[WIP] thing", mergeable: undefined })),
      },
    ],
    calls: [8, 9, 10].map((number) => ({ op: "summary", input: { ...subject, number } })),
  },
  {
    name: "summary failures",
    routes: [
      { match: `${P}/pulls/8`, ...fail.gone },
      { match: `${P}/pulls/9`, ...fail.missing },
      { match: `${P}/pulls/10`, ...fail.auth },
      { match: `${P}/pulls/11`, ...fail.forbidden },
      { match: `${P}/pulls/12`, ...fail.boom },
      { match: `${P}/pulls/13`, ...ok('{"number":13}') },
    ],
    calls: [8, 9, 10, 11, 12, 13].map((number) => ({
      op: "summary",
      input: { ...subject, number },
    })),
  },
  {
    name: "checks",
    routes: [
      {
        match: `${P}/statuses/headsha?sort=recentupdate&limit=50&page=1`,
        ...ok(
          [
            status("build", "failure", "2026-01-02T00:00:00Z"),
            status("build", "success", "2026-01-03T00:00:00Z"),
            status("lint", "error", "2026-01-02T00:00:00Z"),
            status("deploy", "pending", "2026-01-02T00:00:00Z"),
            status("", "weird", "2026-01-02T00:00:00Z", { description: null, target_url: "" }),
          ],
          'link: <x>; rel="next"\n',
        ),
      },
      { match: `${P}/statuses/headsha`, ...ok([]) },
      { match: PULL, ...ok(pr()) },
    ],
    calls: [{ op: "checks", input: subject }],
  },
  {
    name: "detail",
    ordered: false,
    routes: [
      { match: PULL, ...ok(pr()) },
      { match: `${P}/statuses`, ...ok([status("ci", "success", "2026-01-02T00:00:00Z")]) },
      { match: "user", ...ok(user("carol")) },
      { match: P, ...ok(repo()) },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail variants",
    ordered: false,
    routes: [
      {
        match: `${P}/pulls/8`,
        ...ok(
          pr({
            number: 8,
            merge_base: "other",
            body: null,
            changed_files: null,
            requested_reviewers: [user("bob"), { login: "" }],
          }),
        ),
      },
      {
        match: `${P}/pulls/9`,
        ...ok(pr({ number: 9, merge_base: undefined, user: user("carol") })),
      },
      { match: `${P}/pulls/10`, ...ok(pr({ number: 10, merge_base: "", is_locked: true })) },
      { match: `${P}/statuses`, ...ok([]) },
      { match: "user", ...ok(user("carol")) },
      {
        match: P,
        ...ok(
          repo("acme/web", {
            permissions: undefined,
            allow_merge_commits: false,
            allow_squash_merge: undefined,
            allow_rebase: true,
            allow_rebase_update: false,
          }),
        ),
      },
    ],
    calls: [8, 9, 10].map((number) => ({ op: "detail", input: { ...subject, number } })),
  },
  {
    name: "detail fails with any of its reads",
    ordered: false,
    routes: [
      { match: PULL, ...ok(pr()) },
      { match: "user", ...fail.auth },
      { match: P, ...ok(repo()) },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "permissions",
    ordered: false,
    routes: [
      { match: `${P}/pulls/8`, ...ok(pr({ number: 8, user: user("carol") })) },
      { match: `${P}/pulls/9`, ...ok(pr({ number: 9, is_locked: true })) },
      { match: "user", ...ok(user("carol")) },
      { match: P, ...ok(repo("acme/web", { permissions: { push: false, admin: false } })) },
    ],
    calls: [
      { op: "permissions", input: subject },
      { op: "permissions", input: { ...subject, number: 8 } },
      { op: "permissions", input: { ...subject, number: 9 } },
    ],
  },
  {
    name: "permissions of a repository nobody can write",
    ordered: false,
    routes: [
      { match: PULL, ...ok(pr({ is_locked: true })) },
      { match: "user", ...ok(user("alice")) },
      { match: P, ...ok(repo("acme/web", { archived: true })) },
    ],
    calls: [{ op: "permissions", input: subject }],
  },
  {
    name: "activity",
    ordered: false,
    routes: [
      {
        match: `${ISSUE}/comments`,
        ...ok([comment(1), comment(2, { user: null, html_url: undefined })]),
      },
      {
        match: `${PULL}/reviews/20/comments`,
        ...ok([
          inline(30),
          inline(31, { position: 0, original_position: 9, resolver: user("dave") }),
        ]),
      },
      {
        match: `${PULL}/reviews?`,
        ...ok([
          review(20, { state: "COMMENT", comments_count: 2 }),
          review(21, { state: "REQUEST_CHANGES" }),
          review(22, { state: "PENDING", comments_count: 1 }),
          review(23, { state: "REQUEST_REVIEW" }),
          review(24, { html_url: undefined }),
        ]),
      },
      {
        match: `${PULL}/commits?`,
        ...ok([commitRow("c1"), commitRow("c2", { author: null, stats: null })]),
      },
      {
        match: `${ISSUE}/reactions`,
        ...ok([
          reactionRow("+1", "bob"),
          reactionRow("+1", "carol"),
          reactionRow("heart", "erin"),
          reactionRow("zzz", "x"),
        ]),
      },
      { match: `${P}/issues/comments/1/reactions`, ...ok([reactionRow("rocket", "carol")]) },
      { match: `${P}/issues/comments/30/reactions`, ...ok([reactionRow("eyes", "bob")]) },
      { match: "reactions", ...ok([]) },
      { match: "user", ...ok(user("carol")) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity pages",
    ordered: false,
    routes: [
      { match: `${ISSUE}/comments`, ...ok([]) },
      { match: `${PULL}/reviews?limit=50&page=2`, ...ok([review(2)]) },
      {
        match: `${PULL}/reviews?limit=50&page=1`,
        ...ok([review(1)], 'link: <x?page=2>; rel="next"\n'),
      },
      { match: `${PULL}/commits?`, ...ok([]) },
      { match: `${ISSUE}/reactions`, ...ok([]) },
      { match: "reactions", ...ok([]) },
      { match: "user", ...ok(user("carol")) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity fails with a read",
    ordered: false,
    routes: [
      { match: `${ISSUE}/comments`, ...ok([]) },
      { match: `${PULL}/reviews?`, ...fail.rate },
      { match: `${PULL}/commits?`, ...ok([]) },
      { match: `${ISSUE}/reactions`, ...ok([]) },
      { match: "user", ...ok(user("carol")) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "diffs",
    routes: [
      {
        match: `${PULL}.diff`,
        ...ok(
          "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/new.ts b/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/new.ts\n@@ -0,0 +1 @@\n+x\n",
        ),
      },
      { match: `${P}/git/commits/abc.diff`, ...ok("commit diff") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "ignored" } },
      { op: "diff", input: { ...subject, commit: "abc" } },
      { op: "revisions", input: { ...subject, paths: ["src/a.ts", "gone.ts"] } },
      { op: "revisions", input: { ...subject, paths: [] } },
    ],
  },
  {
    name: "diff failures",
    routes: [{ match: ".diff", ...fail.gone }],
    calls: [
      { op: "diff", input: subject },
      { op: "revisions", input: { ...subject, paths: ["a"] } },
    ],
  },
  {
    name: "file contents",
    ordered: false,
    routes: [
      { match: PULL, ...ok(pr({ head: branch("feature", "headsha", repo("fork/web")) })) },
      { match: `${P}/git/commits/c9`, ...ok(commitRow("c9", { parents: [{ sha: "c8" }] })) },
      {
        match: `contents/src/a.ts?ref=basesha`,
        ...ok({ content: Buffer.from("old\n").toString("base64"), encoding: "base64" }),
      },
      {
        match: `repos/fork/web/contents/src/a.ts?ref=headsha`,
        ...ok({ content: Buffer.from("new\n").toString("base64"), encoding: "base64" }),
      },
      {
        match: `contents/src/a.ts?ref=c8`,
        ...ok({ content: Buffer.from("older\n").toString("base64"), encoding: "base64" }),
      },
      {
        match: `repos/fork/web/contents/src/a.ts?ref=c9`,
        ...ok({ content: Buffer.from("newer ünï\n").toString("base64"), encoding: "base64" }),
      },
      {
        match: `contents/src/space%20d/b.ts?ref=basesha`,
        ...ok({ content: "aGk=\nIQ==", encoding: "base64" }),
      },
      { match: `contents/bad`, ...ok({ content: "x", encoding: "text" }) },
    ],
    calls: [
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "src/a.ts", newPath: "src/a.ts" },
      },
      {
        op: "diffFile",
        input: {
          ...subject,
          commit: "c9",
          changeType: "change",
          oldPath: "src/a.ts",
          newPath: "src/a.ts",
        },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "new", oldPath: "src/a.ts", newPath: "src/a.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "deleted", oldPath: "src/a.ts", newPath: "src/a.ts" },
      },
      {
        op: "diffFile",
        input: {
          ...subject,
          changeType: "rename-changed",
          oldPath: "src/space d/b.ts",
          newPath: "src/a.ts",
        },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "bad", newPath: "src/a.ts" },
      },
    ],
  },
  {
    name: "file contents without a merge base or a head repository",
    ordered: false,
    routes: [
      {
        match: PULL,
        ...ok(pr({ merge_base: undefined, head: branch("feature", "headsha", null) })),
      },
      { match: `contents/a?ref=basesha`, ...ok({ content: "b2xk", encoding: "base64" }) },
      { match: `contents/a?ref=headsha`, ...ok({ content: "bmV3", encoding: "base64" }) },
    ],
    calls: [
      { op: "diffFile", input: { ...subject, changeType: "change", oldPath: "a", newPath: "a" } },
    ],
  },
  {
    name: "actions",
    routes: [{ match: "pulls/7", ...ok("{}") }],
    calls: [
      { op: "action", input: { ...subject, action: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "rebase" } },
      { op: "action", input: { ...subject, action: "close" } },
      { op: "action", input: { ...subject, action: "reopen" } },
      { op: "action", input: { ...subject, action: "update-branch" } },
      { op: "action", input: { ...subject, action: "update-branch", updateMethod: "rebase" } },
      { op: "action", input: { ...subject, action: "ready" } },
      { op: "action", input: { ...subject, action: "revert" } },
    ],
  },
  {
    name: "actions fail",
    routes: [{ match: "pulls/7", ...fail.forbidden }],
    calls: [{ op: "action", input: { ...subject, action: "close" } }],
  },
  {
    name: "edits",
    routes: [{ match: "7", ...ok("{}") }],
    calls: [
      { op: "update", input: { ...subject, title: "New" } },
      { op: "update", input: { ...subject, body: "Only body" } },
      { op: "update", input: { ...subject, title: "T", body: "B" } },
      { op: "comment", input: { ...subject, body: 'hello "there"\ntrue' } },
      {
        op: "updateComment",
        input: { ...subject, commentId: "1:2/3", kind: "issue-comment", body: "edited" },
      },
    ],
  },
  {
    name: "reviews",
    routes: [
      { match: "reviews", ...ok("{}") },
      { match: PULL, ...ok(pr()) },
    ],
    calls: [
      { op: "review", input: { ...subject, verdict: "comment", body: "ok", comments: [] } },
      { op: "review", input: { ...subject, verdict: "approve", body: "", comments: [] } },
      { op: "review", input: { ...subject, verdict: "request-changes", body: "no", comments: [] } },
      {
        op: "review",
        input: {
          ...subject,
          verdict: "comment",
          body: "lines",
          comments: [
            { path: "a.ts", position: { kind: "added", newLine: 4 }, body: "added" },
            {
              path: "b.ts",
              oldPath: "old.ts",
              position: { kind: "deleted", oldLine: 7 },
              body: "deleted",
            },
            {
              path: "c.ts",
              position: { kind: "context", oldLine: 2, newLine: 3, side: "left" },
              body: "left",
            },
            {
              path: "d.ts",
              position: { kind: "context", oldLine: 5, newLine: 6, side: "right" },
              body: "right",
            },
          ],
        },
      },
    ],
  },
  {
    name: "review fails to read the pull request",
    routes: [{ match: PULL, ...fail.gone }],
    calls: [{ op: "review", input: { ...subject, verdict: "comment", body: "ok", comments: [] } }],
  },
  {
    name: "reviewers",
    routes: [
      { match: `${P}/assignees?limit=50&page=2`, ...ok([user("erin")]) },
      {
        match: `${P}/assignees`,
        ...ok(
          [user("alice"), user("bob"), user("carol"), { login: "" }],
          'link: <x>; rel="next"\n',
        ),
      },
      { match: "requested_reviewers", ...ok("{}") },
      { match: PULL, ...ok(pr()) },
    ],
    calls: [
      { op: "candidates", input: subject },
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "bob", kind: "user" },
            { id: "carol", kind: "user" },
          ],
          requested: true,
        },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "bob", kind: "user" }], requested: false },
      },
    ],
  },
  {
    name: "labels",
    routes: [
      {
        match: `${P}/labels`,
        ...ok([
          { id: 1, name: "bug", color: "ff0000", description: "A bug" },
          { id: 2, name: "ui", color: null },
          { id: 3, name: "docs" },
        ]),
      },
      { match: "issues/7/labels", ...ok("[]") },
      { match: PULL, ...ok(pr()) },
    ],
    calls: [
      { op: "labelCandidates", input: subject },
      { op: "setLabels", input: { ...subject, labels: ["bug", "docs"], applied: true } },
      { op: "setLabels", input: { ...subject, labels: ["bug", "docs"], applied: false } },
      { op: "setLabels", input: { ...subject, labels: ["bug", "nope"], applied: true } },
      { op: "setLabels", input: { ...subject, labels: [], applied: true } },
    ],
  },
  {
    name: "reactions",
    routes: [
      { match: "reactions", ...ok("{}") },
      { match: `${PULL}/reviews/20`, ...ok(review(20)) },
      { match: `${PULL}/reviews/21`, ...ok(review(21, { html_url: undefined })) },
    ],
    calls: [
      { op: "reaction", input: { ...subject, content: "thumbs-up", reacted: true } },
      { op: "reaction", input: { ...subject, content: "heart", reacted: false } },
      { op: "reaction", input: { ...subject, subjectId: "55", content: "rocket", reacted: true } },
      {
        op: "reaction",
        input: { ...subject, subjectId: "review:20", content: "eyes", reacted: true },
      },
      {
        op: "reaction",
        input: { ...subject, subjectId: "review:21", content: "eyes", reacted: true },
      },
      {
        op: "reaction",
        input: { ...subject, subjectId: "review:0", content: "eyes", reacted: true },
      },
      {
        op: "reaction",
        input: { ...subject, subjectId: "review:x", content: "eyes", reacted: true },
      },
      { op: "reaction", input: { ...subject, subjectId: "abc", content: "eyes", reacted: true } },
      { op: "reaction", input: { ...subject, subjectId: "05", content: "eyes", reacted: true } },
      { op: "reaction", input: { ...subject, content: "hooray", reacted: true } },
      { op: "reaction", input: { ...subject, content: "confused", reacted: true } },
      { op: "reaction", input: { ...subject, content: "laugh", reacted: true } },
      { op: "reaction", input: { ...subject, content: "thumbs-down", reacted: true } },
    ],
  },
  {
    name: "what Forgejo does not offer",
    routes: [],
    calls: [
      { op: "reply", input: { ...subject, threadId: "1", body: "x" } },
      { op: "resolution", input: { ...subject, threadId: "1", resolved: true } },
    ],
  },
];

const hasChecks = true;
void hasChecks;

const callProvider = (provider, call) => {
  const input = { cwd: "/repo", ...call.input };
  switch (call.op) {
    case "viewer":
      return provider.getViewer({ cwd: "/repo", host: "forgejo.test" });
    case "list":
      return provider.listChangeRequests(input);
    case "summary":
      return provider.getChangeRequestSummary(input);
    case "checks":
      return provider.getChangeRequestChecks(input);
    case "detail":
      return provider.getChangeRequest(input);
    case "permissions":
      return provider.getViewerPermissions(input);
    case "activity":
      return provider.getChangeRequestActivity(input);
    case "diff":
      return provider.getDiff(input);
    case "diffFile":
      return provider.getDiffFileContents(input);
    case "revisions":
      return provider.getFileRevisions(input);
    case "action":
      return provider.runAction(input);
    case "update":
      return provider.updateChangeRequest(input);
    case "comment":
      return provider.comment(input);
    case "updateComment":
      return provider.updateComment(input);
    case "review":
      return provider.submitReview(input);
    case "candidates":
      return provider.listReviewerCandidates(input);
    case "request":
      return provider.setReviewerRequest(input);
    case "labelCandidates":
      return provider.listLabelCandidates(input);
    case "setLabels":
      return provider.setLabels(input);
    case "reaction":
      return provider.setReaction(input);
    case "reply":
      return provider.replyToThread(input);
    case "resolution":
      return provider.setThreadResolution(input);
    default:
      throw new Error(`unknown op ${call.op}`);
  }
};

const SUMMARY_KEYS = [
  "number",
  "title",
  "url",
  "headBranch",
  "baseBranch",
  "state",
  "isDraft",
  "closedAt",
  "mergedAt",
  "updatedAt",
  "author",
  "additions",
  "deletions",
  "mergeability",
];
const THREAD_COMMENT_KEYS = ["id", "author", "body", "createdAt", "url", "reactions"];
// Keys the original's rows carry that the neutral shape does not, or the wire drops.
const neutral = (op, value) => {
  const copy = norm(value);
  if (op === "list")
    for (const row of copy.items) {
      delete row.closedAt;
      delete row.mergedAt;
    }
  if (op === "summary")
    return Object.fromEntries(
      SUMMARY_KEYS.map((key) => [key, copy[key]]).filter(([, v]) => v !== undefined),
    );
  if (op === "activity")
    for (const thread of copy.reviewThreads)
      thread.comments = thread.comments.map((entry) =>
        Object.fromEntries(
          THREAD_COMMENT_KEYS.map((key) => [key, entry[key]]).filter(([, v]) => v !== undefined),
        ),
      );
  if (op === "detail") {
    // The original's detail carries the row's own fields once; nothing to take out.
  }
  return copy;
};

for (const scenario of scenarios) {
  const requests = [];
  const routes = scenario.routes;
  const api = (input) => {
    const method = input.method ?? "GET";
    requests.push({
      repository: input.repository ?? null,
      host: input.host ?? null,
      path: input.path,
      method,
      body: input.body ?? null,
    });
    const route = routes.find(
      (candidate) =>
        (candidate.method ?? method) === method && input.path.includes(candidate.match),
    );
    if (route === undefined)
      return Effect.fail(
        new ForgejoCli.ForgejoCliError({
          command: "tea",
          cwd: "/repo",
          detail: `Unexpected Forgejo request: ${method} ${input.path}`,
        }),
      );
    if (route.error)
      return Effect.fail(
        new ForgejoCli.ForgejoCliError({ command: "tea", cwd: "/repo", ...route.error }),
      );
    return Effect.succeed({
      exitCode: 0,
      stdout: route.stdout,
      stderr: route.stderr ?? "",
      stdoutTruncated: false,
      stderrTruncated: false,
    });
  };
  const results = [];
  const callLog = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* ForgejoPullRequestProvider.make;
      for (const call of scenario.calls) {
        const before = requests.length;
        const exit = yield* Effect.exit(callProvider(provider, call));
        results.push(
          exit._tag === "Success"
            ? { ok: neutral(call.op, exit.value ?? null) }
            : (() => {
                const error = exit.cause.reasons[0].error;
                return { error: { reason: error.reason, detail: error.detail } };
              })(),
        );
        callLog.push(requests.slice(before));
      }
    }).pipe(Effect.provide(Layer.mock(ForgejoCli.ForgejoCli)({ api }))),
  );
  rows.push({
    type: "op",
    name: scenario.name,
    ordered: scenario.ordered !== false,
    routes: routes.map(({ match, method, stdout, stderr, error }) => ({
      match,
      method,
      stdout,
      stderr,
      error,
    })),
    calls: scenario.calls,
    results,
    requests: callLog,
  });
}
void lazy;

{
  const provider = await Effect.runPromise(
    ForgejoPullRequestProvider.make.pipe(Effect.provide(Layer.mock(ForgejoCli.ForgejoCli)({}))),
  );
  rows.push({
    type: "declared",
    capabilities: norm(provider.capabilities),
    kind: provider.kind,
    hasChecks: provider.getChangeRequestChecks !== undefined,
    hasSummary: provider.getChangeRequestSummary !== undefined,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/pull-request-forgejo.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(`wrote ${rows.length} rows to ${target.pathname}`);
