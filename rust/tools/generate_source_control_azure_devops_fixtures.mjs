// Runs the original Azure DevOps layer (pull request decoding, web URL assembly, `az account
// show` reading, and the provider over AzureDevOpsCli over the real VcsProcess policy with a
// scripted process runner) and records what it produces. No real process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as AzureDevOpsCli from "../../apps/server/src/sourceControl/AzureDevOpsCli.ts";
import * as AzureDevOpsSourceControlProvider from "../../apps/server/src/sourceControl/AzureDevOpsSourceControlProvider.ts";
import {
  azureDevOpsPullRequestWebUrl,
  decodeAzureDevOpsPullRequestJson,
  decodeAzureDevOpsPullRequestListJson,
} from "../../apps/server/src/sourceControl/azureDevOpsPullRequests.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";
import * as PlatformError from "../../apps/server/node_modules/effect/dist/PlatformError.js";
import * as Contracts from "../../packages/contracts/src/sourceControl.ts";

const rows = [];
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));
const recordJson = (record) => ({
  provider: "azure-devops",
  ...record,
  isCrossRepository: false,
  updatedAt: Option.isSome(record.updatedAt)
    ? { _tag: "Some", value: DateTime.formatIso(record.updatedAt.value) }
    : { _tag: "None" },
});

const pr = (overrides = {}) => ({
  pullRequestId: 12,
  title: " Add thing ",
  sourceRefName: "refs/heads/feature/x",
  targetRefName: "refs/heads/main",
  status: "active",
  creationDate: "2026-01-02T03:04:05.000Z",
  url: "https://dev.azure.com/org/proj/_apis/git/repositories/r/pullRequests/12",
  repository: {
    name: "repo",
    webUrl: "https://dev.azure.com/org/proj/_git/repo",
    project: { name: "proj" },
  },
  ...overrides,
});
const prRows = [
  pr(),
  pr({ status: "completed", closedDate: "2026-01-05T00:00:00Z" }),
  pr({ status: "abandoned", closedDate: "2026-01-06T00:00:00Z" }),
  pr({ status: " Completed " }),
  pr({ status: "weird" }),
  pr({ status: "completed", closedDate: null }),
  pr({ isDraft: true }),
  pr({ isDraft: false }),
  pr({ isDraft: "x" }),
  pr({ _links: { web: { href: " https://web.example/pr/12 " } } }),
  pr({ _links: {} }),
  pr({ _links: { web: {} } }),
  pr({ _links: { web: { href: 5 } } }),
  pr({ _links: null }),
  pr({ repository: { name: "repo", webUrl: "https://dev.azure.com/org/proj/_git/repo/" } }),
  pr({ repository: { name: "re po", project: { name: "pro j" } } }),
  pr({ repository: undefined }),
  pr({
    repository: undefined,
    url: "https://org.visualstudio.com/proj/_apis/git/repositories/r/pullRequests/12",
  }),
  pr({
    repository: { name: "r", project: { name: "p" } },
    url: "https://org.visualstudio.com/p/_apis/git/repositories/r/pullRequests/12",
  }),
  pr({
    repository: { name: "r", project: { name: "p" } },
    url: "https://dev.azure.com/org/p/_apis/git/repositories/r/pullRequests/12",
  }),
  pr({ repository: { name: "r", project: { name: "p" } }, url: "https://other.example/_apis/x" }),
  pr({ repository: undefined, url: "https://dev.azure.com/org/no-apis" }),
  pr({ repository: undefined, url: undefined }),
  pr({ repository: "x" }),
  pr({ repository: { project: "x" } }),
  pr({ repository: { name: 5 } }),
  pr({ sourceRefName: "feature", targetRefName: " refs/heads/dev " }),
  pr({ sourceRefName: "refs/tags/v1" }),
  pr({ sourceRefName: " " }),
  pr({ targetRefName: undefined }),
  pr({ title: "  " }),
  pr({ pullRequestId: 0 }),
  pr({ pullRequestId: "12" }),
  pr({ status: undefined }),
  pr({ creationDate: "bad" }),
  pr({ creationDate: null }),
  pr({ closedDate: "2026-01-05" }),
  5,
  null,
  "x",
  [],
];
for (const row of prRows) {
  const raw = JSON.stringify(row);
  const decoded = decodeAzureDevOpsPullRequestJson(raw);
  rows.push({
    type: "pr",
    raw,
    result: Result.isSuccess(decoded) ? { ok: recordJson(decoded.success) } : { error: true },
  });
}
for (const raw of [
  JSON.stringify(prRows),
  JSON.stringify([pr(), { bad: 1 }, pr({ pullRequestId: 13 })]),
  "[]",
  "{}",
  "[",
  "",
  "null",
]) {
  const decoded = decodeAzureDevOpsPullRequestListJson(raw);
  rows.push({
    type: "prList",
    raw,
    result: Result.isSuccess(decoded) ? { ok: decoded.success.map(recordJson) } : { error: true },
  });
}
const urlInputs = [
  { pullRequestId: 3 },
  { pullRequestId: 3, webLink: " https://w/3 " },
  { pullRequestId: 3, webLink: "  ", repositoryWebUrl: "https://dev.azure.com/o/p/_git/r//" },
  {
    pullRequestId: 3,
    restApiUrl: "https://dev.azure.com/o/p/_apis/git/pullRequests/3",
    projectName: "p q",
    repositoryName: "r/s",
  },
  {
    pullRequestId: 3,
    restApiUrl: "https://o.visualstudio.com/p/_apis/x",
    projectName: "p",
    repositoryName: "r",
  },
  { pullRequestId: 3, restApiUrl: "https://o.visualstudio.com/p/_apis/x" },
  {
    pullRequestId: 3,
    restApiUrl: "https://dev.azure.com/_apis/x",
    projectName: "p",
    repositoryName: "r",
  },
  { pullRequestId: 3, restApiUrl: "not a url" },
  {
    pullRequestId: 3,
    restApiUrl: "https://dev.azure.com/o/p/_APIS/x",
    projectName: "p",
    repositoryName: "r",
  },
  {
    pullRequestId: 3,
    restApiUrl: "https://dev.azure.com:8443/o/_apis/x",
    projectName: "p",
    repositoryName: "r",
  },
];
for (const input of urlInputs)
  rows.push({ type: "webUrl", input, result: azureDevOpsPullRequestWebUrl(input) });

for (const [stdout, stderr, exitCode] of [
  ["user@example.com\n", "", 0],
  ["  user@example.com  \nsecond\n", "", 0],
  ["\r\nuser\r\n", "", 0],
  ["", "", 0],
  ["  \n", "", 0],
  ["user", "warn", 0],
  ["", "Please run 'az login' to setup account.\nmore", 1],
  ["out", "", 1],
  ["", "", 1],
  ["user", "ERROR: x\nTOKEN: y", 2],
])
  rows.push({
    type: "parseAuth",
    stdout,
    stderr,
    exitCode,
    result: jsonOf(
      Contracts.SourceControlProviderAuth,
      AzureDevOpsSourceControlProvider.discovery.parseAuth({ stdout, stderr, exitCode }),
    ),
  });

// ---- the provider over the CLI over scripted processes -----------------------------------------
const output = (overrides = {}) => ({
  stdout: "",
  stderr: "",
  code: 0,
  timedOut: false,
  stdoutTruncated: false,
  stderrTruncated: false,
  stdoutInvalidUtf8: false,
  stderrInvalidUtf8: false,
  ...overrides,
});
function harness(script) {
  const calls = [];
  const runner = ProcessRunner.ProcessRunner.of({
    run: (input) => {
      calls.push({ command: input.command, args: input.args });
      const step = script[Math.min(calls.length - 1, script.length - 1)];
      const fields = { command: input.command, argumentCount: input.args.length, cwd: input.cwd };
      if (step.spawn)
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
      return Effect.succeed(output(step));
    },
  });
  const process = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  const cli = Layer.effect(AzureDevOpsCli.AzureDevOpsCli, AzureDevOpsCli.make).pipe(
    Layer.provide(process),
  );
  return { cli, calls };
}
const repoJson = (overrides = {}) =>
  JSON.stringify({
    name: "repo",
    webUrl: "https://dev.azure.com/o/p/_git/repo",
    remoteUrl: "https://o@dev.azure.com/o/p/_git/repo",
    sshUrl: "git@ssh.dev.azure.com:v3/o/p/repo",
    project: { name: "proj" },
    defaultBranch: "refs/heads/main",
    ...overrides,
  });
const scenarios = [
  {
    name: "list",
    script: [{ stdout: JSON.stringify([pr()]) }],
    calls: [
      { op: "list", input: { headSelector: "feature", state: "open" } },
      { op: "list", input: { headSelector: "fork:feature", state: "all", limit: 4 } },
      { op: "list", input: { headSelector: "x", state: "closed" } },
      { op: "list", input: { headSelector: "x", state: "merged", source: { refName: "src" } } },
    ],
  },
  {
    name: "list empty and invalid",
    script: [
      { stdout: "" },
      { stdout: "nope" },
      { stdout: "{}" },
      { stdout: JSON.stringify([{ bad: 1 }, pr()]) },
    ],
    calls: Array.from({ length: 4 }, () => ({
      op: "list",
      input: { headSelector: "f", state: "open" },
    })),
  },
  {
    name: "list failures",
    script: [
      { spawn: true },
      { code: 1, stderr: "Please run az login" },
      { code: 1, stderr: "Too Many Requests" },
      { code: 1, stderr: "pull request not found" },
      { code: 1, stderr: "boom" },
    ],
    calls: Array.from({ length: 5 }, () => ({
      op: "list",
      input: { headSelector: "f", state: "open" },
    })),
  },
  {
    name: "get",
    script: [{ stdout: JSON.stringify(pr({ pullRequestId: 7 })) }],
    calls: [
      "7",
      "#7",
      "https://dev.azure.com/org/proj/_git/repo/pullrequest/7?x=1",
      " 7 ",
      "weird-ref",
    ].map((reference) => ({ op: "get", input: { reference } })),
  },
  {
    name: "get failures",
    script: [
      { stdout: "nope" },
      { code: 1, stderr: "pull request does not exist" },
      { spawn: true },
    ],
    calls: ["1", "2", "3"].map((reference) => ({ op: "get", input: { reference } })),
  },
  {
    name: "clone urls",
    script: [
      { stdout: repoJson() },
      { stdout: repoJson({ project: undefined }) },
      { stdout: repoJson({ project: { name: " p " } }) },
    ],
    calls: [
      { op: "cloneUrls", input: { repository: "proj/repo" } },
      { op: "cloneUrls", input: { repository: "repo" } },
      { op: "cloneUrls", input: { repository: "x" } },
    ],
  },
  {
    name: "clone urls failures",
    script: [
      { stdout: repoJson({ name: "" }) },
      { stdout: "x" },
      { stdout: repoJson({ project: {} }) },
      { code: 1, stderr: "boom" },
    ],
    calls: Array.from({ length: 4 }, () => ({ op: "cloneUrls", input: { repository: "r" } })),
  },
  {
    name: "create repository",
    script: [{ stdout: repoJson() }],
    calls: [
      { op: "createRepo", input: { repository: "proj/repo", visibility: "private" } },
      { op: "createRepo", input: { repository: "org/proj/repo", visibility: "public" } },
      { op: "createRepo", input: { repository: " solo ", visibility: "private" } },
    ],
  },
  {
    name: "create pull request",
    script: [{ stdout: "{}" }],
    bodyFile: "BODY",
    calls: [
      { op: "createPr", input: { baseRefName: "main", headSelector: "feature", title: "T" } },
      {
        op: "createPr",
        input: {
          baseRefName: "main",
          headSelector: "fork:feature",
          title: "T",
          target: { refName: "release" },
        },
      },
      {
        op: "createPr",
        input: { baseRefName: "main", headSelector: "f", title: "T", source: { refName: "src" } },
      },
    ],
  },
  {
    name: "create pull request failure",
    script: [{ code: 1, stderr: "az devops login" }],
    bodyFile: "BODY",
    calls: [
      { op: "createPr", input: { baseRefName: "main", headSelector: "feature", title: "T" } },
    ],
  },
  {
    name: "default branch",
    script: [
      { stdout: repoJson() },
      { stdout: repoJson({ defaultBranch: null }) },
      { stdout: repoJson({ defaultBranch: "  " }) },
      { stdout: repoJson({ defaultBranch: "dev" }) },
      { stdout: "x" },
    ],
    calls: Array.from({ length: 5 }, () => ({ op: "defaultBranch", input: {} })),
  },
  {
    name: "checkout",
    script: [{ stdout: "" }, { code: 1, stderr: "pull request not found" }],
    calls: [
      { op: "checkout", input: { reference: "7" } },
      {
        op: "checkout",
        input: {
          reference: "#8",
          context: {
            provider: {
              kind: "azure-devops",
              name: "Azure DevOps",
              baseUrl: "https://dev.azure.com",
            },
            remoteName: "azure",
            remoteUrl: "https://dev.azure.com/o/p/_git/r",
          },
        },
      },
    ],
  },
];
for (const scenario of scenarios) {
  const { cli, calls } = harness(scenario.script);
  const results = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* AzureDevOpsSourceControlProvider.make;
      for (const call of scenario.calls) {
        const base = { cwd: "/repo" };
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
                  ? provider.createRepository({ ...base, ...call.input })
                  : call.op === "createPr"
                    ? provider.createChangeRequest({
                        ...base,
                        ...call.input,
                        bodyFile: "<BODY_FILE>",
                      })
                    : call.op === "checkout"
                      ? provider.checkoutChangeRequest({ ...base, ...call.input })
                      : provider.getDefaultBranch({ ...base });
        const exit = yield* Effect.exit(effect);
        results.push(
          exit._tag === "Success"
            ? { ok: exit.value ?? null }
            : { error: jsonOf(Contracts.SourceControlProviderError, exit.cause.reasons[0].error) },
        );
      }
    }).pipe(Effect.provide(cli)),
  );
  rows.push({
    type: "provider",
    name: scenario.name,
    script: scenario.script,
    calls: scenario.calls,
    results,
    processes: calls,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/source-control-azure-devops.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
