// How the original reports renamed, copied and oddly named paths. Each scenario is built in a real
// temporary repository with the machine's git configuration switched off; the git output the original
// reads (`diff-index -M --numstat HEAD`, `status --porcelain=2 --branch`, and the NUL-delimited
// `--numstat -z` of the review and checkpoint diffs) is recorded, and what the original's own code makes
// of it is evaluated from its source text: `parseNumstatEntries`/`parsePorcelainPath` and the two blocks
// of `readStatusDetailsLocal` that fold them into `workingTree.files`, `parseReviewNumstat`, and
// `parseTurnDiffFilesFromNumstat`.
//
// Note what this records: the original's status path reads plain (not `-z`) numstat and keeps what follows
// the first ` => `, so `src/{util.ts => helpers.ts}` becomes `helpers.ts}`, and the porcelain entry of the
// same rename adds the old path as a second file. That is the original's behaviour and the port matches it.
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, copyFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");
const core = read("../../apps/server/src/vcs/GitVcsDriverCore.ts");
const diffs = read("../../apps/server/src/checkpointing/Diffs.ts");

const fromFunction = (text, name) => {
  const start = text.indexOf(`function ${name}(`);
  const end = text.indexOf("\n}\n", start);
  if (start < 0 || end < 0) throw new Error(`Missing original function ${name}`);
  return text.slice(start, end + 3);
};
const between = (text, from, to, offset = 0) => {
  const start = text.indexOf(from, offset);
  const end = text.indexOf(to, start);
  if (start < 0 || end < 0) throw new Error(`Missing original block ${from}`);
  return text.slice(start, end + to.length);
};
const strip = (ts) => stripTypeScriptTypes(ts, { mode: "strip" });

// The porcelain loop, and the fold of numstat entries into `files`, exactly as the original writes them.
const porcelainLoop = between(
  core,
  "for (const line of statusStdout.split(/\\r?\\n/g)) {",
  "\n    }\n",
  core.indexOf("const changedFilesWithoutNumstat = new Set<string>();"),
);
const foldBlock = between(
  core,
  "const numstatEntries = parseNumstatEntries(numstatStdout);",
  "files.sort((a, b) => a.path.localeCompare(b.path));",
);
const original = new Function(
  strip(
    [
      fromFunction(core, "parseBranchAb"),
      fromFunction(core, "parseNumstatEntries"),
      fromFunction(core, "parsePorcelainPath"),
      fromFunction(core, "parseReviewNumstat"),
      fromFunction(diffs, "parseTurnDiffFilesFromNumstat").replace(/^export /, ""),
      `function workingTree(statusStdout, numstatStdout) {
        const includeDivergence = false;
        let refName = null, upstreamRef = null, aheadCount = 0, behindCount = 0, hasWorkingTreeChanges = false;
        const changedFilesWithoutNumstat = new Set();
        ${porcelainLoop}
        ${foldBlock}
        return { files, insertions, deletions, hasWorkingTreeChanges };
      }`,
    ].join("\n"),
  ) +
    "\nreturn { workingTree, parseReviewNumstat, parseTurnDiffFilesFromNumstat, parseNumstatEntries };",
)();

const body = (lines) => `${lines.join("\n")}\n`;
const lines = (name) =>
  Array.from({ length: 12 }, (_, index) => `export const ${name}${index} = ${index};`);
const edited = (name) => body([...lines(name).slice(0, 11), `export const ${name}Edited = true;`]);

// A step is `["write", path, text]`, `["mv", from, to]` (a plain move, not staged), `["cp", from, to]`, or git arguments.
const scenarios = [
  {
    name: "rename within a directory, with an edit",
    base: { "src/util.ts": body(lines("util")) },
    steps: [
      ["git", "mv", "src/util.ts", "src/helpers.ts"],
      ["write", "src/helpers.ts", edited("util")],
    ],
  },
  {
    name: "rename across directories, with an edit",
    base: { "src/util.ts": body(lines("util")) },
    steps: [
      ["git", "mv", "src/util.ts", "lib/util.ts"],
      ["write", "lib/util.ts", edited("util")],
    ],
  },
  {
    name: "rename with a common prefix and suffix",
    base: { "a/b/d.ts": body(lines("d")) },
    steps: [
      ["git", "mv", "a/b/d.ts", "a/c/d.ts"],
      ["write", "a/c/d.ts", edited("d")],
    ],
  },
  {
    name: "rename with no edit",
    base: { "src/util.ts": body(lines("util")) },
    steps: [["git", "mv", "src/util.ts", "src/helpers.ts"]],
  },
  {
    name: "rename of a path containing spaces",
    base: { "my docs/old name.txt": body(lines("doc")) },
    steps: [
      ["git", "mv", "my docs/old name.txt", "my docs/new name.txt"],
      ["write", "my docs/new name.txt", edited("doc")],
    ],
  },
  {
    name: "rename to a path containing ' => ' literally",
    base: { "src/util.ts": body(lines("util")) },
    steps: [
      ["git", "mv", "src/util.ts", "src/a => b.ts"],
      ["write", "src/a => b.ts", edited("util")],
    ],
  },
  {
    name: "edit of a path containing ' => ' literally",
    base: { "src/x => y.ts": body(lines("xy")) },
    steps: [["write", "src/x => y.ts", edited("xy")]],
  },
  {
    name: "edit of a path with non-ASCII characters",
    base: { "src/café.ts": body(lines("cafe")) },
    steps: [["write", "src/café.ts", edited("cafe")]],
  },
  {
    name: "rename of a path with non-ASCII characters",
    base: { "src/naïve.ts": body(lines("naive")) },
    steps: [
      ["git", "mv", "src/naïve.ts", "src/日本語.ts"],
      ["write", "src/日本語.ts", edited("naive")],
    ],
  },
  {
    name: "copy",
    base: { "src/util.ts": body(lines("util")) },
    steps: [
      ["cp", "src/util.ts", "src/copy.ts"],
      ["git", "add", "src/copy.ts"],
    ],
  },
  {
    name: "rename that is also modified in the working tree after staging",
    base: { "src/util.ts": body(lines("util")) },
    steps: [
      ["git", "mv", "src/util.ts", "src/helpers.ts"],
      ["write", "src/helpers.ts", edited("util")],
      ["git", "add", "-A"],
      ["write", "src/helpers.ts", body([...lines("util").slice(0, 10), "export const again = 1;"])],
    ],
  },
];

const environment = {
  PATH: process.env.PATH,
  HOME: tmpdir(),
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_TERMINAL_PROMPT: "0",
  LC_ALL: "C",
};
const git = (cwd, args) =>
  execFileSync("git", args, { cwd, env: environment, encoding: "utf8", maxBuffer: 1 << 24 });
const put = (cwd, path, text) => {
  mkdirSync(dirname(join(cwd, path)), { recursive: true });
  writeFileSync(join(cwd, path), text);
};

const reviewArgs = [
  "diff",
  "--find-renames",
  "--no-color",
  "--no-ext-diff",
  "--no-textconv",
  "--minimal",
  "--src-prefix=a/",
  "--dst-prefix=b/",
];
const rows = [];
for (const scenario of scenarios) {
  const cwd = mkdtempSync(join(tmpdir(), "t3code-rust-port-test-rename-"));
  try {
    git(cwd, ["init", "-q", "-b", "main"]);
    git(cwd, ["config", "user.email", "fixture@example.com"]);
    git(cwd, ["config", "user.name", "Fixture"]);
    for (const [path, text] of Object.entries(scenario.base)) put(cwd, path, text);
    git(cwd, ["add", "-A"]);
    git(cwd, ["commit", "-q", "-m", "base"]);
    for (const [kind, ...rest] of scenario.steps) {
      if (kind === "write") put(cwd, rest[0], rest[1]);
      else if (kind === "cp") {
        mkdirSync(dirname(join(cwd, rest[1])), { recursive: true });
        copyFileSync(join(cwd, rest[0]), join(cwd, rest[1]));
      } else if (kind === "git") {
        // `git mv` needs the destination directory.
        if (rest[0] === "mv") mkdirSync(dirname(join(cwd, rest[2])), { recursive: true });
        git(cwd, rest);
      } else throw new Error(`Unknown step ${kind}`);
    }
    const numstat = git(cwd, ["diff-index", "-M", "--numstat", "HEAD", "--"]);
    const status = git(cwd, ["status", "--porcelain=2", "--branch", "--no-ahead-behind"]);
    const reviewZ = git(cwd, [...reviewArgs, "--numstat", "-z", "HEAD", "--"]);
    // The checkpoint diff is between two commits: commit the index state and diff against the base.
    git(cwd, ["add", "-A"]);
    git(cwd, ["commit", "-q", "-m", "change", "--allow-empty"]);
    const checkpointZ = git(cwd, [
      "diff",
      "--numstat",
      "-z",
      "--no-color",
      "--no-ext-diff",
      "--no-textconv",
      "--src-prefix=a/",
      "--dst-prefix=b/",
      "HEAD~1^{commit}",
      "HEAD^{commit}",
    ]);
    // Which paths git reports changed in the working tree before that commit is what `numstat` and `status` hold.
    const tree = original.workingTree(status, numstat);
    rows.push({
      type: "rename",
      name: scenario.name,
      locale: new Intl.Collator().resolvedOptions().locale,
      base: scenario.base,
      steps: scenario.steps,
      numstat,
      status,
      reviewZ,
      checkpointZ,
      numstatEntries: original.parseNumstatEntries(numstat),
      workingTree: tree,
      review: original.parseReviewNumstat(reviewZ),
      checkpoint: original.parseTurnDiffFilesFromNumstat(checkpointZ),
    });
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
}
const target = new URL("../crates/vcs/tests/fixtures/git-rename.jsonl", import.meta.url);
writeFileSync(target, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
console.log(`${rows.length} scenarios`);
for (const row of rows)
  console.log(`${row.name}: ${JSON.stringify(row.workingTree.files.map((file) => file.path))}`);
