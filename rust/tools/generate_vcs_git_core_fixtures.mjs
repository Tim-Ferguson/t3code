// Pure helpers of the original GitVcsDriverCore.ts, evaluated from its source text (they are
// not exported): stderr classification and the small git output parsers.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";

const source = readFileSync(
  new URL("../../apps/server/src/vcs/GitVcsDriverCore.ts", import.meta.url),
  "utf8",
);
const remoteRefs = readFileSync(
  new URL("../../apps/server/src/git/remoteRefs.ts", import.meta.url),
  "utf8",
);

const fromFunction = (text, name) => {
  const start = text.indexOf(`function ${name}(`);
  const end = text.indexOf("\n}\n", start);
  if (start < 0 || end < 0) throw new Error(`Missing original function ${name}`);
  return text.slice(start, end + 3).replace(/^export /, "");
};
const fromConst = (text, name, terminator) => {
  const start = text.indexOf(`const ${name}`);
  const end = text.indexOf(terminator, start);
  if (start < 0 || end < 0) throw new Error(`Missing original const ${name}`);
  return text.slice(start, end + terminator.length);
};

const pieces = [
  fromConst(source, "GIT_FAILURE_REASON_PATTERNS", "\n];\n"),
  fromConst(source, "GIT_DIAGNOSTIC_LINE_PATTERN", ";\n"),
  fromConst(source, "SSH_TRANSPORT_REFUSAL_PATTERN", ";\n"),
  fromConst(source, "GIT_CHECKOUT_PROGRESS_LINE", ";\n"),
  ...[
    "classifyGitFailure",
    "isNonRepositoryGitStderr",
    "isUnbornHeadStderr",
    "isMissingWorktreeStderr",
    "parseTrackingBranchByUpstreamRef",
    "deriveLocalBranchNameFromRemoteRef",
    "parseGitCheckoutProgressLine",
    "parseBranchAb",
    "parseNumstatEntries",
    "parsePorcelainPath",
    "parseDefaultBranchFromRemoteHeadRef",
    "splitNullSeparatedPaths",
  ].map((name) => fromFunction(source, name)),
  ...["parseRemoteNamesInGitOrder", "parseRemoteNames", "parseRemoteRefWithRemoteNames"].map(
    (name) => fromFunction(remoteRefs, name),
  ),
];
const js = stripTypeScriptTypes(pieces.join("\n"), { mode: "strip" });
const original = new Function(
  `${js}\nreturn { classifyGitFailure, isNonRepositoryGitStderr, isUnbornHeadStderr, isMissingWorktreeStderr, parseTrackingBranchByUpstreamRef, deriveLocalBranchNameFromRemoteRef, parseGitCheckoutProgressLine, parseBranchAb, parseNumstatEntries, parsePorcelainPath, parseDefaultBranchFromRemoteHeadRef, splitNullSeparatedPaths, parseRemoteNamesInGitOrder, parseRemoteNames, parseRemoteRefWithRemoteNames };`,
)();

const rows = [];
const stderrs = [
  "",
  "fatal: not a git repository (or any of the parent directories): .git",
  "error: Not a git repository",
  "fatal: A branch named 'x' already exists.",
  "fatal: a branch named 'feature/a b' already exists",
  "fatal: 'feature' is already used by worktree at '/tmp/wt'",
  "fatal: 'feature' is already checked out at '/tmp/wt'",
  "fatal: '/tmp/wt' already exists",
  "fatal: destination path '/tmp/wt' already exists and is not an empty directory.",
  "fatal: tag already exists",
  "! [rejected]  v1 -> v1 (would clobber existing tag)",
  "error: would clobber existing tag",
  "fatal: Authentication failed for 'https://example.com/repo.git/'",
  "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
  "fatal: could not read Password for 'https://x@github.com'",
  "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.",
  "user@host: Permission denied (publickey,password).",
  "Permission denied, please try again.",
  "Host key verification failed.\nfatal: Could not read from remote repository.",
  "fatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights",
  "fatal: 'origin' does not appear to be a git repository",
  "fatal: repository 'https://example.com/x.git/' not found",
  "remote: Permission denied (publickey)\nremote: fatal: Authentication failed",
  "remote: fatal: could not read Username",
  "hook: authentication failed (pre-push)",
  "pre-push hook says: fatal: not a git repository",
  "remote: error: a branch named x already exists",
  "warning: something\nerror: pathspec 'x' did not match any file(s) known to git",
  "fatal: bad revision 'HEAD'",
  "fatal: ambiguous argument 'HEAD': unknown revision or path not in the working tree.",
  "fatal: 'x' is not a working tree",
  "fatal: cannot remove working tree 'x'",
  "  ! [remote rejected] main -> main (hook declined)",
  "! [rejected] main -> main (fetch first)\r\nerror: failed to push some refs\r\n",
  "error: fatal: Authentication failed\r\nsecond",
  "FATAL: AUTHENTICATION FAILED",
  "Fatal: authentication failed",
  "café fatal: é",
];
for (const stderr of stderrs) {
  rows.push({
    type: "stderr",
    stderr,
    reason: original.classifyGitFailure(stderr),
    nonRepository: original.isNonRepositoryGitStderr(stderr),
    unbornHead: original.isUnbornHeadStderr(stderr),
    missingWorktree: original.isMissingWorktreeStderr(stderr),
  });
}
for (const line of [
  "Updating files:  78% (2104/2700)",
  "Updating files: 100% (5/5), done.",
  "Updating files:   0% (0/9)",
  "Updating files: 250% (1/2)",
  "Updating files:78%(1/2)",
  "nothing here",
  "xx Updating files:  1% (2/3) yy",
]) {
  rows.push({ type: "progress", line, result: original.parseGitCheckoutProgressLine(line) });
}
const tracking = [
  "main\torigin/main\nfeature\torigin/feature\n",
  "a\t\nb\torigin/x\n",
  "\n  \nc\torigin/y",
  "origin/z",
  "d\torigin/main\ne\torigin/main",
  "\tdangling\nf\t origin/main \n",
];
for (const stdout of tracking)
  for (const upstream of ["origin/main", "origin/feature", "origin/x", "origin/y", "nope"]) {
    rows.push({
      type: "tracking",
      stdout,
      upstream,
      result: original.parseTrackingBranchByUpstreamRef(stdout, upstream),
    });
  }
for (const name of [
  "origin/main",
  "origin/feature/x",
  "/leading",
  "trailing/",
  "nosep",
  "",
  "a/ ",
  "a/b ",
  "o/ b",
]) {
  rows.push({ type: "localName", name, result: original.deriveLocalBranchNameFromRemoteRef(name) });
}
for (const value of ["+3 -4", "+0 -0", "+10   -2", "3 4", "+3 -", "", "+1 -2 "])
  rows.push({ type: "ab", value, result: original.parseBranchAb(value) });
for (const stdout of [
  "1\t2\ta.txt\n",
  "-\t-\tbin.png\n",
  "1\t2\told => new\n",
  "1\t2\t{a => b}/c.txt\n",
  "\n\n",
  "x\ty\tz\n",
  "3\t4\ta\tb\tc\n",
  "1\t1\t\n",
  "1\t1\r\n2\t2\tfile\r\n3\t3\t src \n",
  "7\t8\tdir/{x => y}\n",
  "9\t9\t a => \n",
]) {
  rows.push({ type: "numstatEntries", stdout, result: original.parseNumstatEntries(stdout) });
}
for (const line of [
  "? new file.txt",
  "! ignored.log",
  "1 .M N... 100644 100644 100644 aaa bbb src/a.ts",
  "2 R. N... 100644 100644 100644 aaa bbb R100 new.ts\told.ts",
  "u UU N... 100644 100644 100644 100644 a b c conflict.ts",
  "# branch.head main",
  "",
  "1 .M N... 100644 100644 100644 aaa bbb dir with space/a.ts",
  "?   ",
  "? ",
  "1 x\t path\tother",
]) {
  rows.push({ type: "porcelainPath", line, result: original.parsePorcelainPath(line) });
}
for (const [value, remote] of [
  ["refs/remotes/origin/main", "origin"],
  ["refs/remotes/origin/", "origin"],
  ["refs/remotes/upstream/dev", "origin"],
  [" refs/remotes/origin/feature/x \n", "origin"],
  ["origin/main", "origin"],
  ["", "origin"],
]) {
  rows.push({
    type: "defaultBranch",
    value,
    remote,
    result: original.parseDefaultBranchFromRemoteHeadRef(value, remote),
  });
}
for (const [input, truncated] of [
  ["a\0b\0c\0", false],
  ["a\0b\0par", true],
  ["a\0b\0", true],
  ["", false],
  ["\0\0", false],
  ["only", true],
  ["a\0\0b", false],
]) {
  rows.push({
    type: "nullPaths",
    input,
    truncated,
    result: original.splitNullSeparatedPaths(input, truncated),
  });
}
for (const stdout of [
  "origin\nupstream\n",
  " origin \n\n  team/fork\nteam\n",
  "",
  "a\nbb\nccc\ndd\ne\n",
]) {
  rows.push({
    type: "remoteNames",
    stdout,
    inGitOrder: original.parseRemoteNamesInGitOrder(stdout),
    longestFirst: original.parseRemoteNames(stdout),
  });
}
for (const reference of [
  "origin/main",
  " team/fork/feature ",
  "team/feature",
  "origin/",
  "other/x",
  "",
  "origin",
  "upstream/a/b",
]) {
  const names = original.parseRemoteNames(" origin \n\n  team/fork\nteam\nupstream\n");
  rows.push({
    type: "remoteRef",
    reference,
    names,
    result: original.parseRemoteRefWithRemoteNames(reference, names),
  });
}

const target = new URL("../crates/vcs/tests/fixtures/git-core.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
