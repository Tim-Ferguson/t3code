// Runs the original GitLab pull request layer and records what it produces, for
// crates/server/tests/pull_request_gitlab.rs to replay:
//  - every decoder of gitLabMergeRequestJson.ts over payloads that exercise each branch;
//  - GitLabPullRequestProvider over GitLabPullRequestCli over GitLabCli over the real VcsProcess
//    policy with a scripted process runner. A scenario names the answer for each request by a
//    fragment of its arguments, so the order requests are made in (several run together) does not
//    matter; what is recorded is every `glab` argument list and stdin, and the provider's result
//    or failure for each call.
// No network is used and no process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
import * as GitLabCli from "../../apps/server/src/sourceControl/GitLabCli.ts";
import * as J from "../../apps/server/src/pullRequest/gitLabMergeRequestJson.ts";
import * as GitLabPullRequestCli from "../../apps/server/src/pullRequest/GitLabPullRequestCli.ts";
import * as GitLabPullRequestProvider from "../../apps/server/src/pullRequest/GitLabPullRequestProvider.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";

const rows = [];
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );

// ---- payloads ----------------------------------------------------------------------------------
const user = (overrides = {}) => ({
  id: 1,
  username: "alice",
  name: "Alice",
  avatar_url: "https://gitlab.com/uploads/alice.png",
  ...overrides,
});
const mr = (overrides = {}) => ({
  iid: 7,
  title: "Fix the thing",
  web_url: "https://gitlab.com/g/p/-/merge_requests/7",
  description: "Body text",
  author: user(),
  source_branch: "feature",
  target_branch: "main",
  state: "opened",
  draft: false,
  merge_status: "can_be_merged",
  has_conflicts: false,
  created_at: "2026-01-01T00:00:00.000Z",
  updated_at: "2026-01-02T00:00:00.000Z",
  merged_at: null,
  closed_at: null,
  reviewers: [user({ id: 2, username: "bob", name: "Bob", avatar_url: null })],
  labels: ["bug", "  ", "ui"],
  changes_count: "3",
  head_pipeline: {
    status: "success",
    web_url: "https://gitlab.com/g/p/-/pipelines/9",
    source: "merge_request_event",
  },
  user: { can_merge: true },
  merge_when_pipeline_succeeds: false,
  squash_on_merge: true,
  diverged_commits_count: 2,
  ...overrides,
});
const decoded = (result) =>
  Result.isSuccess(result) ? { ok: norm(result.success) } : { error: true };
const decoderRow = (decoder, raw, extra = []) => {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  rows.push({
    type: "decoder",
    decoder,
    raw: text,
    args: extra,
    result: decoded(J[decoder](text, ...extra)),
  });
};

// ---- merge requests ----------------------------------------------------------------------------
const detailCases = [
  mr(),
  mr({ state: "merged", merged_at: "2026-01-03T00:00:00Z" }),
  mr({ state: "merged", merged_at: "  " }),
  mr({ state: " CLOSED ", closed_at: "2026-01-04T00:00:00Z" }),
  mr({ state: "locked" }),
  mr({ state: null }),
  mr({ draft: true }),
  mr({ draft: undefined, work_in_progress: true }),
  mr({ draft: false, work_in_progress: true }),
  mr({ draft: undefined, work_in_progress: undefined }),
  mr({ has_conflicts: true }),
  mr({ has_conflicts: null, merge_status: "cannot_be_merged" }),
  mr({ merge_status: " CAN_BE_MERGED " }),
  mr({ merge_status: "checking" }),
  mr({ merge_status: null }),
  mr({ head_pipeline: null }),
  mr({ head_pipeline: { status: "failed" } }),
  mr({ head_pipeline: { status: "canceled", web_url: " ", source: "push" } }),
  mr({ head_pipeline: { status: "cancelling" } }),
  mr({ head_pipeline: { status: "skipped" } }),
  mr({ head_pipeline: { status: "manual" } }),
  mr({ head_pipeline: { status: "scheduled" } }),
  mr({ head_pipeline: { status: "running" } }),
  mr({ head_pipeline: { status: null } }),
  mr({ head_pipeline: {} }),
  mr({ changes_count: "1000+" }),
  mr({ changes_count: "0" }),
  mr({ changes_count: "abc" }),
  mr({ changes_count: null }),
  mr({ changes_count: " 12 " }),
  mr({ changes_count: 5 }),
  mr({ user: null }),
  mr({ user: {} }),
  mr({ user: { can_merge: false } }),
  mr({ merge_when_pipeline_succeeds: true, auto_merge_enabled: undefined }),
  mr({ merge_when_pipeline_succeeds: false, auto_merge_enabled: true }),
  mr({ merge_when_pipeline_succeeds: null, auto_merge_enabled: null }),
  mr({ merge_when_pipeline_succeeds: undefined, auto_merge_enabled: undefined }),
  mr({ merge_when_pipeline_succeeds: true, squash_on_merge: false }),
  mr({ merge_when_pipeline_succeeds: false, squash_on_merge: true }),
  mr({ merge_when_pipeline_succeeds: true, squash_on_merge: null }),
  mr({ diverged_commits_count: 0 }),
  mr({ diverged_commits_count: null }),
  mr({ diverged_commits_count: undefined }),
  mr({ diverged_commits_count: 1.5 }),
  mr({ author: null }),
  mr({ author: { username: "  " } }),
  mr({ author: { username: "carol", name: "  ", avatar_url: "  " } }),
  mr({ author: { id: "x", username: "carol" } }),
  mr({ author: { id: 4.0, username: "dave" } }),
  mr({ author: {} }),
  mr({ reviewers: null }),
  mr({ reviewers: [] }),
  mr({
    reviewers: [user({ id: undefined, username: "noid" }), user({ id: 9, username: " pad " })],
  }),
  mr({ reviewers: [{ username: 5 }] }),
  mr({ labels: null }),
  mr({ labels: [] }),
  mr({ labels: ["a", 5] }),
  mr({ description: null }),
  mr({ description: undefined }),
  mr({ iid: 7.0 }),
  mr({ iid: "7" }),
  mr({ iid: 7.5 }),
  mr({ title: undefined }),
  mr({ web_url: 5 }),
  mr({ created_at: null }),
  mr({ updated_at: undefined }),
  mr({ source_branch: null }),
  mr({ squash: 5 }),
  mr({ extra_field: { nested: true } }),
  [],
  "not json",
  "",
  null,
];
for (const raw of detailCases) decoderRow("decodeMergeRequestDetailJson", raw);

const listCases = [
  [],
  [mr(), mr({ iid: 8, title: "Second" })],
  [mr(), { bad: true }, mr({ iid: 9 })],
  [{ bad: true }, null, 5],
  [mr({ iid: "x" }), mr({ iid: 10 })],
  { not: "array" },
  "nope",
  "",
];
for (const raw of listCases) decoderRow("decodeMergeRequestListJson", raw);

for (const raw of [
  { username: "alice" },
  { username: "  " },
  { username: null },
  {},
  { username: 5 },
  [],
  "x",
  "",
])
  decoderRow("decodeViewerJson", raw);

const users = [
  user({ id: 3, username: "carol" }),
  user({ id: 4, username: "dave", name: null, avatar_url: null }),
  user({ id: undefined, username: "noid" }),
  user({ id: 5, username: "  " }),
  { bad: true },
];
for (const raw of [users, [], {}, "x", [user({ id: 6, username: "erin", avatar_url: " " })]])
  decoderRow("decodeProjectUsersJson", raw);

for (const settings of [
  { merge_method: "merge", squash_option: "default_off" },
  { merge_method: "rebase_merge", squash_option: "always" },
  { merge_method: " FF ", squash_option: "default_on" },
  { merge_method: "ff", squash_option: "never" },
  { merge_method: "weird", squash_option: "weird" },
  { merge_method: null, squash_option: null },
  {},
  { merge_method: 5 },
  [],
  "x",
])
  decoderRow("decodeProjectMergeCapabilitiesJson", settings);

// ---- conversation ------------------------------------------------------------------------------
const note = (overrides = {}) => ({
  id: 11,
  body: "A comment",
  author: user(),
  created_at: "2026-01-02T00:00:00.000Z",
  system: false,
  type: null,
  ...overrides,
});
for (const raw of [
  [
    note(),
    note({ id: 12, system: true }),
    note({ id: 13, body: "   " }),
    note({ id: 14, body: null }),
  ],
  [note({ type: "DiffNote", position: { new_path: "src/a.ts", old_path: "src/b.ts" } })],
  [note({ type: " DiffNote ", position: { new_path: null, old_path: "src/old.ts" } })],
  [note({ type: "DiscussionNote", position: { new_path: "  ", old_path: "  " } })],
  [note({ author: null }), note({ id: "x" }), { bad: true }],
  [note({ id: 15, body: "  padded  " })],
  [],
  {},
  "x",
])
  decoderRow("decodeNotesJson", raw);

const pos = (overrides = {}) => ({
  position_type: "text",
  new_path: "src/a.ts",
  old_path: "src/a.ts",
  new_line: 12,
  old_line: null,
  ...overrides,
});
const discNote = (overrides = {}) => ({
  id: 21,
  body: "Please fix",
  author: user(),
  created_at: "2026-01-02T00:00:00.000Z",
  system: false,
  resolvable: true,
  resolved: false,
  position: pos(),
  ...overrides,
});
for (const raw of [
  [
    {
      id: "d1",
      notes: [discNote(), discNote({ id: 22, body: "Done", position: null, resolved: true })],
    },
  ],
  [{ id: "d2", notes: [discNote({ resolved: true })] }],
  [{ id: "d3", notes: [discNote({ position: pos({ new_line: null, old_line: 4 }) })] }],
  [{ id: "d4", notes: [discNote({ position: pos({ new_line: null, old_line: null }) })] }],
  [{ id: "d5", notes: [discNote({ position: pos({ new_line: 0 }) })] }],
  [{ id: "d6", notes: [discNote({ position: pos({ position_type: "image" }) })] }],
  [{ id: "d7", notes: [discNote({ position: pos({ new_path: "  " }) })] }],
  [{ id: "d8", notes: [discNote({ system: true }), discNote({ id: 23 })] }],
  [{ id: "d9", notes: [discNote({ system: true })] }],
  [{ id: "d10", notes: [] }, { id: "d11" }, { id: "d12", notes: null }],
  [{ id: 5, notes: [discNote()] }, { bad: true }, { id: "d13", notes: [{ id: "x" }] }],
  [{ id: "d14", notes: [discNote({ position: pos({ new_line: 12.5 }) })] }],
  [],
  {},
  "x",
])
  decoderRow("decodeDiscussionsJson", raw);

const commit = (overrides = {}) => ({
  id: "abc1234def",
  title: "Add feature",
  committed_date: "2026-01-02T00:00:00.000Z",
  created_at: "2026-01-02T00:00:01.000Z",
  parent_ids: ["0000000"],
  author_name: "Alice",
  author_email: "alice@example.com",
  stats: { additions: 3, deletions: 1 },
  ...overrides,
});
for (const raw of [
  [commit(), commit({ id: "bbb2222", title: "Second" })],
  [
    commit({ committed_date: null }),
    commit({ id: "ccc3333", committed_date: null, created_at: null }),
  ],
  [
    commit({ stats: null }),
    commit({ id: "ddd4444", stats: {} }),
    commit({ id: "eee5555", stats: { additions: -4, deletions: 2 } }),
  ],
  [commit({ author_name: null }), commit({ id: "fff6666", author_name: "  ", author_email: null })],
  [commit({ id: "  " }), commit({ id: 5 }), { bad: true }],
  [commit({ title: null })],
  [],
  {},
  "x",
])
  decoderRow("decodeCommitsJson", raw);

for (const raw of [
  commit(),
  commit({ parent_ids: [] }),
  commit({ parent_ids: undefined }),
  commit({ parent_ids: ["  "] }),
  commit({ id: "  " }),
  [],
  "x",
])
  decoderRow("decodeCommitDiffRefsJson", raw);

for (const raw of [
  { diff_refs: { base_sha: "a", head_sha: "b", start_sha: "c" } },
  { diff_refs: null },
  {},
  { diff_refs: { base_sha: "a" } },
  { diff_refs: { base_sha: "a", head_sha: 5, start_sha: "c" } },
  [],
  "x",
])
  decoderRow("decodeDiffRefsJson", raw);

// ---- patches ------------------------------------------------------------------------------------
const diff = (overrides = {}) => ({
  old_path: "src/a.ts",
  new_path: "src/a.ts",
  a_mode: "100644",
  b_mode: "100644",
  new_file: false,
  renamed_file: false,
  deleted_file: false,
  diff: "@@ -1 +1 @@\n-a\n+b\n",
  too_large: false,
  collapsed: false,
  ...overrides,
});
for (const raw of [
  [diff()],
  [diff({ diff: "@@ -1 +1 @@\n-a\n+b" })],
  [diff({ new_file: true, old_path: "src/new.ts", new_path: "src/new.ts", b_mode: "100755" })],
  [diff({ new_file: true, b_mode: null })],
  [
    diff({ deleted_file: true, a_mode: "100755" }),
    diff({ deleted_file: true, a_mode: null, old_path: "x", new_path: "x" }),
  ],
  [diff({ renamed_file: true, old_path: "src/old.ts", new_path: "src/new.ts", diff: "" })],
  [diff({ old_path: 'we "ird".ts', new_path: 'we "ird".ts' })],
  [diff({ old_path: "tab\there", new_path: "tab\there", renamed_file: true })],
  [diff({ diff: "", too_large: true })],
  [diff({ diff: "", collapsed: true })],
  [diff({ diff: null, too_large: null, collapsed: null })],
  [diff({ diff: "", too_large: false })],
  [diff({ diff: "hunks", too_large: true })],
  [diff(), { bad: true }, diff({ old_path: "b.ts", new_path: "b.ts" })],
  [diff({ old_path: undefined })],
  [],
  {},
  "x",
])
  decoderRow("decodeMergeRequestDiffsJson", raw);

// ---- awards ---------------------------------------------------------------------------------------
const award = (name, username) => ({ name, user: { username } });
const awardPage = (overrides = {}) => ({
  data: {
    currentUser: { username: "alice" },
    project: {
      mergeRequest: {
        awardEmoji: {
          nodes: [award("thumbsup", "alice"), award("thumbsup", "bob"), award("tada", "bob")],
        },
        notes: {
          pageInfo: { hasNextPage: false, endCursor: null },
          nodes: [
            {
              id: "gid://gitlab/DiffNote/42",
              awardEmoji: { nodes: [award("heart", "bob"), award("HEART", "carol")] },
            },
            { id: "gid://gitlab/Note/43", awardEmoji: { nodes: [award("unknown_emoji", "bob")] } },
            { id: "gid://gitlab/Note/44", awardEmoji: { nodes: [] } },
            { id: "bad-id", awardEmoji: { nodes: [award("rocket", "bob")] } },
            null,
          ],
        },
      },
    },
    ...overrides,
  },
});
for (const raw of [
  awardPage(),
  awardPage({ currentUser: null }),
  awardPage({ currentUser: undefined }),
  awardPage({ project: null }),
  awardPage({ project: { mergeRequest: null } }),
  awardPage({ project: {} }),
  awardPage({ project: { mergeRequest: { awardEmoji: null, notes: null } } }),
  awardPage({
    project: {
      mergeRequest: {
        awardEmoji: {
          nodes: [null, award(null, "bob"), award("eyes", null), award("laughing", "alice")],
        },
      },
    },
  }),
  awardPage({
    project: {
      mergeRequest: {
        notes: {
          pageInfo: { hasNextPage: true, endCursor: " next " },
          nodes: [
            { id: "gid://gitlab/Note/50", awardEmoji: { nodes: [award("confused", "bob")] } },
          ],
        },
      },
    },
  }),
  awardPage({
    project: {
      mergeRequest: { notes: { pageInfo: { hasNextPage: true, endCursor: null }, nodes: [] } },
    },
  }),
  awardPage({ project: { mergeRequest: { notes: { nodes: [] } } } }),
  awardPage({ project: { mergeRequest: { notes: {} } } }),
  { data: {} },
  {},
  [],
  "x",
])
  decoderRow("decodeAwardEmojiJson", raw);

for (const [raw, content, viewer] of [
  [
    [
      { id: 5, name: "thumbsup", user: { username: "alice" } },
      { id: 6, name: "thumbsup", user: { username: "bob" } },
    ],
    "thumbs-up",
    "alice",
  ],
  [[{ id: 5, name: "thumbsup", user: { username: "bob" } }], "thumbs-up", "alice"],
  [[{ id: 5, name: "THUMBSUP", user: { username: "alice" } }], "thumbs-up", "alice"],
  [[{ id: 7, name: "laughing", user: { username: "alice" } }], "laugh", "alice"],
  [
    [
      { id: "x", name: "thumbsup", user: { username: "alice" } },
      { id: 8, name: "thumbsup", user: { username: "alice" } },
    ],
    "thumbs-up",
    "alice",
  ],
  [[], "heart", "alice"],
  [[{ id: 9, name: "heart", user: null }], "heart", "alice"],
  [{}, "heart", "alice"],
  ["x", "heart", "alice"],
]) {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  rows.push({
    type: "decoder",
    decoder: "decodeOwnAwardIdJson",
    raw: text,
    args: [{ content, viewer }],
    result: decoded(J.decodeOwnAwardIdJson(text, { content, viewer })),
  });
}

const blobs = (nodes, project = true) => ({
  data: { project: project ? { repository: { blobs: { nodes } } } : null },
});
for (const raw of [
  blobs([
    { path: "a.ts", oid: "o1" },
    { path: " b.ts ", oid: " o2 " },
    { path: "c.ts", oid: null },
    { path: "", oid: "o3" },
    null,
    { path: null, oid: "o4" },
  ]),
  blobs([]),
  blobs(null),
  blobs(undefined),
  blobs([], false),
  { data: { project: { repository: null } } },
  { data: { project: { repository: { blobs: null } } } },
  { data: { project: {} } },
  { data: {} },
  {},
  "x",
])
  decoderRow("decodeRepositoryBlobsJson", raw);

// ---- the provider over the CLI over scripted processes ------------------------------------------
const output = (route) => ({
  stdout: route.stdout ?? "",
  stderr: route.stderr ?? "",
  code: route.code ?? 0,
  timedOut: false,
  stdoutTruncated: false,
  stderrTruncated: false,
  stdoutInvalidUtf8: false,
  stderrInvalidUtf8: false,
});
function harness(routes) {
  const calls = [];
  const runner = ProcessRunner.ProcessRunner.of({
    run: (input) => {
      calls.push({ args: input.args, stdin: input.stdin ?? null });
      const joined = input.args.join(" ");
      const route = routes.find(
        (candidate) =>
          joined.includes(candidate.match) &&
          (candidate.stdinIncludes === undefined ||
            (input.stdin ?? "").includes(candidate.stdinIncludes)),
      );
      const fields = { command: input.command, argumentCount: input.args.length, cwd: input.cwd };
      if (route === undefined)
        return Effect.succeed(output({ code: 97, stderr: `no route: ${joined}` }));
      if (route.spawn)
        return Effect.fail(
          new ProcessRunner.ProcessSpawnError({ ...fields, cause: new Error("spawn glab ENOENT") }),
        );
      return Effect.succeed(output(route));
    },
  });
  const process = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  const glab = Layer.effect(GitLabCli.GitLabCli, GitLabCli.make).pipe(Layer.provide(process));
  const cli = Layer.effect(
    GitLabPullRequestCli.GitLabPullRequestCli,
    GitLabPullRequestCli.make,
  ).pipe(Layer.provide(glab));
  return { cli, calls };
}

const json = (value) => JSON.stringify(value);
const REPO = "group/sub/proj";
const MR = `projects/group%2Fsub%2Fproj/merge_requests/7`;
const subject = { repository: REPO, host: "gitlab.com", number: 7 };
const listInput = (overrides = {}) => ({
  repository: REPO,
  host: "gitlab.com",
  state: "open",
  involvement: "all",
  viewer: "alice",
  limit: 99,
  ...overrides,
});
const manyMrs = (count, start = 1) =>
  Array.from({ length: count }, (_, index) =>
    mr({
      iid: start + index,
      title: `MR ${start + index}`,
      updated_at: `2026-01-${String(28 - Math.floor(index / 5)).padStart(2, "0")}T00:00:0${index % 5}Z`,
    }),
  );
const fail = {
  auth: { code: 1, stderr: "glab auth login" },
  rate: { code: 1, stderr: "HTTP 429" },
  boom: { code: 1, stderr: "boom" },
  notFound: { code: 1, stderr: "404 Not Found" },
};
const everything = (extra = []) => [...extra];

const scenarios = [
  // reads
  {
    name: "viewer",
    ordered: false,
    routes: [{ match: "api user", stdout: json({ username: " alice " }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer failures",
    routes: [{ match: "api user", stdout: json({ username: "  " }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unreadable",
    routes: [{ match: "api user", stdout: "nope" }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unauthenticated",
    routes: [{ match: "api user", ...fail.auth }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer missing tool",
    routes: [{ match: "api user", spawn: true }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer rate limited",
    routes: [{ match: "api user", ...fail.rate }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "list first page",
    routes: [{ match: "merge_requests?", stdout: json([mr(), mr({ iid: 8, title: "Second" })]) }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list states and involvement",
    routes: [{ match: "merge_requests?", stdout: "[]" }],
    calls: [
      { op: "list", input: listInput({ state: "closed" }) },
      { op: "list", input: listInput({ state: "merged", involvement: "authored" }) },
      {
        op: "list",
        input: listInput({ state: "all", involvement: "reviewing", viewer: "bob smith" }),
      },
      { op: "list", input: listInput({ query: "  fix & more=1 " }) },
      { op: "list", input: listInput({ query: "   " }) },
    ],
  },
  {
    name: "list empty answer",
    routes: [{ match: "merge_requests?", stdout: "  \n" }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list truncated by the extra row",
    routes: [{ match: "merge_requests?", stdout: json(manyMrs(3)) }],
    calls: [
      { op: "list", input: listInput({ limit: 2 }) },
      { op: "list", input: listInput({ limit: 3 }) },
      { op: "list", input: listInput({ limit: 5 }) },
    ],
  },
  {
    name: "list walks pages",
    routes: [
      { match: "page=2", stdout: json(manyMrs(100, 101)) },
      { match: "page=3", stdout: json(manyMrs(10, 201)) },
      { match: "page=1", stdout: json(manyMrs(100, 1)) },
    ],
    calls: [
      { op: "list", input: listInput({ limit: 150 }) },
      { op: "list", input: listInput({ limit: 250 }) },
    ],
  },
  {
    name: "list continues from a cursor",
    routes: [
      { match: "page=2", stdout: json(manyMrs(100, 101)) },
      { match: "page=1", stdout: json(manyMrs(100, 1)) },
    ],
    calls: [
      {
        op: "list",
        input: listInput({
          limit: 5,
          cursor: { updatedBefore: "2026-01-02T00:00:00Z", delivered: 5 },
        }),
      },
      {
        op: "list",
        input: listInput({
          limit: 99,
          cursor: { updatedBefore: "2026-01-02T00:00:00Z", delivered: 120 },
        }),
      },
      {
        op: "list",
        input: listInput({
          limit: 150,
          cursor: { updatedBefore: "2026-01-02T00:00:00Z", delivered: 100 },
        }),
      },
    ],
  },
  {
    name: "list skips malformed rows",
    routes: [
      { match: "merge_requests?", stdout: json([mr(), { bad: true }, mr({ iid: 9 }), null]) },
    ],
    calls: [
      { op: "list", input: listInput({ limit: 2 }) },
      { op: "list", input: listInput({ limit: 10 }) },
    ],
  },
  {
    name: "list a full page of malformed rows ends the walk",
    routes: [
      { match: "merge_requests?", stdout: json(Array.from({ length: 4 }, () => ({ bad: true }))) },
    ],
    calls: [{ op: "list", input: listInput({ limit: 3 }) }],
  },
  {
    name: "list failures",
    routes: [{ match: "merge_requests?", stdout: "not json" }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list unauthenticated",
    routes: [{ match: "merge_requests?", ...fail.auth }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list not found",
    routes: [{ match: "merge_requests?", ...fail.notFound }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "detail",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr()) },
      {
        match: "proj?license=false",
        stdout: json({ merge_method: "rebase_merge", squash_option: "default_off" }),
      },
    ],
    calls: [
      { op: "detail", input: subject },
      { op: "checks", input: subject },
      { op: "permissions", input: subject },
    ],
  },
  {
    name: "detail variants",
    ordered: false,
    routes: [
      {
        match: `${MR}?include_diverged`,
        stdout: json(
          mr({
            user: { can_merge: false },
            diverged_commits_count: 0,
            merge_when_pipeline_succeeds: true,
          }),
        ),
      },
      {
        match: "proj?license=false",
        stdout: json({ merge_method: "ff", squash_option: "always" }),
      },
    ],
    calls: [
      { op: "detail", input: subject },
      { op: "permissions", input: subject },
    ],
  },
  {
    name: "detail without a divergence count",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr({ diverged_commits_count: undefined })) },
      { match: "proj?license=false", stdout: "{}" },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail failures",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: "nope" },
      { match: "proj?license=false", stdout: "{}" },
    ],
    calls: [
      { op: "detail", input: subject },
      { op: "checks", input: subject },
      { op: "permissions", input: subject },
    ],
  },
  {
    name: "detail project settings unreadable",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr()) },
      { match: "proj?license=false", stdout: "nope" },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail not found",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, ...fail.notFound },
      { match: "proj?license=false", stdout: "{}" },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "activity",
    ordered: false,
    routes: [
      {
        match: `${MR}/notes?`,
        stdout: json([
          note({ id: 11 }),
          note({ id: 12, type: "DiffNote", position: { new_path: "src/a.ts" } }),
          note({ id: 13, system: true }),
        ]),
      },
      {
        match: `${MR}/commits?`,
        stdout: json([commit(), commit({ id: "bbb2222", title: "Second" })]),
      },
      {
        match: `${MR}/discussions?`,
        stdout: json([
          { id: "d1", notes: [discNote({ id: 12 }), discNote({ id: 31, body: "Reply" })] },
          { id: "d2", notes: [note({ id: 11 })] },
        ]),
      },
      { match: "api graphql", stdout: json(awardPage()) },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity degrades each read on its own",
    ordered: false,
    routes: [
      { match: `${MR}/notes?`, ...fail.boom },
      { match: `${MR}/commits?`, stdout: "nope" },
      { match: `${MR}/discussions?`, stdout: json([{ id: "d1", notes: [discNote()] }]) },
      { match: "api graphql", ...fail.auth },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity with every read failing",
    ordered: false,
    routes: [{ match: "api", spawn: true }],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity walks note pages",
    ordered: false,
    routes: [
      { match: `${MR}/notes?per_page=100&page=2`, stdout: json([note({ id: 301 })]) },
      {
        match: `${MR}/notes?`,
        stdout: json(Array.from({ length: 100 }, (_, index) => note({ id: 100 + index }))),
      },
      { match: `${MR}/commits?`, stdout: "[]" },
      { match: `${MR}/discussions?`, stdout: "[]" },
      {
        match: "api graphql",
        stdout: json(
          awardPage({
            project: {
              mergeRequest: {
                awardEmoji: { nodes: [] },
                notes: { pageInfo: { hasNextPage: false }, nodes: [] },
              },
            },
          }),
        ),
      },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity pages awards",
    ordered: false,
    routes: [
      {
        match: "api graphql",
        stdinIncludes: '"cursor":"c1"',
        stdout: json(
          awardPage({
            project: {
              mergeRequest: {
                awardEmoji: { nodes: [award("rocket", "zed")] },
                notes: {
                  pageInfo: { hasNextPage: false },
                  nodes: [
                    { id: "gid://gitlab/Note/12", awardEmoji: { nodes: [award("eyes", "bob")] } },
                  ],
                },
              },
            },
          }),
        ),
      },
      {
        match: "api graphql",
        stdout: json(
          awardPage({
            project: {
              mergeRequest: {
                awardEmoji: { nodes: [award("heart", "bob")] },
                notes: {
                  pageInfo: { hasNextPage: true, endCursor: "c1" },
                  nodes: [
                    { id: "gid://gitlab/Note/11", awardEmoji: { nodes: [award("tada", "bob")] } },
                  ],
                },
              },
            },
          }),
        ),
      },
      { match: `${MR}/notes?`, stdout: json([note({ id: 11 }), note({ id: 12 })]) },
      { match: `${MR}/commits?`, stdout: "[]" },
      { match: `${MR}/discussions?`, stdout: "[]" },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  // diffs
  {
    name: "diff pages",
    routes: [
      {
        match: `${MR}/diffs?per_page=100&page=2`,
        stdout: json([diff({ old_path: "b.ts", new_path: "b.ts" })]),
      },
      {
        match: `${MR}/diffs?`,
        stdout: json(
          Array.from({ length: 100 }, (_, index) =>
            diff({
              old_path: `f${index}.ts`,
              new_path: `f${index}.ts`,
              diff: index === 3 ? "" : undefined,
              too_large: index === 3,
            }),
          ),
        ),
      },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "2" } },
    ],
  },
  {
    name: "diff of a commit",
    routes: [{ match: "repository/commits/", stdout: json([diff()]) }],
    calls: [
      { op: "diff", input: { ...subject, commit: "abc1234" } },
      { op: "diff", input: { ...subject, commit: "ABCDEF0123456789", cursor: "3" } },
    ],
  },
  {
    name: "diff refuses what the walk never handed out",
    routes: [{ match: "diffs", stdout: "[]" }],
    calls: [
      { op: "diff", input: { ...subject, cursor: "0" } },
      { op: "diff", input: { ...subject, cursor: "abc" } },
      { op: "diff", input: { ...subject, cursor: "12345678" } },
      { op: "diff", input: { ...subject, cursor: "1; rm" } },
      { op: "diff", input: { ...subject, commit: "xyz" } },
      { op: "diff", input: { ...subject, commit: "abc12" } },
      { op: "diff", input: { ...subject, commit: "../../x1234567" } },
    ],
  },
  {
    name: "diff unreadable",
    routes: [{ match: "diffs", stdout: "nope" }],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff failure",
    routes: [{ match: "diffs", ...fail.boom }],
    calls: [{ op: "diff", input: subject }],
  },
  // file revisions
  {
    name: "file revisions",
    ordered: false,
    routes: [
      {
        match: `${MR}`,
        stdout: json({ diff_refs: { base_sha: "b0", head_sha: "h1", start_sha: "s2" } }),
      },
      { match: "api graphql", stdout: json(blobs([{ path: "a.ts", oid: "o1" }])) },
    ],
    calls: [
      { op: "revisions", input: { ...subject, paths: ["a.ts", "gone.ts"] } },
      { op: "revisions", input: { ...subject, paths: [] } },
    ],
  },
  {
    name: "file revisions in batches",
    ordered: false,
    routes: [
      {
        match: "merge_requests/7",
        stdout: json({ diff_refs: { base_sha: "b0", head_sha: "h1", start_sha: "s2" } }),
      },
      { match: "api graphql", stdout: json(blobs([{ path: "f0.ts", oid: "o0" }])) },
    ],
    calls: [
      {
        op: "revisions",
        input: { ...subject, paths: Array.from({ length: 250 }, (_, index) => `f${index}.ts`) },
      },
    ],
  },
  {
    name: "file revisions a batch GitLab did not answer",
    ordered: false,
    routes: [
      {
        match: "merge_requests/7",
        stdout: json({ diff_refs: { base_sha: "b0", head_sha: "h1", start_sha: "s2" } }),
      },
      { match: "api graphql", stdout: json(blobs([], false)) },
    ],
    calls: [{ op: "revisions", input: { ...subject, paths: ["a.ts"] } }],
  },
  {
    name: "file revisions without diff refs",
    ordered: false,
    routes: [{ match: "merge_requests/7", stdout: json({ diff_refs: null }) }],
    calls: [{ op: "revisions", input: { ...subject, paths: ["a.ts"] } }],
  },
  // reviewers
  {
    name: "reviewer candidates",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr()) },
      {
        match: "proj/users?",
        stdout: json([
          user({ id: 1, username: "alice" }),
          user({ id: 2, username: "bob" }),
          user({ id: 3, username: "carol", name: null }),
        ]),
      },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "reviewer candidates truncated",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr({ author: null, reviewers: [] })) },
      {
        match: "proj/users?",
        stdout: json(
          Array.from({ length: 100 }, (_, index) =>
            user({ id: index + 10, username: `u${index}` }),
          ),
        ),
      },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "reviewer candidates failure",
    ordered: false,
    routes: [
      { match: `${MR}?include_diverged`, stdout: json(mr()) },
      { match: "proj/users?", stdout: "nope" },
    ],
    calls: [{ op: "candidates", input: subject }],
  },
  {
    name: "request and withdraw reviewers",
    routes: [
      {
        match: `${MR}?include_diverged`,
        stdout: json(
          mr({ reviewers: [user({ id: 2, username: "bob" }), user({ id: 5, username: "eve" })] }),
        ),
      },
      { match: "--method PUT", stdout: "{}" },
    ],
    calls: [
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "9", kind: "user" }], requested: true },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "2", kind: "user" }], requested: false },
      },
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "2", kind: "user" },
            { id: "x", kind: "user" },
            { id: "0", kind: "user" },
            { id: "-3", kind: "user" },
            { id: "1.5", kind: "user" },
            { id: "0x10", kind: "user" },
            { id: " 12 ", kind: "user" },
          ],
          requested: true,
        },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "77", kind: "user" }], requested: false },
      },
    ],
  },
  {
    name: "request reviewers failures",
    routes: [{ match: `${MR}?include_diverged`, ...fail.auth }],
    calls: [
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "9", kind: "user" }], requested: true },
      },
    ],
  },
  // actions
  {
    name: "actions",
    routes: [
      { match: "mr ", stdout: "" },
      { match: "cancel_merge_when_pipeline_succeeds", stdout: "{}" },
    ],
    calls: [
      { op: "action", input: { ...subject, action: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "rebase" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "merge" } },
      { op: "action", input: { ...subject, action: "enable-auto-merge" } },
      { op: "action", input: { ...subject, action: "enable-auto-merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "disable-auto-merge" } },
      { op: "action", input: { ...subject, action: "ready" } },
      { op: "action", input: { ...subject, action: "draft" } },
      { op: "action", input: { ...subject, action: "close" } },
      { op: "action", input: { ...subject, action: "reopen" } },
      { op: "action", input: { ...subject, action: "update-branch", updateMethod: "rebase" } },
    ],
  },
  {
    name: "action failures",
    routes: [
      { match: "mr merge", ...fail.auth },
      { match: "mr close", ...fail.rate },
      { match: "mr reopen", ...fail.boom },
      { match: "cancel_merge", ...fail.notFound },
    ],
    calls: [
      { op: "action", input: { ...subject, action: "merge" } },
      { op: "action", input: { ...subject, action: "close" } },
      { op: "action", input: { ...subject, action: "reopen" } },
      { op: "action", input: { ...subject, action: "disable-auto-merge" } },
    ],
  },
  // writes
  {
    name: "update the title and description",
    routes: [{ match: "--method PUT", stdout: "{}" }],
    calls: [
      { op: "update", input: { ...subject, title: "New title" } },
      { op: "update", input: { ...subject, body: "" } },
      { op: "update", input: { ...subject, title: "T", body: 'Line "one"\nLine two' } },
    ],
  },
  {
    name: "comment and edit a note",
    routes: [{ match: "--method", stdout: "{}" }],
    calls: [
      { op: "comment", input: { ...subject, body: "true" } },
      { op: "comment", input: { ...subject, body: 'multi\nline `code` "quoted"' } },
      {
        op: "updateComment",
        input: { ...subject, commentId: "55", kind: "issue-comment", body: "edited" },
      },
      {
        op: "updateComment",
        input: { ...subject, commentId: "a b/c", kind: "review-comment", body: "edited" },
      },
    ],
  },
  {
    name: "write failures",
    routes: [
      { match: "/notes", ...fail.boom },
      { match: "--method PUT", ...fail.auth },
    ],
    calls: [
      { op: "comment", input: { ...subject, body: "x" } },
      { op: "update", input: { ...subject, title: "x" } },
      {
        op: "updateComment",
        input: { ...subject, commentId: "1", kind: "issue-comment", body: "x" },
      },
    ],
  },
  {
    name: "reply and resolve",
    routes: [{ match: "--method", stdout: "{}" }],
    calls: [
      { op: "reply", input: { ...subject, threadId: "abc123", body: "Thanks" } },
      { op: "reply", input: { ...subject, threadId: "a/b c", body: "Thanks" } },
      { op: "resolution", input: { ...subject, threadId: "abc123", resolved: true } },
      { op: "resolution", input: { ...subject, threadId: "abc123", resolved: false } },
    ],
  },
  {
    name: "submit a review with line comments",
    routes: [
      { match: "--method POST", stdout: "{}" },
      {
        match: "merge_requests/7",
        stdout: json({ diff_refs: { base_sha: "b0", head_sha: "h1", start_sha: "s2" } }),
      },
    ],
    calls: [
      {
        op: "review",
        input: {
          ...subject,
          verdict: "approve",
          body: "Looks good",
          comments: [
            { path: "src/a.ts", position: { kind: "added", newLine: 12 }, body: "Nice" },
            {
              path: "src/b.ts",
              oldPath: "src/old.ts",
              position: { kind: "deleted", oldLine: 4 },
              body: "Why?",
            },
            {
              path: "src/c.ts",
              position: { kind: "context", oldLine: 7, newLine: 9, side: "right" },
              body: "Hmm",
            },
          ],
        },
      },
      { op: "review", input: { ...subject, verdict: "comment", body: "  ", comments: [] } },
      {
        op: "review",
        input: { ...subject, verdict: "comment", body: "Summary only", comments: [] },
      },
      { op: "review", input: { ...subject, verdict: "approve", body: "", comments: [] } },
    ],
  },
  {
    name: "submit a review fails part-way",
    routes: [
      { match: "/discussions", ...fail.boom },
      {
        match: "merge_requests/7",
        stdout: json({ diff_refs: { base_sha: "b0", head_sha: "h1", start_sha: "s2" } }),
      },
    ],
    calls: [
      {
        op: "review",
        input: {
          ...subject,
          verdict: "approve",
          body: "ok",
          comments: [{ path: "a.ts", position: { kind: "added", newLine: 1 }, body: "x" }],
        },
      },
    ],
  },
  {
    name: "submit a review without diff refs",
    routes: [{ match: "merge_requests/7", stdout: json({ diff_refs: null }) }],
    calls: [
      {
        op: "review",
        input: {
          ...subject,
          verdict: "approve",
          body: "ok",
          comments: [{ path: "a.ts", position: { kind: "added", newLine: 1 }, body: "x" }],
        },
      },
    ],
  },
  {
    name: "reactions",
    routes: [
      { match: "award_emoji?name=", stdout: "{}" },
      { match: "api user", stdout: json({ username: "alice" }) },
      { match: "award_emoji/", stdout: "" },
      {
        match: "award_emoji",
        stdout: json([
          { id: 5, name: "thumbsup", user: { username: "alice" } },
          { id: 6, name: "tada", user: { username: "bob" } },
        ]),
      },
    ],
    calls: [
      { op: "reaction", input: { ...subject, content: "thumbs-up", reacted: true } },
      { op: "reaction", input: { ...subject, subjectId: "44", content: "hooray", reacted: true } },
      { op: "reaction", input: { ...subject, content: "thumbs-up", reacted: false } },
      {
        op: "reaction",
        input: { ...subject, subjectId: "44", content: "thumbs-up", reacted: false },
      },
      { op: "reaction", input: { ...subject, content: "rocket", reacted: false } },
    ],
  },
  {
    name: "reactions fail",
    routes: [
      { match: "award_emoji?name=", ...fail.boom },
      { match: "api user", ...fail.auth },
    ],
    calls: [
      { op: "reaction", input: { ...subject, content: "heart", reacted: true } },
      { op: "reaction", input: { ...subject, content: "heart", reacted: false } },
    ],
  },
];

const unusedHelpers = everything;
void unusedHelpers;

const callProvider = (provider, call) => {
  const base = { cwd: "/repo" };
  const input = { ...base, ...call.input };
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

// Keys the original's detail carries that the neutral shape does not.
const NEUTRAL_EXTRAS = ["viewerCanMerge", "reviewerIds", "divergedCommits"];
const neutral = (value) => {
  const copy = norm(value);
  if (copy && typeof copy === "object" && !Array.isArray(copy) && "viewerCanMerge" in copy) {
    for (const key of NEUTRAL_EXTRAS) delete copy[key];
  }
  return copy;
};

for (const scenario of scenarios) {
  const { cli, calls } = harness(scenario.routes);
  const results = [];
  const callLog = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* GitLabPullRequestProvider.make;
      for (const call of scenario.calls) {
        const before = calls.length;
        const exit = yield* Effect.exit(callProvider(provider, call));
        results.push(
          exit._tag === "Success"
            ? { ok: neutral(exit.value ?? null) }
            : (() => {
                const error = exit.cause.reasons[0].error;
                return { error: { reason: error.reason, detail: error.detail } };
              })(),
        );
        callLog.push(calls.slice(before));
      }
    }).pipe(Effect.provide(cli)),
  );
  rows.push({
    type: "op",
    name: scenario.name,
    ordered: scenario.ordered !== false,
    routes: scenario.routes.map(({ match, stdinIncludes, stdout, stderr, code, spawn }) => ({
      match,
      stdinIncludes,
      stdout,
      stderr,
      code,
      spawn,
    })),
    calls: scenario.calls,
    results,
    processes: callLog,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/pull-request-gitlab.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(`wrote ${rows.length} rows to ${target.pathname}`);
