// The git projects of the fixture. Every repository carries a README that says what it is, lives under
// the scratch directory, and is built with git configured from the environment alone (no global
// config, no signing, no hooks, no network).
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";

export const README_LINE = "Created by the T3 Code Rust port browser fixture. Safe to delete.";

export function gitEnv(home) {
  return {
    PATH: "/usr/bin:/bin",
    HOME: home,
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_TERMINAL_PROMPT: "0",
    GIT_ALLOW_PROTOCOL: "file",
    GIT_AUTHOR_NAME: "Fixture Author",
    GIT_AUTHOR_EMAIL: "fixture-author@example.invalid",
    GIT_COMMITTER_NAME: "Fixture Author",
    GIT_COMMITTER_EMAIL: "fixture-author@example.invalid",
  };
}

export function makeGit(home) {
  let tick = 0;
  return (cwd, ...args) => {
    // Commit dates step forward a minute at a time, so history reads as a story and is the same every run.
    tick += 1;
    const date = new Date(Date.UTC(2026, 8, 20, 9, 0, 0) + tick * 60_000).toISOString();
    return execFileSync(
      "git",
      ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", ...args],
      {
        cwd,
        env: { ...gitEnv(home), GIT_AUTHOR_DATE: date, GIT_COMMITTER_DATE: date },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      },
    ).trim();
  };
}

export function write(root, path, content) {
  const full = join(root, path);
  mkdirSync(dirname(full), { recursive: true });
  writeFileSync(full, content);
}

/** Bytes that are the same every run and are not text: the start of a PNG, then a hash chain. */
export function binary(seed, size) {
  const chunks = [
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d]),
  ];
  let block = createHash("sha256").update(seed).digest();
  let length = chunks[0].length;
  while (length < size) {
    chunks.push(block);
    length += block.length;
    block = createHash("sha256").update(block).digest();
  }
  return Buffer.concat(chunks).subarray(0, size);
}

function csv(rows, variant) {
  const lines = ["id,name,region,total,note"];
  for (let i = 1; i <= rows; i += 1) {
    const changed = variant && i % 3 === 0;
    lines.push(
      `${i},widget-${i},region-${i % 9},${(i * 37) % 1000}.${i % 100},${changed ? "revised" : "original"} note for row ${i} ${"x".repeat(i % 7)}`,
    );
  }
  return lines.join("\n") + "\n";
}

function describe(title, extra) {
  return `# ${title}\n\n${README_LINE}\n${extra ? "\n" + extra + "\n" : ""}`;
}

/** An initialised repository on `main` with the fixture identity configured locally too. */
export function initRepo(git, path) {
  mkdirSync(path, { recursive: true });
  git(path, "init", "-q", "-b", "main");
  git(path, "config", "user.name", "Fixture Author");
  git(path, "config", "user.email", "fixture-author@example.invalid");
  git(path, "config", "commit.gpgsign", "false");
}

const APP = (extra = "") =>
  `export function start(name: string) {\n  console.log("starting " + name);\n  return name.length;\n}\n\nexport function stop() {\n  return true;\n}\n${extra}`;

/** The project for the commit dialog and the diff panel: changes of every kind, uncommitted. */
export function buildDirtyProject(git, path) {
  initRepo(git, path);
  write(
    path,
    "README.md",
    describe(
      "Dirty project",
      "Uncommitted changes of every kind, for the commit dialog and the diff panel.",
    ),
  );
  write(
    path,
    "package.json",
    JSON.stringify({ name: "dirty-fixture", version: "0.0.0", private: true }, null, 2) + "\n",
  );
  write(path, "src/app.ts", APP());
  write(
    path,
    "src/util.ts",
    `export const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));\nexport const sum = (items: number[]) => items.reduce((a, b) => a + b, 0);\n`,
  );
  write(path, "src/old.ts", `// Superseded by app.ts.\nexport const legacy = () => "legacy";\n`);
  write(
    path,
    "docs/guide.md",
    `# Guide\n\nThis guide has three sections.\n\n## One\nFirst.\n\n## Two\nSecond.\n\n## Three\nThird.\n`,
  );
  write(path, "assets/logo.png", binary("logo-v1", 4096));
  write(path, "data/table.csv", csv(5000, false));
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Add the starting files");
  write(path, "src/app.ts", APP("\nexport const VERSION = 1;\n"));
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Export a version constant");

  // Modified, with several hunks.
  write(
    path,
    "src/app.ts",
    `import { clamp } from "./util";\n\n${APP("\nexport const VERSION = 2;\n").replace('console.log("starting " + name);', "console.log(`starting ${name}`);").replace("return true;", "return clamp(1, 0, 1) === 1;")}\nexport function restart(name: string) {\n  stop();\n  return start(name);\n}\n`,
  );
  // Modified and already staged, then modified again (both columns of `git status`).
  write(
    path,
    "docs/guide.md",
    `# Guide\n\nThis guide has four sections.\n\n## One\nFirst.\n\n## Two\nSecond.\n\n## Three\nThird.\n\n## Four\nFourth.\n`,
  );
  git(path, "add", "docs/guide.md");
  write(
    path,
    "docs/guide.md",
    `# Guide\n\nThis guide has four sections.\n\n## One\nFirst, revised.\n\n## Two\nSecond.\n\n## Three\nThird.\n\n## Four\nFourth.\n\n## Five\nAdded after staging.\n`,
  );
  // Renamed (staged), with an edit.
  git(path, "mv", "src/util.ts", "src/helpers.ts");
  write(
    path,
    "src/helpers.ts",
    `export const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));\nexport const sum = (items: number[]) => items.reduce((a, b) => a + b, 0);\nexport const mean = (items: number[]) => sum(items) / items.length;\n`,
  );
  git(path, "add", "src/helpers.ts");
  // Added (staged).
  write(path, "src/feature.ts", `export const feature = { name: "dirty", enabled: true };\n`);
  git(path, "add", "src/feature.ts");
  // Deleted (unstaged).
  rmSync(join(path, "src/old.ts"));
  // A binary file, changed.
  write(path, "assets/logo.png", binary("logo-v2", 6144));
  // A tracked file whose diff is larger than the review panel's patch limit.
  write(path, "data/table.csv", csv(5000, true));
  // Untracked: a note, a new binary, and a large file.
  write(path, "docs/notes.md", `# Notes\n\nAn untracked note.\n\n- one\n- two\n`);
  write(path, "assets/new-icon.png", binary("icon", 2048));
  write(
    path,
    "data/large.log",
    Array.from(
      { length: 12000 },
      (_, i) =>
        `2026-09-20T09:${String(i % 60).padStart(2, "0")}:00Z INFO request ${i} completed in ${(i * 7) % 900}ms path=/api/items/${i % 400}`,
    ).join("\n") + "\n",
  );
}

/** A repository with a feature branch of several commits, pushed to a local bare repository. */
export function buildBranchProject(
  git,
  path,
  remotePath,
  remoteUrl,
  { title, branch, description },
) {
  initRepo(git, path);
  write(path, "README.md", describe(title, description));
  write(path, "src/index.ts", `export const name = "${title}";\n`);
  write(path, "src/config.ts", `export const config = { retries: 2, timeoutMs: 1000 };\n`);
  write(path, "src/legacy.ts", `export const legacy = true;\n`);
  write(path, "docs/usage.md", `# Usage\n\nRun it.\n`);
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Initial import");
  write(path, "src/index.ts", `export const name = "${title}";\nexport const ready = true;\n`);
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Mark the project ready");
  mkdirSync(dirname(remotePath), { recursive: true });
  git(path, "init", "-q", "--bare", "-b", "main", remotePath);
  git(path, "remote", "add", "origin", `file://${remotePath}`);
  git(path, "push", "-q", "origin", "main");
  git(path, "switch", "-q", "-c", branch);
  write(
    path,
    "src/config.ts",
    `export const config = { retries: 5, timeoutMs: 2500, backoff: "exponential" };\n`,
  );
  git(path, "commit", "-q", "-a", "-m", "Retry with exponential backoff");
  write(
    path,
    "src/retry.ts",
    `export async function retry<T>(run: () => Promise<T>, attempts: number): Promise<T> {\n  let last: unknown;\n  for (let i = 0; i < attempts; i += 1) {\n    try {\n      return await run();\n    } catch (error) {\n      last = error;\n    }\n  }\n  throw last;\n}\n`,
  );
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Add a retry helper");
  git(path, "mv", "docs/usage.md", "docs/guide.md");
  write(
    path,
    "docs/guide.md",
    `# Guide\n\nRun it. Failed calls are retried with exponential backoff.\n`,
  );
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Rename the usage doc and describe retries");
  git(path, "rm", "-q", "src/legacy.ts");
  git(path, "commit", "-q", "-m", "Remove the legacy module");
  git(path, "push", "-q", "-u", "origin", branch);
  // From here the remote has the host's name, and only pushes still reach the local bare repository:
  // `git remote -v` reports the fetch URL as written, which is what makes the server read a host.
  git(path, "remote", "set-url", "origin", remoteUrl);
  git(path, "config", `url.file://${remotePath}.pushInsteadOf`, remoteUrl);
}

export function buildPlainProject(git, path, title, description) {
  initRepo(git, path);
  write(path, "README.md", describe(title, description));
  write(path, "src/main.ts", `export const main = () => "hello";\n`);
  git(path, "add", ".");
  git(path, "commit", "-q", "-m", "Initial import");
}

export function exists(path) {
  return existsSync(path);
}
