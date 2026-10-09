// Runs the original GitVcsDriver checkpoint capture on scripted temporary repositories and
// records the resulting tree, ref and commit shape, so the Rust port can be held to the same
// git objects. Git runs only inside the temporary directories created here.
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  rmSync,
  renameSync,
  rmdirSync,
  unlinkSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as GitVcsDriver from "../../apps/server/src/vcs/GitVcsDriver.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";

// Fixed dates make commits made by the scenario (nested repositories' gitlinks) reproducible.
const env = {
  ...process.env,
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_TERMINAL_PROMPT: "0",
  GIT_AUTHOR_DATE: "2020-01-01T00:00:00Z",
  GIT_COMMITTER_DATE: "2020-01-01T00:00:00Z",
};
const git = (cwd, args) =>
  execFileSync("git", args, { cwd, env, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });

const identity = [
  ["git", "init"],
  ["git", "config", "user.email", "test@test.com"],
  ["git", "config", "user.name", "Test"],
];
const scenarios = [
  {
    name: "staged-and-unstaged",
    ops: [
      ...identity,
      ["write", "file.txt", "initial\n"],
      ["git", "add", "."],
      ["git", "commit", "-m", "initial"],
      ["write", "file.txt", "staged\n"],
      ["git", "add", "."],
      ["write", "file.txt", "unstaged\n"],
      ["write", "untracked/new.txt", "new\n"],
      ["write", "script.sh", "#!/bin/sh\n"],
    ],
  },
  {
    name: "unborn-head",
    ops: [
      ...identity,
      ["write", "a.txt", "a\n"],
      ["write", "dir/b.txt", "b\n"],
      ["git", "add", "a.txt"],
      ["write", "a.txt", "changed\n"],
    ],
  },
  {
    name: "deleted-and-ignored",
    ops: [
      ...identity,
      ["write", ".gitignore", "ignored.log\nbuild/\n"],
      ["write", "keep.txt", "keep\n"],
      ["write", "gone.txt", "gone\n"],
      ["git", "add", "."],
      ["git", "commit", "-m", "base"],
      ["remove", "gone.txt"],
      ["write", "ignored.log", "x\n"],
      ["write", "build/out.o", "x\n"],
      ["write", "src/lib.rs", "fn main() {}\n"],
    ],
  },
  {
    name: "nested-repositories",
    ops: [
      ...identity,
      ["write", "file.txt", "initial\n"],
      ["git", "add", "."],
      ["git", "commit", "-m", "initial"],
      ["git", "init", "scratch/empty [repo]"],
      ["write", "scratch/empty [repo]/private.txt", "nested\n"],
      ["git", "init", "another empty"],
      ["git", "init", "committed"],
      [
        "git",
        "-C",
        "committed",
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@test.com",
        "commit",
        "--allow-empty",
        "-m",
        "initial",
      ],
      ["write", "untracked.txt", "new\n"],
    ],
  },
  {
    name: "sparse-cone",
    ops: [
      ...identity,
      ["write", "scope/in/edit", "original\n"],
      ["write", "scope/in/delete", "original\n"],
      ["write", "scope/out/deep/absent", "original\n"],
      ["write", "scope/out/present", "original\n"],
      ["write", "elsewhere/file", "original\n"],
      ["git", "add", "."],
      ["git", "commit", "-m", "sparse fixture"],
      ["git", "sparse-checkout", "set", "--cone", "--sparse-index", "scope/in", "elsewhere"],
      ["write", "scope/in/edit", "staged\n"],
      ["git", "add", "."],
      ["git", "config", "sparse.expectFilesOutsideOfPatterns", "true"],
      ["write", "scope/in/edit", "working\n"],
      ["write", "scope/out/present", "modified skipped\n"],
      ["write", "scope/out/new file", "new outside cone\n"],
      ["remove", "scope/in/delete"],
    ],
  },
];

const apply = (cwd, op) => {
  const [kind, ...rest] = op;
  if (kind === "git") git(cwd, rest);
  else if (kind === "write") {
    mkdirSync(dirname(join(cwd, rest[0])), { recursive: true });
    writeFileSync(join(cwd, rest[0]), rest[1]);
  } else if (kind === "remove") unlinkSync(join(cwd, rest[0]));
  else throw new Error(`unknown op ${kind}`);
};

const layer = Layer.mergeAll(GitVcsDriver.layerVcs).pipe(
  Layer.provideMerge(VcsProcess.layer),
  Layer.provideMerge(NodeServices.layer),
);

const rows = [];
for (const scenario of scenarios) {
  const cwd = mkdtempSync(join(tmpdir(), "t3-capture-fixture-"));
  try {
    for (const op of scenario.ops) apply(cwd, op);
    const checkpointRef = "refs/t3/checkpoints/fixture/turn/1";
    await Effect.runPromise(
      Effect.gen(function* () {
        const driver = yield* GitVcsDriver.makeVcsDriverShape();
        yield* driver.checkpoints.captureCheckpoint({ cwd, checkpointRef });
      }).pipe(Effect.provide(layer)),
    );
    rows.push({
      name: scenario.name,
      ops: scenario.ops,
      checkpointRef,
      tree: git(cwd, ["rev-parse", `${checkpointRef}^{tree}`]).trim(),
      lsTree: git(cwd, ["ls-tree", "-r", "-z", checkpointRef]),
      commit: git(cwd, ["log", "-1", "--format=%an|%ae|%cn|%ce|%s|%P", checkpointRef]),
      status: git(cwd, ["status", "--porcelain=v1", "-z"]),
    });
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
}
const target = new URL("../crates/vcs/tests/fixtures/checkpoint-capture.json", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, JSON.stringify(rows, null, 1) + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ scenarios: rows.length }));
