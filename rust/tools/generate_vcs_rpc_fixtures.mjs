// Runs the original `vcs.*` payload schemas, error schemas, worktree location and `t3.json`
// helpers, and the status refresh back-off, and records what they produce. The Rust tests replay
// the rows and require identical decoded values, failure messages and JSON envelopes.
// Also replays scripted repository-detection scenarios through the original VcsDriverRegistry
// (the disk-backed layer of VcsDriverRegistry.test.ts) and records every git call and result.
import {
  mkdtempSync,
  mkdirSync,
  rmSync,
  realpathSync,
  renameSync,
  writeFileSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import * as Duration from "../../apps/server/node_modules/effect/dist/Duration.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as GitContracts from "../../packages/contracts/src/git.ts";
import * as VcsContracts from "../../packages/contracts/src/vcs.ts";
import { resolveWorktreesDirectory } from "../../apps/server/src/worktreesDirectory.ts";
import { parseT3ProjectFile } from "../../packages/shared/src/t3ProjectFile.ts";
import * as VcsStatusBroadcaster from "../../apps/server/src/vcs/VcsStatusBroadcaster.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as VcsProjectConfig from "../../apps/server/src/vcs/VcsProjectConfig.ts";
import * as VcsDriverRegistry from "../../apps/server/src/vcs/VcsDriverRegistry.ts";

const rows = [];
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));

// ---- payload decoding ---------------------------------------------------------------------
const methods = {
  "vcs.refreshStatus": GitContracts.VcsStatusInput,
  "vcs.pull": GitContracts.VcsPullInput,
  subscribeVcsStatus: GitContracts.VcsStatusSubscriptionInput,
  "vcs.listRefs": GitContracts.VcsListRefsInput,
  "vcs.createWorktree": GitContracts.VcsCreateWorktreeInput,
  "vcs.removeWorktree": GitContracts.VcsRemoveWorktreeInput,
  "vcs.createRef": GitContracts.VcsCreateRefInput,
  "vcs.switchRef": GitContracts.VcsSwitchRefInput,
  "vcs.init": GitContracts.VcsInitInput,
};
const decodeRow = (method, original) => {
  // The wire carries JSON: an `undefined` property is simply absent.
  const payload = JSON.parse(JSON.stringify(original));
  try {
    const decoded = Schema.decodeUnknownSync(Schema.toCodecJson(methods[method]))(payload);
    return { method, payload, ok: JSON.parse(JSON.stringify(decoded)) };
  } catch (error) {
    return { method, payload, error: error.message };
  }
};
const long = (n, ch = "x") => ch.repeat(n);
const payloads = {
  "vcs.refreshStatus": [
    { cwd: "/repo" },
    { cwd: "  /repo  " },
    { cwd: "\u00a0/repo\u2003" },
    { cwd: "\ufeff/repo" },
    { cwd: "\u0085/repo" },
    { cwd: "\u180e/repo" },
    { cwd: "   " },
    { cwd: "" },
    { cwd: "\t\n" },
    {},
    { cwd: 5 },
    { cwd: null },
    { cwd: ["/repo"] },
    null,
    "/repo",
    7,
    [],
    [{ cwd: "/repo" }],
    { cwd: "/repo", extra: 1 },
  ],
  "vcs.pull": [{ cwd: "/repo" }, { cwd: " " }, {}, { cwd: false }],
  subscribeVcsStatus: [
    { cwd: "/r" },
    { cwd: "/r", includeRemote: false },
    { cwd: "/r", includeRemote: true },
    { cwd: "/r", includeRemote: null },
    { cwd: "/r", includeRemote: "no" },
    { cwd: "/r", includeRemote: 0 },
    { includeRemote: false },
  ],
  "vcs.listRefs": [
    { cwd: "/r" },
    { cwd: "/r", query: "feat" },
    { cwd: "/r", query: "  feat  " },
    { cwd: "/r", query: "   " },
    { cwd: "/r", query: "" },
    { cwd: "/r", query: long(256) },
    { cwd: "/r", query: long(257) },
    { cwd: "/r", query: ` ${long(256)} ` },
    { cwd: "/r", query: "😀".repeat(128) },
    { cwd: "/r", query: "😀".repeat(129) },
    { cwd: "/r", query: 5 },
    { cwd: "/r", query: null },
    { cwd: "/r", cursor: 0 },
    { cwd: "/r", cursor: 25 },
    { cwd: "/r", cursor: -1 },
    { cwd: "/r", cursor: 1.5 },
    { cwd: "/r", cursor: "1" },
    { cwd: "/r", cursor: null },
    { cwd: "/r", cursor: 1e300 },
    { cwd: "/r", cursor: 9007199254740991 },
    { cwd: "/r", cursor: 9007199254740992 },
    { cwd: "/r", limit: 1 },
    { cwd: "/r", limit: 200 },
    { cwd: "/r", limit: 201 },
    { cwd: "/r", limit: 0 },
    { cwd: "/r", limit: -5 },
    { cwd: "/r", limit: 0.5 },
    { cwd: "/r", limit: 2.5 },
    { cwd: "/r", limit: "3" },
    { cwd: "/r", limit: null },
    { cwd: "/r", limit: 1e3 },
    { cwd: "/r", limit: 50.0 },
    { cwd: "/r", refKind: "all" },
    { cwd: "/r", refKind: "local" },
    { cwd: "/r", refKind: "remote" },
    { cwd: "/r", refKind: "ALL" },
    { cwd: "/r", refKind: 3 },
    { cwd: "/r", refKind: null },
    { cwd: "/r", includeMatchingRemoteRefs: true },
    { cwd: "/r", includeMatchingRemoteRefs: 1 },
    { cwd: "/r", refresh: true },
    { cwd: "/r", refresh: "yes" },
    { cwd: "/r", refresh: null },
    {
      cwd: "/r",
      query: " q ",
      cursor: 3,
      includeMatchingRemoteRefs: false,
      refKind: "local",
      refresh: true,
      limit: 10,
    },
    { cwd: " ", query: long(300) },
    { cwd: "/r", query: long(300), limit: 0 },
    { cwd: "/r", limit: 0, cursor: -1 },
    { cwd: "/r", cursor: -1, query: "   " },
    {},
    { query: "x" },
  ],
  "vcs.createWorktree": [
    { cwd: "/r", refName: "main", path: null },
    { cwd: "/r", refName: "main", path: "/w" },
    {
      cwd: " /r ",
      refName: " main ",
      path: " /w ",
      newRefName: " feat ",
      baseRefName: " origin/main ",
    },
    { cwd: "/r", refName: "main", path: "  " },
    { cwd: "/r", refName: "main" },
    { cwd: "/r", refName: "main", path: undefined },
    { cwd: "/r", refName: "main", path: 5 },
    { cwd: "/r", refName: "main", path: null, newRefName: "   " },
    { cwd: "/r", refName: "main", path: null, newRefName: 3 },
    { cwd: "/r", refName: "main", path: null, baseRefName: null },
    { cwd: "/r", path: null },
    { cwd: "/r", refName: "  ", path: null },
    { refName: "a", path: null },
    { cwd: "/r", refName: "main", path: null, baseRefName: "" },
  ],
  "vcs.removeWorktree": [
    { cwd: "/r", path: "/w" },
    { cwd: "/r", path: "/w", force: true },
    { cwd: "/r", path: "/w", force: false },
    { cwd: "/r", path: "/w", force: "x" },
    { cwd: "/r", path: " " },
    { cwd: "/r" },
    { path: "/w" },
    { cwd: "/r", path: "/w", force: null },
  ],
  "vcs.createRef": [
    { cwd: "/r", refName: "feature/x" },
    { cwd: "/r", refName: " feature/x ", switchRef: true },
    { cwd: "/r", refName: "x", switchRef: false },
    { cwd: "/r", refName: "x", switchRef: 1 },
    { cwd: "/r", refName: "" },
    { cwd: "/r" },
    { refName: "x" },
    { cwd: "/r", refName: "x", switchRef: null },
  ],
  "vcs.switchRef": [
    { cwd: "/r", refName: "main" },
    { cwd: "/r", refName: "  origin/main " },
    { cwd: "/r", refName: " " },
    { cwd: "/r" },
    { refName: "main" },
    { cwd: "/r", refName: 3 },
  ],
  "vcs.init": [
    { cwd: "/r" },
    { cwd: "/r", kind: "git" },
    { cwd: "/r", kind: "jj" },
    { cwd: "/r", kind: "unknown" },
    { cwd: "/r", kind: "svn" },
    { cwd: "/r", kind: null },
    { cwd: "/r", kind: 3 },
    { cwd: " " },
    {},
  ],
};
for (const [method, list] of Object.entries(payloads))
  for (const payload of list) rows.push({ type: "input", ...decodeRow(method, payload) });

// ---- error envelopes -----------------------------------------------------------------------
const encodeError = (schema, error) => jsonOf(schema, error);
const ctx = {
  operation: "GitVcsDriver.initRepository",
  command: "git",
  cwd: "/repo",
  argumentCount: 3,
};
const unsupported = new VcsContracts.VcsUnsupportedOperationError({
  operation: "VcsDriverRegistry.resolve",
  kind: "unknown",
  detail: "No supported VCS repository was detected at /repo.",
});
const detection = new VcsContracts.VcsRepositoryDetectionError({
  operation: "GitVcsDriver.detectRepository",
  cwd: "/repo",
  detail: "boom",
});
const vcsErrors = {
  VcsProcessSpawnError: new VcsContracts.VcsProcessSpawnError({
    ...ctx,
    cause: new Error("spawn git ENOENT"),
  }),
  VcsProcessSpawnErrorNoCount: new VcsContracts.VcsProcessSpawnError({
    operation: "o",
    command: "git",
    cwd: "/repo",
    cause: new Error("x"),
  }),
  VcsProcessExitError: new VcsContracts.VcsProcessExitError({
    ...ctx,
    exitCode: 128,
    detail: "Process exited with a non-zero status.",
    failureKind: "command-failed",
    stderrLength: 42,
    stderrTruncated: false,
  }),
  VcsProcessExitErrorRetryable: new VcsContracts.VcsProcessExitError({
    ...ctx,
    exitCode: 1,
    detail: "d",
    failureKind: "authentication",
    retryable: true,
    stderrLength: 0,
    stderrTruncated: true,
  }),
  VcsProcessExitErrorBare: new VcsContracts.VcsProcessExitError({
    operation: "o",
    command: "git",
    cwd: "/repo",
    exitCode: 2,
    detail: "d",
  }),
  VcsProcessTimeoutError: new VcsContracts.VcsProcessTimeoutError({ ...ctx, timeoutMs: 10000 }),
  VcsProcessStdinWriteError: new VcsContracts.VcsProcessStdinWriteError({
    ...ctx,
    stdinBytes: 12,
    cause: new Error("EPIPE"),
  }),
  VcsProcessOutputReadError: new VcsContracts.VcsProcessOutputReadError({
    ...ctx,
    stream: "exitCode",
    cause: new Error("closed"),
  }),
  VcsProcessOutputLimitError: new VcsContracts.VcsProcessOutputLimitError({
    ...ctx,
    stream: "stdout",
    maxBytes: 65536,
    observedBytes: 70000,
  }),
  VcsProcessMissingExitCodeError: new VcsContracts.VcsProcessMissingExitCodeError({ ...ctx }),
  VcsRepositoryDetectionError: detection,
  VcsUnsupportedOperationError: unsupported,
  VcsUnsupportedOperationErrorJj: new VcsContracts.VcsUnsupportedOperationError({
    operation: "VcsDriverRegistry.get",
    kind: "jj",
    detail: "No jj VCS driver is registered.",
  }),
};
for (const [name, error] of Object.entries(vcsErrors))
  rows.push({ type: "vcsError", name, json: encodeError(VcsContracts.VcsError, error) });
const manager = (cause) =>
  new GitContracts.GitManagerError({
    operation: "GitWorkflowService.localStatus",
    cwd: "/repo",
    detail: "Failed to detect a VCS repository for this Git workflow.",
    ...(cause ? { cause } : {}),
  });
rows.push({
  type: "managerError",
  name: "withCause",
  json: encodeError(GitContracts.GitManagerServiceError, manager(detection)),
});
rows.push({
  type: "managerError",
  name: "withoutCause",
  json: encodeError(GitContracts.GitManagerServiceError, manager()),
});
rows.push({
  type: "managerError",
  name: "unsupportedCause",
  json: encodeError(GitContracts.GitManagerServiceError, manager(unsupported)),
});
const route = (cause) =>
  new GitContracts.GitCommandError({
    operation: "GitWorkflowService.switchRef",
    command: "vcs-route",
    cwd: "/repo",
    detail: "Failed to resolve the VCS driver for this Git command.",
    ...(cause ? { cause } : {}),
  });
rows.push({
  type: "routeError",
  name: "withCause",
  json: encodeError(GitContracts.GitCommandError, route(unsupported)),
});
rows.push({
  type: "routeError",
  name: "withoutCause",
  json: encodeError(
    GitContracts.GitCommandError,
    new GitContracts.GitCommandError({
      operation: "GitWorkflowService.switchRef",
      command: "vcs-route",
      cwd: "/repo",
      detail:
        "The GitWorkflowService.switchRef command currently supports Git repositories only; detected jj.",
    }),
  ),
});
rows.push({
  type: "worktreeLocationError",
  json: encodeError(
    GitContracts.GitCommandError,
    new GitContracts.GitCommandError({
      operation: "GitVcsDriver.createWorktree",
      command: "git worktree add",
      cwd: "/repo",
      detail:
        'The worktree location "D:\\worktrees" must be an absolute folder on this machine, not a drive root. Change it in Settings → Storage.',
    }),
  ),
});

// ---- worktrees directory -------------------------------------------------------------------
const homePath = { ...NodePath.posix };
const process_ = globalThis.process;
process_.env.HOME = "/home/fixture";
for (const setting of [
  "",
  "/srv/worktrees",
  "/srv/worktrees/",
  "/srv//a/./b/../c",
  "~",
  "~/trees",
  "~\\trees",
  "~other/trees",
  "relative/dir",
  "./dir",
  "D:\\worktrees",
  "C:/worktrees",
  "/",
  "//",
  "/..",
  "/a/..",
  "/a/../..",
  "\\\\server\\share",
  "  /padded ",
  "/srv/wörk trees",
]) {
  rows.push({
    type: "worktreesDirectory",
    setting,
    defaultDir: "/home/fixture/.t3/worktrees",
    resolved: resolveWorktreesDirectory(setting, "/home/fixture/.t3/worktrees", homePath),
  });
}

// ---- t3.json -------------------------------------------------------------------------------
const bigScripts = JSON.stringify({
  scripts: Array.from({ length: 51 }, (_, i) => ({ name: `s${i}`, command: "c" })),
});
const okScripts = JSON.stringify({
  scripts: Array.from({ length: 50 }, (_, i) => ({ name: `s${i}`, command: "c" })),
});
const projectFiles = [
  "{}",
  '{"worktreeSubmodules":"none"}',
  '{"worktreeSubmodules":"top-level"}',
  '{"worktreeSubmodules":"recursive"}',
  '{"worktreeSubmodules":"all"}',
  '{"worktreeSubmodules":null}',
  '{"worktreeSubmodules":1}',
  '// c\n{"worktreeSubmodules":"none",}',
  '{"worktreeSubmodules":"none", /* x */ "unknown":{"a":[1,]}}',
  "",
  "   ",
  "{nope",
  "[]",
  "null",
  '"x"',
  "7",
  '{"worktreeSubmodules":"none","iconPath":""}',
  '{"worktreeSubmodules":"none","iconPath":"  "}',
  '{"worktreeSubmodules":"none","iconPath":" a.svg "}',
  `{"worktreeSubmodules":"none","iconPath":"${long(512)}"}`,
  `{"worktreeSubmodules":"none","iconPath":"${long(513)}"}`,
  `{"worktreeSubmodules":"none","iconPath":" ${long(512)}"}`,
  '{"worktreeSubmodules":"none","iconPath":5}',
  '{"worktreeSubmodules":"none","defaultThreadEnvMode":"worktree"}',
  '{"worktreeSubmodules":"none","defaultThreadEnvMode":"local"}',
  '{"worktreeSubmodules":"none","defaultThreadEnvMode":"other"}',
  '{"worktreeSubmodules":"none","$schema":"https://t3.codes/schema/t3.json"}',
  '{"worktreeSubmodules":"none","$schema":5}',
  '{"worktreeSubmodules":"none","scripts":[]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b"}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":" ","command":"b"}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a"}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b","icon":"build","runOnWorktreeCreate":true,"runOnSettle":false,"async":true,"previewUrl":"http://x","autoOpenPreview":true}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b","icon":"rocket"}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b","async":"yes"}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b","previewUrl":" "}]}',
  '{"worktreeSubmodules":"none","scripts":[{"name":"a","command":"b","autoOpenPreview":null}]}',
  '{"worktreeSubmodules":"none","scripts":{"name":"a"}}',
  '{"worktreeSubmodules":"none","scripts":["a"]}',
  '{"worktreeSubmodules":"none","scripts":null}',
  bigScripts,
  okScripts.replace("{", '{"worktreeSubmodules":"top-level",'),
  '\ufeff{"worktreeSubmodules":"none"}',
];
for (const contents of projectFiles) {
  const parsed = parseT3ProjectFile(contents);
  rows.push({
    type: "projectFile",
    contents,
    parsed: parsed === null ? null : { worktreeSubmodules: parsed.worktreeSubmodules ?? null },
  });
}

// ---- status refresh back-off ---------------------------------------------------------------
for (const [failures, configuredMs] of [
  [1, 1000],
  [2, 1000],
  [3, 1000],
  [4, 1000],
  [1, 300000],
  [2, 300000],
  [20, 1000],
  [7, 1000],
  [8, 1000],
  [3, 600000],
  [100, 0],
]) {
  rows.push({
    type: "failureDelay",
    failures,
    configuredMs,
    delayMs: Duration.toMillis(
      VcsStatusBroadcaster.remoteRefreshFailureDelay(failures, Duration.millis(configuredMs)),
    ),
  });
}

// ---- repository detection scenarios --------------------------------------------------------
const processOutput = (stdout, exitCode = 0, stderr = "") => ({
  exitCode,
  stdout,
  stderr,
  stdoutTruncated: false,
  stderrTruncated: false,
});
const scenarios = [
  {
    name: "caches detection per cwd and kind",
    root: ".",
    setup: ["repo/.git/"],
    steps: [
      { resolve: "repo", kind: "git" },
      { resolve: "repo", kind: "git" },
      { detect: "repo" },
      { detect: "repo" },
    ],
  },
  {
    name: "detects again when the repository is removed",
    root: ".",
    setup: ["repo/.git/"],
    steps: [{ detect: "repo" }, { rm: "repo/.git" }, { detect: "repo" }],
  },
  {
    name: "detects again when git init nests a repository",
    root: "parent",
    setup: ["parent/.git/", "parent/project/"],
    steps: [
      { detect: "parent/project" },
      { mkdir: "parent/project/.git" },
      { detect: "parent/project" },
    ],
  },
  {
    name: "detects again when git init lands between the cwd and its root",
    root: "parent",
    setup: ["parent/.git/", "parent/a/b/"],
    steps: [{ detect: "parent/a/b" }, { mkdir: "parent/a/.git" }, { detect: "parent/a/b" }],
  },
  {
    name: "detects a repository created after a negative lookup",
    root: ".",
    setup: ["repo/"],
    steps: [
      { detect: "repo" },
      { detect: "repo" },
      { mkdir: "repo/.git" },
      { detect: "repo" },
      { detect: "repo" },
    ],
  },
  {
    name: "resolve fails with a typed error when nothing is a repository",
    root: ".",
    setup: ["plain/"],
    steps: [
      { resolve: "plain" },
      { resolve: "plain", kind: "git" },
      { resolve: "plain", kind: "unknown" },
      { resolve: "plain", kind: "jj" },
    ],
  },
  {
    name: "kinds are cached apart",
    root: ".",
    setup: ["repo/.git/"],
    steps: [
      { detect: "repo", kind: "git" },
      { detect: "repo" },
      { detect: "repo", kind: "unknown" },
      { detect: "repo", kind: "git" },
      { detect: "repo", kind: "unknown" },
    ],
  },
  {
    name: "an unsupported kind fails detection",
    root: ".",
    setup: ["repo/.git/"],
    steps: [
      { detect: "repo", kind: "jj" },
      { detect: "repo", kind: "jj" },
    ],
  },
  {
    name: "project config selects the kind",
    root: ".",
    setup: ["repo/.git/", "repo/.t3code/", "other/.git/"],
    files: { "repo/.t3code/vcs.json": '{"vcs":{"kind":"jj"}}' },
    steps: [{ detect: "repo" }, { detect: "other" }],
  },
  {
    name: "drivers by kind",
    root: ".",
    setup: [],
    steps: [{ get: "git" }, { get: "jj" }, { get: "unknown" }],
  },
];

const stripRoot = (value, root) => JSON.parse(JSON.stringify(value).split(root).join("$ROOT"));
for (const scenario of scenarios) {
  const root = realpathSync(mkdtempSync(NodePath.join(tmpdir(), "t3-vcs-registry-fixture-")));
  for (const entry of scenario.setup) mkdirSync(NodePath.join(root, entry), { recursive: true });
  for (const [file, contents] of Object.entries(scenario.files ?? {}))
    writeFileSync(NodePath.join(root, file), contents);
  const rootDir = NodePath.join(root, scenario.root);
  const calls = [];
  const normalize = (args) => (args[0] === "-C" && args.length >= 2 ? args.slice(2) : args);
  const layer = Layer.effect(VcsDriverRegistry.VcsDriverRegistry, VcsDriverRegistry.make).pipe(
    Layer.provide(NodeServices.layer),
    Layer.provide(VcsProjectConfig.layer),
    Layer.provide(NodeServices.layer),
    Layer.provide(
      Layer.mock(VcsProcess.VcsProcess)({
        run: (input) =>
          Effect.sync(() => {
            const command = normalize(input.args).join(" ");
            calls.push(command);
            let repoDir = input.cwd;
            while (repoDir !== null && !existsSync(NodePath.join(repoDir, ".git")))
              repoDir = repoDir === rootDir ? null : NodePath.dirname(repoDir);
            if (repoDir === null) return processOutput("", 128, "fatal: not a git repository");
            if (command === "rev-parse --is-inside-work-tree") return processOutput("true\n");
            if (command === "rev-parse --show-toplevel") return processOutput(`${repoDir}\n`);
            if (command === "rev-parse --git-common-dir")
              return processOutput(
                `${NodePath.relative(input.cwd, NodePath.join(repoDir, ".git"))}\n`,
              );
            return processOutput("");
          }),
      }),
    ),
  );
  const results = [];
  const handleJson = (handle) =>
    handle === null
      ? null
      : {
          kind: handle.kind,
          rootPath: NodePath.relative(root, handle.repository.rootPath) || ".",
          metadataPath: handle.repository.metadataPath,
        };
  await Effect.runPromise(
    Effect.gen(function* () {
      const registry = yield* VcsDriverRegistry.VcsDriverRegistry;
      for (const step of scenario.steps) {
        const before = calls.length;
        const record = (extra) => results.push({ step, ...extra, calls: calls.slice(before) });
        if (step.mkdir) {
          mkdirSync(NodePath.join(root, step.mkdir), { recursive: true });
          results.push({ step });
          continue;
        }
        if (step.rm) {
          rmSync(NodePath.join(root, step.rm), { recursive: true, force: true });
          results.push({ step });
          continue;
        }
        if (step.get) {
          const exit = yield* Effect.exit(registry.get(step.get));
          record(
            exit._tag === "Success"
              ? { ok: exit.value.capabilities.kind }
              : {
                  error: stripRoot(
                    jsonOf(VcsContracts.VcsError, exit.cause.reasons[0].error),
                    root,
                  ),
                },
          );
          continue;
        }
        const input = {
          cwd: NodePath.join(root, step.detect ?? step.resolve),
          ...(step.kind ? { requestedKind: step.kind } : {}),
        };
        const exit = yield* Effect.exit(
          step.detect !== undefined ? registry.detect(input) : registry.resolve(input),
        );
        record(
          exit._tag === "Success"
            ? { ok: handleJson(exit.value) }
            : {
                error: stripRoot(jsonOf(VcsContracts.VcsError, exit.cause.reasons[0].error), root),
              },
        );
      }
    }).pipe(Effect.provide(layer)),
  );
  rmSync(root, { recursive: true, force: true });
  rows.push({
    type: "registry",
    scenario: scenario.name,
    root: scenario.root,
    setup: scenario.setup,
    files: scenario.files ?? {},
    results: stripRoot(results, root),
  });
}

const target = new URL("../crates/server/tests/fixtures/vcs-rpc.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
