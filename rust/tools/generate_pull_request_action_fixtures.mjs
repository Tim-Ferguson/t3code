// Runs the original pull request actions and records what they produce, for
// crates/server/src/pull_request_actions.rs and tests/pull_request_actions.rs to replay:
//  - the pure readers and builders (reviewer and label candidates, the bodies of a reviewer or
//    label request, workflow runs, pull request heads, agent-credit removal, viewer permissions);
//  - GitHubPullRequestCli against scripted answers: every request each action sends, in order,
//    and the error it reports (merge, auto-merge, update-branch, ready, draft, close, reopen,
//    revert, approve-workflows, labels, reviewer requests, the two candidate lists);
//  - the provider's checks, with the workflow runs awaiting approval folded in;
//  - PullRequestService's refusals: which gate turns an action down, with what words, and
//    whether the account's permissions were even asked for.
// No network is used and no process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as NodeCrypto from "../../apps/server/node_modules/@effect/platform-node/dist/NodeCrypto.js";
import * as KeyValueStore from "../../apps/server/node_modules/effect/dist/persistence/KeyValueStore.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
import * as Cause from "../../apps/server/node_modules/effect/dist/Cause.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Redacted from "../../apps/server/node_modules/effect/dist/Redacted.js";
import * as Stream from "../../apps/server/node_modules/effect/dist/Stream.js";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
import * as J from "../../apps/server/src/pullRequest/gitHubPullRequestJson.ts";
import * as GitHubApi from "../../apps/server/src/sourceControl/GitHubApi.ts";
import * as GitHubGraphQlBudget from "../../apps/server/src/sourceControl/githubGraphQlBudget.ts";
import * as GitHubPullRequestCli from "../../apps/server/src/pullRequest/GitHubPullRequestCli.ts";
import * as GitHubPullRequestProvider from "../../apps/server/src/pullRequest/GitHubPullRequestProvider.ts";
import { PullRequestProviderError } from "../../apps/server/src/pullRequest/PullRequestProvider.ts";
import * as PullRequestService from "../../apps/server/src/pullRequest/PullRequestService.ts";
import * as PullRequestProviderRegistry from "../../apps/server/src/pullRequest/PullRequestProviderRegistry.ts";
import * as PullRequestReadCache from "../../apps/server/src/pullRequest/PullRequestReadCache.ts";
import * as PullRequestFilesViewed from "../../apps/server/src/persistence/PullRequestFilesViewed.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";
import * as ProjectService from "../../apps/server/src/project/ProjectService.ts";
import * as RepositoryIdentityResolver from "../../apps/server/src/project/RepositoryIdentityResolver.ts";
import * as ServerSettingsService from "../../apps/server/src/serverSettings.ts";
import * as SourceControlProviderRegistry from "../../apps/server/src/sourceControl/SourceControlProviderRegistry.ts";
import * as SourceControlRateLimit from "../../apps/server/src/sourceControl/SourceControlRateLimit.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import { removeAgentCredits } from "../../apps/server/src/pullRequest/mergeMessage.ts";
import { DEFAULT_SERVER_SETTINGS } from "../../packages/contracts/src/settings.ts";

const rows = [];
const norm = (value) =>
  JSON.parse(
    JSON.stringify(value, (key, v) =>
      v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v].sort() : v,
    ),
  );
const run = (effect) => Effect.runPromise(effect);

// ---- pure readers and builders ------------------------------------------------------------
const decode = (decoder, raw) => {
  const text = typeof raw === "string" ? raw : JSON.stringify(raw);
  const result = J[decoder](text);
  rows.push({
    type: "decode",
    decoder,
    raw: text,
    ok: Result.isSuccess(result),
    value: Result.isSuccess(result) ? norm(result.success) : null,
  });
};
const person = (login, extra = {}) => ({
  login,
  name: login.toUpperCase(),
  avatarUrl: `https://avatars.example/${login}`,
  ...extra,
});
const reviewerData = (over = {}) => ({
  data: {
    repository: {
      assignableUsers: {
        pageInfo: { hasNextPage: true },
        nodes: [
          person("alice"),
          person("bob"),
          null,
          person("  "),
          person("carol", { name: null, avatarUrl: " " }),
          person("author"),
        ],
      },
      pullRequest: {
        author: { login: "author" },
        reviewRequests: {
          nodes: [
            { requestedReviewer: person("bob") },
            { requestedReviewer: { slug: "core", name: "Core", avatarUrl: "https://t/core" } },
            { requestedReviewer: { login: "outsider", avatarUrl: null } },
            { requestedReviewer: null },
            { requestedReviewer: {} },
          ],
        },
      },
      ...over,
    },
  },
});
decode("decodeReviewerCandidatesJson", reviewerData());
decode("decodeReviewerCandidatesJson", reviewerData({ pullRequest: null }));
decode("decodeReviewerCandidatesJson", reviewerData({ assignableUsers: { nodes: [] } }));
decode("decodeReviewerCandidatesJson", reviewerData({ pullRequest: { author: null } }));
decode("decodeReviewerCandidatesJson", { data: { repository: null } });
decode("decodeReviewerCandidatesJson", "not json");
const labelData = (over = {}) => ({
  data: {
    repository: {
      labels: {
        pageInfo: { hasNextPage: false },
        nodes: [
          { name: "bug", color: "ff0000", description: "Broken" },
          { name: " docs ", color: " ", description: null },
          null,
          { name: " ", color: "000000" },
          { name: "bug", color: "00ff00", description: "dup" },
        ],
      },
      pullRequest: { labels: { nodes: [{ name: "bug" }, { name: "gone" }, null, { name: "  " }] } },
      ...over,
    },
  },
});
decode("decodeLabelCandidatesJson", labelData());
decode("decodeLabelCandidatesJson", labelData({ pullRequest: null }));
decode("decodeLabelCandidatesJson", labelData({ labels: null }));
decode(
  "decodeLabelCandidatesJson",
  labelData({ labels: { pageInfo: { hasNextPage: true }, nodes: [] } }),
);
decode("decodeLabelCandidatesJson", { data: {} });
decode("decodeWorkflowRunsJson", {
  workflow_runs: [
    { id: 11, name: "CI", html_url: "https://github.com/acme/web/actions/runs/11" },
    { id: 12, name: " ", html_url: null },
    { id: 13 },
  ],
});
decode("decodeWorkflowRunsJson", { workflow_runs: [] });
decode("decodeWorkflowRunsJson", { workflow_runs: [{ id: "x" }] });
decode("decodeWorkflowRunsJson", "[]");
decode("decodePullRequestHeadsJson", {
  data: {
    repository: {
      pullRequests: {
        pageInfo: { hasNextPage: true, endCursor: "n1" },
        nodes: [
          {
            number: 7,
            headRefOid: "abc",
            isCrossRepository: true,
            headRepositoryOwner: { login: "Fork" },
          },
          { number: 8, headRefOid: "def", headRepositoryOwner: null },
        ],
      },
    },
  },
});
decode("decodePullRequestHeadsJson", { data: { repository: null } });
decode("decodePullRequestHeadsJson", { data: { repository: { pullRequests: { nodes: [] } } } });
decode("decodePullRequestHeadsJson", "oops");
const builders = [
  [
    "buildReviewerRequest",
    [
      [
        { id: "alice", kind: "user" },
        { id: "core", kind: "team" },
        { id: "bob", kind: "user" },
      ],
    ],
  ],
  ["buildReviewerRequest", [[{ id: "core", kind: "team" }]]],
  ["buildLabelRequest", [["bug", "needs triage", "a/b"]]],
];
for (const [name, args] of builders)
  rows.push({ type: "build", builder: name, args, value: norm(J[name](...args)) });
const credits = [
  "Fix it\n\nCo-authored-by: Claude <noreply@anthropic.com>\n",
  "Fix it\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\nCo-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>",
  "Fix it\r\n\r\nGenerated with Codex.\r\nSigned-off-by: Human <h@example.com>\r\n",
  "Keep this:\n\n```\nCo-authored-by: Claude <noreply@anthropic.com>\n```\n\nafter",
  "Quoted:\n\n    Co-authored-by: Claude <noreply@anthropic.com>\n\nend",
  "- Co-authored-by: GitHub Copilot <noreply@github.com>\nbody",
  "Co-authored-by: Someone <noreply@github.com>\nbody",
  "~~~\nGenerated by Claude\n~~~\nGenerated by Claude",
  "[Generated with Cursor](https://cursor.com/x)\ntext",
  "plain message",
  "",
];
for (const message of credits)
  rows.push({ type: "credits", message, value: removeAgentCredits(message) });
const accesses = [];
for (const canWrite of [false, true])
  for (const canTriage of [false, true])
    for (const canUpdate of [false, true])
      for (const didAuthor of [false, true])
        for (const canUpdateBranch of [undefined, false, true]) {
          accesses.push({
            canWrite,
            canTriage,
            canUpdate,
            didAuthor,
            ...(canUpdateBranch === undefined ? {} : { canUpdateBranch }),
            mergeCapabilities: { merge: true, squash: true, rebase: true },
          });
        }
for (const access of accesses)
  rows.push({
    type: "permissions",
    access,
    value: norm(GitHubPullRequestProvider.gitHubViewerPermissions(access)),
  });

// ---- GitHubPullRequestCli against scripted answers -----------------------------------------
const ok = (body) => ({
  status: 200,
  headers: {},
  body: typeof body === "string" ? body : JSON.stringify(body),
  truncated: false,
  invalidUtf8: false,
});
const calls = [];
let script = [];
const failureOf = (step) => {
  switch (step.error) {
    case "notFound":
      return new GitHubApi.GitHubApiNotFoundError({ host: "github.com", operation: "x" });
    case "response":
      return new GitHubApi.GitHubApiResponseError({
        host: "github.com",
        operation: "x",
        status: step.status,
        ...(step.githubErrors ? { githubErrors: step.githubErrors } : {}),
      });
    case "rateLimit":
      return new GitHubApi.GitHubApiRateLimitError({
        host: "github.com",
        operation: "x",
        retryAt: step.retryAt,
      });
    default:
      throw new Error(`unknown error ${step.error}`);
  }
};
const answer = (call) => {
  calls.push(call);
  const step = script.shift();
  if (step === undefined)
    return Effect.fail(
      new GitHubApi.GitHubApiResponseError({
        host: "github.com",
        operation: "unscripted",
        status: 599,
      }),
    );
  return step.error ? Effect.fail(failureOf(step)) : Effect.succeed(ok(step.body));
};
const mockApi = Layer.effect(
  GitHubApi.GitHubApi,
  Effect.gen(function* () {
    const budget = yield* GitHubGraphQlBudget.GitHubGraphQlBudget;
    return GitHubApi.GitHubApi.of({
      graphql: (input) =>
        budget
          .query(input.host, input.query, input.allowReserve ? { allowReserve: true } : undefined)
          .pipe(
            Effect.flatMap((query) => answer({ kind: "graphql", ...input, query })),
            Effect.map((response) => response.body),
            Effect.tap((body) => budget.observe(input.host, body)),
          ),
      rest: (input) => answer({ kind: "rest", ...input }),
      credential: (host) =>
        Effect.succeed({ token: Redacted.make("token"), fingerprint: `${host}:token` }),
    });
  }),
);
const cliLayer = GitHubPullRequestCli.layer.pipe(
  Layer.provideMerge(mockApi),
  Layer.provideMerge(GitHubGraphQlBudget.layer),
  Layer.provide(NodeCrypto.layer),
  Layer.provide(VcsProcess.layer.pipe(Layer.provideMerge(NodeServices.layer))),
);
const withCli = (use) =>
  run(
    Effect.scoped(
      Effect.gen(function* () {
        const context = yield* Layer.build(cliLayer);
        return yield* Effect.provideContext(use, context);
      }),
    ),
  );
const record = (call) => {
  const out = { kind: call.kind, operation: call.operation };
  if (call.kind === "rest") {
    out.method = call.method ?? "GET";
    out.path = call.path;
    if (call.body !== undefined) out.body = call.body;
  } else {
    out.query = call.query;
    out.variables = call.variables ?? {};
  }
  if (call.allowReserve !== undefined) out.allowReserve = call.allowReserve;
  return out;
};
const outcome = (exit) =>
  Exit.isSuccess(exit)
    ? { ok: true, value: norm(exit.value ?? null) }
    : {
        ok: false,
        error: (() => {
          const e = Cause.squash(exit.cause);
          return { tag: e._tag ?? "Error", message: e.message ?? String(e) };
        })(),
      };
const cliScenario = async (name, input, steps, call) => {
  calls.length = 0;
  script = [...steps];
  const exit = await withCli(
    Effect.gen(function* () {
      const cli = yield* GitHubPullRequestCli.GitHubPullRequestCli;
      return yield* Effect.exit(
        call(cli, { cwd: "/w", host: "github.com", repository: "acme/web", number: 7, ...input }),
      );
    }),
  );
  rows.push({
    type: "cli",
    name,
    input: { host: "github.com", repository: "acme/web", number: 7, ...input },
    steps,
    calls: calls.map(record),
    unused: script.length,
    ...outcome(exit),
  });
};
const node = (id = "PR_node") => ({ body: { data: { repository: { pullRequest: { id } } } } });
const state = (over = {}) => ({
  body: {
    data: {
      repository: {
        pullRequest: {
          id: "PR_node",
          headRefOid: "abc123",
          isMergeQueueEnabled: false,
          mergeStateStatus: "CLEAN",
          baseRef: { compare: { behindBy: 2 } },
          ...over,
        },
      },
    },
  },
});
const done = { body: { data: {} } };
const action = (name, input, steps) =>
  cliScenario(`action ${name}`, input, steps, (cli, base) => cli.runPullRequestAction(base));
for (const kind of ["ready", "draft", "close", "reopen", "disable-auto-merge", "revert"]) {
  await action(kind, { action: kind }, [node(), done]);
}
await action("close node id missing", { action: "close" }, [
  { body: { data: { repository: { pullRequest: null } } } },
]);
await action("close refused", { action: "close" }, [
  node(),
  { error: "response", status: 200, githubErrors: ["Pull request is already closed"] },
]);
await action("close not found", { action: "close" }, [{ error: "notFound" }]);
await action("merge default", { action: "merge" }, [state(), done]);
for (const mergeMethod of ["merge", "squash", "rebase"])
  await action(`merge ${mergeMethod}`, { action: "merge", mergeMethod }, [state(), done]);
await action("merge queue", { action: "merge", mergeMethod: "squash" }, [
  state({ isMergeQueueEnabled: true }),
  done,
]);
await action("merge rate limited", { action: "merge" }, [
  state(),
  { error: "rateLimit", retryAt: 1790000000000 },
]);
await action("enable auto-merge clean", { action: "enable-auto-merge", mergeMethod: "squash" }, [
  state({ mergeStateStatus: "CLEAN" }),
  done,
]);
for (const status of [
  "BLOCKED",
  "BEHIND",
  "DIRTY",
  "UNKNOWN",
  "UNSTABLE",
  "HAS_HOOKS",
  "draft",
  null,
]) {
  await action(`enable auto-merge ${status}`, { action: "enable-auto-merge" }, [
    state({ mergeStateStatus: status }),
    done,
  ]);
}
await action("enable auto-merge queue", { action: "enable-auto-merge", mergeMethod: "rebase" }, [
  state({ isMergeQueueEnabled: true, mergeStateStatus: "BLOCKED" }),
  done,
]);
const body = "Fix it\n\nCo-authored-by: Claude <noreply@anthropic.com>\n";
const mergeMessage = (over = {}) => ({
  body: {
    data: {
      repository: {
        pullRequest: {
          isMergeQueueEnabled: false,
          headRefOid: "def456",
          viewerMergeBodyText: body,
          ...over,
        },
      },
    },
  },
});
await action(
  "merge credits removed squash",
  { action: "merge", mergeMethod: "squash", removeAgentCreditsOnMerge: true },
  [state(), mergeMessage(), done],
);
await action(
  "merge credits removed default",
  { action: "merge", removeAgentCreditsOnMerge: true },
  [state(), mergeMessage(), done],
);
await action(
  "merge credits rebase",
  { action: "merge", mergeMethod: "rebase", removeAgentCreditsOnMerge: true },
  [state(), done],
);
await action(
  "merge credits none to remove",
  { action: "merge", mergeMethod: "squash", removeAgentCreditsOnMerge: true },
  [state(), mergeMessage({ viewerMergeBodyText: "Fix it" }), done],
);
await action(
  "merge credits queue",
  { action: "merge", mergeMethod: "squash", removeAgentCreditsOnMerge: true },
  [state({ isMergeQueueEnabled: true }), mergeMessage({ isMergeQueueEnabled: true }), done],
);
await action(
  "auto-merge credits",
  { action: "enable-auto-merge", mergeMethod: "squash", removeAgentCreditsOnMerge: true },
  [state({ mergeStateStatus: "BLOCKED" }), mergeMessage(), done],
);
await action("update-branch behind", { action: "update-branch" }, [state(), done]);
await action("update-branch rebase", { action: "update-branch", updateMethod: "rebase" }, [
  state(),
  done,
]);
await action("update-branch merge", { action: "update-branch", updateMethod: "merge" }, [
  state(),
  done,
]);
await action("update-branch current", { action: "update-branch" }, [
  state({ baseRef: { compare: { behindBy: 0 } } }),
]);
await action(
  "update-branch no comparison",
  { action: "update-branch" },
  [state({ baseRef: null })].concat([done]),
);
await action("update-branch unreadable", { action: "update-branch" }, [{ body: "not json" }]);

// approve-workflows: the pull request must be a fork's, with a head GitHub reported in full
const crossCore = (over = {}) => ({
  body: {
    data: {
      repository: {
        mergeCommitAllowed: true,
        squashMergeAllowed: true,
        rebaseMergeAllowed: true,
        viewerPermission: "WRITE",
        pullRequest: {
          number: 7,
          title: "T",
          url: "https://github.com/acme/web/pull/7",
          headRefName: "feature",
          baseRefName: "main",
          headRefOid: "abc123",
          state: "OPEN",
          createdAt: "2026-07-01T00:00:00Z",
          updatedAt: "2026-07-02T00:00:00Z",
          viewerCanUpdate: true,
          viewerDidAuthor: false,
          viewerCanUpdateBranch: true,
          isCrossRepository: true,
          headRepositoryOwner: { login: "Fork" },
          baseRef: { compare: { behindBy: 0 } },
          reviewRequests: { nodes: [] },
          labels: { nodes: [] },
          commits: { nodes: [] },
          ...over,
        },
      },
    },
  },
});
const heads = (nodes, hasNext = false) => ({
  body: {
    data: {
      repository: {
        pullRequests: {
          pageInfo: { hasNextPage: hasNext, endCursor: hasNext ? "next" : null },
          nodes,
        },
      },
    },
  },
});
const unique = heads([
  {
    number: 7,
    headRefOid: "abc123",
    isCrossRepository: true,
    headRepositoryOwner: { login: "fork" },
  },
]);
const runs = (list) => ({ body: { workflow_runs: list } });
const runList = [
  { id: 21, name: "CI", html_url: "https://github.com/acme/web/actions/runs/21" },
  { id: 22, name: "Lint", html_url: null },
];
await action("approve same repository", { action: "approve-workflows" }, [
  { ...crossCore({ isCrossRepository: false }) },
]);
await action("approve head unavailable", { action: "approve-workflows" }, [
  crossCore({ headRefOid: null }),
]);
await action("approve owner unavailable", { action: "approve-workflows" }, [
  crossCore({ headRepositoryOwner: null }),
]);
await action("approve none waiting", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  runs([]),
]);
await action("approve two runs", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  runs(runList),
  crossCore(),
  unique,
  runs(runList),
  { body: {} },
  crossCore(),
  unique,
  runs(runList),
  { body: {} },
]);
await action("approve run no longer waiting", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  runs(runList),
  crossCore(),
  unique,
  runs([runList[1]]),
  crossCore(),
  unique,
  runs([runList[1]]),
  { body: {} },
]);
await action("approve head changed", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  runs(runList),
  crossCore({ headRefOid: "zzz999" }),
]);
await action("approve head not unique", { action: "approve-workflows" }, [
  crossCore(),
  heads([
    {
      number: 7,
      headRefOid: "abc123",
      isCrossRepository: true,
      headRepositoryOwner: { login: "fork" },
    },
    {
      number: 9,
      headRefOid: "abc123",
      isCrossRepository: true,
      headRepositoryOwner: { login: "fork" },
    },
  ]),
  runs(runList),
]);
await action("approve head other number", { action: "approve-workflows" }, [
  crossCore(),
  heads([
    {
      number: 9,
      headRefOid: "abc123",
      isCrossRepository: true,
      headRepositoryOwner: { login: "fork" },
    },
  ]),
  runs(runList),
]);
await action("approve other fork", { action: "approve-workflows" }, [
  crossCore(),
  heads([
    {
      number: 7,
      headRefOid: "abc123",
      isCrossRepository: true,
      headRepositoryOwner: { login: "someone" },
    },
  ]),
  runs([]),
]);
await action("approve approval refused", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  runs([runList[0]]),
  crossCore(),
  unique,
  runs([runList[0]]),
  { error: "response", status: 403, githubErrors: ["Resource not accessible"] },
]);
await action("approve run list truncated", { action: "approve-workflows" }, [
  crossCore(),
  unique,
  ...Array.from({ length: 11 }, (_, page) => ({
    body: {
      workflow_runs: Array.from({ length: 100 }, (_, i) => ({ id: page * 100 + i + 1, name: "w" })),
    },
  })),
]);
await action("approve head list paged", { action: "approve-workflows" }, [
  crossCore(),
  heads(
    [{ number: 3, headRefOid: "x", isCrossRepository: false, headRepositoryOwner: null }],
    true,
  ),
  unique,
  runs([]),
]);

await cliScenario(
  "labels apply",
  { labels: ["bug", "needs triage"], applied: true },
  [{ body: [] }],
  (cli, base) => cli.setLabels(base),
);
await cliScenario(
  "labels remove",
  { labels: ["bug", "needs triage", "a/b"], applied: false },
  [{ body: {} }, { body: {} }, { body: {} }],
  (cli, base) => cli.setLabels(base),
);
await cliScenario(
  "labels remove missing",
  { labels: ["bug", "gone"], applied: false },
  [{ body: {} }, { error: "notFound" }],
  (cli, base) => cli.setLabels(base),
);
await cliScenario(
  "labels apply refused",
  { labels: ["bug"], applied: true },
  [{ error: "response", status: 403, githubErrors: ["Must have triage access"] }],
  (cli, base) => cli.setLabels(base),
);
const reviewers = [
  { id: "alice", kind: "user" },
  { id: "core", kind: "team" },
];
await cliScenario(
  "reviewers request",
  { reviewers, requested: true },
  [{ body: {} }],
  (cli, base) => cli.setReviewerRequest(base),
);
await cliScenario(
  "reviewers withdraw",
  { reviewers, requested: false },
  [{ body: {} }],
  (cli, base) => cli.setReviewerRequest(base),
);
await cliScenario(
  "reviewers refused",
  { reviewers, requested: true },
  [
    {
      error: "response",
      status: 422,
      githubErrors: ["Review cannot be requested from pull request author."],
    },
  ],
  (cli, base) => cli.setReviewerRequest(base),
);
await cliScenario("reviewer candidates", {}, [{ body: reviewerData() }], (cli, base) =>
  cli.listReviewerCandidates(base),
);
await cliScenario(
  "reviewer candidates unreadable",
  {},
  [{ body: { data: { repository: {} } } }],
  (cli, base) => cli.listReviewerCandidates(base),
);
await cliScenario("label candidates", {}, [{ body: labelData() }], (cli, base) =>
  cli.listLabelCandidates(base),
);
await cliScenario("label candidates not found", {}, [{ error: "notFound" }], (cli, base) =>
  cli.listLabelCandidates(base),
);
await cliScenario(
  "workflow runs",
  {
    headSha: "abc123",
    headBranch: "feature",
    headRepositoryOwner: "fork",
    isCrossRepository: true,
  },
  [unique, runs(runList)],
  (cli, base) => cli.listWorkflowRunsRequiringApproval(base),
);
await cliScenario(
  "workflow runs branch needing escape",
  {
    headSha: "abc123",
    headBranch: "fix/a b&c",
    headRepositoryOwner: "fork",
    isCrossRepository: true,
  },
  [unique, runs(runList)],
  (cli, base) => cli.listWorkflowRunsRequiringApproval(base),
);

// ---- the provider's checks, with the workflow runs awaiting approval --------------------------
const detailOf = (over = {}) => ({
  number: 7,
  title: "T",
  url: "https://github.com/acme/web/pull/7",
  author: null,
  headBranch: "feature",
  baseBranch: "main",
  state: "open",
  isDraft: false,
  mergeability: "mergeable",
  additions: 0,
  deletions: 0,
  createdAt: "2026-07-01T00:00:00Z",
  updatedAt: "2026-07-02T00:00:00Z",
  reviewRequestLogins: [],
  labels: [],
  body: "",
  changedFiles: 0,
  mergedAt: null,
  closedAt: null,
  headSha: "abc123",
  headRepositoryOwner: "fork",
  isCrossRepository: true,
  checks: [
    {
      name: "build",
      status: "success",
      description: null,
      url: "https://github.com/acme/web/actions/runs/31/job/1",
    },
  ],
  viewerAccess: {
    canWrite: true,
    canTriage: true,
    canUpdate: true,
    didAuthor: false,
    mergeCapabilities: { merge: true, squash: true, rebase: true },
  },
  comparison: null,
  ...over,
});
const waiting = (id, name, url) => ({ id, name, url });
const providerCase = async (name, detail, waitingRuns) => {
  let askedRuns = 0;
  const cli = new Proxy(
    {},
    {
      get: (_, key) => {
        if (key === "getPullRequestDetail") return () => Effect.succeed(detail);
        if (key === "listWorkflowRunsRequiringApproval")
          return () => {
            askedRuns++;
            return waitingRuns instanceof Error
              ? Effect.fail(waitingRuns)
              : Effect.succeed(waitingRuns);
          };
        return () => Effect.die(`unexpected ${String(key)}`);
      },
    },
  );
  const result = await run(
    Effect.gen(function* () {
      const provider = yield* GitHubPullRequestProvider.make.pipe(
        Effect.provideService(GitHubPullRequestCli.GitHubPullRequestCli, cli),
      );
      return yield* Effect.exit(
        provider.getChangeRequest({
          cwd: "/w",
          repository: "acme/web",
          host: "github.com",
          number: 7,
        }),
      );
    }),
  );
  rows.push({
    type: "providerChecks",
    name,
    detail: norm(detail),
    waiting: waitingRuns instanceof Error ? { error: waitingRuns._tag } : waitingRuns,
    askedRuns,
    ...(Exit.isSuccess(result)
      ? {
          ok: true,
          checks: norm(result.value.checks),
          workflowApprovalsRequired: result.value.workflowApprovalsRequired ?? null,
        }
      : {
          ok: false,
          error: {
            tag: Cause.squash(result.cause)._tag ?? "Error",
            message: Cause.squash(result.cause).message,
          },
        }),
  });
};
const checkUrl = (id) => `https://github.com/acme/web/actions/runs/${id}/job/9`;
await providerCase("fork with runs awaiting approval", detailOf(), [
  waiting(21, "CI", "https://github.com/acme/web/actions/runs/21"),
  waiting(22, "Lint", null),
]);
await providerCase(
  "fork run already shown as a check",
  detailOf({
    checks: [{ name: "CI", status: "action-required", description: null, url: checkUrl(21) }],
  }),
  [waiting(21, "CI", "https://github.com/acme/web/actions/runs/21"), waiting(22, "Lint", null)],
);
await providerCase("fork with nothing waiting", detailOf(), []);
await providerCase("same repository", detailOf({ isCrossRepository: false }), []);
await providerCase("closed fork", detailOf({ state: "closed" }), []);
await providerCase("fork without head", detailOf({ headSha: null }), []);
await providerCase("fork without owner", detailOf({ headRepositoryOwner: null }), []);
await providerCase(
  "fork read fails",
  detailOf(),
  new GitHubApi.GitHubApiResponseError({ host: "github.com", operation: "x", status: 500 }),
);
await providerCase(
  "fork read rate limited",
  detailOf(),
  new GitHubApi.GitHubApiRateLimitError({ host: "github.com", operation: "x" }),
);

// ---- PullRequestService's gates ------------------------------------------------------------
const FULL_REVIEW = {
  inlineComment: true,
  reply: true,
  resolve: true,
  verdicts: ["comment", "approve", "request-changes"],
};
const hostCaps = {
  github: {
    diff: true,
    comment: true,
    actions: [
      "merge",
      "ready",
      "draft",
      "close",
      "reopen",
      "update-branch",
      "enable-auto-merge",
      "disable-auto-merge",
      "revert",
      "approve-workflows",
    ],
    mergeMethods: ["merge", "squash", "rebase"],
    updateMethods: ["merge", "rebase"],
    search: true,
    reactions: true,
    review: FULL_REVIEW,
    reviewers: { request: true, listCandidates: true },
    edit: { changeRequest: true, comment: true },
    labels: true,
  },
  limited: {
    diff: true,
    comment: true,
    actions: ["merge", "close"],
    mergeMethods: ["merge"],
    search: true,
    reactions: true,
    review: FULL_REVIEW,
    reviewers: { request: true, listCandidates: true },
    edit: { changeRequest: true, comment: true },
  },
  stacks: {
    diff: true,
    comment: true,
    actions: ["merge", "update-branch"],
    mergeMethods: ["merge", "squash", "rebase"],
    updateMethods: ["merge", "rebase"],
    search: true,
    reactions: true,
    review: FULL_REVIEW,
    reviewers: { request: true, listCandidates: true },
    edit: { changeRequest: true, comment: true },
    stackActions: true,
  },
  noReviewers: {
    diff: true,
    comment: true,
    actions: ["merge"],
    mergeMethods: ["merge"],
    search: true,
    reactions: true,
    review: FULL_REVIEW,
    reviewers: { request: false, listCandidates: false },
    edit: { changeRequest: true, comment: true },
  },
};
const viewers = {
  writer: {
    actions: [
      "merge",
      "enable-auto-merge",
      "disable-auto-merge",
      "revert",
      "approve-workflows",
      "ready",
      "draft",
      "close",
      "reopen",
      "update-branch",
    ],
    comment: true,
    resolve: true,
    verdicts: ["comment", "approve", "request-changes"],
    requestReviewers: true,
    updateMethods: ["merge", "rebase"],
    labels: true,
    stackRebase: true,
  },
  author: {
    actions: ["ready", "draft", "close", "reopen"],
    comment: true,
    resolve: true,
    verdicts: ["comment"],
    requestReviewers: false,
    labels: false,
  },
  reader: {
    actions: [],
    comment: true,
    resolve: false,
    verdicts: ["comment", "approve", "request-changes"],
    requestReviewers: false,
    labels: false,
  },
  mergeOnly: {
    actions: ["merge"],
    comment: true,
    resolve: false,
    verdicts: ["comment"],
    requestReviewers: false,
    labels: false,
  },
  mergeOnlyLabels: {
    actions: ["merge"],
    comment: true,
    resolve: false,
    verdicts: ["comment"],
    requestReviewers: true,
  },
  mergeUpdateOnly: {
    actions: ["merge", "update-branch"],
    comment: true,
    resolve: false,
    verdicts: ["comment"],
    requestReviewers: false,
    updateMethods: ["merge"],
  },
  noRebaseStack: {
    actions: ["merge", "update-branch"],
    comment: true,
    resolve: false,
    verdicts: ["comment"],
    requestReviewers: false,
    updateMethods: ["merge", "rebase"],
  },
};
const projectOf = {
  id: "p1",
  title: "web",
  workspaceRoot: "/w",
  repositoryIdentity: {
    canonicalKey: "github.com/acme/web",
    locator: {
      source: "git-remote",
      remoteName: "origin",
      remoteUrl: "https://github.com/acme/web.git",
    },
    provider: "github",
    displayName: "acme/web",
  },
  defaultModelSelection: null,
  scripts: [],
  createdAt: "2026-07-01T00:00:00Z",
  updatedAt: "2026-07-01T00:00:00Z",
};
const makeService = (settings, providers) =>
  Effect.flatMap(
    Layer.build(
      Layer.mergeAll(
        Layer.mock(ServerSettingsService.ServerSettingsService)({
          getSettings: Effect.succeed(settings),
        }),
        Layer.succeed(
          PullRequestProviderRegistry.PullRequestProviderRegistry,
          PullRequestProviderRegistry.fromProviders(providers),
        ),
        Layer.mock(SourceControlProviderRegistry.SourceControlProviderRegistry)({
          resolveLink: () => undefined,
          resolveHandle: () => Effect.die("unexpected"),
        }),
        Layer.mock(ProjectService.ProjectService)({
          listShells: (options) =>
            Effect.succeed([projectOf].filter((p) => options?.projectIds?.includes(p.id) ?? true)),
          getShell: (id) =>
            Effect.succeed(Option.fromNullishOr(id === projectOf.id ? projectOf : null)),
        }),
        Layer.mock(RepositoryIdentityResolver.RepositoryIdentityResolver)({
          resolve: () => Effect.succeed(null),
        }),
        SourceControlRateLimit.layer,
        PullRequestFilesViewed.layer.pipe(Layer.provide(SqlitePersistence.layerMemory)),
        Layer.effect(PullRequestReadCache.PullRequestReadCache, PullRequestReadCache.make).pipe(
          Layer.provide(KeyValueStore.layerMemory),
          Layer.provide(NodeServices.layer),
        ),
      ),
    ),
    (context) => Effect.provideContext(PullRequestService.make, context),
  );
const serviceCase = async (
  name,
  {
    caps,
    viewer,
    input,
    settings = DEFAULT_SERVER_SETTINGS,
    kind = "github",
    method = "runAction",
    failViewer = false,
    confirm = "open",
  },
) => {
  const events = [];
  const provider = {
    kind,
    capabilities: hostCaps[caps],
    getViewer: () => Effect.succeed("octo"),
    getViewerPermissions: (request) => {
      events.push({
        call: "getViewerPermissions",
        includeUpdateBranch: request.includeUpdateBranch ?? null,
      });
      return failViewer
        ? Effect.fail(
            new PullRequestProviderError({
              provider: "github",
              operation: "getViewerPermissions",
              reason: "failed",
              detail: "down",
            }),
          )
        : Effect.succeed(viewers[viewer]);
    },
    listChangeRequests: () => Effect.succeed({ items: [], truncated: false, continues: true }),
    getChangeRequest: () => Effect.die("unused"),
    getChangeRequestSummary: () => {
      events.push({ call: "getChangeRequestSummary" });
      return confirm === "fails"
        ? Effect.fail(
            new PullRequestProviderError({
              provider: "github",
              operation: "getChangeRequestSummary",
              reason: "failed",
              detail: "down",
            }),
          )
        : Effect.succeed({
            number: 7,
            title: "T",
            url: "https://github.com/acme/web/pull/7",
            headBranch: "f",
            baseBranch: "main",
            state: confirm === "merged" ? "merged" : "open",
            mergedAt: confirm === "merged" ? "2026-07-03T00:00:00Z" : null,
            updatedAt: "2026-07-03T00:00:00Z",
          });
    },
    getChangeRequestActivity: () => Effect.die("unused"),
    getDiff: () => Effect.die("unused"),
    runAction: (request) => {
      const { cwd, ...rest } = request;
      events.push({ call: "runAction", ...rest });
      return Effect.void;
    },
    updateChangeRequest: () => Effect.void,
    comment: () => Effect.void,
    updateComment: () => Effect.void,
    submitReview: () => Effect.void,
    replyToThread: () => Effect.void,
    setThreadResolution: () => Effect.void,
    setReaction: () => Effect.void,
    listReviewerCandidates: (request) => {
      events.push({ call: "listReviewerCandidates" });
      return Effect.succeed({ candidates: [], truncated: false });
    },
    setReviewerRequest: (request) => {
      events.push({
        call: "setReviewerRequest",
        reviewers: request.reviewers,
        requested: request.requested,
      });
      return Effect.void;
    },
    listLabelCandidates: () => {
      events.push({ call: "listLabelCandidates" });
      return Effect.succeed({ candidates: [], truncated: false });
    },
    setLabels: (request) => {
      events.push({ call: "setLabels", labels: request.labels, applied: request.applied });
      return Effect.void;
    },
  };
  const exit = await run(
    Effect.scoped(
      Effect.gen(function* () {
        const service = yield* makeService(settings, [provider]);
        const merges = yield* service.subscribeMerges;
        const exit = yield* Effect.exit(
          service[method]({ projectId: "p1", repository: "acme/web", number: 7, ...input }),
        );
        const merged = yield* Stream.runHead(merges).pipe(Effect.timeoutOption("20 millis"));
        if (Option.isSome(merged) && Option.isSome(merged.value))
          events.push({
            call: "mergeEvent",
            projectId: merged.value.value.projectId,
            repository: merged.value.value.repository,
            number: merged.value.value.number,
          });
        return exit;
      }),
    ),
  );
  rows.push({
    type: "service",
    name,
    caps,
    viewer,
    capabilities: hostCaps[caps],
    viewerPermissions: viewers[viewer],
    kind,
    method,
    input,
    settings: {
      removeAgentCreditsOnMerge: settings.removeAgentCreditsOnMerge,
      projectSettingsOverrides: settings.projectSettingsOverrides,
    },
    failViewer,
    events,
    ...(Exit.isSuccess(exit)
      ? { ok: true }
      : {
          ok: false,
          error: (() => {
            const e = Cause.squash(exit.cause);
            return { tag: e._tag, operation: e.operation ?? null, detail: e.detail ?? null };
          })(),
        }),
  });
};
const actionsList = [
  "merge",
  "ready",
  "draft",
  "close",
  "reopen",
  "update-branch",
  "enable-auto-merge",
  "disable-auto-merge",
  "revert",
  "approve-workflows",
];
for (const caps of ["github", "limited"])
  for (const viewer of ["writer", "author", "reader", "mergeOnly"])
    for (const act of actionsList) {
      await serviceCase(`${act} by ${viewer} on ${caps}`, { caps, viewer, input: { action: act } });
    }
for (const mergeMethod of ["merge", "squash", "rebase"])
  for (const caps of ["github", "limited"]) {
    await serviceCase(`merge ${mergeMethod} on ${caps}`, {
      caps,
      viewer: "writer",
      input: { action: "merge", mergeMethod },
    });
    await serviceCase(`auto-merge ${mergeMethod} on ${caps}`, {
      caps,
      viewer: "writer",
      input: { action: "enable-auto-merge", mergeMethod },
    });
  }
for (const updateMethod of ["merge", "rebase"])
  for (const viewer of ["writer", "mergeUpdateOnly", "mergeOnly"]) {
    await serviceCase(`update-branch ${updateMethod} by ${viewer}`, {
      caps: "github",
      viewer,
      input: { action: "update-branch", updateMethod },
    });
  }
await serviceCase("update-branch method on limited host", {
  caps: "stacks",
  viewer: "writer",
  input: { action: "update-branch", updateMethod: "rebase" },
});
await serviceCase("merge confirmed", {
  caps: "github",
  viewer: "writer",
  input: { action: "merge", mergeMethod: "squash" },
  confirm: "merged",
});
await serviceCase("merge only queued", {
  caps: "github",
  viewer: "writer",
  input: { action: "merge" },
  confirm: "open",
});
await serviceCase("merge confirmation fails", {
  caps: "github",
  viewer: "writer",
  input: { action: "merge" },
  confirm: "fails",
});
await serviceCase("close is not confirmed", {
  caps: "github",
  viewer: "writer",
  input: { action: "close" },
  confirm: "merged",
});
await serviceCase("viewer lookup fails", {
  caps: "github",
  viewer: "writer",
  input: { action: "merge" },
  failViewer: true,
});
for (const input of [
  { action: "merge", stackNumber: 3, expectedStackHeads: [{ number: 7, headSha: "abc" }] },
  { action: "merge", stackNumber: 3 },
  { action: "close", stackNumber: 3, expectedStackHeads: [{ number: 7, headSha: "abc" }] },
  {
    action: "update-branch",
    stackNumber: 3,
    expectedStackHeads: [{ number: 7, headSha: "abc" }],
    updateMethod: "merge",
  },
]) {
  await serviceCase(`stack ${JSON.stringify(input)} on github`, {
    caps: "github",
    viewer: "writer",
    input,
  });
}
const overridden = (value, override) => ({
  ...DEFAULT_SERVER_SETTINGS,
  removeAgentCreditsOnMerge: value,
  projectSettingsOverrides:
    override === undefined ? {} : { p1: { removeAgentCreditsOnMerge: override } },
});
for (const [env, override] of [
  [false, undefined],
  [true, undefined],
  [false, true],
  [true, false],
])
  for (const act of ["merge", "enable-auto-merge", "close"]) {
    await serviceCase(`credits env ${env} project ${override} ${act}`, {
      caps: "github",
      viewer: "writer",
      input: { action: act, mergeMethod: act === "close" ? undefined : "squash" },
      settings: overridden(env, override),
    });
  }
for (const caps of ["github", "noReviewers"])
  for (const viewer of ["writer", "author", "mergeOnlyLabels"]) {
    await serviceCase(`reviewer candidates ${viewer} on ${caps}`, {
      caps,
      viewer,
      method: "reviewerCandidates",
      input: {},
    });
    await serviceCase(`request reviewers ${viewer} on ${caps}`, {
      caps,
      viewer,
      method: "requestReviewers",
      input: { reviewers: [{ id: "alice", kind: "user" }], requested: true },
    });
  }
for (const caps of ["github", "limited"])
  for (const viewer of ["writer", "author", "mergeOnly", "mergeOnlyLabels"]) {
    await serviceCase(`label candidates ${viewer} on ${caps}`, {
      caps,
      viewer,
      method: "labelCandidates",
      input: {},
    });
    await serviceCase(`set labels ${viewer} on ${caps}`, {
      caps,
      viewer,
      method: "setLabels",
      input: { labels: ["bug"], applied: false },
    });
  }

const target = new URL(
    "../crates/server/tests/fixtures/pull-request-actions.jsonl",
    import.meta.url,
  ),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(
  JSON.stringify({
    rows: rows.length,
    byType: Object.fromEntries(
      [...new Set(rows.map((r) => r.type))].map((t) => [
        t,
        rows.filter((r) => r.type === t).length,
      ]),
    ),
  }),
);
