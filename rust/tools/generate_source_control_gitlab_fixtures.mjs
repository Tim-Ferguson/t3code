// Runs the original GitLab layer (merge request decoding, `glab auth status` reading, and the
// provider over GitLabCli over the real VcsProcess policy with a scripted process runner) and
// records what it produces: results, the error envelopes, and every `glab` argument list.
// No real process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as GitLabCli from "../../apps/server/src/sourceControl/GitLabCli.ts";
import * as GitLabSourceControlProvider from "../../apps/server/src/sourceControl/GitLabSourceControlProvider.ts";
import { parseGitLabAuthStatusHosts } from "../../apps/server/src/sourceControl/gitLabAuthStatus.ts";
import {
  decodeGitLabMergeRequestJson,
  decodeGitLabMergeRequestListJson,
} from "../../apps/server/src/sourceControl/gitLabMergeRequests.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";
import * as Contracts from "../../packages/contracts/src/sourceControl.ts";
import * as Result from "../../apps/server/node_modules/effect/dist/Result.js";

const rows = [];
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));
const summaryJson = (record) => ({
  ...record,
  provider: "gitlab",
  updatedAt: Option.isSome(record.updatedAt ?? Option.none())
    ? { _tag: "Some", value: DateTime.formatIso(record.updatedAt.value) }
    : { _tag: "None" },
});

// ---- merge request rows --------------------------------------------------------------------
const mr = (overrides = {}) => ({
  iid: 5,
  title: "  Fix  ",
  web_url: "https://gitlab.com/g/p/-/merge_requests/5",
  source_branch: "feature",
  target_branch: "main",
  state: "opened",
  updated_at: "2026-01-02T03:04:05.000Z",
  ...overrides,
});
const mrRows = [
  mr(),
  mr({ state: "merged", merged_at: "2026-01-03T00:00:00Z" }),
  mr({ state: "closed", closed_at: "2026-01-04T00:00:00Z" }),
  mr({ state: " MERGED " }),
  mr({ state: null }),
  mr({ state: undefined }),
  mr({ draft: true }),
  mr({ work_in_progress: true }),
  mr({ draft: false, work_in_progress: false }),
  mr({ draft: "yes" }),
  mr({ source_project_id: 1, target_project_id: 2 }),
  mr({ source_project_id: 3, target_project_id: 3 }),
  mr({ source_project_id: 3, target_project_id: null }),
  mr({
    source_project: { path_with_namespace: "Fork/Proj" },
    target_project: { path_with_namespace: "g/p" },
  }),
  mr({
    source_project: { path_with_namespace: "g/p" },
    target_project: { pathWithNamespace: "G/P" },
  }),
  mr({
    source_project: { namespace: { full_path: "group/sub" } },
    target_project: { namespace: { path: "group" } },
  }),
  mr({ source_project: { namespace: { fullPath: "camel/path" } } }),
  mr({ source_project: { namespace: { path: "only" } } }),
  mr({ source_project: { path_with_namespace: "  ", namespace: { path: "fallback" } } }),
  mr({ source_project: null, target_project: null }),
  mr({ source_project: "x" }),
  mr({ source_project: { namespace: "x" } }),
  mr({ source_project: { path_with_namespace: 5 } }),
  mr({
    source_project_id: 1,
    target_project_id: 2,
    source_project: { path_with_namespace: "same/p" },
    target_project: { path_with_namespace: "same/p" },
  }),
  mr({ iid: 0 }),
  mr({ iid: "5" }),
  mr({ iid: 5.5 }),
  mr({ title: " " }),
  mr({ web_url: "" }),
  mr({ source_branch: undefined }),
  mr({ target_branch: 3 }),
  mr({ updated_at: null }),
  mr({ updated_at: "bad" }),
  mr({ updated_at: "2026-01-02" }),
  mr({ closed_at: 5 }),
  mr({ merged_at: null }),
  5,
  null,
  "x",
  [],
];
for (const row of mrRows) {
  const raw = JSON.stringify(row === undefined ? null : row);
  const single = decodeGitLabMergeRequestJson(raw);
  rows.push({
    type: "mr",
    raw,
    result: Result.isSuccess(single) ? { ok: summaryJson(single.success) } : { error: true },
  });
}
const lists = [
  JSON.stringify(mrRows),
  JSON.stringify([mr(), { bad: true }, mr({ iid: 6 })]),
  "[]",
  "{}",
  "[",
  "",
  "null",
  JSON.stringify("x"),
];
for (const raw of lists) {
  const list = decodeGitLabMergeRequestListJson(raw);
  rows.push({
    type: "mrList",
    raw,
    result: Result.isSuccess(list) ? { ok: list.success.map(summaryJson) } : { error: true },
  });
}

// ---- auth ----------------------------------------------------------------------------------
const authTexts = [
  "gitlab.com\n  ✓ Logged in to gitlab.com as gitlab-user (keyring)\n  ✓ Git operations for gitlab.com configured to use https protocol.\n  ✓ Token: **********\n",
  "gitlab.com\n  x gitlab.com: authentication failed\n",
  "",
  "   \n",
  "nothing to see",
  "gitlab.example.com:8443\n  ✓ Logged in to gitlab.example.com:8443 as tim\n\ngitlab.com\n  x not logged in\n",
  "gitlab.com\n  x No token provided\ngit.corp\n  ✓ Logged in to git.corp as corp-user (token)\n",
  "Logged in to gitlab.com as inline-user",
  "account: bob (x)",
  "Logged in to gitlab.com account acct-user",
  "  indented.host\n  Logged in to indented.host as nobody\n",
  "GITLAB.COM\n  Logged in to GITLAB.COM as Upper\n",
  "[::1]:80\n  Logged in to [::1]:80 as v6\n",
  "-bad.host\n  Logged in to x as y\n",
  "first.host\nsecond.host\n  Logged in to second.host as second\n",
];
for (const text of authTexts) {
  rows.push({ type: "authHosts", text, result: parseGitLabAuthStatusHosts(text) });
  for (const exitCode of [0, 1]) {
    for (const [stdout, stderr] of [
      [text, ""],
      ["", text],
      [text, "stderr line"],
    ]) {
      rows.push({
        type: "parseAuth",
        stdout,
        stderr,
        exitCode,
        result: jsonOf(
          Contracts.SourceControlProviderAuth,
          GitLabSourceControlProvider.discovery.parseAuth({ stdout, stderr, exitCode }),
        ),
      });
    }
  }
}
const contexts = [
  {
    provider: {
      kind: "unknown",
      name: "gitlab.example.com:8443",
      baseUrl: "https://gitlab.example.com:8443",
    },
    remoteName: "origin",
    remoteUrl: "https://gitlab.example.com:8443/g/p",
  },
  {
    provider: { kind: "unknown", name: "git.corp", baseUrl: "https://git.corp" },
    remoteName: "origin",
    remoteUrl: "git@git.corp:g/p.git",
  },
  {
    provider: { kind: "unknown", name: "Git.Corp", baseUrl: "https://Git.Corp" },
    remoteName: "origin",
    remoteUrl: "git@Git.Corp:g/p.git",
  },
  {
    provider: { kind: "unknown", name: "other.host", baseUrl: "https://other.host" },
    remoteName: "origin",
    remoteUrl: "https://other.host/g/p",
  },
];
for (const context of contexts)
  for (const text of authTexts) {
    rows.push({
      type: "refine",
      context,
      stdout: text,
      result: GitLabSourceControlProvider.discovery.refineUnknownRemote({
        cwd: "/repo",
        context,
        auth: { stdout: text, stderr: "", exitCode: 0 },
      }),
    });
  }

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
          new ProcessRunner.ProcessSpawnError({ ...fields, cause: new Error("spawn glab ENOENT") }),
        );
      if (step.timeout)
        return Effect.fail(new ProcessRunner.ProcessTimeoutError({ ...fields, timeoutMs: 30000 }));
      return Effect.succeed(output(step));
    },
  });
  const process = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  const cli = Layer.effect(GitLabCli.GitLabCli, GitLabCli.make).pipe(Layer.provide(process));
  return { cli, calls };
}
const listJson = (...items) => JSON.stringify(items);
const scenarios = [
  {
    name: "list open",
    script: [{ stdout: listJson(mr()) }],
    calls: [
      { op: "list", input: { headSelector: "feature", state: "open" } },
      { op: "list", input: { headSelector: "feature", state: "open", limit: 3 } },
    ],
  },
  {
    name: "list states",
    script: [{ stdout: listJson(mr({ state: "merged" })) }],
    calls: ["closed", "merged", "all"].map((state) => ({
      op: "list",
      input: { headSelector: "feature", state },
    })),
  },
  {
    name: "list owner:branch selectors",
    script: [{ stdout: "[]" }],
    calls: [
      { op: "list", input: { headSelector: "fork:feature", state: "open" } },
      { op: "list", input: { headSelector: " fork : feature ", state: "open" } },
      {
        op: "list",
        input: {
          headSelector: "fork:feature",
          state: "open",
          source: { refName: "explicit", owner: "o" },
        },
      },
      { op: "list", input: { headSelector: "feature/x", state: "open" } },
    ],
  },
  {
    name: "list empty output",
    script: [{ stdout: "  \n" }],
    calls: [{ op: "list", input: { headSelector: "f", state: "open" } }],
  },
  {
    name: "list invalid",
    script: [
      { stdout: "not json" },
      { stdout: "{}" },
      { stdout: listJson({ bad: true }, mr({ iid: 9 })) },
    ],
    calls: [
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
      { op: "list", input: { headSelector: "f", state: "open" } },
    ],
  },
  {
    name: "list failures",
    script: [
      { spawn: true },
      { code: 1, stderr: "glab auth login" },
      { code: 1, stderr: "API rate limit exceeded" },
      { code: 1, stderr: "boom" },
      { timeout: true },
      { code: 1, stderr: "404 Not Found" },
    ],
    calls: Array.from({ length: 6 }, () => ({
      op: "list",
      input: { headSelector: "f", state: "open" },
    })),
  },
  {
    name: "get",
    script: [{ stdout: JSON.stringify(mr({ iid: 7 })) }],
    calls: [
      { op: "get", input: { reference: "7" } },
      { op: "get", input: { reference: "https://gitlab.com/g/p/-/merge_requests/7" } },
    ],
  },
  {
    name: "get failures",
    script: [
      { code: 1, stderr: "merge request not found" },
      { code: 1, stderr: "401 Unauthorized" },
      { stdout: "nope" },
      { stdout: '{"iid":0}' },
      { spawn: true },
      { code: 2, stderr: "oops" },
    ],
    calls: ["1", "2", "3", "4", "5", "6"].map((reference) => ({ op: "get", input: { reference } })),
  },
  {
    name: "clone urls",
    script: [
      {
        stdout: JSON.stringify({
          path_with_namespace: "g/sub/p",
          web_url: "https://gitlab.com/g/sub/p",
          http_url_to_repo: "https://gitlab.com/g/sub/p.git",
          ssh_url_to_repo: "git@gitlab.com:g/sub/p.git",
        }),
      },
    ],
    calls: [
      { op: "cloneUrls", input: { repository: "g/sub/p" } },
      { op: "cloneUrls", input: { repository: "p" } },
      { op: "cloneUrls", input: { repository: "a b/ü" } },
    ],
  },
  {
    name: "clone urls failures",
    script: [
      { stdout: JSON.stringify({ path_with_namespace: "g/p" }) },
      { stdout: "x" },
      { code: 1, stderr: "404 not found" },
      { spawn: true },
    ],
    calls: Array.from({ length: 4 }, () => ({ op: "cloneUrls", input: { repository: "g/p" } })),
  },
  {
    name: "create repository",
    script: [
      { stdout: JSON.stringify({ id: 42 }) },
      {
        stdout: JSON.stringify({
          path_with_namespace: "g/new",
          web_url: "https://gitlab.com/g/new",
          http_url_to_repo: "h",
          ssh_url_to_repo: "git@gitlab.com:g/new.git",
        }),
      },
      {
        stdout: JSON.stringify({
          path_with_namespace: "solo",
          web_url: "https://gitlab.com/solo",
          http_url_to_repo: "h",
          ssh_url_to_repo: "s",
        }),
      },
    ],
    calls: [
      { op: "createRepo", input: { repository: "g/new", visibility: "private" } },
      { op: "createRepo", input: { repository: " solo ", visibility: "public" } },
    ],
  },
  {
    name: "create repository nested namespace and failures",
    script: [
      { stdout: JSON.stringify({ id: 7 }) },
      {
        stdout: JSON.stringify({
          path_with_namespace: "a/b/c",
          web_url: "w",
          http_url_to_repo: "h",
          ssh_url_to_repo: "s",
        }),
      },
      { stdout: "nope" },
      { stdout: JSON.stringify({ id: "x" }) },
      { code: 1, stderr: "glab auth login" },
    ],
    calls: [
      { op: "createRepo", input: { repository: "a/b/c", visibility: "private" } },
      { op: "createRepo", input: { repository: "a/b/c", visibility: "private" } },
      { op: "createRepo", input: { repository: "a/b/c", visibility: "private" } },
    ],
  },
  {
    name: "create merge request",
    script: [{ stdout: "{}" }],
    bodyFile: "BODY",
    calls: [
      { op: "createMr", input: { baseRefName: "main", headSelector: "feature", title: "T: x" } },
      {
        op: "createMr",
        input: {
          baseRefName: "main",
          headSelector: "fork:feature",
          title: "T",
          target: { refName: "release" },
        },
      },
      {
        op: "createMr",
        input: {
          baseRefName: "main",
          headSelector: "feature",
          title: "T",
          source: { refName: "explicit", owner: "own" },
        },
      },
      {
        op: "createMr",
        input: {
          baseRefName: "main",
          headSelector: "feature",
          title: "T",
          source: { refName: "explicit", owner: "own", repository: "own/proj" },
        },
      },
    ],
  },
  {
    name: "create merge request failure",
    script: [{ code: 1, stderr: "boom" }],
    bodyFile: "BODY",
    calls: [
      { op: "createMr", input: { baseRefName: "main", headSelector: "feature", title: "T" } },
    ],
  },
  {
    name: "default branch",
    script: [
      { stdout: JSON.stringify({ default_branch: " main " }) },
      { stdout: JSON.stringify({ default_branch: null }) },
      { stdout: "{}" },
      { stdout: JSON.stringify({ default_branch: "  " }) },
      { stdout: "x" },
      { code: 1, stderr: "404" },
    ],
    calls: Array.from({ length: 6 }, () => ({ op: "defaultBranch", input: {} })),
  },
  {
    name: "checkout",
    script: [
      { stdout: "" },
      { code: 1, stderr: "merge request not found" },
      { code: 1, stderr: "other" },
    ],
    calls: [
      { op: "checkout", input: { reference: "7" } },
      { op: "checkout", input: { reference: "8" } },
      { op: "checkout", input: { reference: "9", force: true } },
    ],
  },
];
for (const scenario of scenarios) {
  const { cli, calls } = harness(scenario.script);
  const results = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const provider = yield* GitLabSourceControlProvider.make;
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
                  : call.op === "createMr"
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
    bodyFile: scenario.bodyFile ?? null,
    results,
    processes: calls,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/source-control-gitlab.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
