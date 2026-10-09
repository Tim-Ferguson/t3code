// Runs the original GitManager.runStackedAction against real temporary repositories with a local
// bare remote and a scripted source-control provider, and records for each scenario the progress
// events, the result or the error, the requests made of the provider and what the repositories
// look like afterwards. The Rust test builds the same repositories, runs its own workflow with
// the same scripted text generation, and compares. Commit ids, paths and timings are replaced by
// markers because they differ from run to run. No network is used.
import {
  writeFileSync,
  renameSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  chmodSync,
  rmSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
import * as Cause from "../../apps/server/node_modules/effect/dist/Cause.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as GitManager from "../../apps/server/src/git/GitManager.ts";
import * as GitVcsDriver from "../../apps/server/src/vcs/GitVcsDriver.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as ServerSettings from "../../apps/server/src/serverSettings.ts";
import * as TextGeneration from "../../apps/server/src/textGeneration/TextGeneration.ts";
import * as ProviderRegistry from "../../apps/server/src/provider/ProviderRegistry.ts";
import * as ProjectSetupScriptRunner from "../../apps/server/src/project/ProjectSetupScriptRunner.ts";
import * as ProjectionStore from "../../apps/server/src/orchestration-v2/ProjectionStore.ts";
import * as ProjectStore from "../../apps/server/src/orchestration-v2/ProjectStore.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";
import * as SourceControlProviderRegistry from "../../apps/server/src/sourceControl/SourceControlProviderRegistry.ts";

const VISIBLE = "https://github.com/acme/web.git";
const git = (cwd, args, env = {}) => {
  const result = spawnSync(
    "git",
    [
      "-c",
      "user.name=Fixture",
      "-c",
      "user.email=fixture@example.com",
      "-c",
      "commit.gpgsign=false",
      ...args,
    ],
    { cwd, encoding: "utf8", env: { ...process.env, ...env } },
  );
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
};

/** A scenario's steps, run identically by this generator and by the Rust test. */
const base = [
  { remote: ["init", "--bare", "--initial-branch=main"] },
  { work: ["init", "--initial-branch=main"] },
  { write: { path: "README.md", content: "hello\n" } },
  { work: ["add", "README.md"] },
  { work: ["commit", "-m", "Initial commit"] },
  { work: ["remote", "add", "origin", "{remote}"] },
  // The host the pull requests live on: never fetched from or pushed to, only read for its name.
  { work: ["remote", "add", "upstream", VISIBLE] },
  { work: ["push", "-u", "origin", "main"] },
];
const w = (path, content) => ({ write: { path, content } });
const g = (...args) => ({ work: args });
const hook = (name, script) => ({ hook: { name, script } });
const remoteHook = (name, script) => ({ remoteHook: { name, script } });
const pr = (number, headRefName, over = {}) => ({
  number,
  title: `Existing ${number}`,
  url: `https://github.com/acme/web/pull/${number}`,
  baseRefName: "main",
  headRefName,
  state: "open",
  ...over,
});
const onFeature = [g("checkout", "-b", "feature/x"), w("a.txt", "a\n")];
const pushedFeature = [
  g("checkout", "-b", "feature/x"),
  w("a.txt", "a\n"),
  g("add", "a.txt"),
  g("commit", "-m", "Add a"),
  g("push", "-u", "origin", "feature/x"),
];

const scenarios = [
  {
    name: "commit with a supplied message",
    steps: [w("a.txt", "a\n")],
    input: { action: "commit", commitMessage: "Add a\n\nSecond paragraph." },
  },
  {
    name: "commit with nothing to commit",
    steps: [],
    input: { action: "commit", commitMessage: "Nothing" },
  },
  {
    name: "commit with a generated message",
    steps: [w("a.txt", "a\n")],
    input: { action: "commit" },
  },
  {
    name: "commit of named files only",
    steps: [w("a.txt", "a\n"), w("b.txt", "b\n")],
    input: { action: "commit", commitMessage: "Only a", filePaths: ["a.txt"] },
  },
  {
    name: "commit_push publishes a new branch",
    steps: onFeature,
    input: { action: "commit_push", commitMessage: "Add a" },
  },
  {
    name: "commit_push on a tracked branch",
    steps: [...pushedFeature, w("b.txt", "b\n")],
    input: { action: "commit_push", commitMessage: "Add b" },
  },
  { name: "push with nothing new", steps: [], input: { action: "push" } },
  {
    name: "push of a local commit",
    steps: [w("a.txt", "a\n"), g("add", "a.txt"), g("commit", "-m", "Local only")],
    input: { action: "push" },
  },
  {
    name: "push from a new branch",
    steps: [
      g("checkout", "-b", "feature/y"),
      w("a.txt", "a\n"),
      g("add", "a.txt"),
      g("commit", "-m", "Add a"),
    ],
    input: { action: "push" },
  },
  {
    name: "commit_push to the default branch",
    steps: [w("a.txt", "a\n")],
    input: { action: "commit_push", commitMessage: "Straight to main" },
  },
  {
    name: "commit_push_pr opens the existing pull request",
    steps: [...pushedFeature, w("b.txt", "b\n")],
    prs: [pr(7, "feature/x")],
    input: { action: "commit_push_pr", commitMessage: "Add b" },
  },
  {
    name: "commit_push_pr creates a pull request",
    steps: onFeature,
    input: { action: "commit_push_pr", commitMessage: "Add a" },
  },
  {
    name: "create_pr with a dirty tree",
    steps: [...pushedFeature, w("b.txt", "b\n")],
    input: { action: "create_pr" },
  },
  { name: "create_pr for a pushed branch", steps: pushedFeature, input: { action: "create_pr" } },
  {
    name: "create_pr for an unpushed branch",
    steps: [
      g("checkout", "-b", "feature/z"),
      w("a.txt", "a\n"),
      g("add", "a.txt"),
      g("commit", "-m", "Add a"),
    ],
    input: { action: "create_pr" },
  },
  {
    name: "create_pr when one is open",
    steps: pushedFeature,
    prs: [pr(8, "feature/x")],
    input: { action: "create_pr" },
  },
  {
    name: "feature branch from a supplied message",
    steps: [w("a.txt", "a\n")],
    input: { action: "commit", commitMessage: "Add the A file", featureBranch: true },
  },
  {
    name: "feature branch name collision",
    steps: [g("branch", "feature/add-the-a-file"), w("a.txt", "a\n")],
    input: { action: "commit", commitMessage: "Add the A file", featureBranch: true },
  },
  {
    name: "feature branch with nothing to commit",
    steps: [],
    input: { action: "commit", commitMessage: "Nothing", featureBranch: true },
  },
  { name: "feature branch for a push", steps: [], input: { action: "push", featureBranch: true } },
  {
    name: "feature branch with generated words",
    steps: [w("a.txt", "a\n")],
    input: { action: "commit_push", featureBranch: true },
  },
  {
    name: "commit hook that talks",
    steps: [hook("pre-commit", "echo checking >&1\necho warning >&2\nexit 0"), w("a.txt", "a\n")],
    input: { action: "commit", commitMessage: "Add a" },
  },
  {
    name: "commit hook that fails",
    steps: [hook("pre-commit", "echo nope >&2\nexit 1"), w("a.txt", "a\n")],
    input: { action: "commit_push", commitMessage: "Add a" },
  },
  {
    name: "push refused after the commit",
    steps: [
      remoteHook("pre-receive", "echo refused >&2\nexit 1"),
      g("checkout", "-b", "feature/x"),
      w("a.txt", "a\n"),
    ],
    input: { action: "commit_push", commitMessage: "Add a" },
  },
  { name: "detached head push", steps: [g("checkout", "--detach")], input: { action: "push" } },
  {
    name: "detached head pull request",
    steps: [g("checkout", "--detach")],
    input: { action: "create_pr" },
  },
];

const sha = /\b[0-9a-f]{40}\b/g;
const makeLayers = (state) => {
  const provider = {
    kind: "github",
    listChangeRequests: (input) => {
      state.calls.push({ op: "list", headSelector: input.headSelector, state: input.state });
      const head = input.headSelector.includes(":")
        ? input.headSelector.split(":").slice(1).join(":")
        : input.headSelector;
      return Effect.succeed(
        state.prs.filter(
          (p) => p.headRefName === head && (input.state === "all" || p.state === input.state),
        ),
      );
    },
    createChangeRequest: (input) =>
      Effect.sync(() => {
        state.calls.push({
          op: "create",
          baseRefName: input.baseRefName,
          headSelector: input.headSelector,
          title: input.title,
          body: readFileSync(input.bodyFile, "utf8"),
        });
        state.prs.push(
          pr(
            42,
            input.headSelector.includes(":")
              ? input.headSelector.split(":").slice(1).join(":")
              : input.headSelector,
            {
              title: input.title,
              baseRefName: input.baseRefName,
              updatedAt: "2026-07-01T00:00:00Z",
            },
          ),
        );
      }),
    getDefaultBranch: () => Effect.succeed("main"),
  };
  const textGeneration = TextGeneration.TextGeneration.of({
    generateCommitMessage: (input) =>
      Effect.sync(() => {
        state.calls.push({
          op: "generateCommitMessage",
          branch: input.branch,
          includeBranch: input.includeBranch === true,
        });
        return {
          subject: "Implement stacked git actions",
          body: "",
          ...(input.includeBranch ? { branch: "feature/implement-stacked-git-actions" } : {}),
        };
      }),
    generatePrContent: (input) =>
      Effect.sync(() => {
        state.calls.push({
          op: "generatePrContent",
          baseBranch: input.baseBranch,
          headBranch: input.headBranch,
        });
        return {
          title: "Add stacked git actions",
          body: "## Summary\n- Add stacked git workflow\n\n## Testing\n- Not run",
        };
      }),
    generateBranchName: () => Effect.succeed({ branch: "update-workflow" }),
    generateThreadTitle: () => Effect.succeed({ title: "Thread" }),
  });
  const registry = SourceControlProviderRegistry.SourceControlProviderRegistry.of({
    resolveLink: () => undefined,
    get: () => Effect.succeed(provider),
    resolveHandle: () => Effect.succeed({ provider, context: null }),
    resolve: () => Effect.succeed(provider),
    discover: Effect.succeed([]),
  });
  const layerVcs = GitVcsDriver.layer.pipe(
    Layer.provideMerge(VcsProcess.layer),
    Layer.provideMerge(NodeServices.layer),
    Layer.provideMerge(
      ServerConfig.layerTest(process.cwd(), { prefix: "t3-git-stacked-fixtures-" }),
    ),
  );
  const layerManager = Layer.mergeAll(
    Layer.succeed(TextGeneration.TextGeneration, textGeneration),
    Layer.mock(ProviderRegistry.ProviderRegistry)({ getProviders: Effect.succeed([]) }),
    Layer.succeed(ProjectSetupScriptRunner.ProjectSetupScriptRunner, {
      runForThread: () => Effect.succeed({ status: "no-script" }),
    }),
    layerVcs,
    ServerSettings.ServerSettingsService.layerTest({}),
  ).pipe(
    Layer.provideMerge(
      Layer.succeed(SourceControlProviderRegistry.SourceControlProviderRegistry, registry),
    ),
    Layer.provideMerge(NodeServices.layer),
  );
  const layerStores = Layer.merge(ProjectionStore.layer, ProjectStore.layer).pipe(
    Layer.provideMerge(SqlitePersistence.layerMemory),
  );
  return Effect.gen(function* () {
    const stores = yield* Layer.build(layerStores);
    return yield* GitManager.make.pipe(Effect.provide(layerManager), Effect.provideContext(stores));
  });
};

const rows = [];
for (const scenario of scenarios) {
  const root = mkdtempSync(NodePath.join(tmpdir(), "t3-stacked-fixture-"));
  const work = NodePath.join(root, "work"),
    remote = NodePath.join(root, "remote.git");
  mkdirSync(work);
  mkdirSync(remote);
  const substitute = (arg) => arg.replaceAll("{remote}", remote);
  const play = (steps) => {
    for (const step of steps) {
      if (step.remote) git(remote, step.remote.map(substitute));
      else if (step.work) git(work, step.work.map(substitute));
      else if (step.write) {
        mkdirSync(NodePath.dirname(NodePath.join(work, step.write.path)), { recursive: true });
        writeFileSync(NodePath.join(work, step.write.path), step.write.content);
      } else if (step.hook) {
        const file = NodePath.join(work, ".git", "hooks", step.hook.name);
        writeFileSync(file, `#!/bin/sh\n${step.hook.script}\n`);
        chmodSync(file, 0o755);
      } else if (step.remoteHook) {
        const file = NodePath.join(remote, "hooks", step.remoteHook.name);
        mkdirSync(NodePath.dirname(file), { recursive: true });
        writeFileSync(file, `#!/bin/sh\n${step.remoteHook.script}\n`);
        chmodSync(file, 0o755);
      }
    }
  };
  play(base.filter((step) => !step.write || true).map((step) => step));
  play(scenario.steps);
  const state = { calls: [], prs: (scenario.prs ?? []).map((p) => ({ ...p })) };
  const events = [];
  const input = { actionId: "action-1", cwd: work, ...scenario.input };
  const exit = await Effect.runPromise(
    Effect.scoped(
      Effect.gen(function* () {
        const manager = yield* makeLayers(state);
        return yield* Effect.exit(
          manager.runStackedAction(input, {
            actionId: "action-1",
            progressReporter: {
              publish: (event) =>
                Effect.sync(() => {
                  events.push(event);
                }),
            },
          }),
        );
      }),
    ),
  );
  const head = git(work, ["rev-parse", "HEAD"]);
  const markers = (text) =>
    text
      .replaceAll(head, "<sha>")
      .replaceAll(head.slice(0, 7), "<sha7>")
      .replace(sha, "<sha>")
      .replaceAll(work, "<cwd>")
      .replaceAll(remote, "<remote>");
  const clean = (value) =>
    JSON.parse(
      markers(
        JSON.stringify(value, (key, v) =>
          key === "durationMs" && typeof v === "number" ? "<ms>" : v,
        ),
      ),
    );
  const refs = git(remote, ["for-each-ref", "--format=%(refname) %(subject)", "refs/heads"])
    .split("\n")
    .filter(Boolean)
    .sort();
  rows.push({
    type: "stacked",
    name: scenario.name,
    base: base.map((s) => s),
    steps: scenario.steps,
    input: scenario.input,
    prs: scenario.prs ?? [],
    events: clean(events),
    ...(Exit.isSuccess(exit)
      ? { ok: true, result: clean(exit.value) }
      : {
          ok: false,
          error: clean(
            (() => {
              const e = Cause.squash(exit.cause);
              return {
                tag: e._tag ?? "Error",
                message: e.message,
                ...(e.detail !== undefined ? { detail: e.detail } : {}),
                ...(e.operation !== undefined ? { operation: e.operation } : {}),
              };
            })(),
          ),
        }),
    providerCalls: clean(state.calls),
    after: clean({
      branch: git(work, ["rev-parse", "--abbrev-ref", "HEAD"]),
      subject: git(work, ["log", "-1", "--format=%B"]).trim(),
      remoteRefs: refs,
      status: git(work, ["status", "--porcelain"]),
    }),
  });
  rmSync(root, { recursive: true, force: true });
}
const target = new URL("../crates/server/tests/fixtures/git-stacked-action.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(
  JSON.stringify({ scenarios: rows.length, failed: rows.filter((r) => !r.ok).map((r) => r.name) }),
);
