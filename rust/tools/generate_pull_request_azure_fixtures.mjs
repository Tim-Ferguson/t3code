// Runs the original Azure DevOps pull request layer and records what it produces, for
// crates/server/tests/pull_request_azure.rs to replay:
//  - every decoder of azureDevOpsPullRequestJson.ts over payloads that exercise each branch;
//  - the diff builder of azureDevOpsDiff.ts (file patches and the cursor) over inputs that cross
//    each of its ceilings;
//  - the capabilities and the viewer permissions the provider declares;
//  - AzureDevOpsPullRequestProvider over AzureDevOpsPullRequestCli over AzureDevOpsCli over the
//    real VcsProcess policy with a scripted process runner. A scenario names the answer for each
//    request by a fragment of its arguments, so the order requests are made in (several run
//    together) does not matter; what is recorded is every `az` argument list, and the provider's
//    result or failure for each call.
// No network is used and no process is spawned.
import { createHash } from "node:crypto";
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
import * as PlatformError from "../../apps/server/node_modules/effect/dist/PlatformError.js";
import * as AzureDevOpsCli from "../../apps/server/src/sourceControl/AzureDevOpsCli.ts";
import * as J from "../../apps/server/src/pullRequest/azureDevOpsPullRequestJson.ts";
import * as D from "../../apps/server/src/pullRequest/azureDevOpsDiff.ts";
import * as AzureCli from "../../apps/server/src/pullRequest/AzureDevOpsPullRequestCli.ts";
import * as AzureProvider from "../../apps/server/src/pullRequest/AzureDevOpsPullRequestProvider.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";

const rows = [];
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );
const json = (value) => JSON.stringify(value);

// ---- payloads ----------------------------------------------------------------------------------
const who = (name, unique, image) => ({
  displayName: name,
  uniqueName: unique,
  imageUrl: image,
});
const pr = (overrides = {}) => ({
  pullRequestId: 7,
  title: "Pull request 7",
  description: "Body text",
  status: "active",
  isDraft: false,
  mergeStatus: "succeeded",
  createdBy: who("Alice", "alice@acme.io", "https://img.example/alice.png"),
  reviewers: [who("Bob", "bob@acme.io", null)],
  sourceRefName: "refs/heads/feature",
  targetRefName: "refs/heads/main",
  creationDate: "2026-01-01T00:00:00Z",
  url: "https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/7",
  repository: {
    name: "web",
    webUrl: "https://dev.azure.com/acme/shop/_git/web",
    project: { name: "shop" },
  },
  _links: { web: { href: "https://dev.azure.com/acme/shop/_git/web/pullrequest/7" } },
  ...overrides,
});
const decoded = (result) =>
  Result.isSuccess(result) ? { ok: norm(result.success) } : { error: true };
const decoderRow = (decoder, raw) => {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  rows.push({ type: "decoder", decoder, raw: text, result: decoded(J[decoder](text)) });
};

// ---- pull requests -----------------------------------------------------------------------------
const prCases = [
  pr(),
  pr({ status: "completed", closedDate: "2026-01-05T00:00:00Z" }),
  pr({ status: "abandoned", closedDate: "2026-01-06T00:00:00Z" }),
  pr({ status: " Completed " }),
  pr({ status: "weird" }),
  pr({ status: null }),
  pr({ status: undefined }),
  pr({ closedDate: "  " }),
  pr({ closedDate: null }),
  pr({ isDraft: true }),
  pr({ isDraft: null }),
  pr({ isDraft: "yes" }),
  pr({ mergeStatus: "conflicts" }),
  pr({ mergeStatus: " Failure " }),
  pr({ mergeStatus: "rejectedByPolicy" }),
  pr({ mergeStatus: "queued" }),
  pr({ mergeStatus: "notSet" }),
  pr({ mergeStatus: null }),
  pr({ mergeStatus: 3 }),
  pr({ autoCompleteSetBy: who("Alice", "alice@acme.io") }),
  pr({
    autoCompleteSetBy: who("Alice", "alice@acme.io"),
    completionOptions: { mergeStrategy: "squash" },
  }),
  pr({ autoCompleteSetBy: who("A", "a@a"), completionOptions: { mergeStrategy: " Rebase " } }),
  pr({ autoCompleteSetBy: who("A", "a@a"), completionOptions: { mergeStrategy: "rebaseMerge" } }),
  pr({ autoCompleteSetBy: who("A", "a@a"), completionOptions: { mergeStrategy: "noFastForward" } }),
  pr({
    autoCompleteSetBy: who("A", "a@a"),
    completionOptions: { mergeStrategy: "other", squashMerge: true },
  }),
  pr({ autoCompleteSetBy: who("A", "a@a"), completionOptions: { squashMerge: false } }),
  pr({ autoCompleteSetBy: who("A", "a@a"), completionOptions: null }),
  pr({ autoCompleteSetBy: null, completionOptions: { mergeStrategy: "squash" } }),
  pr({ autoCompleteSetBy: "someone" }),
  pr({ completionOptions: { mergeStrategy: 4 } }),
  pr({ createdBy: null }),
  pr({ createdBy: who(null, null, null) }),
  pr({ createdBy: who("  Only Display  ", "  ", " https://i.example/x.png ") }),
  pr({ createdBy: who("Unique", " u@x.io ", undefined) }),
  pr({ createdBy: "alice" }),
  pr({ reviewers: null }),
  pr({ reviewers: [] }),
  pr({ reviewers: [who("Bob", "bob@acme.io"), who(null, null), who("Cy", null)] }),
  pr({ reviewers: [who("Bob", "bob@acme.io"), "x"] }),
  pr({ reviewers: "x" }),
  pr({ description: null }),
  pr({ description: undefined }),
  pr({ description: 4 }),
  pr({ title: "" }),
  pr({ title: 4 }),
  pr({ pullRequestId: 1.5 }),
  pr({ pullRequestId: "7" }),
  pr({ pullRequestId: undefined }),
  pr({ sourceRefName: "  " }),
  pr({ sourceRefName: " refs/heads/ " }),
  pr({ sourceRefName: "refs/heads/" }),
  pr({ sourceRefName: " refs/heads/topic/x " }),
  pr({ sourceRefName: "refs/tags/v1" }),
  pr({ targetRefName: "" }),
  pr({ targetRefName: undefined }),
  pr({ creationDate: "" }),
  pr({ creationDate: " 2026-02-02T00:00:00Z " }),
  pr({ creationDate: undefined }),
  pr({ _links: undefined }),
  pr({ _links: null }),
  pr({ _links: { web: null } }),
  pr({ _links: { web: {} } }),
  pr({ _links: { web: { href: " https://web.example/pr/7 " } } }),
  pr({ _links: { web: { href: "  " } } }),
  pr({ _links: { web: { href: null } } }),
  pr({ _links: { web: "x" } }),
  pr({
    _links: undefined,
    repository: { name: "web", webUrl: "https://dev.azure.com/acme/shop/_git/web///" },
  }),
  pr({ _links: undefined, repository: { name: "web", webUrl: "  ", project: { name: "shop" } } }),
  pr({ _links: undefined, repository: { name: "re po", project: { name: "my proj" } } }),
  pr({
    _links: undefined,
    repository: { name: "re po", project: { name: "my proj" } },
    url: "https://acme.visualstudio.com/_apis/git/repositories/x/pullRequests/7",
  }),
  pr({
    _links: undefined,
    repository: { name: "web", project: { name: "shop" } },
    url: "https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/7",
  }),
  pr({
    _links: undefined,
    repository: { name: "web", project: { name: "shop" } },
    url: "https://tfs.corp.example/tfs/_apis/git/repositories/web/pullRequests/7",
  }),
  pr({
    _links: undefined,
    repository: undefined,
    url: "https://dev.azure.com/acme/_apis/git/pr/7",
  }),
  pr({ _links: undefined, repository: undefined, url: undefined }),
  pr({ _links: undefined, repository: undefined, url: null }),
  pr({ repository: null }),
  pr({ repository: { name: "web", project: null } }),
  pr({ repository: { name: "web" } }),
  pr({ repository: { project: { name: "shop" } } }),
  pr({ repository: { name: "  ", project: { name: "shop" } } }),
  pr({ repository: { name: " web ", project: { name: " shop " } } }),
  pr({ repository: "web" }),
  pr({ repository: { name: 4 } }),
  pr({ url: 4 }),
];
for (const value of prCases) decoderRow("decodePullRequestJson", value);
decoderRow("decodePullRequestJson", "not json");
decoderRow("decodePullRequestJson", "[]");
decoderRow("decodePullRequestJson", "null");
decoderRow("decodePullRequestListJson", [pr(), pr({ pullRequestId: 8 })]);
decoderRow("decodePullRequestListJson", [
  { bad: 1 },
  pr(),
  null,
  "x",
  pr({ title: "" }),
  pr({ pullRequestId: 9 }),
]);
decoderRow("decodePullRequestListJson", []);
decoderRow("decodePullRequestListJson", {});
decoderRow("decodePullRequestListJson", "nope");
decoderRow("decodePullRequestListJson", prCases);

// ---- the signed-in account -----------------------------------------------------------------------
for (const value of [
  { user: { name: " alice@acme.io " } },
  { user: { name: "  " } },
  { user: { name: null } },
  { user: { name: 4 } },
  { user: null },
  { user: {} },
  { user: "x" },
  {},
  [],
  { user: { name: "a@b.c", other: 1 } },
]) {
  decoderRow("decodeViewerJson", value);
}
decoderRow("decodeViewerJson", '{"user":null}');
decoderRow("decodeViewerJson", "nope");

// ---- the conversation ------------------------------------------------------------------------------
const comment = (overrides = {}) => ({
  id: 1,
  content: "Looks fine",
  author: who("Bob", "bob@acme.io", "https://img.example/bob.png"),
  publishedDate: "2026-01-02T00:00:00Z",
  ...overrides,
});
const thread = (overrides = {}) => ({ id: 10, comments: [comment()], ...overrides });
for (const value of [
  { value: [thread()] },
  { value: [] },
  {
    value: [
      thread({
        comments: [comment({ id: 2, publishedDate: "2026-01-03T00:00:00Z" }), comment({ id: 1 })],
      }),
    ],
  },
  {
    value: [
      thread({ id: 11, comments: [comment({ publishedDate: "2026-01-05T00:00:00Z" })] }),
      thread(),
    ],
  },
  { value: [thread({ threadContext: { filePath: "/src/a.ts" } })] },
  { value: [thread({ threadContext: { filePath: "  " } })] },
  { value: [thread({ threadContext: null })] },
  { value: [thread({ threadContext: { filePath: null } })] },
  { value: [thread({ threadContext: "x" })] },
  { value: [thread({ isDeleted: true })] },
  { value: [thread({ isDeleted: false })] },
  { value: [thread({ comments: [comment({ isDeleted: true }), comment({ id: 2 })] })] },
  {
    value: [
      thread({
        comments: [comment({ commentType: "system" }), comment({ id: 2, commentType: " System " })],
      }),
    ],
  },
  {
    value: [
      thread({
        comments: [comment({ commentType: "text" }), comment({ id: 2, commentType: null })],
      }),
    ],
  },
  {
    value: [
      thread({
        comments: [
          comment({ content: "   " }),
          comment({ id: 2, content: null }),
          comment({ id: 3, content: undefined }),
        ],
      }),
    ],
  },
  {
    value: [
      thread({
        comments: [
          comment({ publishedDate: null }),
          comment({ id: 2, publishedDate: "  " }),
          comment({ id: 3, publishedDate: undefined }),
        ],
      }),
    ],
  },
  { value: [thread({ comments: [comment({ id: null }), comment({ id: undefined })] })] },
  { value: [thread({ comments: [comment({ id: 1.5 })] })] },
  { value: [thread({ comments: [comment({ author: null })] })] },
  { value: [thread({ comments: [comment({ author: who(null, null, null) })] })] },
  { value: [thread({ comments: null })] },
  { value: [thread({ comments: [] })] },
  { value: [thread({ comments: "x" })] },
  { value: [thread({ id: "10" }), thread({ id: 12 })] },
  { value: [thread({ id: 1.5 }), thread({ id: 13 })] },
  { value: [null, "x", 4, thread()] },
  { value: [thread({ comments: [comment({ content: "  padded  " })] })] },
  {
    value: [
      thread({
        id: 1,
        comments: [
          comment({ id: 1, publishedDate: "2026-01-02T00:00:00.5Z" }),
          comment({ id: 2, publishedDate: "2026-01-02T00:00:00Z" }),
          comment({ id: 3, publishedDate: "2026-01-02T00:00:00.25Z" }),
          comment({ id: 4, publishedDate: "2026-01-01T23:59:59Z" }),
          comment({ id: 5, publishedDate: "2026-01-02T00:00:00Z" }),
        ],
      }),
    ],
  },
  { count: 0 },
  { value: "x" },
  [],
]) {
  decoderRow("decodeThreadsJson", value);
}
decoderRow("decodeThreadsJson", "nope");

// ---- pushes and their changes ---------------------------------------------------------------------------
const iteration = (id, head, base) => ({
  id,
  sourceRefCommit: head === undefined ? undefined : { commitId: head },
  commonRefCommit: base === undefined ? undefined : { commitId: base },
});
for (const value of [
  { value: [iteration(1, "h1", "b1"), iteration(2, "h2", "b2")] },
  { value: [iteration(3, "h3", "b3"), iteration(1, "h1", "b1"), iteration(2, "h2", "b2")] },
  {
    value: [iteration(1, "h1", "b1"), iteration(2, undefined, "b2"), iteration(3, "h3", undefined)],
  },
  { value: [iteration(1, " h1 ", " b1 "), iteration(2, "  ", "b2"), iteration(3, "h3", "")] },
  { value: [{ id: 1, sourceRefCommit: null, commonRefCommit: { commitId: "b" } }] },
  { value: [{ id: 1, sourceRefCommit: { commitId: null }, commonRefCommit: { commitId: "b" } }] },
  { value: [{ id: 1, sourceRefCommit: "x", commonRefCommit: { commitId: "b" } }] },
  { value: [{ id: "1", sourceRefCommit: { commitId: "h" }, commonRefCommit: { commitId: "b" } }] },
  { value: [{ id: 1.5, sourceRefCommit: { commitId: "h" }, commonRefCommit: { commitId: "b" } }] },
  { value: [null, "x", iteration(2, "h2", "b2")] },
  { value: [iteration(2, "h2", "b2"), iteration(2, "h2b", "b2b")] },
  { value: [] },
  { value: "x" },
  {},
  [],
]) {
  decoderRow("decodeIterationsJson", value);
}
decoderRow("decodeIterationsJson", "nope");

const entry = (overrides = {}, item = {}) => ({
  changeType: "edit",
  item: {
    path: "/src/a.ts",
    objectId: "o1",
    originalObjectId: "p1",
    gitObjectType: "blob",
    ...item,
  },
  ...overrides,
});
for (const value of [
  { changeEntries: [entry()] },
  {
    changeEntries: [
      entry({ changeType: "add" }),
      entry({ changeType: "delete" }, { path: "/b.ts" }),
    ],
  },
  {
    changeEntries: [
      entry({ changeType: "Add, Edit" }),
      entry({ changeType: "edit, delete" }, { path: "/c.ts" }),
    ],
  },
  { changeEntries: [entry({ changeType: "rename", sourceServerItem: "/old/a.ts" })] },
  { changeEntries: [entry({ changeType: "rename, edit", sourceServerItem: "/old/a.ts" })] },
  { changeEntries: [entry({ changeType: "edit", sourceServerItem: "/old/a.ts" })] },
  { changeEntries: [entry({ changeType: "rename", originalPath: "/orig/a.ts" })] },
  {
    changeEntries: [
      entry({ changeType: "rename", sourceServerItem: "/src/a.ts", originalPath: "/orig/a.ts" }),
    ],
  },
  {
    changeEntries: [
      entry({ changeType: "rename", sourceServerItem: "  ", originalPath: "/orig/a.ts" }),
    ],
  },
  { changeEntries: [entry({ changeType: "rename" })] },
  {
    changeEntries: [
      entry({ changeType: null }),
      entry({ changeType: "undelete" }, { path: "/d.ts" }),
    ],
  },
  { changeEntries: [entry({ changeType: undefined })] },
  { changeEntries: [entry({ changeType: 3 })] },
  {
    changeEntries: [
      entry({}, { isFolder: true }),
      entry({}, { path: "/dir", gitObjectType: "tree" }),
    ],
  },
  {
    changeEntries: [
      entry({}, { isFolder: false }),
      entry({}, { path: "/e.ts", gitObjectType: "Blob" }),
    ],
  },
  {
    changeEntries: [
      entry({}, { gitObjectType: undefined }),
      entry({}, { path: "/f.ts", gitObjectType: null }),
    ],
  },
  { changeEntries: [entry({}, { path: "/ spaced.ts " })] },
  { changeEntries: [entry({}, { path: "//double/slash.ts" })] },
  {
    changeEntries: [
      entry({}, { path: "/" }),
      entry({}, { path: "" }),
      entry({}, { path: null }),
      entry({}, { path: undefined }),
    ],
  },
  {
    changeEntries: [
      entry({}, { objectId: "  " }),
      entry({}, { path: "/g.ts", objectId: null, originalObjectId: " p " }),
    ],
  },
  { changeEntries: [entry({ item: null })] },
  { changeEntries: [entry({ item: "x" })] },
  { changeEntries: [{}] },
  { changeEntries: [null, "x", 4, entry()] },
  { changeEntries: [entry()], nextSkip: 2000 },
  { changeEntries: [entry()], nextSkip: 0 },
  { changeEntries: [entry()], nextSkip: -5 },
  { changeEntries: [entry()], nextSkip: 1.5 },
  { changeEntries: [entry()], nextSkip: null },
  { changeEntries: [entry()], nextSkip: "2000" },
  { changeEntries: [entry()], nextSkip: 9007199254740993 },
  { changeEntries: [], nextSkip: 7 },
  { changeEntries: "x" },
  { changeEntries: [entry()], continuationToken: ["abc"], count: 1 },
  {},
  [],
]) {
  decoderRow("decodeIterationChangesJson", value);
}
decoderRow("decodeIterationChangesJson", "nope");

for (const value of [
  { content: "line one\nline two\n", contentMetadata: { isBinary: false } },
  { content: "", contentMetadata: { isBinary: false } },
  { content: "AAEC", contentMetadata: { isBinary: true } },
  { content: null, contentMetadata: null },
  { content: "x" },
  { contentMetadata: { isBinary: null } },
  { contentMetadata: { isBinary: "true" } },
  { contentMetadata: {} },
  { contentMetadata: { contentType: "application/octet-stream", isBinary: true } },
  { content: 4 },
  { content: "x", contentMetadata: "binary" },
  {},
  [],
  "x",
]) {
  decoderRow("decodeItemContentJson", value);
}
decoderRow("decodeItemContentJson", "nope");
decoderRow("decodeItemContentJson", "");

// ---- the diff builder --------------------------------------------------------------------------------
const spec = (value) => {
  if (typeof value === "string") return value;
  if (value.parts !== undefined) return value.parts.map(spec).join("");
  if (value.repeat !== undefined) return value.repeat[0].repeat(value.repeat[1]);
  // `lines` lines of `prefix`, each unique, so two sides with different prefixes share nothing.
  const pad = "z".repeat(value.width ?? 0);
  return `${Array.from({ length: value.lines }, (_, line) => `${value.prefix} ${line} ${pad}`).join("\n")}${value.noFinalNewline ? "" : "\n"}`;
};
const change = (overrides = {}) => ({
  path: "src/a.ts",
  oldPath: "src/a.ts",
  changeKind: "change",
  objectId: "8f80",
  originalObjectId: "0ca4",
  ...overrides,
});
const digest = (text) => ({
  length: Buffer.byteLength(text),
  sha256: createHash("sha256").update(text).digest("hex"),
});
const patchCases = [
  {
    change: change(),
    old: "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n",
    new: "a\nb\nc\nX\ne\nf\ng\nh\ni\nj\n",
  },
  { change: change(), old: "one\n", new: "two\n" },
  { change: change(), old: "same\n", new: "same\n" },
  { change: change(), old: "a\nb\n", new: "a\nb" },
  { change: change(), old: "a\nb", new: "a\nb\n" },
  { change: change(), old: "a\r\nb\r\nc\r\n", new: "a\r\nB\r\nc\r\n" },
  {
    change: change(),
    old: "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\n",
    new: "l1\nCHANGED\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nCHANGED\nl15\n",
  },
  {
    change: change(),
    old: "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\n",
    new: "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nO\n",
  },
  { change: change(), old: "x\n", new: "x\ny\n" },
  { change: change(), old: "y\nx\n", new: "x\n" },
  { change: change(), old: "a\n\nb\n", new: "a\nb\n" },
  { change: change(), old: "a\nb\n", new: "a\n\n\nb\n" },
  { change: change({ changeKind: "new", oldPath: "src/a.ts" }), old: "", new: "hello\nworld\n" },
  { change: change({ changeKind: "new" }), old: "", new: "" },
  { change: change({ changeKind: "new" }), old: "", new: "no newline" },
  { change: change({ changeKind: "new" }), old: "", new: "one line\n" },
  { change: change({ changeKind: "deleted" }), old: "hello\nworld\n", new: "" },
  { change: change({ changeKind: "deleted" }), old: "gone", new: "" },
  {
    change: change({ changeKind: "rename-pure", path: "src/b.ts", oldPath: "src/a.ts" }),
    old: "same\n",
    new: "same\n",
  },
  {
    change: change({ changeKind: "rename-changed", path: "src/b.ts", oldPath: "src/a.ts" }),
    old: "same\nold\n",
    new: "same\nnew\n",
  },
  {
    change: change({
      path: "docs/new\tname.md",
      oldPath: "docs/old\tname.md",
      changeKind: "rename-pure",
    }),
    old: "same\n",
    new: "same\n",
  },
  {
    change: change({ path: "line\nfile.txt", oldPath: "line\nfile.txt" }),
    old: "one\n",
    new: "two\n",
  },
  {
    change: change({ path: 'every\t\n"kind"\\of.md', oldPath: 'every\t\n"kind"\\of.md' }),
    old: "one\n",
    new: "two\n",
  },
  {
    change: change({ path: "logo\tmark.png", oldPath: "logo\tmark.png" }),
    old: "PNG\u0000old",
    new: "PNG\u0000new",
  },
  { change: change({ path: "ünï/cödé.ts", oldPath: "ünï/cödé.ts" }), old: "a\n", new: "b\n" },
  { change: change(), old: "plain\n", new: "plain\n", binary: true },
  { change: change(), old: "has\u0000nul\n", new: "plain\n" },
  { change: change(), old: "plain\n", new: "has\u0000nul\n" },
  { change: change({ changeKind: "new" }), old: "", new: "nul\u0000here" },
  { change: change(), old: { repeat: ["a", 512 * 1024] }, new: "b\n" },
  { change: change(), old: { repeat: ["a", 512 * 1024 + 1] }, new: "b\n" },
  { change: change(), old: "a\n", new: { repeat: ["a", 512 * 1024 + 1] } },
  { change: change(), old: { repeat: ["é", 300_000] }, new: "x\n" },
  { change: change(), old: { repeat: ["é", 262_144] }, new: "x\n" },
  { change: change({ changeKind: "new" }), old: "", new: { repeat: ["z\n", 262_144] } },
  { change: change({ changeKind: "new" }), old: "", new: { repeat: ["z\n", 262_143] } },
  { change: change({ changeKind: "new" }), old: "", new: { repeat: ["a", 512 * 1024] } },
  { change: change({ changeKind: "new" }), old: "", new: { lines: 3000, prefix: "new", width: 8 } },
  {
    change: change({ changeKind: "deleted" }),
    old: { lines: 3000, prefix: "old", width: 8 },
    new: "",
  },
  {
    change: change(),
    old: { lines: 1000, prefix: "old", width: 4 },
    new: { lines: 1000, prefix: "new", width: 4 },
  },
  {
    change: change(),
    old: { lines: 1001, prefix: "old", width: 4 },
    new: { lines: 1000, prefix: "new", width: 4 },
  },
  {
    change: change(),
    old: { lines: 1100, prefix: "old", width: 4 },
    new: { lines: 1100, prefix: "new", width: 4 },
  },
  {
    change: change(),
    old: { lines: 2100, prefix: "old", width: 4 },
    new: { lines: 2100, prefix: "new", width: 4 },
  },
  {
    change: change(),
    old: { lines: 5000, prefix: "same", width: 0 },
    new: { lines: 5000, prefix: "same", width: 0 },
  },
  {
    change: change(),
    old: { parts: [{ lines: 4000, prefix: "same", width: 0 }, "tail\n"] },
    new: { parts: [{ lines: 4000, prefix: "same", width: 0 }, "tail changed\n"] },
  },
  {
    change: change(),
    old: { parts: ["head\n", { lines: 4000, prefix: "same", width: 0 }] },
    new: { parts: ["head changed\n", { lines: 4000, prefix: "same", width: 0 }] },
  },
  {
    change: change(),
    old: { parts: [{ repeat: ["q", 200_000] }, "\nk\n"] },
    new: { parts: [{ repeat: ["r", 200_000] }, "\nk\n"] },
  },
  {
    change: change(),
    old: { parts: [{ repeat: ["q", 300_000] }, "\nk\n"] },
    new: { parts: [{ repeat: ["r", 300_000] }, "\nk\n"] },
  },
];
for (const input of patchCases) {
  const texts = {
    oldContents: spec(input.old),
    newContents: spec(input.new),
    binary: input.binary === true,
  };
  const patch = D.azureDevOpsFilePatch({ change: input.change, texts });
  rows.push({
    type: "patch",
    change: input.change,
    old: input.old,
    new: input.new,
    binary: input.binary === true,
    result: {
      section: patch.section.length <= 4096 ? patch.section : digest(patch.section),
      truncated: patch.truncated,
      abandoned: patch.abandoned,
      edits: patch.edits,
    },
  });
}
for (const input of [
  change(),
  change({ changeKind: "new" }),
  change({ changeKind: "rename-changed", path: "b/x\ty.ts", oldPath: "a/y.ts" }),
]) {
  const patch = D.azureDevOpsUnreadableFilePatch(input);
  rows.push({ type: "unreadable", change: input, result: norm(patch) });
}
for (const raw of [
  undefined,
  null,
  "",
  "abc",
  "1",
  "0:4",
  "1:-2",
  "1:2:3",
  "1:",
  ":4",
  "1: ",
  " 1:4",
  "1:0x2",
  "0x1:2",
  "1e2:0",
  "1:4.0",
  "3:12",
  "03:07",
  "1:0",
  "9007199254740991:1",
  "9007199254740992:1",
  "9007199254740993:0",
  "1:9007199254740992",
  "٣:1",
  "1:2 ",
]) {
  rows.push({
    type: "cursor",
    raw: raw ?? null,
    undefinedInput: raw === undefined,
    result: norm(D.parseAzureDevOpsDiffCursor(raw)),
  });
}
for (const cursor of [
  { iterationId: 3, fileIndex: 12 },
  { iterationId: 1, fileIndex: 0 },
]) {
  rows.push({ type: "format", cursor, result: D.formatAzureDevOpsDiffCursor(cursor) });
}

// ---- the provider over the CLI over scripted processes -----------------------------------------
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
      const route = routes.find((candidate) => joined.includes(candidate.match));
      const fields = { command: input.command, argumentCount: input.args.length, cwd: input.cwd };
      if (route === undefined)
        return Effect.succeed(output({ code: 97, stderr: `no route: ${joined}` }));
      if (route.stdoutItem)
        return Effect.succeed(
          output({
            stdout: item(spec(route.stdoutItem.content), route.stdoutItem.binary === true),
          }),
        );
      if (route.spawn)
        return Effect.fail(
          new ProcessRunner.ProcessSpawnError({
            ...fields,
            cause: PlatformError.systemError({
              _tag: "NotFound",
              module: "ChildProcess",
              method: "spawn",
              syscall: "spawn az",
              pathOrDescriptor: "az",
            }),
          }),
        );
      return Effect.succeed(output(route));
    },
  });
  const process = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  const azure = Layer.effect(AzureDevOpsCli.AzureDevOpsCli, AzureDevOpsCli.make).pipe(
    Layer.provide(process),
  );
  const cli = Layer.effect(AzureCli.AzureDevOpsPullRequestCli, AzureCli.make).pipe(
    Layer.provide(azure),
  );
  return { cli, calls };
}

const subject = { repository: "web", host: "dev.azure.com", number: 7 };
const listInput = (overrides = {}) => ({
  repository: "web",
  host: "dev.azure.com",
  state: "open",
  involvement: "all",
  viewer: "alice@acme.io",
  limit: 20,
  ...overrides,
});
const fail = {
  auth: { code: 1, stderr: "Please run az login to setup account." },
  devopsLogin: {
    code: 1,
    stderr: "Before you can run Azure DevOps commands, you need to run az devops login",
  },
  rate: { code: 1, stderr: "HTTP 429 Too Many Requests" },
  boom: { code: 1, stderr: "boom" },
  notFound: { code: 1, stderr: "TF401180: The requested pull request was not found" },
  gone: { code: 1, stderr: "pull request does not exist" },
};
const SHOW = "repos pr show";
const THREADS = "--resource pullRequestThreads";
const ITERATIONS = "--resource pullRequestIterations";
const CHANGES = "--resource pullRequestIterationChanges";
const ITEMS = "--resource items";
const iterations = (...list) => json({ value: list });
const oneIteration = iterations(iteration(3, "head3", "base3"));
const changes = (entries, extra = {}) => json({ changeEntries: entries, ...extra });
const item = (content, binary = false) => json({ content, contentMetadata: { isBinary: binary } });
const at = (path, commit) =>
  `--query-parameters path=/${path} versionDescriptor.versionType=commit versionDescriptor.version=${commit} `;
const base = (path) => at(path, "base3");
const head = (path) => at(path, "head3");
const lines = (count, prefix) =>
  Array.from({ length: count }, (_, i) => `${prefix}${i}`).join("\n") + "\n";
const many = (count, prefix = "f") =>
  Array.from({ length: count }, (_, i) =>
    entry({ changeType: "add" }, { path: `/${prefix}${i}.txt`, objectId: `o${i}` }),
  );
const manyRoutes = (count, prefix = "f") =>
  Array.from({ length: count }, (_, i) => ({
    match: head(`${prefix}${i}.txt`),
    stdout: item(`file ${i}\n`),
  }));

const scenarios = [
  // ---- the account
  {
    name: "viewer reads",
    routes: [{ match: "account show", stdout: json({ name: " alice@acme.io ", type: "user" }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer empty and nameless",
    routes: [{ match: "account show", stdout: "" }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer nameless",
    routes: [{ match: "account show", stdout: json({ name: "  " }) }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer null",
    routes: [{ match: "account show", stdout: "null" }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer unreadable",
    routes: [{ match: "account show", stdout: "nope" }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer failures",
    routes: [{ match: "account show", ...fail.auth }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer devops login",
    routes: [{ match: "account show", ...fail.devopsLogin }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer rate limited",
    routes: [{ match: "account show", ...fail.rate }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer missing tool",
    routes: [{ match: "account show", spawn: true }],
    calls: [{ op: "viewer", input: {} }],
  },
  {
    name: "viewer failed command",
    routes: [{ match: "account show", ...fail.boom }],
    calls: [{ op: "viewer", input: {} }],
  },
  // ---- the listing
  {
    name: "list first page",
    routes: [
      { match: "repos pr list", stdout: json([pr(), pr({ pullRequestId: 8, title: "Second" })]) },
    ],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list states and involvement",
    routes: [{ match: "repos pr list", stdout: "[]" }],
    calls: [
      { op: "list", input: listInput({ state: "closed" }) },
      { op: "list", input: listInput({ state: "merged", involvement: "authored" }) },
      {
        op: "list",
        input: listInput({ state: "all", involvement: "reviewing", viewer: "bob smith" }),
      },
      { op: "list", input: listInput({ repository: "other repo" }) },
    ],
  },
  {
    name: "list drops the search text",
    routes: [{ match: "repos pr list", stdout: json([pr()]) }],
    calls: [
      { op: "list", input: listInput({ query: "  fix & more " }) },
      { op: "list", input: listInput({ query: "   " }) },
    ],
  },
  {
    name: "list empty answer",
    routes: [{ match: "repos pr list", stdout: "  \n" }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list steps over what was handed over",
    routes: [{ match: "repos pr list", stdout: json([pr({ pullRequestId: 5 })]) }],
    calls: [
      {
        op: "list",
        input: listInput({ cursor: { updatedBefore: "2026-01-01T00:00:00.000Z", delivered: 10 } }),
      },
      {
        op: "list",
        input: listInput({
          limit: 1,
          cursor: { updatedBefore: "2026-01-01T00:00:00.000Z", delivered: 3 },
        }),
      },
    ],
  },
  {
    name: "list truncated by the extra row",
    routes: [
      {
        match: "repos pr list",
        stdout: json([
          pr({ pullRequestId: 1 }),
          pr({ pullRequestId: 2 }),
          pr({ pullRequestId: 3 }),
        ]),
      },
    ],
    calls: [
      { op: "list", input: listInput({ limit: 2 }) },
      { op: "list", input: listInput({ limit: 3 }) },
      { op: "list", input: listInput({ limit: 5 }) },
      { op: "list", input: listInput({ limit: 1 }) },
    ],
  },
  {
    name: "list keeps reading past malformed rows",
    routes: [
      { match: "--skip 4", stdout: json([pr({ pullRequestId: 30 })]) },
      { match: "--skip 2", stdout: json([{ bad: 1 }, pr({ pullRequestId: 20 })]) },
      {
        match: "repos pr list",
        stdout: json([{ bad: 1 }, pr({ title: "" }), pr({ pullRequestId: 10 })]),
      },
    ],
    calls: [{ op: "list", input: listInput({ limit: 2 }) }],
  },
  {
    name: "list counts malformed rows before the page's end",
    routes: [
      {
        match: "repos pr list",
        stdout: json([
          pr({ pullRequestId: 1 }),
          { bad: 1 },
          pr({ pullRequestId: 2 }),
          pr({ pullRequestId: 3 }),
        ]),
      },
    ],
    calls: [
      { op: "list", input: listInput({ limit: 3 }) },
      { op: "list", input: listInput({ limit: 2 }) },
    ],
  },
  {
    name: "list keeps reading while the page is full of malformed rows",
    routes: [
      { match: "--skip 3", stdout: json([{ bad: 2 }]) },
      { match: "repos pr list", stdout: json([{ bad: 1 }, { bad: 1 }, { bad: 1 }]) },
    ],
    calls: [{ op: "list", input: listInput({ limit: 2 }) }],
  },
  {
    name: "list unreadable",
    routes: [{ match: "repos pr list", stdout: "nope" }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list not an array",
    routes: [{ match: "repos pr list", stdout: "{}" }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list failures",
    routes: [{ match: "repos pr list", ...fail.auth }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list rate limited",
    routes: [{ match: "repos pr list", ...fail.rate }],
    calls: [{ op: "list", input: listInput() }],
  },
  {
    name: "list missing tool",
    routes: [{ match: "repos pr list", spawn: true }],
    calls: [{ op: "list", input: listInput() }],
  },
  // ---- the summary and the detail
  {
    name: "summary costs the one read",
    routes: [{ match: SHOW, stdout: json(pr()) }],
    calls: [{ op: "summary", input: subject }],
  },
  {
    name: "summary of states",
    routes: [
      {
        match: "--id 8",
        stdout: json(
          pr({ pullRequestId: 8, status: "completed", closedDate: "2026-01-05T00:00:00Z" }),
        ),
      },
      {
        match: "--id 9",
        stdout: json(
          pr({
            pullRequestId: 9,
            status: "abandoned",
            closedDate: "2026-01-06T00:00:00Z",
            isDraft: true,
            createdBy: null,
          }),
        ),
      },
    ],
    calls: [
      { op: "summary", input: { ...subject, number: 8 } },
      { op: "summary", input: { ...subject, number: 9 } },
    ],
  },
  {
    name: "summary failures",
    routes: [
      { match: "--id 8", ...fail.notFound },
      {
        match: "--id 9",
        stdout: json({
          pullRequestId: 9,
          title: "x",
          sourceRefName: "refs/heads/",
          targetRefName: "main",
          creationDate: "2026-01-01T00:00:00Z",
        }),
      },
      { match: "--id 10", stdout: "nope" },
      { match: "--id 11", ...fail.auth },
    ],
    calls: [
      { op: "summary", input: { ...subject, number: 8 } },
      { op: "summary", input: { ...subject, number: 9 } },
      { op: "summary", input: { ...subject, number: 10 } },
      { op: "summary", input: { ...subject, number: 11 } },
    ],
  },
  {
    name: "preview is the detail",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes([entry(), entry({}, { path: "/b.ts" })]) },
    ],
    calls: [{ op: "preview", input: subject }],
  },
  {
    name: "detail reports the file count",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      {
        match: ITERATIONS,
        stdout: iterations(iteration(1, "head1", "base1"), iteration(2, "head2", "base2")),
      },
      {
        match: "iterationId=2",
        stdout: changes([
          entry(),
          entry({}, { path: "/b.ts" }),
          entry({}, { path: "/dir", isFolder: true }),
        ]),
      },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail states",
    routes: [
      {
        match: "--id 8",
        stdout: json(
          pr({
            pullRequestId: 8,
            status: "completed",
            closedDate: "2026-01-05T00:00:00Z",
            autoCompleteSetBy: who("A", "a@a"),
            completionOptions: { squashMerge: true },
          }),
        ),
      },
      {
        match: "--id 9",
        stdout: json(
          pr({
            pullRequestId: 9,
            status: "abandoned",
            closedDate: "2026-01-06T00:00:00Z",
            isDraft: true,
            mergeStatus: "conflicts",
            reviewers: [],
          }),
        ),
      },
      { match: ITERATIONS, stdout: iterations() },
    ],
    calls: [
      { op: "detail", input: { ...subject, number: 8 } },
      { op: "detail", input: { ...subject, number: 9 } },
    ],
  },
  {
    name: "detail without a location",
    routes: [{ match: SHOW, stdout: json(pr({ repository: { name: "web" } })) }],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail loses only the file count when the iterations fail",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, ...fail.boom },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail loses only the file count when the changes are unreadable",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: "nope" },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail loses the file count even to a signed-out CLI",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, ...fail.auth },
    ],
    calls: [{ op: "detail", input: subject }],
  },
  {
    name: "detail failures",
    routes: [
      { match: "--id 8", ...fail.notFound },
      { match: "--id 9", ...fail.rate },
      { match: "--id 10", stdout: "[]" },
      { match: "--id 12", ...fail.gone },
    ],
    calls: [
      { op: "detail", input: { ...subject, number: 8 } },
      { op: "detail", input: { ...subject, number: 9 } },
      { op: "detail", input: { ...subject, number: 10 } },
      { op: "detail", input: { ...subject, number: 12 } },
    ],
  },
  {
    name: "detail missing tool",
    routes: [{ match: SHOW, spawn: true }],
    calls: [{ op: "detail", input: subject }],
  },
  { name: "no checks of its own", routes: [], calls: [{ op: "hasChecks", input: {} }] },
  {
    name: "permissions are the constant",
    routes: [],
    calls: [{ op: "permissions", input: subject }],
  },
  // ---- the conversation
  {
    name: "activity reads the threads",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      {
        match: THREADS,
        stdout: json({
          value: [
            thread({
              id: 3,
              comments: [
                comment({ id: 2, publishedDate: "2026-01-04T00:00:00Z" }),
                comment({ id: 1, publishedDate: "2026-01-03T00:00:00Z" }),
              ],
            }),
            thread({
              id: 4,
              threadContext: { filePath: "/src/a.ts" },
              comments: [
                comment({ id: 1, publishedDate: "2026-01-02T00:00:00Z", content: "Rename this" }),
              ],
            }),
            thread({ id: 5, comments: [comment({ commentType: "system" })] }),
          ],
        }),
      },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity without a location",
    routes: [{ match: SHOW, stdout: json(pr({ repository: { name: "web" } })) }],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity loses the conversation to a failure",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: THREADS, ...fail.boom },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity loses the conversation even to a signed-out CLI",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: THREADS, ...fail.auth },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity loses the conversation to unreadable output",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: THREADS, stdout: "nope" },
    ],
    calls: [{ op: "activity", input: subject }],
  },
  {
    name: "activity fails with the pull request",
    routes: [{ match: SHOW, ...fail.notFound }],
    calls: [{ op: "activity", input: subject }],
  },
  // ---- the patch
  {
    name: "diff of one file",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes([entry()]) },
      { match: base("src/a.ts"), stdout: item("a\nb\nc\n") },
      { match: head("src/a.ts"), stdout: item("a\nB\nc\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff scope",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      {
        match: "iterationId=2",
        stdout: changes([entry({ changeType: "add" }, { path: "/old.txt" })]),
      },
      {
        match: "iterationId=3",
        stdout: changes([entry({ changeType: "add" }, { path: "/new.txt" })]),
      },
      {
        match: ITERATIONS,
        stdout: iterations(iteration(2, "head2", "base2"), iteration(3, "head3", "base3")),
      },
      { match: at("old.txt", "head2"), stdout: item("old\n") },
      { match: at("new.txt", "head3"), stdout: item("new\n") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "2:0" } },
      { op: "diff", input: { ...subject, cursor: "9:0" } },
      { op: "diff", input: { ...subject, cursor: "garbage" } },
      { op: "diff", input: { ...subject, cursor: "3:5" } },
      { op: "diff", input: { ...subject, commit: "abc1234" } },
    ],
  },
  {
    name: "diff of nothing",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: iterations() },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff without a location",
    routes: [{ match: SHOW, stdout: json(pr({ repository: undefined })) }],
    calls: [
      { op: "diff", input: subject },
      { op: "diffFile", input: { ...subject, changeType: "change", oldPath: "a", newPath: "a" } },
    ],
  },
  {
    name: "diff reads a location once",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes([entry({ changeType: "add" }, { path: "/a.txt" })]) },
      { match: head("a.txt"), stdout: item("a\n") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, number: 8 } },
    ],
  },
  {
    name: "diff kinds of change",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([
          entry({ changeType: "add" }, { path: "/new.txt" }),
          entry({ changeType: "delete" }, { path: "/gone.txt" }),
          entry({ changeType: "rename", sourceServerItem: "/before.txt" }, { path: "/after.txt" }),
          entry({ changeType: "rename, edit", sourceServerItem: "/was.txt" }, { path: "/is.txt" }),
          entry({ changeType: "edit" }, { path: "/ spaced.txt " }),
        ]),
      },
      { match: head("new.txt"), stdout: item("brand new\n") },
      { match: base("gone.txt"), stdout: item("so long\n") },
      { match: base("before.txt"), stdout: item("same\n") },
      { match: head("after.txt"), stdout: item("same\n") },
      { match: base("was.txt"), stdout: item("one\ntwo\n") },
      { match: head("is.txt"), stdout: item("one\n2\n") },
      { match: base(" spaced.txt "), stdout: item("a\n") },
      { match: head(" spaced.txt "), stdout: item("b\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff leaves a file the host refused listed without its hunks",
    ordered: false,
    // The side of a file read beside one that failed is started in the original only when the
    // failure comes late; this port asks for both sides always.
    racy: true,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([
          entry({}, { path: "/ok.txt" }),
          entry({}, { path: "/refused.txt" }),
          entry({}, { path: "/garbled.txt" }),
          entry({}, { path: "/missing.txt" }),
        ]),
      },
      { match: at("ok.txt", "base3"), stdout: item("a\n") },
      { match: at("ok.txt", "head3"), stdout: item("b\n") },
      { match: at("refused.txt", "base3"), ...fail.boom },
      { match: at("refused.txt", "head3"), stdout: item("x\n") },
      { match: at("garbled.txt", "base3"), stdout: "nope" },
      { match: at("garbled.txt", "head3"), stdout: item("x\n") },
      { match: at("missing.txt", "base3"), ...fail.notFound },
      { match: at("missing.txt", "head3"), stdout: item("x\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff fails with the connection",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([entry({}, { path: "/a.txt" }), entry({}, { path: "/b.txt" })]),
      },
      { match: at("a.txt", "base3"), stdout: item("a\n") },
      { match: at("a.txt", "head3"), ...fail.auth },
      { match: at("b.txt", "base3"), stdout: item("a\n") },
      { match: at("b.txt", "head3"), stdout: item("b\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff fails with a rate limit",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes([entry({}, { path: "/a.txt" })]) },
      { match: at("a.txt", "base3"), ...fail.rate },
      { match: at("a.txt", "head3"), stdout: item("b\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff fails with the changes",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, ...fail.boom },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff fails with the iterations",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: "nope" },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff takes the host's word that a file is binary",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes([entry({}, { path: "/logo.png" })]) },
      { match: at("logo.png", "base3"), stdout: item("QQ==", true) },
      { match: at("logo.png", "head3"), stdout: item("Qg==", true) },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff spends its edit budget",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([
          entry({ changeType: "add" }, { path: "/huge.txt" }),
          entry({ changeType: "add" }, { path: "/after1.txt" }),
          entry({ changeType: "add" }, { path: "/after2.txt" }),
        ]),
      },
      { match: head("huge.txt"), stdout: item(lines(4001, "x")) },
      { match: head("after1.txt"), stdout: item("1\n") },
      { match: head("after2.txt"), stdout: item("2\n") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "3:1" } },
    ],
  },
  {
    name: "diff gives up on files too far apart and stops there",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([entry({}, { path: "/far.txt" }), entry({}, { path: "/near.txt" })]),
      },
      { match: at("far.txt", "base3"), stdout: item(lines(2100, "old")) },
      { match: at("far.txt", "head3"), stdout: item(lines(2100, "new")) },
      { match: at("near.txt", "base3"), stdout: item("a\n") },
      { match: at("near.txt", "head3"), stdout: item("b\n") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "3:1" } },
    ],
  },
  {
    name: "diff stops on the byte ceiling",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([
          entry({ changeType: "add" }, { path: "/big.txt" }),
          entry({ changeType: "add" }, { path: "/small.txt" }),
        ]),
      },
      {
        match: head("big.txt"),
        stdoutItem: {
          content: { parts: [{ repeat: ["y", 200_000] }, "\n", { repeat: ["w", 70_000] }, "\n"] },
        },
      },
      { match: head("small.txt"), stdout: item("s\n") },
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "3:1" } },
    ],
  },
  {
    name: "diff reads on from the file the cursor names",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes(many(5)) },
      ...manyRoutes(5),
    ],
    calls: [
      { op: "diff", input: { ...subject, cursor: "3:2" } },
      { op: "diff", input: { ...subject, cursor: "3:5" } },
      { op: "diff", input: { ...subject, cursor: "3:9" } },
    ],
  },
  {
    name: "diff carries at most a slice's worth of files",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, stdout: changes(many(301)) },
      ...manyRoutes(301),
    ],
    calls: [
      { op: "diff", input: subject },
      { op: "diff", input: { ...subject, cursor: "3:300" } },
    ],
  },
  {
    name: "diff follows a long change page by page",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: "$skip=2000", stdout: changes([entry({ changeType: "add" }, { path: "/b.txt" })]) },
      {
        match: "$skip=0",
        stdout: changes([entry({ changeType: "add" }, { path: "/a.txt" })], { nextSkip: 2000 }),
      },
      { match: head("a.txt"), stdout: item("a\n") },
      { match: head("b.txt"), stdout: item("b\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff stops following pages that do not move on",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: CHANGES,
        stdout: changes([entry({ changeType: "add" }, { path: "/a.txt" })], { nextSkip: 2000 }),
      },
      { match: head("a.txt"), stdout: item("a\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff stops following pages past the ceiling",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: "$skip=2000",
        stdout: changes([entry({ changeType: "add" }, { path: "/b.txt" })], { nextSkip: 10000 }),
      },
      {
        match: "$skip=0",
        stdout: changes([entry({ changeType: "add" }, { path: "/a.txt" })], { nextSkip: 2000 }),
      },
      { match: head("a.txt"), stdout: item("a\n") },
      { match: head("b.txt"), stdout: item("b\n") },
    ],
    calls: [{ op: "diff", input: subject }],
  },
  {
    name: "diff file contents",
    ordered: false,
    routes: [
      { match: SHOW, stdout: json(pr()) },
      {
        match: ITERATIONS,
        stdout: iterations(iteration(1, "head1", "base1"), iteration(3, "head3", "base3")),
      },
      { match: base("a.ts"), stdout: item("before\n") },
      { match: head("a.ts"), stdout: item("after\n") },
      { match: base("old.ts"), stdout: item("moved\n") },
      { match: head("new.ts"), stdout: item("moved\n") },
      { match: base("gone.ts"), stdout: item("gone\n") },
      { match: head("fresh.ts"), stdout: item("fresh\n") },
      { match: base("bin.png"), stdout: item("QQ==", true) },
      { match: head("bin.png"), stdout: item("Qg==", true) },
      { match: base("bad.ts"), ...fail.boom },
    ],
    calls: [
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "a.ts", newPath: "a.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "rename-pure", oldPath: "old.ts", newPath: "new.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "deleted", oldPath: "gone.ts", newPath: "gone.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "new", oldPath: "fresh.ts", newPath: "fresh.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "bin.png", newPath: "bin.png" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "bad.ts", newPath: "bad.ts" },
      },
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "/lead.ts", newPath: "/lead.ts" },
      },
    ],
  },
  {
    name: "diff file contents without a push",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: iterations() },
    ],
    calls: [
      {
        op: "diffFile",
        input: { ...subject, changeType: "change", oldPath: "a.ts", newPath: "a.ts" },
      },
    ],
  },
  // ---- the files already read
  {
    name: "revisions name the head's blob",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      {
        match: ITERATIONS,
        stdout: iterations(iteration(1, "head1", "base1"), iteration(2, "head2", "base2")),
      },
      {
        match: "iterationId=2",
        stdout: changes([
          entry({}, { path: "/a.ts", objectId: "blob-a" }),
          entry({}, { path: "/b.ts", objectId: "blob-b" }),
          entry({}, { path: "/c.ts", objectId: null }),
        ]),
      },
    ],
    calls: [
      { op: "revisions", input: { ...subject, paths: ["a.ts", "b.ts", "untouched.ts", "c.ts"] } },
      { op: "revisions", input: { ...subject, paths: [] } },
      { op: "revisions", input: { ...subject, paths: ["a.ts"] } },
    ],
  },
  {
    name: "revisions say nothing about the files past a change given up on",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      {
        match: "$skip=2000",
        stdout: changes([entry({}, { path: "/b.ts", objectId: "blob-b" })], { nextSkip: 12000 }),
      },
      {
        match: "$skip=0",
        stdout: changes([entry({}, { path: "/a.ts", objectId: "blob-a" })], { nextSkip: 2000 }),
      },
    ],
    calls: [{ op: "revisions", input: { ...subject, paths: ["a.ts", "b.ts", "other.ts"] } }],
  },
  {
    name: "revisions with no push or no location",
    routes: [
      { match: "--id 8", stdout: json(pr({ pullRequestId: 8, repository: undefined })) },
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: iterations() },
    ],
    calls: [
      { op: "revisions", input: { ...subject, number: 8, paths: ["a.ts"] } },
      { op: "revisions", input: { ...subject, paths: ["a.ts"] } },
    ],
  },
  {
    name: "revisions fail",
    routes: [
      { match: SHOW, stdout: json(pr()) },
      { match: ITERATIONS, stdout: oneIteration },
      { match: CHANGES, ...fail.rate },
    ],
    calls: [{ op: "revisions", input: { ...subject, paths: ["a.ts"] } }],
  },
  // ---- reviewers
  {
    name: "reviewers added and taken off",
    routes: [{ match: "repos pr reviewer", stdout: "[]" }],
    calls: [
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "bob@acme.io", kind: "user" }], requested: true },
      },
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "bob@acme.io", kind: "user" },
            { id: "8f5a-guid", kind: "user" },
          ],
          requested: false,
        },
      },
    ],
  },
  {
    name: "reviewers refused before anything runs",
    routes: [{ match: "repos pr reviewer", stdout: "[]" }],
    calls: [
      {
        op: "request",
        input: {
          ...subject,
          reviewers: [
            { id: "bob@acme.io", kind: "user" },
            { id: "--delete", kind: "user" },
          ],
          requested: true,
        },
      },
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: " -x", kind: "user" }], requested: true },
      },
    ],
  },
  {
    name: "reviewers fail",
    routes: [{ match: "repos pr reviewer", ...fail.auth }],
    calls: [
      {
        op: "request",
        input: { ...subject, reviewers: [{ id: "bob@acme.io", kind: "user" }], requested: true },
      },
    ],
  },
  // ---- actions and edits
  {
    name: "actions",
    routes: [{ match: "repos pr update", stdout: "{}" }],
    calls: [
      { op: "action", input: { ...subject, action: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "merge" } },
      { op: "action", input: { ...subject, action: "merge", mergeMethod: "rebase" } },
      { op: "action", input: { ...subject, action: "enable-auto-merge" } },
      { op: "action", input: { ...subject, action: "enable-auto-merge", mergeMethod: "squash" } },
      { op: "action", input: { ...subject, action: "enable-auto-merge", mergeMethod: "merge" } },
      { op: "action", input: { ...subject, action: "disable-auto-merge" } },
      { op: "action", input: { ...subject, action: "ready" } },
      { op: "action", input: { ...subject, action: "draft" } },
      { op: "action", input: { ...subject, action: "close" } },
      { op: "action", input: { ...subject, action: "reopen" } },
      { op: "action", input: { ...subject, action: "update-branch" } },
    ],
  },
  {
    name: "actions fail",
    routes: [
      { match: "--id 8", ...fail.auth },
      { match: "--id 9", ...fail.rate },
      { match: "--id 10", ...fail.notFound },
      { match: "--id 11", ...fail.boom },
    ],
    calls: [8, 9, 10, 11].map((number) => ({
      op: "action",
      input: { ...subject, number, action: "close" },
    })),
  },
  {
    name: "actions missing tool",
    routes: [{ match: "repos pr update", spawn: true }],
    calls: [{ op: "action", input: { ...subject, action: "close" } }],
  },
  {
    name: "updates travel as one argument each",
    routes: [{ match: "repos pr update", stdout: "{}" }],
    calls: [
      { op: "update", input: { ...subject, title: "New title" } },
      { op: "update", input: { ...subject, body: "- starts with a dash\nsecond line" } },
      { op: "update", input: { ...subject, title: "T", body: "B" } },
      { op: "update", input: { ...subject, title: "", body: "" } },
    ],
  },
  {
    name: "updates fail",
    routes: [{ match: "repos pr update", ...fail.boom }],
    calls: [{ op: "update", input: { ...subject, title: "x" } }],
  },
  // ---- what the host does not offer
  {
    name: "the reviews are not written",
    routes: [],
    calls: [
      { op: "comment", input: { ...subject, body: "hi" } },
      { op: "review", input: { ...subject, verdict: "comment", body: "ok", comments: [] } },
      { op: "reply", input: { ...subject, threadId: "1", body: "ok" } },
      { op: "reaction", input: { ...subject, content: "heart", reacted: true } },
      { op: "resolution", input: { ...subject, threadId: "1", resolved: true } },
      { op: "candidates", input: subject },
    ],
  },
];

const callProvider = (provider, call) => {
  const input = { cwd: "/repo", ...call.input };
  switch (call.op) {
    case "viewer":
      return provider.getViewer({ cwd: "/repo" });
    case "list":
      return provider.listChangeRequests(input);
    case "summary":
      return provider.getChangeRequestSummary(input);
    case "preview":
      return provider.getChangeRequest(input).pipe(
        Effect.map((detail) => ({
          number: detail.number,
          title: detail.title,
          url: detail.url,
          author: detail.author,
          state: detail.state,
          isDraft: detail.isDraft,
          createdAt: detail.createdAt,
        })),
      );
    case "detail":
      return provider.getChangeRequest(input);
    case "hasChecks":
      return Effect.succeed(provider.getChangeRequestChecks !== undefined);
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
    case "request":
      return provider.setReviewerRequest(input);
    case "action":
      return provider.runAction(input);
    case "update":
      return provider.updateChangeRequest(input);
    case "comment":
      return provider.comment(input);
    case "review":
      return provider.submitReview(input);
    case "reply":
      return provider.replyToThread(input);
    case "reaction":
      return provider.setReaction(input);
    case "resolution":
      return provider.setThreadResolution(input);
    case "candidates":
      return provider.listReviewerCandidates(input);
    default:
      throw new Error(`unknown op ${call.op}`);
  }
};

// Keys the original's rows carry that the neutral shape does not.
const neutral = (op, value) => {
  const copy = norm(value);
  // The wire schema carries the two texts only; the flag rides along in the original's object.
  if (op === "diffFile" && copy) delete copy.binary;
  if (op === "diff" && copy && copy.patch.length > 4096) copy.patch = digest(copy.patch);
  if (op === "list" && copy && Array.isArray(copy.items)) {
    for (const row of copy.items) {
      delete row.closedAt;
      delete row.mergedAt;
    }
  }
  return copy;
};

for (const scenario of scenarios) {
  const { cli, calls } = harness(scenario.routes);
  const results = [];
  const callLog = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* AzureProvider.make;
      for (const call of scenario.calls) {
        const before = calls.length;
        const exit = yield* Effect.exit(callProvider(provider, call));
        results.push(
          exit._tag === "Success"
            ? { ok: neutral(call.op, exit.value ?? null) }
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
    racy: scenario.racy === true,
    routes: scenario.routes.map(({ match, stdout, stdoutItem, stderr, code, spawn }) => ({
      match,
      stdout,
      stdoutItem,
      stderr,
      code,
      spawn,
    })),
    calls: scenario.calls,
    results,
    processes: callLog,
  });
}

// ---- what the provider declares -----------------------------------------------------------------
{
  const { cli } = harness([]);
  const provider = await Effect.runPromise(AzureProvider.make.pipe(Effect.provide(cli)));
  const permissions = await Effect.runPromise(provider.getViewerPermissions({}));
  rows.push({
    type: "declared",
    capabilities: norm(provider.capabilities),
    permissions: norm(permissions),
    kind: provider.kind,
    spawns: AzureProvider.MAX_DIFF_SPAWNS,
    locations: AzureProvider.LOCATION_CACHE_CAPACITY,
    limits: {
      files: D.MAX_DIFF_SLICE_FILES,
      bytes: D.MAX_DIFF_SLICE_BYTES,
      edits: D.MAX_DIFF_SLICE_EDITS,
      fileEdits: D.MAX_FILE_DIFF_EDITS,
    },
  });
}

const target = new URL("../crates/server/tests/fixtures/pull-request-azure.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(`wrote ${rows.length} rows to ${target.pathname}`);
