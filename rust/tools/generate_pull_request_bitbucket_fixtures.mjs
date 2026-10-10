// Runs the original Bitbucket pull request layer and records what it produces, for
// crates/server/tests/pull_request_bitbucket.rs to replay:
//  - every decoder of bitbucketPullRequestJson.ts and the patch reader of bitbucketDiffRevisions.ts
//    over payloads that exercise each branch;
//  - BitbucketPullRequestProvider over BitbucketPullRequestApi over a scripted `request`. A scenario
//    names the answer for each request by the method and a fragment of its url, so the order
//    requests are made in (several run together) does not matter; what is recorded is every
//    request's method, url and body, and the provider's result or failure for each call.
// No network is used and nothing is spawned. Run with TZ=UTC.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
import * as BitbucketApi from "../../apps/server/src/sourceControl/BitbucketApi.ts";
import * as J from "../../apps/server/src/pullRequest/bitbucketPullRequestJson.ts";
import { parseDiffFileRevisions } from "../../apps/server/src/pullRequest/bitbucketDiffRevisions.ts";
import * as BitbucketPullRequestApi from "../../apps/server/src/pullRequest/BitbucketPullRequestApi.ts";
import * as BitbucketPullRequestProvider from "../../apps/server/src/pullRequest/BitbucketPullRequestProvider.ts";

const rows = [];
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );
// The instant the Rust tests' clock stands at (2026-10-05T12:00:00Z), which a Retry-After counts from.
const NOW_MS = 1_791_201_600_000;
const BASE = "https://api.bitbucket.org/2.0";

// ---- payloads ----------------------------------------------------------------------------------
const user = (nick, overrides = {}) => ({
  uuid: `{${nick}-uuid}`,
  nickname: nick,
  display_name: nick.charAt(0).toUpperCase() + nick.slice(1),
  links: { avatar: { href: `https://bitbucket.org/account/${nick}/avatar.png` } },
  ...overrides,
});
const pr = (overrides = {}) => ({
  id: 7,
  title: "Fix the thing",
  description: "Body text",
  state: "OPEN",
  draft: false,
  author: user("alice"),
  source: { branch: { name: "feature" }, repository: { full_name: "acme/web" } },
  destination: { branch: { name: "main" }, repository: { full_name: "acme/web" } },
  created_on: "2026-01-01T00:00:00.123456+00:00",
  updated_on: "2026-01-02T00:00:00+00:00",
  reviewers: [user("bob")],
  participants: [
    {
      user: user("bob"),
      role: "REVIEWER",
      approved: true,
      state: "approved",
      participated_on: "2026-01-03T10:00:00+00:00",
    },
  ],
  links: { html: { href: "https://bitbucket.org/acme/web/pull-requests/7" } },
  ...overrides,
});
const json = (value) => JSON.stringify(value);
const page = (values, extra = {}) => ({ values, pagelen: 50, ...extra });

const decoded = (result) =>
  Result.isSuccess(result) ? { ok: norm(result.success) } : { error: true };
const decoderRow = (decoder, raw, project = (value) => value) => {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  const result = J[decoder](text);
  rows.push({
    type: "decoder",
    decoder,
    raw: text,
    result: Result.isSuccess(result) ? { ok: norm(project(result.success)) } : { error: true },
  });
};

// ---- pull requests -----------------------------------------------------------------------------
const pullRequestCases = [
  pr(),
  pr({ state: "MERGED" }),
  pr({ state: " declined " }),
  pr({ state: "SUPERSEDED" }),
  pr({ state: null }),
  pr({ state: undefined }),
  pr({ state: "weird" }),
  pr({ draft: true }),
  pr({ draft: null }),
  pr({ draft: "yes" }),
  pr({ description: null }),
  pr({ description: undefined }),
  pr({ description: 5 }),
  pr({ title: "" }),
  pr({ title: undefined }),
  pr({ id: 1.5 }),
  pr({ id: "7" }),
  pr({ id: 12.0 }),
  pr({ author: null }),
  pr({ author: undefined }),
  pr({ author: { display_name: "App Bot" } }),
  pr({ author: { nickname: "  ", display_name: "  " } }),
  pr({
    author: {
      nickname: " carol ",
      display_name: " Carol C ",
      links: { avatar: { href: " https://x/y.png " } },
    },
  }),
  pr({ author: { nickname: "dave", links: null } }),
  pr({ author: { nickname: "dave", links: { avatar: null } } }),
  pr({ author: { nickname: "dave", links: { avatar: { href: 5 } } } }),
  pr({ author: { nickname: 5 } }),
  pr({ author: "alice" }),
  pr({ source: { branch: { name: " padded " } } }),
  pr({ source: { branch: { name: "  " } } }),
  pr({ source: { branch: {} } }),
  pr({ source: undefined }),
  pr({ source: { branch: { name: "f" }, repository: null } }),
  pr({ source: { branch: { name: "f" }, repository: { full_name: " fork/web " } } }),
  pr({ source: { branch: { name: "f" }, repository: { full_name: "  " } } }),
  pr({ source: { branch: { name: "f" }, repository: {} } }),
  pr({ destination: undefined }),
  pr({ destination: { branch: { name: "" } } }),
  pr({ created_on: "2026-01-01T00:00:00Z" }),
  pr({ created_on: "2026-01-01T05:30:00+05:30" }),
  pr({ created_on: "2026-01-01T00:00:00.1+00:00" }),
  pr({ created_on: "2026-01-01" }),
  pr({ created_on: "not a date" }),
  pr({ created_on: "" }),
  pr({ created_on: undefined }),
  pr({ created_on: 5 }),
  pr({ updated_on: undefined }),
  pr({ updated_on: "2026-06-16T05:04:33.987654321+00:00" }),
  pr({ reviewers: null }),
  pr({ reviewers: undefined }),
  pr({ reviewers: [] }),
  pr({
    reviewers: [
      user("bob"),
      { display_name: "No Handle App" },
      { nickname: " " },
      user("carol", { uuid: " {c} " }),
    ],
  }),
  pr({
    reviewers: [
      user("bob", { uuid: undefined }),
      user("dave", { uuid: null }),
      user("erin", { uuid: "  " }),
    ],
  }),
  pr({ reviewers: [5] }),
  pr({ reviewers: "bob" }),
  pr({ participants: null }),
  pr({ participants: undefined }),
  pr({ participants: [] }),
  pr({
    participants: [
      { user: user("bob"), approved: true, participated_on: "2026-01-03T10:00:00+00:00" },
      { user: user("carol"), approved: false, participated_on: "2026-01-03T11:00:00+00:00" },
      {
        user: user("dave"),
        state: "changes_requested",
        participated_on: "2026-01-03T12:00:00.5+00:00",
      },
      {
        user: user("erin"),
        state: "  ",
        approved: true,
        participated_on: "2026-01-03T13:00:00+00:00",
      },
      { user: user("frank"), state: "approved" },
      { user: user("gina"), state: "approved", participated_on: "  " },
      { state: "approved", participated_on: "2026-01-03T13:00:00+00:00" },
      { user: null, state: "approved", participated_on: "2026-01-03T13:00:00+00:00" },
      {
        user: { display_name: "A Bot" },
        state: "approved",
        participated_on: "2026-01-03T14:00:00+00:00",
      },
      { user: user("hank"), role: null, state: null, approved: true, participated_on: "bad date" },
    ],
  }),
  pr({ participants: [{ user: user("bob"), approved: "yes" }] }),
  pr({ participants: [{ user: user("bob"), role: 5 }] }),
  pr({ participants: ["bob"] }),
  pr({ links: { html: { href: " https://bitbucket.org/acme/web/pull-requests/7 " } } }),
  pr({ links: { html: { href: "" } } }),
  pr({ links: { html: {} } }),
  pr({ links: {} }),
  pr({ links: undefined }),
  pr({ extra: "ignored", merge_commit: { hash: "abc" } }),
];
for (const raw of pullRequestCases) decoderRow("decodePullRequestJson", raw);
decoderRow("decodePullRequestJson", "not json");
decoderRow("decodePullRequestJson", "");
decoderRow("decodePullRequestJson", "[]");
decoderRow("decodePullRequestJson", "null");

const pageCases = [
  page([pr(), pr({ id: 8 })]),
  page([]),
  page([pr()], { next: "https://api.bitbucket.org/2.0/repositories/acme/web/pullrequests?page=2" }),
  page([pr()], { next: "  https://api.bitbucket.org/2.0/x?page=3  " }),
  page([pr()], { next: "" }),
  page([pr()], { next: "   " }),
  page([pr()], { next: null }),
  page([pr()], { next: 5 }),
  page([pr()], { size: 1 }),
  page([pr()], { size: null }),
  page([pr()], { size: 1.5 }),
  page([pr()], { size: "1" }),
  page([pr(), { id: "bad" }, 5, null, pr({ id: 9, source: { branch: {} } }), pr({ id: 10 })]),
  page([pr({ id: 11 })]),
  { pagelen: 50 },
  { values: null },
  { values: {} },
  [],
  "not a page",
];
for (const raw of pageCases) decoderRow("decodePullRequestPageJson", raw);
decoderRow("decodePullRequestPageJson", "not json");
decoderRow("decodePullRequestPageJson", "");

for (const raw of [
  { nickname: "alice", display_name: "Alice" },
  { nickname: " alice " },
  { display_name: "Some App" },
  { nickname: "  ", display_name: " App " },
  { nickname: null, display_name: null },
  {},
  { nickname: 5 },
  { display_name: [] },
  [],
  "alice",
])
  decoderRow("decodeViewerJson", raw);
decoderRow("decodeViewerJson", "nope");

for (const raw of [
  { values: [{ permission: "admin" }] },
  { values: [{ permission: "write" }] },
  { values: [{ permission: " WRITE " }] },
  { values: [{ permission: "read" }] },
  { values: [{ permission: "Admin" }, { permission: "read" }] },
  { values: [{ permission: "read" }, { permission: "admin" }] },
  { values: [] },
  { values: [{}] },
  { values: [{ permission: null }] },
  { values: [{ permission: "  " }] },
  { values: [{ permission: 5 }] },
  { values: [5] },
  { values: null },
  {},
  { values: "admin" },
  [],
])
  decoderRow("decodeRepositoryPermissionJson", raw);
decoderRow("decodeRepositoryPermissionJson", "nope");

const member = (nick, overrides = {}) => ({ user: user(nick, overrides) });
for (const raw of [
  page([member("alice"), member("bob")]),
  page([member("alice")], { next: "https://api.bitbucket.org/2.0/workspaces/acme/members?page=2" }),
  page([
    member("alice", { uuid: undefined }),
    member("bob", { uuid: " " }),
    member("carol", { uuid: null }),
  ]),
  page([
    { user: null },
    {},
    5,
    { user: { uuid: "{x}" } },
    { user: { uuid: "{y}", display_name: "App" } },
  ]),
  page([member("alice", { uuid: " {padded} ", nickname: " alice " })]),
  page([{ user: "alice" }, member("dave")]),
  page([]),
  { values: 5 },
])
  decoderRow("decodeWorkspaceMembersJson", raw);

// ---- comments ----------------------------------------------------------------------------------
const comment = (id, overrides = {}) => ({
  id,
  content: { raw: `Comment ${id}` },
  user: user("bob"),
  created_on: `2026-01-02T00:0${id % 10}:00+00:00`,
  links: { html: { href: `https://bitbucket.org/acme/web/pull-requests/7/_/diff#comment-${id}` } },
  ...overrides,
});
const inline = (path, extra = {}) => ({ path, to: 4, ...extra });
const commentCases = [
  page([comment(1), comment(2)]),
  page([
    comment(1, { deleted: true }),
    comment(2, { pending: true }),
    comment(3),
    comment(4, { deleted: false, pending: false }),
  ]),
  page([
    comment(1, { content: { raw: "  " } }),
    comment(2, { content: { raw: "" } }),
    comment(3, { content: null }),
    comment(4, { content: {} }),
    comment(5, { content: undefined }),
  ]),
  page([comment(1, { content: { raw: 5 } })]),
  page([
    comment(1, { inline: inline("a.ts") }),
    comment(2, { inline: inline("  ") }),
    comment(3, { inline: { path: null } }),
    comment(4, { inline: null }),
  ]),
  page([
    comment(1, { inline: inline("a.ts", { to: null, from: 3 }), resolution: { type: "resolved" } }),
  ]),
  page([
    comment(1, { inline: inline("a.ts", { to: 0 }) }),
    comment(2, { inline: inline("b.ts", { to: null, from: 0 }) }),
    comment(3, { inline: inline("c.ts", { to: null, from: null }) }),
  ]),
  page([
    comment(1, { inline: inline("a.ts", { outdated: true }) }),
    comment(2, { inline: inline("b.ts", { outdated: null }) }),
    comment(3, { inline: inline("c.ts", { outdated: false }) }),
  ]),
  page([comment(1, { inline: inline("a.ts", { from: 1.5 }) })]),
  page([comment(1, { inline: inline("a.ts", { outdated: "yes" }) })]),
  // a thread: a line comment with replies, one of them a reply to a reply, one out of order
  page([
    comment(10, { inline: inline("a.ts"), created_on: "2026-01-02T00:00:00+00:00" }),
    comment(12, { parent: { id: 11 }, created_on: "2026-01-02T00:02:00+00:00" }),
    comment(11, {
      parent: { id: 10 },
      created_on: "2026-01-02T00:01:00+00:00",
      user: user("carol"),
    }),
    comment(13, {
      inline: inline("b.ts", { to: null, from: 9 }),
      created_on: "2026-01-02T00:00:30+00:00",
      resolution: { user: "x" },
    }),
    comment(14, { parent: { id: 99 }, created_on: "2026-01-02T00:03:00+00:00" }),
    comment(15, { parent: { id: 13 }, deleted: true }),
    comment(16, { created_on: "2026-01-02T00:04:00+00:00" }),
  ]),
  // a cycle of parents terminates
  page([
    comment(20, { parent: { id: 21 }, inline: inline("a.ts") }),
    comment(21, { parent: { id: 20 } }),
  ]),
  page([
    comment(30, { parent: null, inline: inline("z.ts", { to: 7 }) }),
    comment(31, { parent: { id: 30 }, created_on: "2026-01-02T00:00:00.5+00:00" }),
    comment(32, { parent: { id: 30 }, created_on: "2026-01-02T00:00:00+00:00" }),
  ]),
  page([comment(40, { created_on: "bad" }), comment(41, { created_on: "2026-01-02T00:00:00Z" })]),
  page([
    comment(1, { user: null }),
    comment(2, { user: { display_name: "Robot" } }),
    comment(3, { user: undefined }),
  ]),
  page([
    comment(1, { links: null }),
    comment(2, { links: { html: null } }),
    comment(3, { links: { html: { href: "  " } } }),
    comment(4, { links: { html: {} } }),
  ]),
  page([
    comment(1),
    { id: "x" },
    5,
    null,
    { content: { raw: "no id" }, created_on: "2026-01-02T00:00:00Z" },
    comment(2, { created_on: undefined }),
  ]),
  page([comment(1, { parent: { id: "x" } })]),
  page([
    comment(1, { resolution: null }),
    comment(2, { resolution: undefined }),
    comment(3, { resolution: {} }),
    comment(4, { resolution: false }),
    comment(5, { resolution: 0 }),
  ]),
  page([comment(1)], {
    next: "https://api.bitbucket.org/2.0/repositories/acme/web/pullrequests/7/comments?page=2",
  }),
  page([]),
  { nope: 1 },
];
for (const raw of commentCases)
  decoderRow("decodeCommentsJson", raw, (value) => ({
    comments: value.comments,
    next: value.next,
    threads: J.buildReviewThreads(value.entries),
  }));
decoderRow("decodeCommentsJson", "nope");

// ---- commits -----------------------------------------------------------------------------------
const commit = (hash, overrides = {}) => ({
  hash,
  message: `Subject ${hash}\n\nBody lines`,
  date: "2026-01-02T00:00:00+00:00",
  author: { raw: "Dev <dev@example.com>", user: user("bob") },
  ...overrides,
});
for (const raw of [
  page([commit("c3"), commit("c2"), commit("c1")]),
  page([commit("c1")], { next: "https://api.bitbucket.org/2.0/x/commits?page=2" }),
  page([commit(""), commit("  "), commit("c1"), commit(undefined)]),
  page([commit(" c1 ")]),
  page([
    commit("c1", { date: null }),
    commit("c2", { date: "  " }),
    commit("c3", { date: undefined }),
    commit("c4", { date: "bad" }),
  ]),
  page([
    commit("c1", { message: null }),
    commit("c2", { message: undefined }),
    commit("c3", { message: "" }),
    commit("c4", { message: "\nstarts blank" }),
  ]),
  page([
    commit("c1", { author: null }),
    commit("c2", { author: undefined }),
    commit("c3", { author: {} }),
  ]),
  page([
    commit("c1", { author: { raw: "Raw Only <r@x>" } }),
    commit("c2", { author: { raw: "  ", user: null } }),
    commit("c3", { author: { user: { display_name: "App" } } }),
  ]),
  page([commit("c1", { author: { raw: "Fallback", user: { nickname: " " } } })]),
  page([commit("c1", { author: { raw: 5 } })]),
  page([commit("c1", { message: 5 })]),
  page([commit("c1"), 5, { hash: 5 }, null]),
  page([]),
  { values: 1 },
])
  decoderRow("decodeCommitsJson", raw);

// ---- statuses ----------------------------------------------------------------------------------
const status = (key, overrides = {}) => ({
  key,
  name: `Build ${key}`,
  state: "SUCCESSFUL",
  description: `Description ${key}`,
  url: `https://bitbucket.org/acme/web/addon/pipelines/home#!/results/${key}`,
  ...overrides,
});
for (const raw of [
  page([status("a")]),
  page([
    status("a", { state: "FAILED" }),
    status("b", { state: "STOPPED" }),
    status("c", { state: "INPROGRESS" }),
    status("d", { state: "weird" }),
    status("e", { state: null }),
    status("f", { state: undefined }),
    status("g", { state: " successful " }),
  ]),
  page([
    status("a", { name: null }),
    status("b", { name: "  " }),
    status("c", { name: undefined, key: " ckey " }),
  ]),
  page([
    status(null, { name: null }),
    status(undefined, { name: undefined }),
    status("  ", { name: "  " }),
  ]),
  page([status("same", { name: "Build" }), status("other", { name: "Build" })]),
  page([
    status("same", { name: "Build", state: "FAILED" }),
    status("same", { name: "Build", state: "SUCCESSFUL" }),
  ]),
  page([
    status("a", { description: null, url: null }),
    status("b", { description: "  ", url: "  " }),
  ]),
  page([status("a", { description: 5 })]),
  page([status("a"), 5, null, { state: 5 }]),
  page([status("a")], { next: "https://api.bitbucket.org/2.0/x/statuses?page=2" }),
  page([]),
  { values: "x" },
])
  decoderRow("decodeStatusesJson", raw);

// ---- diffstat and conflicts --------------------------------------------------------------------
for (const raw of [
  page([
    { lines_added: 3, lines_removed: 1 },
    { lines_added: 5, lines_removed: 0 },
  ]),
  page([{ lines_added: 3 }, { lines_removed: 2 }, {}, { lines_added: null, lines_removed: null }]),
  page([
    { lines_added: 3, lines_removed: 1 },
    { lines_added: "x" },
    5,
    null,
    { lines_removed: 1.5 },
  ]),
  page([{ lines_added: 3 }], { next: "https://api.bitbucket.org/2.0/x/diffstat?page=2" }),
  page([]),
  { values: null },
])
  decoderRow("decodeDiffstatJson", raw);
for (const raw of [
  page([]),
  page([{ path: "a.ts" }]),
  page([5, null]),
  { values: [], size: 0 },
  { values: {} },
  {},
  "nope",
])
  decoderRow("decodeConflictsJson", raw);

// ---- the patch reader ---------------------------------------------------------------------------
const patchCases = [
  "",
  "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
  "diff --git a/gone.ts b/gone.ts\ndeleted file mode 100644\nindex 3333333..0000000\n--- a/gone.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
  "diff --git a/old.ts b/new.ts\nsimilarity index 80%\nrename from old.ts\nrename to new.ts\nindex 4444444..5555555 100644\n--- a/old.ts\n+++ b/new.ts\n@@ -1 +1 @@\n-a\n+b\n",
  "diff --git a/old.ts b/new.ts\nsimilarity index 100%\nrename from old.ts\nrename to new.ts\n",
  "diff --git a/excluded.min.js b/excluded.min.js\nBinary files differ\ndiff --git a/ok.ts b/ok.ts\nindex 6666666..7777777 100644\n--- a/ok.ts\n+++ b/ok.ts\n@@ -1 +1 @@\n-a\n+b\n",
  "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1,3 +1,3 @@\n+diff --git a/fake.ts b/fake.ts\n+index aaaaaaa..bbbbbbb 100644\n-index ccccccc..ddddddd\n context\n",
  "diff --git a/one two b/one two\nindex 8888888..9999999 100644\n--- a/one two\t\n+++ b/one two\t\n@@ -1 +1 @@\n-a\n+b\n",
  "diff --git a/a b/b b/a b/b\nindex 8888888..9999999 100644\n@@ -1 +1 @@\n-a\n",
  "diff --git a/x b/x\nnew file mode 100644\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+a\n",
  'diff --git "a/tab\\tname" "b/tab\\tname"\nindex aaaaaaa..bbbbbbb 100644\n--- "a/tab\\tname"\n+++ "b/tab\\tname"\n@@ -1 +1 @@\n-a\n',
  'diff --git "a/caf\\303\\251.txt" "b/caf\\303\\251.txt"\nindex ccccccc..ddddddd 100644\n--- "a/caf\\303\\251.txt"\n+++ "b/caf\\303\\251.txt"\n@@ -1 +1 @@\n-a\n',
  'diff --git "a/old name" b/new\nindex eeeeeee..fffffff 100644\nrename from "old name"\nrename to new\n@@ -1 +1 @@\n',
  'diff --git a/old b/"new name"\nindex 1010101..2020202 100644\nrename from old\nrename to "new name"\n@@ -1 +1 @@\n',
  "diff --git a/emoji\u{1F600}.txt b/emoji\u{1F600}.txt\nindex 3030303..4040404 100644\n--- a/emoji\u{1F600}.txt\n+++ b/emoji\u{1F600}.txt\n@@ -1 +1 @@\n",
  "diff --git a/ts.txt b/ts.txt\nindex 5050505..6060606 100644\n--- a/ts.txt\t2026-01-02 00:00:00.000000000 +0000\n+++ b/ts.txt\t2026-01-02 00:00:00.000000000 +0000\n@@ -1 +1 @@\n",
  "diff --git a/a.ts b/a.ts\nindex 1111111..2222222\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/b.ts b/b.ts\nindex 3333333..4444444 100644\n--- a/b.ts\n+++ b/b.ts\n@@ -1 +1 @@\n-a\n+b",
  "diff --git a/a.ts b/a.ts\nindex ..\n@@ -1 +1 @@\n",
  "diff --git a/a.ts b/a.ts\nindex abc\n@@ -1 +1 @@\n",
  "diff --git a/a.ts b/a.ts\nindex 1111111..\n@@ -1 +1 @@\n",
  "not a diff\nindex 1..2\n",
  'diff --git a/"half b/x\nindex 7070707..8080808 100644\n',
  'diff --git "a/unterminated b/x\nindex 7070707..8080808 100644\n',
  'diff --git "a/q" b/q\nindex 9090909..a0a0a0a 100644\n',
  'diff --git "a/q"b/q\nindex 9090909..a0a0a0a 100644\n',
  'diff --git "a/back\\\\slash" "b/back\\\\slash"\nindex 1212121..3434343 100644\n@@ -1 +1 @@\n',
  'diff --git "a/trail\\" "b/trail\\"\nindex 1212121..3434343 100644\n@@ -1 +1 @@\n',
  'diff --git "a/odd\\q\\400" "b/odd\\q\\400"\nindex 1212121..3434343 100644\n@@ -1 +1 @@\n',
];
for (const patch of patchCases)
  rows.push({ type: "revisions", patch, result: norm(parseDiffFileRevisions(patch)) });

// ---- the provider over the API over a scripted `request` ---------------------------------------
const fail = {
  auth: { status: 401 },
  forbidden: { status: 403 },
  notFound: { status: 404 },
  gone: { status: 410 },
  rate: { status: 429, retryAfter: 30 },
  rateNoHeader: { status: 429 },
  boom: { status: 500 },
};
const relative = (url) => (url.startsWith(BASE) ? url.slice(BASE.length) : url);
function harness(routes) {
  const calls = [];
  const hits = routes.map(() => 0);
  const request = (input) => {
    const url = relative(input.url);
    calls.push({ method: input.method, url, body: input.body ?? null });
    const index = routes.findIndex(
      (route) =>
        (route.method === undefined || route.method === input.method) && url.includes(route.match),
    );
    const route = index === -1 ? { status: 599 } : routes[index];
    const step = route.seq ? route.seq[Math.min(hits[index], route.seq.length - 1)] : route;
    if (index !== -1) hits[index] += 1;
    if (step.error === "request")
      return Effect.fail(
        new BitbucketApi.BitbucketRequestError({
          operation: "request",
          cause: new Error("connect failed"),
        }),
      );
    const status = step.status ?? 200;
    if (status >= 200 && status < 300)
      return Effect.succeed({ body: step.body ?? "", truncated: step.truncated === true });
    return Effect.fail(
      new BitbucketApi.BitbucketResponseError({
        operation: "request",
        status,
        responseBodyLength: 0,
        ...(step.retryAfter === undefined ? {} : { retryAt: NOW_MS + step.retryAfter * 1000 }),
      }),
    );
  };
  const layer = BitbucketPullRequestApi.layer.pipe(
    Layer.provide(Layer.mock(BitbucketApi.BitbucketApi)({ request })),
  );
  return { layer, calls };
}

const REPO = "acme/web";
const PRS = "/repositories/acme/web/pullrequests";
const subject = { repository: REPO, host: "bitbucket.org", number: 7 };
const listInput = (overrides = {}) => ({
  repository: REPO,
  host: "bitbucket.org",
  state: "open",
  involvement: "all",
  viewer: "alice",
  limit: 20,
  ...overrides,
});
const rowsOf = (count, first = 1, overrides = {}) =>
  Array.from({ length: count }, (_, index) =>
    pr({
      id: first + index,
      title: `Pull request ${first + index}`,
      updated_on: `2026-01-${String(28 - Math.floor(index / 5)).padStart(2, "0")}T00:00:0${index % 5}+00:00`,
      ...overrides,
    }),
  );
const NEXT = (suffix) => `${BASE}${PRS}${suffix}`;
const ok = (body) => ({ body: typeof body === "string" ? body : json(body) });

// the routes a detail read makes
const detailRoutes = (extra = []) => [
  ...extra,
  {
    match: "/diffstat",
    ...ok(
      page([
        { lines_added: 3, lines_removed: 1 },
        { lines_added: 5, lines_removed: 0 },
      ]),
    ),
  },
  { match: "/conflicts", ...ok(page([])) },
  { match: "/statuses", ...ok(page([status("a")])) },
  { match: "/user/permissions/repositories", ...ok({ values: [{ permission: "write" }] }) },
  { match: `${PRS}/7`, ...ok(pr()) },
];

const scenarios = [
  // ---- the signed-in account
  {
    name: "viewer",
    routes: [{ match: "/user", ...ok({ nickname: " alice " }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer display name",
    routes: [{ match: "/user", ...ok({ display_name: "App Account" }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer without a handle",
    routes: [{ match: "/user", ...ok({ nickname: " " }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unreadable",
    routes: [{ match: "/user", ...ok("nope") }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unauthenticated",
    routes: [{ match: "/user", ...fail.auth }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer forbidden",
    routes: [{ match: "/user", ...fail.forbidden }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer rate limited",
    routes: [{ match: "/user", ...fail.rate }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer rate limited without a header",
    routes: [{ match: "/user", ...fail.rateNoHeader }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unreachable",
    routes: [{ match: "/user", error: "request" }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer not found",
    routes: [{ match: "/user", ...fail.notFound }],
    calls: [{ op: "viewer", input: {} }],
  },

  // ---- listings
  {
    name: "list asks for reviewers, newest first, at the page ceiling",
    routes: [{ match: PRS, ...ok(page(rowsOf(3))) }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list follows the cursor Bitbucket sends",
    routes: [
      { match: "page=2", ...ok(page(rowsOf(2, 51), { next: NEXT("?page=3") })) },
      { match: "page=3", ...ok(page(rowsOf(2, 53))) },
      { match: PRS, ...ok(page(rowsOf(50), { next: NEXT("?page=2") })) },
    ],
    calls: [{ op: "list", input: listInput({ limit: 200 }) }],
  },
  {
    name: "list stops at the caller's page and says more remain",
    routes: [{ match: PRS, ...ok(page(rowsOf(50), { next: NEXT("?page=2") })) }],
    calls: [{ op: "list", input: listInput({ limit: 10 }) }],
  },
  {
    name: "list counts the rows it walked past as more to come",
    routes: [{ match: PRS, ...ok(page(rowsOf(12))) }],
    calls: [
      { op: "list", input: listInput({ limit: 10 }) },
      { op: "list", input: listInput({ limit: 12 }) },
      { op: "list", input: listInput({ limit: 13 }) },
    ],
  },
  {
    name: "list gives up after ten pages",
    routes: [
      { match: "page=", ...ok(page(rowsOf(1, 1), { next: NEXT("?page=99") })) },
      { match: PRS, ...ok(page(rowsOf(1, 1), { next: NEXT("?page=1") })) },
    ],
    calls: [{ op: "list", input: listInput({ limit: 500 }) }],
  },
  {
    name: "list searches with a filter expression",
    routes: [{ match: PRS, ...ok(page([])) }],
    calls: [
      { op: "list", input: listInput({ query: "fix login" }) },
      { op: "list", input: listInput({ query: 'say "hi" \\ there' }) },
      { op: "list", input: listInput({ query: "a & b = c ~ d" }) },
      { op: "list", input: listInput({ query: "   " }) },
      { op: "list", input: listInput({ query: "" }) },
      { op: "list", input: listInput({ query: "  padded  " }) },
    ],
  },
  {
    name: "list carries on from the instant the last slice ended on",
    routes: [{ match: PRS, ...ok(page([])) }],
    calls: [
      {
        op: "list",
        input: listInput({ cursor: { updatedBefore: "2026-01-02T00:00:00.000Z", delivered: 3 } }),
      },
      {
        op: "list",
        input: listInput({
          cursor: { updatedBefore: "2026-01-02T00:00:00.000Z", delivered: 0 },
          query: "words",
        }),
      },
    ],
  },
  {
    name: "list per state",
    routes: [{ match: PRS, ...ok(page([])) }],
    calls: ["open", "closed", "merged", "all"].map((state) => ({
      op: "list",
      input: listInput({ state }),
    })),
  },
  {
    name: "list ignores involvement and filters",
    routes: [{ match: PRS, ...ok(page([pr()])) }],
    calls: [
      { op: "list", input: listInput({ involvement: "reviewing" }) },
      { op: "list", input: listInput({ involvement: "authored", filters: { draft: "only" } }) },
    ],
  },
  {
    name: "list maps the rows",
    routes: [
      {
        match: PRS,
        ...ok(
          page([
            pr(),
            pr({
              id: 8,
              state: "MERGED",
              draft: true,
              author: null,
              source: { branch: { name: "fork" }, repository: { full_name: "fork/web" } },
            }),
            pr({ id: 9, state: "DECLINED", reviewers: [] }),
            { id: "bad" },
          ]),
        ),
      },
    ],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list refuses a repository that is not workspace and slug",
    routes: [{ match: PRS, ...ok(page([])) }],
    calls: [
      { op: "list", input: listInput({ repository: "web" }) },
      { op: "list", input: listInput({ repository: "a/b/c" }) },
      { op: "list", input: listInput({ repository: "" }) },
      { op: "list", input: listInput({ repository: " acme / web " }) },
      { op: "list", input: listInput({ repository: "acme//web/" }) },
      { op: "list", input: listInput({ repository: "ac me/we b" }) },
    ],
  },
  {
    name: "list unreadable",
    routes: [{ match: PRS, ...ok("nope") }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list failures",
    routes: [
      { match: PRS, seq: [fail.auth, fail.rate, fail.notFound, fail.boom, { error: "request" }] },
    ],
    calls: Array.from({ length: 5 }, () => ({ op: "list", input: listInput() })),
  },
  {
    name: "list fails on a later page",
    routes: [
      { match: "page=2", ...fail.boom },
      { match: PRS, ...ok(page(rowsOf(50), { next: NEXT("?page=2") })) },
    ],
    calls: [{ op: "list", input: listInput({ limit: 100 }) }],
  },

  // ---- checks
  {
    name: "checks read the state and the statuses, not the diff",
    ordered: false,
    routes: [
      { match: "/statuses", ...ok(page([status("a"), status("b", { state: "FAILED" })])) },
      { match: `${PRS}/7`, ...ok(pr({ state: "MERGED" })) },
    ],
    calls: [{ op: "checks", input: subject }],
  },
  {
    name: "checks follow the status pages",
    ordered: false,
    routes: [
      { match: "page=2", ...ok(page([status("c")])) },
      { match: "/statuses", ...ok(page([status("a")], { next: NEXT("/7/statuses?page=2") })) },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "checks", input: subject }],
  },
  {
    name: "checks fail with the pull request",
    ordered: false,
    routes: [
      { match: "/statuses", ...ok(page([])) },
      { match: `${PRS}/7`, ...fail.notFound },
    ],
    calls: [{ op: "checks", input: subject }],
  },
  {
    name: "checks fail with the statuses",
    ordered: false,
    routes: [
      { match: "/statuses", ...fail.boom },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "checks", input: subject }],
  },

  // ---- the detail
  {
    name: "detail",
    ordered: false,
    routes: detailRoutes(),
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail with conflicts and a read-only account",
    ordered: false,
    routes: [
      { match: "/conflicts", ...ok(page([{ path: "a.ts" }])) },
      { match: "/user/permissions/repositories", ...ok({ values: [{ permission: "read" }] }) },
      ...detailRoutes().filter(
        (route) => !["/conflicts", "/user/permissions/repositories"].includes(route.match),
      ),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail sums every diffstat page",
    ordered: false,
    routes: [
      { match: "page=2", ...ok(page([{ lines_added: 10, lines_removed: 4 }])) },
      {
        match: "/diffstat",
        ...ok(page([{ lines_added: 3, lines_removed: 1 }], { next: NEXT("/7/diffstat?page=2") })),
      },
      ...detailRoutes().filter((route) => route.match !== "/diffstat"),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail grants a permission it could not read",
    ordered: false,
    routes: [
      {
        match: "/user/permissions/repositories",
        seq: [
          fail.gone,
          fail.boom,
          fail.forbidden,
          { error: "request" },
          ok("nope"),
          ok({ values: [] }),
        ],
      },
      ...detailRoutes().filter((route) => route.match !== "/user/permissions/repositories"),
    ],
    calls: Array.from({ length: 6 }, () => ({ op: "detail", input: subject })),
  },
  {
    name: "detail leaves out the parts Bitbucket could not answer",
    ordered: false,
    routes: [
      { match: "/conflicts", ...fail.boom },
      { match: "/statuses", ...fail.forbidden },
      ...detailRoutes().filter((route) => !["/conflicts", "/statuses"].includes(route.match)),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail recovers unreadable parts too",
    ordered: false,
    routes: [
      { match: "/conflicts", ...ok("nope") },
      { match: "/statuses", ...ok("nope") },
      ...detailRoutes().filter((route) => !["/conflicts", "/statuses"].includes(route.match)),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail is refused on a rate limit in any part",
    ordered: false,
    routes: [
      { match: "/conflicts", ...fail.rate },
      ...detailRoutes().filter((route) => route.match !== "/conflicts"),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail fails with the pull request or the diffstat",
    ordered: false,
    routes: [
      { match: `${PRS}/7`, ...fail.notFound },
      ...detailRoutes().filter((route) => route.match !== `${PRS}/7`),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail fails with the diffstat",
    ordered: false,
    routes: [
      { match: "/diffstat", ...fail.auth },
      ...detailRoutes().filter((route) => route.match !== "/diffstat"),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail unreadable",
    ordered: false,
    routes: [
      { match: `${PRS}/7`, ...ok("nope") },
      ...detailRoutes().filter((route) => route.match !== `${PRS}/7`),
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail refuses a repository that is not workspace and slug",
    routes: [],
    calls: [{ op: "detail", input: { ...subject, repository: "web" } }],
  },
  {
    name: "detail of a merged fork pull request",
    ordered: false,
    routes: [
      ...detailRoutes().filter((route) => route.match !== `${PRS}/7`),
      {
        match: `${PRS}/7`,
        ...ok(
          pr({
            state: "MERGED",
            draft: true,
            source: { branch: { name: "fork-branch" }, repository: { full_name: "fork/web" } },
          }),
        ),
      },
    ],
    calls: [{ op: "detail", input: subject }],
  },

  // ---- the viewer's standing
  {
    name: "permissions ask for this repository's permission only",
    routes: [
      { match: "/user/permissions/repositories", ...ok({ values: [{ permission: "admin" }] }) },
    ],
    calls: [{ op: "permissions", input: subject }],
  },
  {
    name: "permissions per level",
    routes: [
      {
        match: "/user/permissions/repositories",
        seq: [
          ok({ values: [{ permission: "write" }] }),
          ok({ values: [{ permission: "read" }] }),
          ok({ values: [] }),
          fail.gone,
        ],
      },
    ],
    calls: Array.from({ length: 4 }, () => ({ op: "permissions", input: subject })),
  },
  {
    name: "permissions escape the repository name inside the filter",
    routes: [
      { match: "/user/permissions/repositories", ...ok({ values: [{ permission: "read" }] }) },
    ],
    calls: [
      { op: "permissions", input: { ...subject, repository: 'acme/we"b' } },
      { op: "permissions", input: { ...subject, repository: " acme/web " } },
    ],
  },
  {
    name: "permissions fail",
    routes: [
      {
        match: "/user/permissions/repositories",
        seq: [fail.boom, fail.auth, ok("nope"), fail.rate, { error: "request" }],
      },
    ],
    calls: Array.from({ length: 5 }, () => ({ op: "permissions", input: subject })),
  },
  {
    name: "permissions refuse a repository that is not workspace and slug",
    routes: [],
    calls: [{ op: "permissions", input: { ...subject, repository: "web" } }],
  },

  // ---- the conversation
  {
    name: "activity reads the conversation, the commits and the votes",
    ordered: false,
    routes: [
      { match: "/commits", ...ok(page([commit("c2"), commit("c1")])) },
      {
        match: "/comments",
        ...ok(
          page([
            comment(1),
            comment(2, { inline: inline("a.ts") }),
            comment(3, { parent: { id: 2 }, created_on: "2026-01-02T00:09:00+00:00" }),
          ]),
        ),
      },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity follows the comment cursor and rebuilds a thread across pages",
    ordered: false,
    routes: [
      {
        match: "/comments?page=2",
        ...ok(page([comment(3, { parent: { id: 2 }, created_on: "2026-01-02T00:09:00+00:00" })])),
      },
      {
        match: "/comments",
        ...ok(page([comment(2, { inline: inline("a.ts") })], { next: NEXT("/7/comments?page=2") })),
      },
      { match: "/commits", ...ok(page([])) },
      { match: `${PRS}/7`, ...ok(pr({ participants: [] })) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity stops the comment walk at its bound",
    ordered: false,
    routes: [
      { match: "/comments", ...ok(page([comment(1)], { next: NEXT("/7/comments?page=more") })) },
      { match: "/commits", ...ok(page([])) },
      { match: `${PRS}/7`, ...ok(pr({ participants: [] })) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity follows the commit pages, oldest first",
    ordered: false,
    routes: [
      { match: "/commits?page=2", ...ok(page([commit("c1")])) },
      {
        match: "/commits",
        ...ok(page([commit("c4"), commit("c3")], { next: NEXT("/7/commits?page=2") })),
      },
      { match: "/comments", ...ok(page([])) },
      { match: `${PRS}/7`, ...ok(pr({ participants: [] })) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity degrades the parts Bitbucket could not answer",
    ordered: false,
    routes: [
      { match: "/commits", ...fail.boom },
      { match: "/comments", ...fail.forbidden },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity is refused on a rate limit in any part",
    ordered: false,
    routes: [
      { match: "/commits", ...fail.rate },
      { match: "/comments", ...ok(page([])) },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity fails with the pull request",
    ordered: false,
    routes: [
      { match: "/commits", ...ok(page([])) },
      { match: "/comments", ...ok(page([])) },
      { match: `${PRS}/7`, ...fail.auth },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity reads the comments unreadable as a cut short conversation",
    ordered: false,
    routes: [
      { match: "/commits", ...ok("nope") },
      { match: "/comments", ...ok("nope") },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "activity", input: subject }],
  },

  // ---- the diff
  {
    name: "diff is the patch, verbatim",
    routes: [
      { match: "/diff", body: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n" },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "ignored" } },
    ],
  },
  {
    name: "diff of one commit",
    routes: [{ match: "/diff/", body: "diff --git a/c b/c\n" }],
    calls: [
      { op: "diff", input: { ...subject, commit: "abc1234" } },
      { op: "diff", input: { ...subject, commit: "ABCDEF0123456789abcdef0123456789abcdef01" } },
    ],
  },
  {
    name: "diff refuses a commit that is not a sha",
    routes: [{ match: "/diff", body: "x" }],
    calls: [
      "nope",
      "abc123",
      "abc123g",
      "../../etc",
      "a".repeat(65),
      "",
      "abc1234/../x",
      "abc 1234",
    ].map((commit) => ({ op: "diff", input: { ...subject, commit } })),
  },
  {
    name: "diff cut short",
    routes: [{ match: "/diff", body: "partial", truncated: true }],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff failures",
    routes: [{ match: "/diff", seq: [fail.notFound, fail.rate, fail.auth, fail.boom] }],
    calls: Array.from({ length: 4 }, () => ({ op: "diff", input: subject })),
  },
  {
    name: "diff refuses a repository that is not workspace and slug",
    routes: [],
    calls: [{ op: "diff", input: { ...subject, repository: "web" } }],
  },

  // ---- file versions
  {
    name: "revisions read every file the patch states, and say what the asked paths lack",
    routes: [
      {
        match: "/diff",
        body: "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/gone.ts b/gone.ts\ndeleted file mode 100644\nindex 3333333..0000000\n--- a/gone.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
      },
    ],
    calls: [
      { op: "revisions", input: { ...subject, paths: ["a.ts", "untouched.ts"] } },
      { op: "revisions", input: { ...subject, paths: ["gone.ts"] } },
      { op: "revisions", input: { ...subject, paths: [] } },
    ],
  },
  {
    name: "revisions say nothing about the files past a patch cut short",
    routes: [
      {
        match: "/diff",
        body: "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n",
        truncated: true,
      },
    ],
    calls: [{ op: "revisions", input: { ...subject, paths: ["a.ts", "later.ts"] } }],
  },
  {
    name: "revisions ask nothing when no file was ticked",
    routes: [],
    calls: [{ op: "revisions", input: { ...subject, paths: [] } }],
  },
  {
    name: "revisions hold the patch for a run of ticks but not a failure",
    routes: [
      {
        match: "/diff",
        seq: [
          fail.boom,
          { body: "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n" },
        ],
      },
    ],
    calls: [
      { op: "revisions", input: { ...subject, paths: ["a.ts"] } },
      { op: "revisions", input: { ...subject, paths: ["a.ts"] } },
      { op: "revisions", input: { ...subject, paths: ["b.ts"] } },
    ],
  },
  {
    name: "revisions of another pull request are another patch",
    routes: [
      {
        match: "/7/diff",
        body: "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n",
      },
      {
        match: "/8/diff",
        body: "diff --git a/b.ts b/b.ts\nindex 3333333..4444444 100644\n@@ -1 +1 @@\n",
      },
    ],
    calls: [
      { op: "revisions", input: { ...subject, paths: ["a.ts"] } },
      { op: "revisions", input: { ...subject, number: 8, paths: ["b.ts"] } },
      { op: "revisions", input: { ...subject, paths: ["b.ts"] } },
    ],
  },

  // ---- reviewers
  {
    name: "candidates are the workspace's people, the author left out",
    ordered: false,
    routes: [
      {
        match: "/workspaces/acme/members",
        ...ok(
          page([member("alice"), member("bob"), member("carol"), member("noid", { uuid: null })]),
        ),
      },
      { match: `${PRS}/7`, ...ok(pr({ reviewers: [user("bob")] })) },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "candidates say when the workspace has more people",
    ordered: false,
    routes: [
      {
        match: "/workspaces/acme/members",
        ...ok(page([member("bob")], { next: `${BASE}/workspaces/acme/members?page=2` })),
      },
      { match: `${PRS}/7`, ...ok(pr({ author: null })) },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "candidates escape the workspace",
    ordered: false,
    routes: [
      { match: "/workspaces/", ...ok(page([])) },
      { match: "/pullrequests/7", ...ok(pr()) },
    ],
    calls: [{ op: "candidates", input: { ...subject, repository: "we ird/web" } }],
  },
  {
    name: "candidates failures",
    ordered: false,
    routes: [
      { match: "/workspaces/acme/members", ...fail.forbidden },
      { match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "candidates fail with the pull request",
    ordered: false,
    routes: [
      { match: "/workspaces/acme/members", ...ok(page([])) },
      { match: `${PRS}/7`, ...fail.notFound },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "request writes the reviewer set back whole",
    routes: [
      { method: "GET", match: `${PRS}/7`, ...ok(pr({ reviewers: [user("bob"), user("dave")] })) },
      { method: "PUT", match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "{carol-uuid}", kind: "user" }], requested: true },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "{bob-uuid}", kind: "user" }], requested: false },
      },
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "{bob-uuid}", kind: "user" },
            { id: "{x}", kind: "user" },
          ],
          requested: true,
        },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "{nobody}", kind: "user" }], requested: false },
      },
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "{bob-uuid}", kind: "user" },
            { id: "{dave-uuid}", kind: "user" },
          ],
          requested: false,
        },
      },
    ],
  },
  {
    name: "request skips reviewers without ids and repeats",
    routes: [
      {
        method: "GET",
        match: `${PRS}/7`,
        ...ok(pr({ reviewers: [user("bob", { uuid: null }), user("dave"), user("dave")] })),
      },
      { method: "PUT", match: `${PRS}/7`, ...ok(pr()) },
    ],
    calls: [
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "{e}", kind: "user" },
            { id: "{e}", kind: "user" },
          ],
          requested: true,
        },
      },
    ],
  },
  {
    name: "request failures",
    routes: [
      { method: "GET", match: `${PRS}/7`, seq: [ok(pr()), ok(pr()), fail.notFound] },
      { method: "PUT", match: `${PRS}/7`, seq: [fail.forbidden, fail.rate] },
    ],
    calls: Array.from({ length: 3 }, () => ({
      op: "request",
      input: { ...subject, reviewers: [{ id: "{x}", kind: "user" }], requested: true },
    })),
  },

  // ---- actions and writes
  {
    name: "action merges with Bitbucket's name for the strategy",
    routes: [{ match: "/merge", ...ok("{}") }],
    calls: [
      { op: "action", input: { ...subject, action: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "rebase" } },
    ],
  },
  {
    name: "action closes a pull request by declining it",
    routes: [{ match: "/decline", ...ok("{}") }],
    calls: [{ op: "action", input: { ...subject, action: "close" } }],
  },
  {
    name: "action everything else is a decline, as only merge and close reach it",
    routes: [{ match: "/decline", ...ok("{}") }],
    calls: [{ op: "action", input: { ...subject, action: "reopen" } }],
  },
  {
    name: "action failures",
    routes: [
      { match: "/merge", seq: [fail.forbidden, fail.rate, fail.auth, { error: "request" }] },
    ],
    calls: Array.from({ length: 4 }, () => ({
      op: "action",
      input: { ...subject, action: "merge" },
    })),
  },
  {
    name: "update sends only the words it rewrites",
    routes: [{ method: "PUT", match: `${PRS}/7`, ...ok(pr()) }],
    calls: [
      { op: "update", input: { ...subject, title: "New title" } },
      { op: "update", input: { ...subject, body: "New body" } },
      { op: "update", input: { ...subject, title: "T", body: "B" } },
      { op: "update", input: { ...subject, title: "", body: "" } },
      { op: "update", input: { ...subject } },
    ],
  },
  {
    name: "update failure",
    routes: [{ method: "PUT", match: `${PRS}/7`, ...fail.forbidden }],
    calls: [{ op: "update", input: { ...subject, title: "x" } }],
  },
  {
    name: "comment posts a JSON document",
    routes: [{ method: "POST", match: "/comments", ...ok("{}") }],
    calls: [
      { op: "comment", input: { ...subject, body: 'hello "there"\ntrue' } },
      { op: "comment", input: { ...subject, body: "line one\nline two \\ backslash \u{1F600}" } },
    ],
  },
  {
    name: "comment failure",
    routes: [{ method: "POST", match: "/comments", seq: [fail.rate, fail.auth] }],
    calls: [
      { op: "comment", input: { ...subject, body: "x" } },
      { op: "comment", input: { ...subject, body: "x" } },
    ],
  },
  {
    name: "updateComment rewrites a comment where it stands, whichever kind",
    routes: [{ method: "PUT", match: "/comments/", ...ok("{}") }],
    calls: [
      {
        op: "updateComment",
        input: { ...subject, commentId: "11", kind: "issue-comment", body: "edited" },
      },
      {
        op: "updateComment",
        input: { ...subject, commentId: "12", kind: "review-comment", body: "edited too" },
      },
      {
        op: "updateComment",
        input: { ...subject, commentId: "a/b c", kind: "issue-comment", body: "odd id" },
      },
    ],
  },
  {
    name: "review replays a review as its requests, the verdict last",
    routes: [
      { method: "POST", match: "/comments", ...ok("{}") },
      { method: "POST", match: "/approve", ...ok("{}") },
      { method: "POST", match: "/request-changes", ...ok("{}") },
    ],
    calls: [
      {
        op: "review",
        input: {
          ...subject,
          verdict: "approve",
          body: "Looks good",
          comments: [
            { path: "a.ts", position: { kind: "added", newLine: 4 }, body: "Nice" },
            { path: "b.ts", position: { kind: "deleted", oldLine: 9 }, body: "Why?" },
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
            {
              path: "e.ts",
              oldPath: "old.ts",
              position: { kind: "added", newLine: 1 },
              body: "renamed",
            },
          ],
        },
      },
      { op: "review", input: { ...subject, verdict: "request-changes", body: "No", comments: [] } },
      {
        op: "review",
        input: { ...subject, verdict: "comment", body: "Just a note", comments: [] },
      },
      { op: "review", input: { ...subject, verdict: "comment", body: "  ", comments: [] } },
      { op: "review", input: { ...subject, verdict: "approve", body: "", comments: [] } },
      {
        op: "review",
        input: {
          ...subject,
          verdict: "comment",
          body: "",
          comments: [
            { path: "a.ts", position: { kind: "added", newLine: 4 }, body: "only a line" },
          ],
        },
      },
    ],
  },
  {
    name: "review stops at the first failure and never reaches the verdict",
    routes: [
      { method: "POST", match: "/comments", seq: [ok("{}"), fail.boom] },
      { method: "POST", match: "/approve", ...ok("{}") },
    ],
    calls: [
      {
        op: "review",
        input: {
          ...subject,
          verdict: "approve",
          body: "summary",
          comments: [
            { path: "a.ts", position: { kind: "added", newLine: 4 }, body: "one" },
            { path: "b.ts", position: { kind: "added", newLine: 5 }, body: "two" },
          ],
        },
      },
      { op: "review", input: { ...subject, verdict: "approve", body: "", comments: [] } },
    ],
  },
  {
    name: "review verdict failure",
    routes: [
      { method: "POST", match: "/approve", ...fail.forbidden },
      { method: "POST", match: "/comments", ...ok("{}") },
    ],
    calls: [{ op: "review", input: { ...subject, verdict: "approve", body: "ok", comments: [] } }],
  },
  {
    name: "reply names the comment it answers",
    routes: [{ method: "POST", match: "/comments", ...ok("{}") }],
    calls: [
      { op: "reply", input: { ...subject, threadId: "10", body: "done" } },
      { op: "reply", input: { ...subject, threadId: "not-a-number", body: "odd" } },
      { op: "reply", input: { ...subject, threadId: "", body: "empty" } },
      { op: "reply", input: { ...subject, threadId: " 12 ", body: "padded" } },
    ],
  },
  {
    name: "resolution creates and deletes the sub-resource",
    routes: [{ match: "/resolve", ...ok("{}") }],
    calls: [
      { op: "resolution", input: { ...subject, threadId: "10", resolved: true } },
      { op: "resolution", input: { ...subject, threadId: "10", resolved: false } },
      { op: "resolution", input: { ...subject, threadId: "a/b", resolved: true } },
    ],
  },
  {
    name: "resolution failure",
    routes: [{ match: "/resolve", seq: [fail.notFound, fail.forbidden] }],
    calls: [
      { op: "resolution", input: { ...subject, threadId: "10", resolved: true } },
      { op: "resolution", input: { ...subject, threadId: "10", resolved: false } },
    ],
  },
  {
    name: "reaction is refused",
    routes: [],
    calls: [
      { op: "reaction", input: { ...subject, subjectId: "10", content: "heart", reacted: true } },
    ],
  },
  {
    name: "writes refuse a repository that is not workspace and slug",
    routes: [],
    calls: [
      { op: "action", input: { ...subject, repository: "web", action: "merge" } },
      { op: "update", input: { ...subject, repository: "web", title: "x" } },
      { op: "comment", input: { ...subject, repository: "web", body: "x" } },
      {
        op: "updateComment",
        input: { ...subject, repository: "web", commentId: "1", kind: "issue-comment", body: "x" },
      },
      {
        op: "review",
        input: { ...subject, repository: "web", verdict: "comment", body: "x", comments: [] },
      },
      { op: "reply", input: { ...subject, repository: "web", threadId: "1", body: "x" } },
      { op: "resolution", input: { ...subject, repository: "web", threadId: "1", resolved: true } },
      { op: "request", input: { ...subject, repository: "web", reviewers: [], requested: true } },
      { op: "candidates", input: { ...subject, repository: "web" } },
      { op: "activity", input: { ...subject, repository: "web" } },
      { op: "checks", input: { ...subject, repository: "web" } },
      { op: "revisions", input: { ...subject, repository: "web", paths: ["a"] } },
    ],
  },
];

// The pull request's own url is a prefix of every url below it, so its route answers last.
for (const scenario of scenarios) {
  const own = (route) => route.method === undefined && route.match === `${PRS}/7`;
  scenario.routes = [
    ...scenario.routes.filter((route) => !own(route)),
    ...scenario.routes.filter(own),
  ];
}

const callProvider = (provider, call) => {
  const input = { cwd: "/repo", ...call.input };
  switch (call.op) {
    case "viewer":
      return provider.getViewer({ cwd: "/repo" });
    case "list":
      return provider.listChangeRequests(input);
    case "checks":
      return provider.getChangeRequestChecks(input);
    case "detail":
      return provider.getChangeRequest(input);
    case "activity":
      return provider.getChangeRequestActivity(input);
    case "permissions":
      return provider.getViewerPermissions(input);
    case "diff":
      return provider.getDiff(input);
    case "revisions":
      return provider.getFileRevisions(input);
    case "candidates":
      return provider.listReviewerCandidates(input);
    case "request":
      return provider.setReviewerRequest(input);
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
    case "reply":
      return provider.replyToThread(input);
    case "reaction":
      return provider.setReaction(input);
    case "resolution":
      return provider.setThreadResolution(input);
    default:
      throw new Error(`unknown op ${call.op}`);
  }
};

for (const scenario of scenarios) {
  const { layer, calls } = harness(scenario.routes);
  const results = [];
  const callLog = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* BitbucketPullRequestProvider.make;
      for (const call of scenario.calls) {
        const before = calls.length;
        const exit = yield* Effect.exit(callProvider(provider, call));
        results.push(
          exit._tag === "Success"
            ? { ok: norm(exit.value ?? null) }
            : (() => {
                const error = exit.cause.reasons[0].error;
                return {
                  error: {
                    reason: error.reason,
                    detail: error.detail,
                    ...(error.retryAt === undefined ? {} : { retryAt: error.retryAt }),
                  },
                };
              })(),
        );
        callLog.push(calls.slice(before));
      }
    }).pipe(Effect.provide(layer)),
  );
  rows.push({
    type: "op",
    name: scenario.name,
    ordered: scenario.ordered !== false,
    routes: scenario.routes.map(
      ({ method, match, body, status, retryAfter, truncated, error, seq }) => ({
        method,
        match,
        body,
        status,
        retryAfter,
        truncated,
        error,
        seq,
      }),
    ),
    calls: scenario.calls,
    results,
    requests: callLog,
  });
}

// ---- capabilities and permissions ---------------------------------------------------------------
{
  const { layer } = harness([]);
  const capabilities = await Effect.runPromise(
    BitbucketPullRequestProvider.make.pipe(
      Effect.map((provider) => norm(provider.capabilities)),
      Effect.provide(layer),
    ),
  );
  rows.push({
    type: "capabilities",
    capabilities,
    permissions: {
      write: norm(BitbucketPullRequestProvider.bitbucketViewerPermissions({ canWrite: true })),
      read: norm(BitbucketPullRequestProvider.bitbucketViewerPermissions({ canWrite: false })),
    },
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/pull-request-bitbucket.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(`wrote ${rows.length} rows to ${target.pathname}`);
