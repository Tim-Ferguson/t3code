// Replays the original VcsProcess.ts policy layer (exit classification, error shapes,
// checkpoint-capture retry) against a scripted ProcessRunner. No real process is spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";

const baseInput = {
  operation: "test.process-boundary",
  command: "git",
  args: ["status", "--short"],
  cwd: "/workspace",
};

const describeError = (error) => {
  const fields = { _tag: error._tag, message: error.message };
  for (const key of Object.keys(error)) {
    const value = error[key];
    if (key === "cause") fields.hasCause = value !== undefined;
    else if (value === undefined) fields[key] = null;
    else if (typeof value !== "object") fields[key] = value;
  }
  return fields;
};

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

const runWith = async (runner, input) => {
  const service = await Effect.runPromise(
    VcsProcess.make.pipe(
      Effect.provideService(
        ProcessRunner.ProcessRunner,
        ProcessRunner.ProcessRunner.of({ run: runner }),
      ),
    ),
  );
  const exit = await Effect.runPromise(
    service.run(input).pipe(
      Effect.map((value) => ({ ok: value })),
      Effect.catch((error) => Effect.succeed({ error: describeError(error) })),
    ),
  );
  return exit;
};

const rows = [];

// Exit classification and the fixed error detail per failure kind.
const stderrs = [
  "",
  "fatal: not a git repository",
  "authentication failed for token super-secret-token",
  "Authentication Failed",
  "gh auth login required",
  "glab auth login",
  "az devops login",
  "Please run az login",
  "no oauth token found",
  "401 Unauthorized",
  "GraphQL: API rate limit already exceeded for user ID 51714798",
  "rate limit exceeded",
  "You have exceeded a secondary rate limit",
  "429 Too Many Requests",
  "HTTP 429: slow down",
  "GraphQL: Could not resolve to a PullRequest with the number of 4.",
  "repository.pullRequest: not found",
  'no pull requests found for branch "x"',
  "pull request not found",
  "merge request not found",
  "404 Not Found",
  "something not found",
  "Pull request 5 does not exist",
  "pull request 5 not found in project",
  "fatal: Unable to create '/private/repo/index.lock': File exists",
  "fatal: Unable to create '/private/repo/index.lock': Permission denied",
  "fatal: Unable to create '/repo/.git/refs/t3/x.lock': File exists.\nsecond line",
  'fatal: unable to create "/x/y.lock": file exists',
  'error: open("/private/repo/file"): No such file or directory',
  "fatal: unable to stat '/private/repo/file': No such file or directory",
  'error: lstat("gone"): No such file or directory',
  'error: open("/private/repo/file"): Permission denied',
  "error: '/private/repo/child/' does not have a commit checked out",
  "fatal: index file corrupt",
  "unable to stat\n: no such file or directory",
  "café 😀 authentication failed É",
  "😀😀 plain failure",
  "ASCII only",
];
for (const command of ["git", "gh", "glab", "az", "node"]) {
  for (const stderr of stderrs) {
    for (const stderrTruncated of [false, true]) {
      const input = { ...baseInput, command };
      const result = await runWith(
        () => Effect.succeed(output({ stderr, code: 128, stderrTruncated })),
        input,
      );
      rows.push({ type: "exit", command, stderr, stderrTruncated, exitCode: 128, result });
    }
  }
}

// Allowed non-zero exits and missing exit codes.
for (const allowNonZeroExit of [false, true]) {
  for (const code of [0, 1, 2, null]) {
    const result = await runWith(
      () => Effect.succeed(output({ code, stdout: "out", stderr: "err" })),
      { ...baseInput, allowNonZeroExit },
    );
    rows.push({ type: "boundary", allowNonZeroExit, code, result });
  }
}

// Runner failures mapped to VCS errors.
const fields = { command: "git", argumentCount: 2, cwd: "/workspace" };
const failures = [
  ["spawn", new ProcessRunner.ProcessSpawnError({ ...fields, cause: new Error("spawn failed") })],
  [
    "stdin",
    new ProcessRunner.ProcessStdinError({
      ...fields,
      stdinBytes: 47,
      cause: new Error("secret stdin failure"),
    }),
  ],
  [
    "output-limit-stdout",
    new ProcessRunner.ProcessOutputLimitError({
      ...fields,
      stream: "stdout",
      maxBytes: 128,
      observedBytes: 2048,
    }),
  ],
  [
    "output-limit-stderr",
    new ProcessRunner.ProcessOutputLimitError({
      ...fields,
      stream: "stderr",
      maxBytes: 8,
      observedBytes: 9,
    }),
  ],
  [
    "read-stdout",
    new ProcessRunner.ProcessReadError({ ...fields, stream: "stdout", cause: new Error("x") }),
  ],
  [
    "read-stderr",
    new ProcessRunner.ProcessReadError({ ...fields, stream: "stderr", cause: new Error("x") }),
  ],
  [
    "read-exit-code",
    new ProcessRunner.ProcessReadError({ ...fields, stream: "exitCode", cause: new Error("x") }),
  ],
  ["timeout", new ProcessRunner.ProcessTimeoutError({ ...fields, timeoutMs: 30000 })],
];
for (const [name, failure] of failures) {
  const result = await runWith(() => Effect.fail(failure), baseInput);
  rows.push({ type: "runner-failure", name, result });
}

// Checkpoint capture retries transient git lock failures; nothing else is retried.
const lock = "fatal: Unable to create '/repo/index.lock': File exists";
for (const operation of [VcsProcess.CHECKPOINT_CAPTURE_OPERATION, "other.operation"]) {
  for (const command of ["git", "gh"]) {
    for (const streaming of [false, true]) {
      for (const stderr of [lock, "fatal: index file corrupt"]) {
        for (const failures of [0, 1, 2, 3, 100]) {
          let attempts = 0;
          const input = {
            ...baseInput,
            command,
            operation,
            ...(streaming ? { onStdoutChunk: () => {} } : {}),
          };
          const result = await runWith(
            () =>
              Effect.sync(() => {
                attempts += 1;
                return output({
                  code: attempts > failures ? 0 : attempts,
                  stderr: attempts > failures ? "" : stderr,
                });
              }),
            input,
          );
          rows.push({
            type: "retry",
            operation,
            command,
            streaming,
            stderr,
            failures,
            attempts,
            result,
          });
        }
      }
    }
  }
}

const target = new URL("../crates/vcs/tests/fixtures/vcs-process.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
