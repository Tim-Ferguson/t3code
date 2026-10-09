// Feeds scripted GitHub answers through the original decoders in gitHubPullRequestJson.ts and
// records what they produce, for crates/server/src/pull_request_decode.rs to replay.
import { writeFileSync, renameSync } from "node:fs";
import * as J from "../../apps/server/src/pullRequest/gitHubPullRequestJson.ts";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );
const rows = [];
const run = (decoder, name, raw, extra) => {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  const result = J[name](text, ...(extra ?? []));
  rows.push({
    decoder,
    raw: text,
    ok: Result.isSuccess(result),
    value: Result.isSuccess(result) ? norm(result.success) : null,
  });
};
const actor = (login, extra = {}) => ({
  __typename: "User",
  login,
  avatarUrl: `https://avatars.example/${login}`,
  name: login.toUpperCase(),
  id: `ID_${login}`,
  ...extra,
});
const ctx = (o) => o;
const pr = (over = {}) => ({
  number: 7,
  title: " Fix it ",
  url: "https://github.com/o/r/pull/7",
  body: "Body",
  state: "OPEN",
  isDraft: false,
  mergeable: "MERGEABLE",
  reviewDecision: "REVIEW_REQUIRED",
  additions: 10,
  deletions: 2,
  changedFiles: 3,
  createdAt: "2026-01-01T00:00:00Z",
  updatedAt: "2026-01-02T00:00:00Z",
  mergedAt: null,
  closedAt: null,
  headRefName: "feature",
  baseRefName: "main",
  headRefOid: " abc123 ",
  isCrossRepository: false,
  headRepositoryOwner: { login: "o" },
  author: actor("alice"),
  autoMergeRequest: null,
  viewerCanUpdate: true,
  viewerDidAuthor: false,
  viewerCanUpdateBranch: true,
  baseRef: { compare: { behindBy: 2 } },
  reviewRequests: {
    nodes: [
      { requestedReviewer: { login: "bob", name: "Bob" } },
      { requestedReviewer: { slug: "core", name: "Core" } },
      { requestedReviewer: null },
    ],
  },
  labels: {
    nodes: [
      { name: "bug", color: "ff0000" },
      { name: " ", color: null },
    ],
  },
  commits: {
    nodes: [
      {
        commit: {
          statusCheckRollup: {
            contexts: {
              nodes: [
                {
                  __typename: "CheckRun",
                  name: "build",
                  status: "COMPLETED",
                  conclusion: "SUCCESS",
                  startedAt: "2026-01-01T00:00:00Z",
                  completedAt: "2026-01-01T00:05:00Z",
                  detailsUrl: "https://ci/1",
                  isRequired: true,
                  checkSuite: { workflowRun: { workflow: { name: "CI" } } },
                },
                {
                  __typename: "CheckRun",
                  name: "build",
                  status: "COMPLETED",
                  conclusion: "FAILURE",
                  startedAt: "2026-01-01T00:06:00Z",
                  completedAt: "2026-01-01T00:07:00Z",
                  detailsUrl: "https://ci/2",
                  checkSuite: { workflowRun: { workflow: { name: "CI" } } },
                },
                {
                  __typename: "CheckRun",
                  name: "lint",
                  status: "IN_PROGRESS",
                  conclusion: null,
                  startedAt: "0001-01-01T00:00:00Z",
                  completedAt: "0001-01-01T00:00:00Z",
                  checkSuite: null,
                },
                {
                  __typename: "CheckRun",
                  name: "test",
                  status: "COMPLETED",
                  conclusion: "SUCCESS",
                  checkSuite: { workflowRun: { workflow: { name: "A" } } },
                },
                {
                  __typename: "CheckRun",
                  name: "test",
                  status: "COMPLETED",
                  conclusion: "SKIPPED",
                  checkSuite: { workflowRun: { workflow: { name: "B" } } },
                },
                {
                  __typename: "StatusContext",
                  context: "ci/legacy",
                  state: "PENDING",
                  targetUrl: "https://legacy",
                  description: "wait",
                },
                { __typename: "StatusContext", context: "  ", state: "SUCCESS" },
              ],
              pageInfo: { hasNextPage: false },
            },
          },
        },
      },
    ],
  },
  ...over,
});
const core = (prOver, repoOver = {}) => ({
  data: {
    repository: {
      mergeCommitAllowed: true,
      squashMergeAllowed: false,
      rebaseMergeAllowed: true,
      viewerPermission: "WRITE",
      pullRequest: pr(prOver),
      ...repoOver,
    },
  },
});
run("core", "decodePullRequestCoreJson", core());
run(
  "core",
  "decodePullRequestCoreJson",
  core({
    state: "MERGED",
    mergedAt: "2026-01-03T00:00:00Z",
    autoMergeRequest: { mergeMethod: "SQUASH" },
    isCrossRepository: true,
  }),
);
run(
  "core",
  "decodePullRequestCoreJson",
  core({
    state: "CLOSED",
    closedAt: "2026-01-03T00:00:00Z",
    isDraft: true,
    mergeable: "CONFLICTING",
    baseRef: null,
    reviewDecision: "APPROVED",
    viewerCanUpdate: false,
    viewerDidAuthor: true,
  }),
);
{
  const p = pr();
  delete p.autoMergeRequest;
  delete p.headRepositoryOwner;
  delete p.headRefOid;
  delete p.body;
  delete p.changedFiles;
  p.commits = { nodes: [] };
  p.baseRef = { compare: null };
  run("core", "decodePullRequestCoreJson", {
    data: {
      repository: {
        mergeCommitAllowed: false,
        squashMergeAllowed: true,
        rebaseMergeAllowed: false,
        viewerPermission: "TRIAGE",
        pullRequest: p,
      },
    },
  });
}
run(
  "core",
  "decodePullRequestCoreJson",
  core(
    {
      commits: {
        nodes: [
          {
            commit: {
              statusCheckRollup: { contexts: { nodes: [], pageInfo: { hasNextPage: true } } },
            },
          },
        ],
      },
    },
    { viewerPermission: "READ" },
  ),
);
run("core", "decodePullRequestCoreJson", core({ reviewDecision: null }));
run("core", "decodePullRequestCoreJson", {
  data: {
    repository: {
      mergeCommitAllowed: true,
      squashMergeAllowed: true,
      rebaseMergeAllowed: true,
      pullRequest: { ...pr(), viewerCanUpdateBranch: undefined },
    },
  },
});
run("core", "decodePullRequestCoreJson", { data: { repository: null } });
run("core", "decodePullRequestCoreJson", "not json");
const contexts = (over = {}) => ({
  data: {
    repository: {
      pullRequest: {
        headRefOid: "abc",
        commits: {
          nodes: [
            {
              commit: {
                statusCheckRollup: {
                  contexts: {
                    nodes: [
                      {
                        __typename: "CheckRun",
                        name: "a",
                        status: "COMPLETED",
                        conclusion: "CANCELLED",
                        checkSuite: { workflowRun: { workflow: { name: "W" } } },
                      },
                    ],
                    pageInfo: { hasNextPage: true, endCursor: "c1" },
                  },
                },
              },
            },
          ],
        },
        ...over,
      },
    },
  },
});
run("contexts", "decodePullRequestCheckContextsJson", contexts());
run("contexts", "decodePullRequestCheckContextsJson", contexts({ commits: { nodes: [] } }));
run("contexts", "decodePullRequestCheckContextsJson", {
  data: { repository: { pullRequest: { commits: { nodes: [] } } } },
});
const previewPr = (over = {}) => ({
  data: {
    repository: {
      pullRequest: {
        number: 7,
        title: "T",
        url: "u",
        state: "OPEN",
        isDraft: false,
        createdAt: "2026-01-01T00:00:00Z",
        author: actor("alice"),
        ...over,
      },
    },
  },
});
run("preview", "decodePullRequestPreviewJson", previewPr());
run(
  "preview",
  "decodePullRequestPreviewJson",
  previewPr({ state: "CLOSED", author: null, isDraft: true }),
);
run(
  "preview",
  "decodePullRequestPreviewJson",
  previewPr({ author: { __typename: "Bot", login: "dependabot[bot]", avatarUrl: null } }),
);
run("preview", "decodePullRequestPreviewJson", previewPr({ isDraft: undefined }));
const perms = (repo, pullRequest) => ({
  data: {
    repository: {
      mergeCommitAllowed: true,
      squashMergeAllowed: true,
      rebaseMergeAllowed: false,
      ...repo,
      pullRequest,
    },
  },
});
for (const permission of ["ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ", "NONE", undefined, null])
  run(
    "perms",
    "decodeViewerPermissionsJson",
    perms({ viewerPermission: permission }, { viewerCanUpdate: true, viewerDidAuthor: true }),
  );
run(
  "perms",
  "decodeViewerPermissionsJson",
  perms({ viewerPermission: "READ" }, { viewerCanUpdate: false }),
);
run("perms", "decodeViewerPermissionsJson", perms({ viewerPermission: "READ" }, null));
run("perms", "decodeViewerPermissionsJson", { data: { repository: { viewerPermission: "READ" } } });
const reactionGroups = [
  {
    content: "THUMBS_UP",
    viewerHasReacted: true,
    reactors: { totalCount: 3, nodes: [{ login: "me" }, { login: "bob" }, {}] },
  },
  { content: "EYES", viewerHasReacted: false, reactors: { totalCount: 0, nodes: [] } },
  { content: "ROCKET", reactors: { nodes: [{ login: "zed" }] } },
  { content: "WAT", reactors: { totalCount: 2, nodes: [{ login: "x" }] } },
  { content: "heart", viewerHasReacted: false, reactors: { totalCount: 1, nodes: [null] } },
];
const act = (pull) => ({ data: { repository: { pullRequest: pull } } });
run(
  "activity",
  "decodePullRequestActivityJson",
  act({
    author: actor("alice"),
    commits: {
      nodes: [
        {
          commit: {
            oid: "c1",
            messageHeadline: "one",
            committedDate: "2026-01-01T00:00:00Z",
            authors: {
              nodes: [
                { name: "Al", email: "a@x", user: { login: "alice" } },
                { name: null, email: "e@x", user: null },
                { name: null, email: null, user: null },
              ],
            },
          },
        },
      ],
    },
    comments: {
      pageInfo: { hasNextPage: true, endCursor: " cc " },
      nodes: [
        {
          id: "IC_1",
          body: "hi",
          createdAt: "2026-01-02T00:00:00Z",
          lastEditedAt: "2026-01-03T00:00:00Z",
          url: "https://c/1",
          author: actor("bob"),
        },
        { id: "IC_2", body: "", createdAt: "2026-01-01T12:00:00Z", url: "  ", author: null },
      ],
    },
    reviews: {
      pageInfo: { hasNextPage: false, endCursor: "r" },
      nodes: [
        {
          id: "R_1",
          body: "",
          state: "APPROVED",
          submittedAt: "2026-01-02T01:00:00Z",
          lastEditedAt: null,
          url: "https://r/1",
          author: actor("carol"),
        },
        {
          id: "R_2",
          body: "  ",
          state: "COMMENTED",
          submittedAt: "2026-01-02T02:00:00Z",
          author: actor("dan"),
        },
        {
          id: "R_3",
          body: "text",
          state: "COMMENTED",
          submittedAt: "2026-01-02T03:00:00Z",
          author: actor("dan"),
        },
        { id: "R_4", body: "pending", state: "PENDING", submittedAt: null },
      ],
    },
  }),
);
run(
  "activity",
  "decodePullRequestActivityJson",
  act({
    author: null,
    comments: null,
    reviews: { pageInfo: { hasNextPage: true, endCursor: null }, nodes: [] },
  }),
);
run(
  "activity",
  "decodePullRequestActivityJson",
  act({ comments: { nodes: [{ body: "no id", createdAt: "x" }] } }),
);
run("activity", "decodePullRequestActivityJson", act({}));
const thread = (over = {}) => ({
  id: "T_1",
  isResolved: false,
  isOutdated: false,
  path: "src/a.ts",
  line: 12,
  diffSide: "RIGHT",
  comments: {
    totalCount: 2,
    pageInfo: { hasNextPage: true, endCursor: "tc" },
    nodes: [
      {
        id: "RC_1",
        author: actor("bob"),
        body: "nit",
        createdAt: "2026-01-02T00:00:00Z",
        lastEditedAt: null,
        url: "https://t/1",
        reactionGroups,
      },
      {
        id: "RC_2",
        author: { __typename: "Bot", login: "bot[bot]", avatarUrl: "https://b" },
        createdAt: "2026-01-02T01:00:00Z",
      },
    ],
  },
  ...over,
});
const threadPr = (over = {}) => ({
  reviewThreads: {
    totalCount: 3,
    pageInfo: { hasNextPage: true, endCursor: "next" },
    nodes: [
      thread(),
      thread({ id: "T_2", line: 0, isResolved: true, isOutdated: true, diffSide: "left" }),
      thread({ id: null }),
      thread({ path: "" }),
      thread({ id: "T_5", comments: { nodes: [] } }),
      thread({
        id: "T_6",
        line: null,
        diffSide: null,
        comments: { nodes: [{ id: "RC_9", createdAt: "2026-01-02T05:00:00Z" }] },
      }),
    ],
  },
  viewerCanUpdate: false,
  viewerDidAuthor: true,
  author: actor("alice"),
  reactionGroups,
  comments: {
    nodes: [
      { id: "IC_1", lastEditedAt: "2026-01-05T00:00:00Z", author: actor("bob"), reactionGroups },
      { id: "IC_2", author: null },
    ],
  },
  reviews: { nodes: [{ id: "R_1", author: actor("carol"), reactionGroups: [] }] },
  reviewRequests: {
    nodes: [
      {
        requestedReviewer: { login: "bob", name: "Bob", avatarUrl: "https://avatars.example/bob" },
      },
      { requestedReviewer: null },
      { requestedReviewer: {} },
    ],
  },
  latestReviews: {
    nodes: [
      { state: "APPROVED", author: actor("bob") },
      { state: "CHANGES_REQUESTED", author: actor("erin") },
    ],
  },
  reviewDismissals: {
    pageInfo: { hasNextPage: true, endCursor: "dc" },
    nodes: [
      { dismissalMessage: " stale ", review: { id: "R_1" } },
      { dismissalMessage: null, review: { id: "R_2" } },
      {},
    ],
  },
  commits: {
    nodes: [
      {
        commit: {
          oid: "c1",
          messageHeadline: "one",
          committedDate: "2026-01-01T00:00:00Z",
          additions: 5,
          deletions: 1,
          parents: { totalCount: 1 },
          authors: {
            nodes: [
              { name: "Al", avatarUrl: "https://a", user: { login: "alice" } },
              { name: "Anon", avatarUrl: null, user: null },
            ],
          },
        },
      },
      {
        commit: {
          oid: "c2",
          messageHeadline: "merge",
          committedDate: "2026-01-01T01:00:00Z",
          additions: 50,
          deletions: 50,
          parents: { totalCount: 2 },
        },
      },
      { commit: { oid: " ", committedDate: "x" } },
      { commit: { oid: "c3" } },
    ],
  },
  ...over,
});
run("threads", "decodeReviewThreadsJson", {
  data: { viewer: { login: "ME" }, repository: { pullRequest: threadPr() } },
});
run("threads", "decodeReviewThreadsJson", {
  data: {
    repository: {
      pullRequest: threadPr({
        reviewThreads: { nodes: [], pageInfo: { hasNextPage: false, endCursor: "x" } },
        commits: null,
        reviewDismissals: null,
        viewerCanUpdate: undefined,
      }),
    },
  },
});
run("threads", "decodeReviewThreadsJson", { data: { repository: { pullRequest: {} } } });
run("dismissals", "decodeReviewDismissalsJson", {
  data: {
    repository: {
      pullRequest: {
        timelineItems: {
          pageInfo: { hasNextPage: true, endCursor: "d" },
          nodes: [
            { dismissalMessage: "why", review: { id: "R_1" } },
            { dismissalMessage: "", review: { id: "R_2" } },
            {},
          ],
        },
      },
    },
  },
});
run("dismissals", "decodeReviewDismissalsJson", { data: { repository: { pullRequest: {} } } });
const tc = (over = {}) => ({
  data: {
    viewer: { login: "me" },
    repository: { pullRequest: { id: "PR_1" } },
    node: {
      pullRequest: { id: "PR_1" },
      comments: {
        pageInfo: { hasNextPage: false, endCursor: "e" },
        nodes: [
          {
            id: "RC_5",
            author: actor("bob"),
            body: "b",
            createdAt: "2026-01-02T00:00:00Z",
            lastEditedAt: "2026-01-03T00:00:00Z",
            url: " ",
            reactionGroups,
          },
        ],
      },
    },
    ...over,
  },
});
run("threadComments", "decodeReviewThreadCommentsJson", tc());
run(
  "threadComments",
  "decodeReviewThreadCommentsJson",
  tc({ node: { pullRequest: { id: "PR_OTHER" }, comments: { nodes: [] } } }),
);
run("threadComments", "decodeReviewThreadCommentsJson", tc({ node: null }));
run("threadComments", "decodeReviewThreadCommentsJson", tc({ repository: null }));
run("threadComments", "decodeReviewThreadCommentsJson", { data: {} });
const file = (over) => ({
  filename: "a.ts",
  status: "modified",
  patch: "@@ -1 +1 @@\n-a\n+b",
  additions: 1,
  deletions: 1,
  ...over,
});
run("files", "decodePullRequestFilesJson", [
  file(),
  file({ filename: "new.ts", status: "added", patch: "@@ -0,0 +1 @@\n+x\n" }),
  file({ filename: "gone.ts", status: "removed" }),
  file({ filename: "to.ts", status: "renamed", previous_filename: "from.ts", patch: "" }),
  file({
    filename: "pure.ts",
    status: "renamed",
    previous_filename: "old name.ts",
    patch: null,
    additions: 0,
    deletions: 0,
  }),
  file({ filename: "img.png", status: "added", patch: undefined, additions: 0, deletions: 0 }),
  file({ filename: "big.js", patch: undefined, additions: 900, deletions: 3 }),
  file({ filename: 'we"ird\tname', status: "modified" }),
  { status: "modified" },
  file({ filename: "sp ace.ts", status: " RENAMED ", previous_filename: "" }),
]);
run("files", "decodePullRequestFilesJson", []);
run("files", "decodePullRequestFilesJson", { files: [] });
run("commitFiles", "decodeCommitFilesJson", {
  sha: "abc",
  files: [file(), file({ filename: "b.ts", status: "added", patch: "@@ -0,0 +1 @@\n+b" })],
});
run("commitFiles", "decodeCommitFilesJson", { sha: "abc", files: null });
run("commitFiles", "decodeCommitFilesJson", { sha: "abc" });
run("commitFiles", "decodeCommitFilesJson", []);
const viewed = (files, hasNext = false) => ({
  data: {
    repository: {
      pullRequest: {
        files: {
          pageInfo: { hasNextPage: hasNext, endCursor: hasNext ? "vc" : null },
          nodes: files,
        },
      },
    },
  },
});
run(
  "filesViewed",
  "decodePullRequestFilesViewedJson",
  viewed(
    [
      { path: "a", viewerViewedState: "VIEWED" },
      { path: "b", viewerViewedState: "dismissed" },
      { path: "c", viewerViewedState: "UNVIEWED" },
      { path: "d", viewerViewedState: "WAT" },
      null,
      { path: "", viewerViewedState: "VIEWED" },
    ],
    true,
  ),
);
run("filesViewed", "decodePullRequestFilesViewedJson", viewed(null));
run("filesViewed", "decodePullRequestFilesViewedJson", { data: { repository: null } });
run("filesViewed", "decodePullRequestFilesViewedJson", {
  data: { repository: { pullRequest: null } },
});
run("filesViewed", "decodePullRequestFilesViewedJson", {
  data: { repository: { pullRequest: { files: { pageInfo: { hasNextPage: "yes" }, nodes: [] } } } },
});
run("stats", "decodePullRequestStatsJson", {
  data: {
    s0: { pullRequest: { additions: 5, deletions: 2 } },
    s1: { pullRequest: null },
    s2: null,
    s3: { pullRequest: { additions: null } },
    bad: { pullRequest: { additions: 1 } },
    s10: { pullRequest: { additions: 7, deletions: 8 } },
  },
});
run("stats", "decodePullRequestStatsJson", { data: null });
run("stats", "decodePullRequestStatsJson", { errors: [] });
run("avatars", "decodeActorAvatarsJson", {
  data: {
    nodes: [
      { login: "a", avatarUrl: "https://a" },
      null,
      { login: "b", avatarUrl: null },
      { login: " c ", avatarUrl: " https://c " },
    ],
  },
});

const row = (over = {}) => ({
  number: 3,
  title: "Row",
  url: "https://github.com/o/r/pull/3",
  author: actor("alice"),
  headRefName: "h",
  baseRefName: "main",
  state: "OPEN",
  isDraft: false,
  mergeable: "UNKNOWN",
  reviewDecision: null,
  latestReviews: {
    nodes: [
      { state: "COMMENTED", author: { login: "x" } },
      null,
      { state: "APPROVED", author: { login: "y" } },
    ],
  },
  additions: 1,
  deletions: 2,
  createdAt: "2026-01-01T00:00:00Z",
  updatedAt: "2026-01-02T00:00:00Z",
  mergedAt: null,
  repository: { nameWithOwner: "o/r" },
  reviewRequests: {
    nodes: [
      { requestedReviewer: { login: "bob" } },
      { requestedReviewer: { slug: "core" } },
      { requestedReviewer: {} },
      null,
    ],
  },
  labels: { nodes: [{ name: "bug", color: "f00" }, null] },
  commits: { nodes: [{ commit: { statusCheckRollup: { state: "FAILURE" } } }] },
  ...over,
});
const searchOf = (nodes, page = { hasNextPage: true, endCursor: " sc " }) => ({
  data: { search: { pageInfo: page, nodes } },
});
run(
  "search",
  "decodePullRequestSearchJson",
  searchOf([
    row(),
    row({
      number: 4,
      repository: { nameWithOwner: "o/other" },
      stack: { number: 9, size: 3, baseRefName: "main" },
      stackEntry: { position: 2 },
      state: "MERGED",
      commits: { nodes: [] },
    }),
    row({ number: 5, repository: null }),
    row({ title: undefined }),
    {},
    null,
    "junk",
    row({
      number: 6,
      state: "CLOSED",
      mergedAt: "2026-01-03T00:00:00Z",
      reviewDecision: "CHANGES_REQUESTED",
      commits: { nodes: [{ commit: { statusCheckRollup: { state: "SUCCESS" } } }] },
    }),
  ]),
);
run("search", "decodePullRequestSearchJson", searchOf(null, { hasNextPage: false }));
run("search", "decodePullRequestSearchJson", searchOf([], null));
run("search", "decodePullRequestSearchJson", { data: {} });
run("list", "decodePullRequestListJson", {
  data: {
    repository: {
      pullRequests: {
        pageInfo: { hasNextPage: true, endCursor: "lc" },
        nodes: [row(), row({ number: 8, repository: null, additions: undefined })],
      },
    },
  },
});
run("list", "decodePullRequestListJson", { data: { repository: null } });
run("list", "decodePullRequestListJson", {
  data: { repository: { pullRequests: { nodes: null } } },
});
run("list", "decodePullRequestListJson", { data: { repository: {} } });
const summary = (over = {}) => ({
  number: 3,
  title: "S",
  url: "u",
  state: "OPEN",
  isDraft: true,
  mergeable: "MERGEABLE",
  reviewDecision: "APPROVED",
  additions: 5,
  deletions: 1,
  changedFiles: 2,
  updatedAt: "2026-01-02T00:00:00Z",
  mergedAt: null,
  closedAt: null,
  headRefName: "h",
  baseRefName: "main",
  author: actor("alice"),
  latestReviews: { nodes: [] },
  commits: { nodes: [{ commit: { statusCheckRollup: { state: "PENDING" } } }] },
  ...over,
});
run("summaries", "decodePullRequestSummariesJson", {
  data: {
    s0: { pullRequest: summary() },
    s1: {
      pullRequest: summary({
        number: 4,
        state: "CLOSED",
        closedAt: "2026-01-03T00:00:00Z",
        stack: null,
        reviewDecision: null,
        latestReviews: { nodes: [{ state: "CHANGES_REQUESTED" }] },
      }),
    },
    s2: {
      pullRequest: summary({
        number: 5,
        state: "MERGED",
        mergedAt: "2026-01-04T00:00:00Z",
        stack: { number: 1, size: 2, baseRefName: "main" },
        stackEntry: { position: 1 },
        commits: null,
      }),
    },
    s3: { pullRequest: null },
    s4: null,
    s5: { pullRequest: summary({ title: null }) },
    x: { pullRequest: summary() },
  },
});
run("summaries", "decodePullRequestSummariesJson", { data: null });
run("summaries", "decodePullRequestSummariesJson", []);
const stackPr = (o) => ({
  number: 1,
  head: { ref: "a", sha: "s1" },
  state: "open",
  draft: false,
  title: "T",
  merged_at: null,
  ...o,
});
run("stacks", "decodePullRequestStacksJson", [
  {
    id: 5,
    number: 2,
    url: "https://api/x",
    html_url: " https://h/x ",
    base: "main",
    pull_requests: [
      stackPr(),
      stackPr({
        number: 2,
        head: { ref: "b" },
        state: "closed",
        merged_at: "2026-01-01T00:00:00Z",
      }),
      stackPr({ number: 3, head: { ref: "c" }, state: null, draft: undefined, title: undefined }),
    ],
  },
]);
run("stacks", "decodePullRequestStacksJson", [
  { number: 2, node_id: "N1", url: "u", base: { ref: "dev" }, pull_requests: [] },
]);
run("stacks", "decodePullRequestStacksJson", [
  { number: 2, url: "u", base: "main", pull_requests: [] },
]);
run("stacks", "decodePullRequestStacksJson", []);
run("stacks", "decodePullRequestStacksJson", [{ number: 2, url: "u", base: "main" }]);
run("stacks", "decodePullRequestStacksJson", {});
const target = new URL(
    "../crates/server/tests/fixtures/pull-request-decode.jsonl",
    import.meta.url,
  ),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length, failures: rows.filter((row) => !row.ok).length }));
