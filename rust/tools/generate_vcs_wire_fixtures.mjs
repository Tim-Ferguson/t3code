// JSON for the wire types the vcs crate produces and consumes, encoded by the original
// contract schemas exactly as the RPC layer does (`Schema.toCodecJson`), so the Rust serde
// shapes can be held to them.
import { writeFileSync, renameSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  GitCommandError,
  VcsListRefsInput,
  VcsListRefsResult,
  VcsPullResult,
  VcsStatusLocalResult,
  VcsStatusRemoteResult,
  VcsStatusResult,
} from "../../packages/contracts/src/git.ts";
import {
  VcsDriverCapabilities,
  VcsFreshness,
  VcsListRemotesResult,
  VcsListWorkspaceFilesResult,
  VcsRepositoryIdentity,
} from "../../packages/contracts/src/vcs.ts";
import {
  ReviewDiffFileContentsInput,
  ReviewDiffFileContentsResult,
  ReviewDiffPreviewInput,
  ReviewDiffPreviewResult,
} from "../../packages/contracts/src/review.ts";

const rows = [];
const encode = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));
const decode = (schema, json) => Schema.decodeUnknownSync(Schema.toCodecJson(schema))(json);
const add = (type, schema, value) => rows.push({ type, json: encode(schema, value) });
const time = (text) => DateTime.makeUnsafe(text);
const live = {
  source: "live-local",
  observedAt: time("2024-01-02T03:04:05.678Z"),
  expiresAt: Option.none(),
};

add("freshness", VcsFreshness, live);
add("freshness", VcsFreshness, {
  source: "cached-remote",
  observedAt: time("2024-01-02T03:04:05.000Z"),
  expiresAt: Option.some(time("2024-06-07T08:09:10.011Z")),
});
add("freshness", VcsFreshness, {
  source: "explicit-remote",
  observedAt: time("2024-01-02T03:04:05.100Z"),
  expiresAt: Option.none(),
});
add("capabilities", VcsDriverCapabilities, {
  kind: "git",
  supportsWorktrees: true,
  supportsBookmarks: false,
  supportsAtomicSnapshot: false,
  supportsPushDefaultRemote: true,
  ignoreClassifier: "native",
});
add("capabilities", VcsDriverCapabilities, {
  kind: "jj",
  supportsWorktrees: false,
  supportsBookmarks: true,
  supportsAtomicSnapshot: true,
  supportsPushDefaultRemote: false,
  ignoreClassifier: "git-compatible-fallback",
});
add("repositoryIdentity", VcsRepositoryIdentity, {
  kind: "git",
  rootPath: "/repo",
  metadataPath: "/repo/.git",
  freshness: live,
});
add("repositoryIdentity", VcsRepositoryIdentity, {
  kind: "unknown",
  rootPath: "/repo",
  metadataPath: null,
  freshness: live,
});
add("workspaceFiles", VcsListWorkspaceFilesResult, {
  paths: ["a.txt", "dir/b é.txt"],
  truncated: true,
  freshness: live,
});
add("remotes", VcsListRemotesResult, {
  remotes: [
    {
      name: "origin",
      url: "https://example.com/a.git",
      pushUrl: Option.some("git@example.com:a.git"),
      isPrimary: true,
    },
    { name: "fork", url: "https://example.com/b.git", pushUrl: Option.none(), isPrimary: false },
  ],
  freshness: live,
});

const ref = (name, extra = {}) => ({
  name,
  current: false,
  isRemote: false,
  isDefault: false,
  worktreePath: null,
  ...extra,
});
add("listRefsResult", VcsListRefsResult, {
  refs: [
    ref("main", { current: true, isDefault: true, worktreePath: "/repo" }),
    ref("origin/main", { isRemote: true, remoteName: "origin" }),
    ref("feature/x", { worktreePath: "/wt" }),
    ref("lonely", { isRemote: true }),
  ],
  isRepo: true,
  hasPrimaryRemote: true,
  nextCursor: 4,
  totalCount: 9,
});
add("listRefsResult", VcsListRefsResult, {
  refs: [],
  isRepo: false,
  hasPrimaryRemote: false,
  nextCursor: null,
  totalCount: 0,
});
add("pull", VcsPullResult, { status: "pulled", refName: "main", upstreamRef: "origin/main" });
add("pull", VcsPullResult, { status: "skipped_up_to_date", refName: "main", upstreamRef: null });

const workingTree = {
  files: [{ path: "a.txt", insertions: 2, deletions: 1 }],
  insertions: 2,
  deletions: 1,
};
const local = {
  isRepo: true,
  hasPrimaryRemote: true,
  isDefaultRef: false,
  refName: "feature/x",
  hasWorkingTreeChanges: true,
  workingTree,
};
add("statusLocal", VcsStatusLocalResult, local);
add("statusLocal", VcsStatusLocalResult, {
  ...local,
  refName: null,
  branchChanges: { baseRef: "origin/main", insertions: 5, deletions: 3 },
});
add("statusLocal", VcsStatusLocalResult, {
  ...local,
  branchChanges: { baseRef: null, insertions: 0, deletions: 0 },
});
add("statusRemote", VcsStatusRemoteResult, {
  hasUpstream: true,
  aheadCount: 1,
  behindCount: 2,
  aheadOfDefaultCount: 3,
  pr: null,
});
add("statusRemote", VcsStatusRemoteResult, {
  hasUpstream: false,
  aheadCount: 0,
  behindCount: 0,
  pr: null,
});
add("status", VcsStatusResult, {
  ...local,
  hasUpstream: true,
  aheadCount: 1,
  behindCount: 0,
  aheadOfDefaultCount: 1,
  pr: null,
});
add("status", VcsStatusResult, {
  ...local,
  branchChanges: { baseRef: "main", insertions: 1, deletions: 1 },
  hasUpstream: false,
  aheadCount: 0,
  behindCount: 0,
  pr: null,
});

const stat = (path, previousPath, additions, deletions) => ({
  path,
  previousPath,
  additions,
  deletions,
});
const source = {
  id: "working-tree",
  kind: "working-tree",
  title: "Uncommitted",
  baseRef: "HEAD",
  headRef: null,
  diff: "diff --git a/x b/x\n",
  diffHash: "ab".repeat(32),
  truncated: false,
};
add("reviewPreview", ReviewDiffPreviewResult, {
  cwd: "/repo",
  generatedAt: time("2024-01-02T03:04:05.678Z"),
  sources: [
    { ...source, files: [stat("x", null, 1, 0), stat("new name", "old name", 0, 0)] },
    {
      ...source,
      id: "branch-range",
      kind: "branch-range",
      title: "Changes vs main",
      baseRef: "main",
      headRef: "feature/x",
      truncated: true,
    },
  ],
});
add("reviewPreview", ReviewDiffPreviewResult, {
  cwd: "/repo",
  generatedAt: time("2024-01-02T03:04:05.000Z"),
  sources: [],
});
add("reviewContents", ReviewDiffFileContentsResult, { oldContents: "old\n", newContents: "" });

const input = (type, schema, json) =>
  rows.push({ type, json: encode(schema, decode(schema, json)) });
input("listRefsInput", VcsListRefsInput, { cwd: "/repo" });
input("listRefsInput", VcsListRefsInput, {
  cwd: "/repo",
  query: "feat",
  cursor: 100,
  includeMatchingRemoteRefs: true,
  refKind: "remote",
  refresh: false,
  limit: 50,
});
input("reviewPreviewInput", ReviewDiffPreviewInput, { cwd: "/repo" });
input("reviewPreviewInput", ReviewDiffPreviewInput, {
  cwd: "/repo",
  baseRef: "main",
  ignoreWhitespace: true,
  file: { path: "a.txt", previousPath: null, sourceKind: "branch-range" },
});
input("reviewPreviewInput", ReviewDiffPreviewInput, {
  cwd: "/repo",
  file: { path: "b.txt", previousPath: "a.txt", sourceKind: "working-tree" },
});
for (const changeType of ["change", "rename-pure", "rename-changed", "new", "deleted"]) {
  input("reviewContentsInput", ReviewDiffFileContentsInput, {
    cwd: "/repo",
    sourceKind: "working-tree",
    changeType,
    baseRef: null,
    headRef: null,
    oldPath: "a",
    newPath: "b",
  });
}
input("reviewContentsInput", ReviewDiffFileContentsInput, {
  cwd: "/repo",
  sourceKind: "branch-range",
  changeType: "change",
  baseRef: "main",
  headRef: "feature/x",
  oldPath: "a",
  newPath: "a",
});

const error = (fields) =>
  rows.push({
    type: "gitCommandError",
    json: encode(GitCommandError, new GitCommandError(fields)),
  });
error({ operation: "GitVcsDriver.x", command: "git", cwd: "/repo", detail: "d" });
error({
  operation: "GitVcsDriver.x",
  command: "git",
  cwd: "/repo",
  argumentCount: 3,
  exitCode: 128,
  stdoutLength: 4,
  stderrLength: 5,
  outputLength: 6,
  reason: "remote_unreachable",
  detail: "failed",
});
error({
  operation: "GitVcsDriver.x",
  command: "git",
  cwd: "/repo",
  exitCode: -1,
  reason: "not_a_repository",
  detail: "negative exit",
});

const target = new URL("../crates/vcs/tests/fixtures/wire.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
