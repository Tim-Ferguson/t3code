// Runs the original GitHub source-control layer and records what it produces:
//  - the pure readers (auth status JSON, account choice, environment tokens, API URLs, base
//    repository selection, pull request row decoding, branch naming);
//  - GitHubApi against scripted HTTP answers (classification of every kind of refusal, the
//    rate-limit pause, token invalidation);
//  - GitHubCli operations against scripted HTTP answers and a scripted `git remote -v`
//    (repository resolution, every request it sends, the errors it reports);
//  - the provider-level error envelope a failed operation reaches the wire as.
// The Rust tests replay the rows against a local HTTP server and temporary git repositories.
// No network is used and no real process is spawned.
import { writeFileSync, renameSync, mkdtempSync, writeFileSync as write } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Redacted from "../../apps/server/node_modules/effect/dist/Redacted.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Fiber from "../../apps/server/node_modules/effect/dist/Fiber.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as TestClock from "../../apps/server/node_modules/effect/dist/testing/TestClock.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import {
  HttpClient,
  HttpClientResponse,
} from "../../apps/server/node_modules/effect/dist/http/index.js";
import { ChildProcessSpawner } from "../../apps/server/node_modules/effect/dist/process/index.js";
import * as GitHubApi from "../../apps/server/src/sourceControl/GitHubApi.ts";
import * as GitHubCli from "../../apps/server/src/sourceControl/GitHubCli.ts";
import * as GitHubCredentials from "../../apps/server/src/sourceControl/GitHubCredentials.ts";
import * as GitHubGraphQlBudget from "../../apps/server/src/sourceControl/githubGraphQlBudget.ts";
import * as SourceControlRateLimit from "../../apps/server/src/sourceControl/SourceControlRateLimit.ts";
import * as GitHubSourceControlProvider from "../../apps/server/src/sourceControl/GitHubSourceControlProvider.ts";
import {
  parseGitHubAuthStatus,
  effectiveGitHubAccount,
} from "../../apps/server/src/sourceControl/gitHubAuthStatus.ts";
import { decodeGitHubPullRequestEntries } from "../../apps/server/src/sourceControl/gitHubPullRequests.ts";
import * as GitVcsDriver from "../../apps/server/src/vcs/GitVcsDriver.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as Contracts from "../../packages/contracts/src/sourceControl.ts";
import { DEFAULT_SERVER_SETTINGS } from "../../packages/contracts/src/settings.ts";
import { HostProcessEnvironment } from "../../packages/shared/src/hostProcess.ts";

const rows = [];
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));
const NOW = Date.parse("2026-10-05T12:00:00.000Z");

// ---- pure readers --------------------------------------------------------------------------
const account = (overrides = {}) => ({
  state: "success",
  active: true,
  host: "github.com",
  login: "octo",
  tokenSource: "keyring",
  ...overrides,
});
const statusDocs = [
  { hosts: { "github.com": [account()] } },
  { hosts: { "github.com": [account({ active: false, login: "second" }), account()] } },
  { hosts: { "github.com": [account({ state: "error", error: "  token expired  " })] } },
  { hosts: { "github.com": [account({ tokenSource: "GH_TOKEN" })] } },
  {
    hosts: {
      "github.com": [account({ tokenSource: "GITHUB_ENTERPRISE_TOKEN", host: "GHE.Acme.test" })],
      "ghe.acme.test": [account({ host: "ghe.acme.test", login: "work", active: false })],
    },
  },
  {
    hosts: {
      "github.com": [
        account({ login: "  " }),
        account({ host: " ", login: "x" }),
        account({ login: " padded " }),
      ],
    },
  },
  { hosts: {} },
  { hosts: { "github.com": [] } },
  { hosts: { "github.com": [{ state: "success", active: true, host: "github.com" }] } },
  { hosts: { "github.com": [account({ error: null })] } },
  { hosts: { "github.com": [account({ tokenSource: null })] } },
  { hosts: { "github.com": "nope" } },
  { hosts: [] },
  { hosts: null },
  {},
  [],
  "text",
  null,
  { hosts: { "github.com": [account({ active: "yes" })] } },
  { hosts: { "github.com": [account(), 5] } },
];
const authTexts = [
  ...statusDocs.map((doc) => JSON.stringify(doc)),
  "",
  "not json",
  "{",
  "Logged in to github.com",
  "unknown flag: --json",
];
for (const text of authTexts)
  rows.push({ type: "authStatus", text, result: parseGitHubAuthStatus(text) });

const settingsList = [
  { hosts: {}, tokens: {} },
  { hosts: { "github.com": { enabled: false } }, tokens: {} },
  { hosts: { "github.com": { account: "second", enabled: true } }, tokens: {} },
  { hosts: { "github.com": { account: "gone", enabled: true } }, tokens: {} },
  { hosts: { "ghe.acme.test": { enabled: false } }, tokens: {} },
  { hosts: { "github.com": { enabled: false }, "ghe.acme.test": { enabled: false } }, tokens: {} },
];
const authCases = [
  ["", "", 0],
  ["", "", 1],
  ["oops", "", 0],
  ["", "not logged in\nTOKEN: x\nrun gh auth login", 1],
  ["", "unknown flag: --json\nUsage: gh auth status", 1],
  ["", "unknown flag: --json", 0],
  ["Token: x\n", "", 1],
  ["   ", "   ", 1],
  ...statusDocs.slice(0, 11).map((doc) => [JSON.stringify(doc), "", 0]),
  [JSON.stringify(statusDocs[0]), "warning: something\n", 1],
  [JSON.stringify({ hosts: { "github.com": [account({ state: "error", error: "bad" })] } }), "", 1],
  [JSON.stringify({ hosts: { "github.com": [] } }), "", 1],
];
const { parseGitHubAuth } = GitHubSourceControlProvider;
for (const [stdout, stderr, exitCode] of authCases) {
  for (const settings of settingsList) {
    const full = { ...DEFAULT_SERVER_SETTINGS.github, ...settings };
    rows.push({
      type: "parseAuth",
      stdout,
      stderr,
      exitCode,
      settings,
      result: jsonOf(
        Contracts.SourceControlProviderAuth,
        parseGitHubAuth({ stdout, stderr, exitCode }, full),
      ),
    });
  }
}
for (const doc of statusDocs.slice(0, 7)) {
  const parsed = parseGitHubAuthStatus(JSON.stringify(doc));
  for (const settings of settingsList) {
    for (const host of ["github.com", "ghe.acme.test", "other"]) {
      const choice = effectiveGitHubAccount(host, parsed.accounts, {
        ...DEFAULT_SERVER_SETTINGS.github,
        ...settings,
      });
      rows.push({
        type: "effective",
        text: JSON.stringify(doc),
        settings,
        host,
        result: choice ?? null,
      });
    }
  }
}

const envs = [
  {},
  {
    GH_TOKEN: "gh",
    GITHUB_TOKEN: "github",
    GH_ENTERPRISE_TOKEN: "ghe",
    GITHUB_ENTERPRISE_TOKEN: "ghe2",
  },
  { GITHUB_TOKEN: "github" },
  { GH_TOKEN: "  " },
  { GH_TOKEN: " padded " },
  { GH_HOST: "git.acme.internal", GH_ENTERPRISE_TOKEN: "ghe" },
  { GH_HOST: " GIT.Acme.Internal ", GITHUB_ENTERPRISE_TOKEN: "ghe2" },
  { GH_ENTERPRISE_TOKEN: "ghe" },
  { GH_HOST: "other.example", GH_ENTERPRISE_TOKEN: "ghe" },
];
for (const env of envs)
  for (const host of [
    "github.com",
    "acme.ghe.com",
    "git.acme.internal",
    "ghe.com",
    "other.example",
  ]) {
    rows.push({
      type: "envToken",
      env,
      host,
      result: GitHubCredentials.environmentToken(host, env),
    });
  }
for (const host of [
  "github.com",
  "GitHub.com",
  " github.com ",
  "acme.ghe.com",
  "git.acme.internal",
  "ghe.com",
  "X.GHE.COM:8443",
]) {
  rows.push({ type: "apiUrls", host, result: GitHubApi.gitHubApiUrls(host) });
}

const remote = (name, url) => `${name}\t${url} (fetch)\n${name}\t${url} (push)`;
const baseCases = [
  { remotes: remote("origin", "git@github.com:acme/web.git"), resolved: "", host: "github.com" },
  {
    remotes: remote("origin", "git@github.com:acme/web.git"),
    resolved: "remote.origin.gh-resolved base",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("upstream", "git@github.com:acme/web.git")}`,
    resolved: "",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("upstream", "git@github.com:acme/web.git")}`,
    resolved: "remote.origin.gh-resolved base",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("upstream", "git@github.com:acme/web.git")}`,
    resolved: "remote.upstream.gh-resolved Acme/Other",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("upstream", "git@github.com:acme/web.git")}`,
    resolved: "remote.origin.gh-resolved base\nremote.upstream.gh-resolved base",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("fork", "git@github.com:you/web.git")}`,
    resolved: "",
    host: "github.com",
  },
  {
    remotes: `${remote("one", "git@github.com:me/web.git")}\n${remote("two", "git@github.com:you/web.git")}`,
    resolved: "",
    host: "github.com",
  },
  {
    remotes: `${remote("ORIGIN", "git@github.com:me/web.git")}\n${remote("GitHub", "git@github.com:you/web.git")}`,
    resolved: "",
    host: "github.com",
  },
  {
    remotes: `${remote("origin", "git@github.com:me/web.git")}\n${remote("other", "git@gitlab.com:you/web.git")}`,
    resolved: "",
    host: "github.com",
  },
  { remotes: remote("origin", "git@github-work:acme/web.git"), resolved: "", host: "github.com" },
  {
    remotes: remote("origin", "https://github.example.com/acme/web"),
    resolved: "",
    host: "github.example.com",
  },
  {
    remotes: remote("origin", "https://github.com/acme/group/web"),
    resolved: "",
    host: "github.com",
  },
  { remotes: "", resolved: "", host: "github.com" },
  { remotes: "garbage\nmore garbage", resolved: "", host: "github.com" },
  {
    remotes: remote("origin", "git@github.com:acme/web.git"),
    resolved: "remote.gone.gh-resolved base",
    host: "github.com",
  },
  {
    remotes: remote("origin", "git@github.com:acme/web.git"),
    resolved: "remote.origin.gh-resolved a/b/c",
    host: "github.com",
  },
  {
    remotes: remote("origin", "git@github.com:acme/web.git"),
    resolved: "remote.origin.gh-resolved nonsense",
    host: "github.com",
  },
  { remotes: remote("origin", "git@github.com:acme/web.git"), resolved: "", host: "GitHub.com" },
];
for (const input of baseCases)
  rows.push({ type: "baseRepository", input, result: GitHubCli.selectGitHubBaseRepository(input) });
for (const url of [
  "git@github.com:a/b.git",
  "git@github-work:a/b.git",
  "git@work:a/b.git",
  "ssh://git@github.example.com/a/b",
  "https://github.example.com/a/b",
  "git@gitlab.com:a/b.git",
  "https://gitlab.com/a/b",
  "git@alias:a/b.git",
  "ssh://alias/a/b",
  "https://github.com/a/b",
  "file:///x/y",
  "",
  "git@acme.ghe.com:a/b.git",
  "https://git.github.acme.test:8443/a/b",
  "git@github.corp.example:a/b.git",
])
  rows.push({ type: "apiHostForRemote", url, result: GitHubCli.gitHubApiHostForRemote(url) });
for (const input of [
  { headRefName: "feature", headOwner: "fork", isCrossRepository: true, defaultBranch: "main" },
  { headRefName: "main", headOwner: "fork", isCrossRepository: true, defaultBranch: "main" },
  { headRefName: "main", headOwner: "fork", isCrossRepository: false, defaultBranch: "main" },
  { headRefName: "main", headOwner: null, isCrossRepository: true, defaultBranch: "main" },
  { headRefName: "main", headOwner: "fork", isCrossRepository: true, defaultBranch: null },
  { headRefName: "dev", headOwner: "fork", isCrossRepository: true, defaultBranch: "main" },
])
  rows.push({
    type: "checkoutBranch",
    input,
    result: GitHubCli.pullRequestCheckoutBranchName(input),
  });

const prRows = [
  {
    number: 7,
    title: "  Title  ",
    url: " https://github.com/acme/web/pull/7 ",
    baseRefName: " main ",
    headRefName: " feature ",
    state: "OPEN",
    updatedAt: "2026-01-02T03:04:05Z",
  },
  {
    number: 8,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    state: "MERGED",
    mergedAt: "2026-01-03T00:00:00Z",
    closedAt: "2026-01-03T00:00:00Z",
    isDraft: true,
    headRefOid: " abc123 ",
    isCrossRepository: true,
    headRepository: { nameWithOwner: "fork/web", name: "web" },
    headRepositoryOwner: { login: "fork" },
  },
  {
    number: 9,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    state: "closed",
    mergedAt: "  ",
    closedAt: null,
    updatedAt: null,
    isDraft: false,
  },
  {
    number: 10,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    state: null,
    mergedAt: "2026-01-01T00:00:00Z",
  },
  {
    number: 11,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepository: { name: "web" },
    headRepositoryOwner: { login: " fork " },
  },
  {
    number: 12,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepository: { nameWithOwner: "fork/web" },
  },
  {
    number: 13,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepository: { nameWithOwner: "/web" },
  },
  {
    number: 14,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepository: null,
    headRepositoryOwner: null,
  },
  {
    number: 15,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepository: { nameWithOwner: "  ", name: "x" },
  },
  {
    number: 16,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRefOid: null,
    updatedAt: "2026-02-30T00:00:00Z",
  },
  { number: 17, title: "T", url: "u", baseRefName: "b", headRefName: "h", updatedAt: "not a date" },
  { number: 0, title: "T", url: "u", baseRefName: "b", headRefName: "h" },
  { number: -1, title: "T", url: "u", baseRefName: "b", headRefName: "h" },
  { number: 1.5, title: "T", url: "u", baseRefName: "b", headRefName: "h" },
  { number: "3", title: "T", url: "u", baseRefName: "b", headRefName: "h" },
  { number: 18, title: "   ", url: "u", baseRefName: "b", headRefName: "h" },
  { number: 19, title: "T", url: "", baseRefName: "b", headRefName: "h" },
  { number: 20, title: "T", url: "u", baseRefName: "b" },
  { number: 21, title: "T", url: "u", baseRefName: "b", headRefName: "h", isDraft: "yes" },
  { number: 22, title: "T", url: "u", baseRefName: "b", headRefName: "h", isCrossRepository: "no" },
  { number: 23, title: "T", url: "u", baseRefName: "b", headRefName: "h", state: 5 },
  { number: 24, title: "T", url: "u", baseRefName: "b", headRefName: "h", headRepository: "x" },
  {
    number: 25,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    headRepositoryOwner: { login: 5 },
  },
  { number: 26, title: "T", url: "u", baseRefName: "b", headRefName: "h", updatedAt: "2026-01-02" },
  {
    number: 27,
    title: "T",
    url: "u",
    baseRefName: "b",
    headRefName: "h",
    updatedAt: "2026-01-02T03:04:05.678+02:00",
  },
  5,
  "x",
  null,
  [],
];
const recordJson = (record) => ({
  ...record,
  provider: "github",
  updatedAt: Option.isSome(record.updatedAt)
    ? { _tag: "Some", value: DateTime.formatIso(record.updatedAt.value) }
    : { _tag: "None" },
});
rows.push({
  type: "pullRequests",
  entries: prRows,
  result: decodeGitHubPullRequestEntries(prRows).map(recordJson),
});
for (const entry of prRows)
  rows.push({
    type: "pullRequests",
    entries: [entry],
    result: decodeGitHubPullRequestEntries([entry]).map(recordJson),
  });

// ---- GitHubApi against scripted answers -------------------------------------------------------
const encodeAny = Schema.encodeSync(Schema.fromJsonString(Schema.Unknown));
const respond = (script, requests) => (request) => {
  const step = script[Math.min(requests.length - 1, script.length - 1)];
  if (step.network) throw new Error("connect ECONNREFUSED");
  const body =
    step.raw !== undefined ? step.raw : step.body === undefined ? null : JSON.stringify(step.body);
  return new Response(step.status === 204 || step.status === 304 ? null : body, {
    status: step.status ?? 200,
    headers: { "content-type": "application/json", ...step.headers },
  });
};
function apiLayer(script, { tokens = ["first", "second"], credentialError } = {}) {
  const requests = [];
  let remaining = [...tokens];
  let invalidations = 0;
  const credentials = Layer.succeed(
    GitHubCredentials.GitHubCredentials,
    GitHubCredentials.GitHubCredentials.of({
      get: (host) =>
        credentialError
          ? Effect.fail(credentialError(host))
          : Effect.sync(() => ({
              host,
              token: Redacted.make(remaining[0]),
              source: "gh",
              fingerprint: `${host}:${remaining[0]}`,
            })),
      invalidate: () =>
        Effect.sync(() => {
          invalidations++;
          remaining = remaining.slice(1);
        }),
    }),
  );
  const http = Layer.succeed(
    HttpClient.HttpClient,
    HttpClient.make((request) =>
      Effect.suspend(() => {
        requests.push(request);
        try {
          return Effect.succeed(
            HttpClientResponse.fromWeb(request, respond(script, requests)(request)),
          );
        } catch (error) {
          return Effect.die(error);
        }
      }),
    ),
  );
  const layer = GitHubApi.layer.pipe(
    Layer.provide(Layer.mergeAll(credentials, http)),
    Layer.provideMerge(GitHubGraphQlBudget.layer),
    Layer.provideMerge(SourceControlRateLimit.layer),
  );
  return { layer, requests, invalidations: () => invalidations };
}
const describeApiError = (error) => ({
  _tag: error._tag,
  message: error.message,
  ...(error.retryAt !== undefined ? { retryAt: error.retryAt } : {}),
  ...(error.status !== undefined ? { status: error.status } : {}),
});
const requestJson = (request) => ({
  method: request.method,
  url: request.url,
  authorization: request.headers.authorization ?? null,
  accept: request.headers.accept ?? null,
  ifNoneMatch: request.headers["if-none-match"] ?? null,
  body:
    request.body && request.body._tag === "Uint8Array"
      ? JSON.parse(new TextDecoder().decode(request.body.body))
      : null,
});
async function runApi(scenario) {
  const { layer, requests, invalidations } = apiLayer(scenario.script, scenario.options);
  const results = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      yield* TestClock.setTime(NOW);
      const api = yield* GitHubApi.GitHubApi;
      for (const call of scenario.calls) {
        const run = call.kind === "graphql" ? api.graphql(call.input) : api.rest(call.input);
        const effect = call.interactive
          ? run.pipe(Effect.provideService(GitHubApi.AllowGitHubReserve, true))
          : run;
        const exit = yield* Effect.exit(effect);
        if (exit._tag === "Success") {
          results.push(
            typeof exit.value === "string"
              ? { ok: { body: exit.value } }
              : {
                  ok: {
                    status: exit.value.status,
                    body: exit.value.body,
                    truncated: exit.value.truncated,
                  },
                },
          );
        } else {
          const failure = exit.cause.reasons[0];
          results.push({
            error: failure.error
              ? describeApiError(failure.error)
              : { defect: String(failure.defect) },
          });
        }
      }
    }).pipe(Effect.provide(layer), Effect.provide(TestClock.layer())),
  );
  return { results, requests: requests.map(requestJson), invalidations: invalidations() };
}
const QUERY = "query { viewer { login } }";
const rest = (path = "repos/acme/web/pulls", extra = {}) => ({
  kind: "rest",
  input: { host: "github.com", operation: "op", path, ...extra },
});
const gql = (query = QUERY, extra = {}) => ({
  kind: "graphql",
  input: { host: "github.com", operation: "op", query, ...extra },
});
const reset = Math.floor(NOW / 1000) + 600;
const rateHeaders = { "x-ratelimit-remaining": "0", "x-ratelimit-reset": String(reset) };
const apiScenarios = [
  {
    name: "graphql ok with budget",
    script: [
      {
        body: {
          data: {
            viewer: { login: "julius" },
            rateLimit: { cost: 1, limit: 5000, remaining: 4999, resetAt: "2026-10-05T13:00:00Z" },
          },
        },
      },
    ],
    calls: [gql()],
  },
  {
    name: "graphql ok truncated",
    script: [{ raw: "x".repeat(200) }],
    calls: [gql(QUERY, { maxResponseBytes: 50 })],
  },
  {
    name: "graphql errors with reason",
    script: [
      { body: { data: null, errors: [{ type: "FORBIDDEN", message: "Resource not accessible" }] } },
    ],
    calls: [gql()],
  },
  {
    name: "graphql errors without messages",
    script: [{ body: { errors: [{ type: "FORBIDDEN" }] } }],
    calls: [gql()],
  },
  {
    name: "graphql not found",
    script: [
      { body: { data: null, errors: [{ type: "NOT_FOUND", message: "Could not resolve" }] } },
    ],
    calls: [gql()],
  },
  {
    name: "graphql not found plus other",
    script: [
      {
        body: {
          errors: [{ type: "NOT_FOUND", message: "x" }, { message: "Something went wrong" }],
        },
      },
    ],
    calls: [gql()],
  },
  { name: "graphql errors empty array", script: [{ body: { errors: [] } }], calls: [gql()] },
  {
    name: "graphql errors wrong shape",
    script: [{ body: { errors: [{ type: 5 }] } }],
    calls: [gql()],
  },
  {
    name: "graphql rate limited typed pause",
    script: [
      {
        headers: rateHeaders,
        body: { errors: [{ type: "RATE_LIMITED", message: "API rate limit exceeded" }] },
      },
      { body: {} },
    ],
    calls: [gql(), rest("repos/acme/web/stacks")],
  },
  {
    name: "graphql untyped quota",
    script: [
      {
        headers: rateHeaders,
        body: { errors: [{ message: "API rate limit already exceeded for user ID 1." }] },
      },
    ],
    calls: [gql()],
  },
  {
    name: "graphql untyped quota message only",
    script: [{ body: { errors: [{ message: "API rate limit exceeded for x" }] } }],
    calls: [gql()],
  },
  {
    name: "graphql error with exhausted header",
    script: [
      { headers: { "x-ratelimit-remaining": "0" }, body: { errors: [{ message: "other" }] } },
    ],
    calls: [gql()],
  },
  {
    name: "graphql 429 retry-after",
    script: [{ status: 429, headers: { "retry-after": "120" }, body: {} }],
    calls: [gql()],
  },
  {
    name: "graphql 429 no hint",
    script: [{ status: 429, body: {} }, { body: {} }, { body: {} }],
    calls: [gql(), gql(), gql()],
  },
  {
    name: "graphql 401 then new token",
    script: [{ status: 401, body: {} }, { body: { data: {} } }],
    calls: [gql(), gql()],
  },
  { name: "graphql 500", script: [{ status: 500, raw: "oops" }], calls: [gql()] },
  {
    name: "graphql 502 with json message",
    script: [{ status: 502, body: { message: "Bad gateway" } }],
    calls: [gql()],
  },
  {
    name: "graphql 404",
    script: [{ status: 404, body: { message: "Not Found" } }],
    calls: [gql()],
  },
  {
    name: "graphql mutation unchanged",
    script: [{ body: { data: {} } }],
    calls: [gql("mutation { x }")],
  },
  {
    name: "graphql shorthand read",
    script: [{ body: { data: {} } }],
    calls: [gql("{ viewer { login } }")],
  },
  { name: "rest ok", script: [{ body: { ok: true } }], calls: [rest()] },
  {
    name: "rest ok no content",
    script: [{ status: 204 }],
    calls: [rest("repos/acme/web/x", { method: "DELETE" })],
  },
  {
    name: "rest 422 with reasons",
    script: [
      {
        status: 422,
        body: {
          message: "Validation Failed",
          errors: [
            {
              resource: "PullRequest",
              code: "custom",
              message: "A pull request already exists for acme:feature.",
            },
          ],
        },
      },
    ],
    calls: [rest("repos/acme/web/pulls", { method: "POST", body: { title: "t" } })],
  },
  {
    name: "rest 422 field and code",
    script: [
      {
        status: 422,
        body: {
          message: "Validation Failed",
          errors: [
            { resource: "Issue", field: "title", code: "missing_field" },
            "plain",
            { resource: "x" },
          ],
        },
      },
    ],
    calls: [rest()],
  },
  {
    name: "rest 422 bad errors shape",
    script: [{ status: 422, body: { message: "Validation Failed", errors: "nope" } }],
    calls: [rest()],
  },
  { name: "rest 422 no body", script: [{ status: 422 }], calls: [rest()] },
  { name: "rest 400 not json", script: [{ status: 400, raw: "<html>" }], calls: [rest()] },
  {
    name: "rest 403 exhausted quota",
    script: [{ status: 403, headers: rateHeaders, body: { message: "API rate limit exceeded" } }],
    calls: [rest()],
  },
  {
    name: "rest 403 secondary limit",
    script: [{ status: 403, headers: { "retry-after": "60" }, body: { message: "secondary" } }],
    calls: [rest()],
  },
  {
    name: "rest 403 rate limit text",
    script: [{ status: 403, body: { message: "You have exceeded a secondary rate limit" } }],
    calls: [rest()],
  },
  {
    name: "rest 403 plain",
    script: [{ status: 403, body: { message: "Resource not accessible by integration" } }],
    calls: [rest()],
  },
  {
    name: "rest 304 with etag",
    script: [{ status: 304 }],
    calls: [rest("repos/acme/web", { ifNoneMatch: '"abc"' })],
  },
  { name: "rest 304 without etag", script: [{ status: 304 }], calls: [rest()] },
  {
    name: "rest 401 refuses",
    script: [
      { status: 401, body: {} },
      { status: 401, body: {} },
    ],
    calls: [rest(), rest()],
  },
  { name: "rest 404", script: [{ status: 404, body: {} }], calls: [rest()] },
  {
    name: "rest accept override and path slash",
    script: [{ body: {} }],
    calls: [rest("/repos/acme/web/pulls/7.diff", { accept: "application/vnd.github.diff" })],
  },
  {
    name: "rest ghe host urls",
    script: [{ body: {} }, { body: {} }, { body: {} }],
    calls: [
      { kind: "rest", input: { host: "git.acme.internal", operation: "op", path: "user" } },
      { kind: "rest", input: { host: "acme.ghe.com", operation: "op", path: "user" } },
      { kind: "graphql", input: { host: "Git.Acme.Internal", operation: "op", query: QUERY } },
    ],
  },
  {
    name: "interactive passes a pause",
    script: [
      { status: 403, headers: rateHeaders, body: { message: "API rate limit exceeded" } },
      { body: { ok: true } },
      { body: { ok: true } },
    ],
    calls: [
      rest(),
      rest(),
      { ...rest("repos/acme/web/pulls/7/merge", { method: "PUT" }), interactive: true },
    ],
  },
  {
    name: "credential missing",
    script: [{ body: {} }],
    options: { credentialError: (host) => new GitHubCredentials.GitHubCliMissingError({ host }) },
    calls: [rest(), gql()],
  },
  {
    name: "credential not signed in",
    script: [{ body: {} }],
    options: { credentialError: (host) => new GitHubCredentials.GitHubNotSignedInError({ host }) },
    calls: [rest()],
  },
  {
    name: "credential host disabled",
    script: [{ body: {} }],
    options: { credentialError: (host) => new GitHubCredentials.GitHubHostDisabledError({ host }) },
    calls: [rest()],
  },
  {
    name: "credential gh failed",
    script: [{ body: {} }],
    options: {
      credentialError: (host) =>
        new GitHubCredentials.GitHubCliFailedError({ host, cause: new Error("boom") }),
    },
    calls: [rest()],
  },
];
for (const scenario of apiScenarios) {
  const outcome = await runApi(scenario);
  rows.push({
    type: "api",
    name: scenario.name,
    script: scenario.script,
    calls: scenario.calls,
    credentialError: scenario.options?.credentialError
      ? scenario.options.credentialError("github.com")._tag
      : null,
    ...outcome,
  });
}

// ---- the provider over GitHubCli over GitHubApi, with scripted git and HTTP ---------------------------
const processOutput = (stdout, exitCode = 0) => ({
  exitCode: ChildProcessSpawner.ExitCode(exitCode),
  stdout,
  stderr: "",
  stdoutTruncated: false,
  stderrTruncated: false,
});
function providerHarness(scenario) {
  const { layer: apiLayerValue, requests } = apiLayer(scenario.script ?? [{ body: {} }]);
  const processLayer = Layer.mock(VcsProcess.VcsProcess)({
    run: (args) =>
      Effect.succeed(
        args.args[0] === "remote"
          ? processOutput(scenario.remotes ?? "")
          : args.args[0] === "config" && scenario.resolved
            ? processOutput(scenario.resolved)
            : processOutput("", 1),
      ),
  });
  const driver = Layer.mock(GitVcsDriver.GitVcsDriver)({});
  const env = Layer.succeed(HostProcessEnvironment, scenario.env ?? {});
  const cli = Layer.effect(GitHubCli.GitHubCli, GitHubCli.make).pipe(
    Layer.provide(Layer.mergeAll(driver, processLayer, apiLayerValue, env, NodeServices.layer)),
  );
  return { cli, apiLayerValue, requests, env };
}
const remoteLines = (...entries) =>
  entries.map(([name, url]) => `${name}\t${url} (fetch)\n${name}\t${url} (push)`).join("\n");
const ctx = (baseUrl = "https://github.com") => ({
  provider: { kind: "github", name: "GitHub", baseUrl },
  remoteName: "origin",
  remoteUrl: "git@github.com:acme/web.git",
});
const node = (number, headRefName, owner = "acme", extra = {}) => ({
  number,
  title: `PR ${number}`,
  url: `https://github.com/acme/web/pull/${number}`,
  baseRefName: "main",
  headRefName,
  state: "OPEN",
  isCrossRepository: owner !== "acme",
  updatedAt: "2026-01-02T00:00:00Z",
  headRepository: { name: "web", nameWithOwner: `${owner}/web` },
  headRepositoryOwner: { login: owner },
  ...extra,
});
const listBody = (...nodes) => ({ data: { repository: { h0: { nodes } } } });
const byNumber = (n) => ({ data: { repository: { pullRequest: n } } });
const repoBody = (fullName = "acme/web", extra = {}) => ({
  full_name: fullName,
  html_url: `https://github.com/${fullName}`,
  ssh_url: `git@github.com:${fullName}.git`,
  default_branch: "main",
  ...extra,
});
const origin = remoteLines(["origin", "git@github.com:acme/web.git"]);
const bodyFileText = "Body line 1\nBody line 2 with ünïcode";
const providerScenarios = [
  {
    name: "list open by branch",
    remotes: origin,
    script: [{ body: listBody(node(1, "feature")) }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" }, context: ctx() }],
  },
  {
    name: "list open with limit and padded selector",
    remotes: origin,
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "  feature  ", state: "open", limit: 5 } }],
  },
  {
    name: "list closed",
    remotes: origin,
    script: [
      {
        body: listBody(
          node(2, "feature", "acme", { state: "CLOSED", closedAt: "2026-01-03T00:00:00Z" }),
        ),
      },
    ],
    calls: [{ op: "list", input: { headSelector: "feature", state: "closed", limit: 3 } }],
  },
  {
    name: "list merged default limit",
    remotes: origin,
    script: [
      {
        body: listBody(
          node(3, "feature", "acme", { state: "MERGED", mergedAt: "2026-01-03T00:00:00Z" }),
        ),
      },
    ],
    calls: [{ op: "list", input: { headSelector: "feature", state: "merged" } }],
  },
  {
    name: "list all clamps limit",
    remotes: origin,
    script: [{ body: listBody() }, { body: listBody() }, { body: listBody() }],
    calls: [
      { op: "list", input: { headSelector: "feature", state: "all", limit: 0 } },
      { op: "list", input: { headSelector: "feature", state: "all", limit: 500 } },
      { op: "list", input: { headSelector: "feature", state: "all", limit: 7.9 } },
    ],
  },
  {
    name: "list fork head filters by owner",
    remotes: origin,
    script: [
      {
        body: listBody(
          node(4, "main", "someone"),
          node(5, "main", "Fork"),
          node(6, "main", "fork"),
        ),
      },
    ],
    calls: [
      { op: "list", input: { headSelector: "fork:main", state: "open", limit: 5 } },
      { op: "list", input: { headSelector: "fork:main", state: "all", limit: 1 } },
    ],
  },
  {
    name: "list skips malformed rows",
    remotes: origin,
    script: [{ body: listBody({ number: "x" }, node(8, "feature"), 5) }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list repository gone",
    remotes: origin,
    script: [{ body: { data: { repository: null } } }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list alias missing is empty",
    remotes: origin,
    script: [
      { body: { data: { repository: {} } } },
      { body: { data: { repository: { h0: null } } } },
    ],
    calls: [
      { op: "list", input: { headSelector: "feature", state: "open" } },
      { op: "list", input: { headSelector: "feature", state: "open" } },
    ],
  },
  {
    name: "list bad shape",
    remotes: origin,
    script: [
      { body: { data: {} } },
      { body: { nope: 1 } },
      { raw: "not json" },
      { body: { data: { repository: { h0: { nodes: "x" } } } } },
    ],
    calls: [
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "all" } },
    ],
  },
  {
    name: "list api failures",
    remotes: origin,
    script: [
      { status: 401, body: {} },
      { status: 404, body: {} },
      { headers: rateHeaders, status: 403, body: { message: "API rate limit exceeded" } },
      { status: 500, raw: "x" },
    ],
    calls: [
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
    ],
  },
  {
    name: "list no github remote",
    remotes: remoteLines(["origin", "git@gitlab.com:acme/web.git"]),
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list ssh alias remote",
    remotes: remoteLines(["origin", "git@github-work:acme/web.git"]),
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list enterprise remote",
    remotes: remoteLines(["origin", "git@github.acme.corp:team/app.git"]),
    script: [{ body: listBody() }],
    calls: [
      {
        op: "list",
        input: { headSelector: "feature", state: "open" },
        context: ctx("https://github.acme.corp"),
      },
    ],
  },
  {
    name: "list GH_REPO",
    remotes: "",
    env: { GH_REPO: "other/thing" },
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list GH_REPO with host and GH_HOST",
    remotes: "",
    env: { GH_REPO: "github.acme.corp/other/thing", GH_HOST: "ignored.example" },
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list GH_REPO unusable falls back to remotes",
    remotes: origin,
    env: { GH_REPO: "a/b/c/d" },
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list GH_HOST default host without remotes",
    remotes: "",
    env: { GH_HOST: "Git.Acme.Corp" },
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list upstream preferred",
    remotes: remoteLines(
      ["origin", "git@github.com:me/web.git"],
      ["upstream", "git@github.com:acme/web.git"],
    ),
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list gh-resolved mark",
    remotes: remoteLines(
      ["origin", "git@github.com:me/web.git"],
      ["fork", "git@github.com:acme/web.git"],
    ),
    resolved: "remote.fork.gh-resolved base",
    script: [{ body: listBody() }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" } }],
  },
  {
    name: "list context host wins",
    remotes: remoteLines(
      ["origin", "git@github.com:acme/web.git"],
      ["ghe", "git@github.acme.corp:team/app.git"],
    ),
    script: [{ body: listBody() }],
    calls: [
      {
        op: "list",
        input: { headSelector: "feature", state: "open" },
        context: ctx("https://github.acme.corp"),
      },
    ],
  },
  {
    name: "get by number",
    remotes: origin,
    script: [{ body: byNumber(node(7, "feature")) }],
    calls: [
      { op: "get", input: { reference: "7" } },
      { op: "get", input: { reference: " #7 " } },
    ],
  },
  {
    name: "get by url on its own repository",
    remotes: origin,
    script: [{ body: byNumber(node(9, "feature")) }],
    calls: [
      { op: "get", input: { reference: "https://github.example.com/other/repo/pull/9/files" } },
    ],
  },
  {
    name: "get missing",
    remotes: origin,
    script: [
      { body: byNumber(null) },
      { body: { data: { repository: null } } },
      { status: 404, body: {} },
    ],
    calls: [
      { op: "get", input: { reference: "7" } },
      { op: "get", input: { reference: "8" } },
      { op: "get", input: { reference: "9" } },
    ],
  },
  {
    name: "get bad node",
    remotes: origin,
    script: [
      { body: byNumber({ number: "x" }) },
      { body: { data: { repository: { nope: 1 } } } },
      { body: { data: 5 } },
    ],
    calls: [
      { op: "get", input: { reference: "7" } },
      { op: "get", input: { reference: "8" } },
      { op: "get", input: { reference: "9" } },
    ],
  },
  {
    name: "get by branch prefers open",
    remotes: origin,
    script: [{ body: listBody(node(11, "feature")) }],
    calls: [{ op: "get", input: { reference: "feature" } }],
  },
  {
    name: "get by branch falls back to any state",
    remotes: origin,
    script: [
      { body: listBody() },
      {
        body: listBody(
          node(12, "feature", "acme", { state: "MERGED", mergedAt: "2026-01-03T00:00:00Z" }),
        ),
      },
    ],
    calls: [{ op: "get", input: { reference: "feature" } }],
  },
  {
    name: "get by branch none",
    remotes: origin,
    script: [{ body: listBody() }, { body: listBody() }],
    calls: [{ op: "get", input: { reference: "feature" } }],
  },
  {
    name: "get not a pull url is a branch",
    remotes: origin,
    script: [{ body: listBody() }, { body: listBody() }],
    calls: [{ op: "get", input: { reference: "https://github.com/acme/web/issues/3" } }],
  },
  {
    name: "clone urls selectors",
    remotes: origin,
    script: [{ body: repoBody() }],
    calls: [
      { op: "cloneUrls", input: { repository: "acme/web" } },
      { op: "cloneUrls", input: { repository: "acme/web.git" } },
      { op: "cloneUrls", input: { repository: "github.acme.corp/team/app" } },
      { op: "cloneUrls", input: { repository: "https://github.example.com/team/app.git" } },
      { op: "cloneUrls", input: { repository: "web" } },
      { op: "cloneUrls", input: { repository: "a/b/c/d" } },
      { op: "cloneUrls", input: { repository: "  acme//web  " } },
    ],
  },
  {
    name: "clone urls decode",
    remotes: origin,
    script: [
      { body: { full_name: "x" } },
      { body: { full_name: " a/b ", html_url: " u ", ssh_url: " s " } },
      { raw: "nope" },
      { status: 404, body: {} },
    ],
    calls: [
      { op: "cloneUrls", input: { repository: "acme/web" } },
      { op: "cloneUrls", input: { repository: "acme/web" } },
      { op: "cloneUrls", input: { repository: "acme/web" } },
      { op: "cloneUrls", input: { repository: "acme/web" } },
    ],
  },
  {
    name: "clone urls without remotes use GH_HOST",
    remotes: "",
    env: { GH_HOST: "git.acme.corp" },
    script: [{ body: repoBody() }],
    calls: [{ op: "cloneUrls", input: { repository: "acme/web" } }],
  },
  {
    name: "create repository user and org",
    remotes: origin,
    script: [
      { body: { login: "Tim" } },
      { body: repoBody("Tim/new") },
      { body: { login: "tim" } },
      { body: repoBody("tim/other") },
      { body: { login: "tim" } },
      { body: repoBody("acme/team") },
      { body: repoBody("solo") },
    ],
    calls: [
      { op: "createRepo", input: { repository: "Tim/new", visibility: "private" } },
      { op: "createRepo", input: { repository: "tim/other", visibility: "public" } },
      { op: "createRepo", input: { repository: "acme/team", visibility: "private" } },
      { op: "createRepo", input: { repository: "  solo ", visibility: "public" } },
    ],
  },
  {
    name: "create repository viewer failures",
    remotes: "",
    script: [
      { status: 401, body: {} },
      { body: { nope: 1 } },
      { body: { login: "tim" } },
      {
        status: 422,
        body: {
          message: "Repository creation failed.",
          errors: [{ message: "name already exists on this account" }],
        },
      },
    ],
    calls: [
      { op: "createRepo", input: { repository: "tim/a", visibility: "private" } },
      { op: "createRepo", input: { repository: "tim/a", visibility: "private" } },
      { op: "createRepo", input: { repository: "tim/a", visibility: "private" } },
    ],
  },
  {
    name: "create repository enterprise host",
    remotes: "",
    env: { GH_HOST: "git.acme.corp" },
    script: [{ body: { login: "tim" } }, { body: repoBody("tim/x") }, { body: repoBody("solo") }],
    calls: [
      { op: "createRepo", input: { repository: "tim/x", visibility: "public" } },
      { op: "createRepo", input: { repository: "solo", visibility: "private" } },
    ],
  },
  {
    name: "create pull request",
    remotes: origin,
    script: [
      { status: 201, body: { number: 1 } },
      {
        status: 422,
        body: {
          message: "Validation Failed",
          errors: [
            {
              resource: "PullRequest",
              code: "custom",
              message: "A pull request already exists for acme:feature.",
            },
          ],
        },
      },
    ],
    bodyFile: bodyFileText,
    calls: [
      {
        op: "createPr",
        input: { baseRefName: "main", headSelector: "feature", title: "Add thing" },
      },
      {
        op: "createPr",
        input: { baseRefName: "main", headSelector: "fork:feature", title: "Add thing" },
      },
    ],
  },
  {
    name: "create pull request missing body file",
    remotes: origin,
    script: [{ body: {} }],
    bodyFile: null,
    calls: [
      { op: "createPr", input: { baseRefName: "main", headSelector: "feature", title: "T" } },
    ],
  },
  {
    name: "default branch",
    remotes: origin,
    script: [
      { body: repoBody("acme/web", { default_branch: " develop " }) },
      { body: repoBody("acme/web", { default_branch: "  " }) },
      { body: repoBody("acme/web", { default_branch: null }) },
      { body: { full_name: "x" } },
      { status: 404, body: {} },
    ],
    calls: [
      { op: "defaultBranch", input: {}, context: ctx() },
      { op: "defaultBranch", input: {} },
      { op: "defaultBranch", input: {} },
      { op: "defaultBranch", input: {} },
      { op: "defaultBranch", input: {} },
    ],
  },
];
for (const scenario of providerScenarios) {
  const harness = providerHarness(scenario);
  const dir = mkdtempSync(NodePath.join(tmpdir(), "t3-scp-"));
  const bodyFile =
    scenario.bodyFile === undefined
      ? null
      : scenario.bodyFile === null
        ? NodePath.join(dir, "missing.md")
        : NodePath.join(dir, "body.md");
  if (typeof scenario.bodyFile === "string") write(bodyFile, scenario.bodyFile);
  const results = [];
  const layer = Layer.mergeAll(harness.cli, harness.apiLayerValue);
  await Effect.runPromise(
    Effect.gen(function* () {
      yield* TestClock.setTime(NOW);
      const provider = yield* GitHubSourceControlProvider.make;
      for (const call of scenario.calls) {
        const withContext = call.context ? { context: call.context } : {};
        const base = { cwd: "/repo", ...withContext };
        const effect =
          call.op === "list"
            ? provider
                .listChangeRequests({ ...base, ...call.input })
                .pipe(
                  Effect.map((items) => items.map((item) => jsonOf(Contracts.ChangeRequest, item))),
                )
            : call.op === "get"
              ? provider
                  .getChangeRequest({ ...base, ...call.input })
                  .pipe(Effect.map((item) => jsonOf(Contracts.ChangeRequest, item)))
              : call.op === "cloneUrls"
                ? provider.getRepositoryCloneUrls({ ...base, ...call.input })
                : call.op === "createRepo"
                  ? provider.createRepository({ cwd: "/repo", ...call.input })
                  : call.op === "createPr"
                    ? provider.createChangeRequest({ ...base, ...call.input, bodyFile })
                    : provider.getDefaultBranch({ ...base });
        // Head lookups wait a batching window on the clock; let it pass.
        const fiber = yield* Effect.forkChild(Effect.exit(effect));
        yield* TestClock.adjust("1 second");
        const exit = yield* Fiber.join(fiber);
        results.push(
          exit._tag === "Success"
            ? { ok: exit.value ?? null }
            : {
                error: exit.cause.reasons[0].error
                  ? jsonOf(Contracts.SourceControlProviderError, exit.cause.reasons[0].error)
                  : { defect: String(exit.cause.reasons[0].defect) },
              },
        );
      }
    }).pipe(Effect.provide(layer), Effect.provide(TestClock.layer())),
  );
  rows.push({
    type: "provider",
    name: scenario.name,
    remotes: scenario.remotes ?? "",
    resolved: scenario.resolved ?? "",
    env: scenario.env ?? {},
    script: scenario.script ?? [],
    bodyFile: scenario.bodyFile === undefined ? "none" : scenario.bodyFile,
    calls: scenario.calls,
    results: JSON.parse(
      JSON.stringify(results).replaceAll(bodyFile ?? "\u0000never", "<BODY_FILE>"),
    ),
    requests: harness.requests
      .map(requestJson)
      .map((request) =>
        JSON.parse(JSON.stringify(request).replaceAll(bodyFile ?? "\u0000never", "<BODY_FILE>")),
      ),
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/source-control-github.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
