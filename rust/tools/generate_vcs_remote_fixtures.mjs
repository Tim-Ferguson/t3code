// More pure helpers of the original GitVcsDriverCore.ts and packages/shared/src/git.ts,
// evaluated from their source: fetch failure diagnoses, review numstat parsing, worktree
// listing, remote naming and URL normalization, upstream splitting, ref de-duplication, the
// review diff hash and the upstream refresh back-off.
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Duration from "../../apps/server/node_modules/effect/dist/Duration.js";
import {
  dedupeRemoteBranchesWithLocalMatches,
  deriveLocalBranchNameFromRemoteRef,
  normalizeGitRemoteUrl,
} from "../../packages/shared/src/git.ts";

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
  fromConst(source, "STATUS_UPSTREAM_REFRESH_FAILURE_BASE_COOLDOWN", ";\n"),
  fromConst(source, "STATUS_UPSTREAM_REFRESH_FAILURE_MAX_COOLDOWN", ";\n"),
  ...[
    "fetchFailureDetail",
    "parseReviewNumstat",
    "parseWorktreeBranchPaths",
    "sanitizeRemoteName",
    "parseRemoteFetchUrls",
    "parseUpstreamRefWithRemoteNames",
    "parseUpstreamRefByFirstSeparator",
    "statusUpstreamRefreshFailureCooldown",
  ].map((name) => fromFunction(source, name)),
  ...["parseRemoteNames", "parseRemoteNamesInGitOrder", "parseRemoteRefWithRemoteNames"].map(
    (name) => fromFunction(remoteRefs, name),
  ),
];
const js = stripTypeScriptTypes(pieces.join("\n"), { mode: "strip" });
const original = new Function(
  "Duration",
  `${js}\nreturn { fetchFailureDetail, parseReviewNumstat, parseWorktreeBranchPaths, sanitizeRemoteName, parseRemoteFetchUrls, parseUpstreamRefWithRemoteNames, parseUpstreamRefByFirstSeparator, parseRemoteNames, statusUpstreamRefreshFailureCooldown };`,
)(Duration);

const rows = [];
const secret = "https://user:secret-token@example.com/private?token=secret-token";
for (const stderr of [
  "",
  "fatal: Authentication failed for",
  "git@example.com: Permission denied (publickey).",
  "fatal: could not read Username: terminal prompts disabled",
  "fatal: could not read Password for 'https://x@y'",
  "fatal: Could not resolve host: example.com",
  "Could not resolve hostname example.com",
  "ssh: Could not resolve hostname x",
  "ssh: connect to host example.com port 22: Connection refused",
  "ssh: connect to host example.com port 22: Connection timed out",
  "ssh: connect to host h port 22: Network is unreachable",
  "fatal: unable to access 'https://example.com/repo.git/': Could not resolve host: example.com",
  "fatal: unable to access 'https://e/r.git/': Failed to connect to e port 443",
  "remote: Repository not found.",
  "remote: Repository not found",
  "fatal: repository 'https://x/y.git/' not found",
  "fatal: remote does not appear to be a git repository",
  "error: cannot lock ref 'refs/remotes/origin/main': is at abc but expected def",
  "fatal: cannot lock ref x",
  "fatal: Unable to create '/repo/.git/FETCH_HEAD.lock': File exists.",
  "remote: Help: authentication failed, connection refused, cannot lock ref\nremote: unrelated service error",
  "fatal: unexpected remote failure",
  "  fatal: Authentication failed  \r\nmore",
  "FATAL: AUTHENTICATION FAILED",
  "warning: x\nfatal: Could not resolve host: h\n",
  "fatal: Authentication failedx",
  `fatal: Authentication failed for '${secret}'`,
  "remote: fatal: Authentication failed",
  "hook: Permission denied (publickey)",
])
  rows.push({ type: "fetchDetail", stderr, result: original.fetchFailureDetail(stderr) ?? null });

for (const stdout of [
  "1\t2\ta.txt\0",
  "-\t-\tbin.dat\0",
  "5\t1\t\0before.ts\0after.ts\0",
  "1\t0\ttab\tand\nnewline.txt\0",
  "1\t1\ta\x00" + "2\t2\tb\0",
  "",
  "garbage\0",
  "3\t4\t\0only-previous\0",
  "0\t0\t\0old name\0new name\0-\t-\t\0x\0y\0",
  "12\t34\t [literal].txt\0",
  "1\t1\t\0a\0",
])
  rows.push({ type: "reviewNumstat", stdout, result: original.parseReviewNumstat(stdout) });

for (const stdout of [
  "worktree /repo\0HEAD abc\0branch refs/heads/main\0\0worktree /repo/wt\0HEAD def\0branch refs/heads/feature/x\0\0",
  "worktree /gone\0HEAD abc\0branch refs/heads/stale\0prunable gitdir file points to non-existent location\0\0",
  "worktree /a\0HEAD abc\0detached\0\0worktree /b\0HEAD d\0branch refs/heads/dup\0\0worktree /c\0HEAD e\0branch refs/heads/dup\0\0",
  "worktree /bare\0bare\0\0",
  "",
  "worktree /x\0branch refs/heads/last\0prunable\0",
  "worktree /y\0branch refs/heads/no-terminator",
])
  rows.push({ type: "worktrees", stdout, result: [...original.parseWorktreeBranchPaths(stdout)] });

for (const value of [
  "pingdotgg",
  " My Fork ",
  "a/b\\c",
  "--x--",
  "",
  "   ",
  "é-ü",
  "a.b_c-d",
  "..",
  "-",
  "o'brien",
]) {
  rows.push({ type: "sanitizeRemote", value, result: original.sanitizeRemoteName(value) });
}

for (const url of [
  "https://github.com/pingdotgg/t3code.git",
  "git@github.com:pingdotgg/t3code.git",
  "ssh://git@github.com/pingdotgg/t3code",
  "ssh://github.com/pingdotgg/t3code",
  "ssh://git@github.com:22/pingdotgg/t3code",
  "ssh://git@github.com:22/pingdotgg/t3code.git",
  "HTTPS://GitHub.com/Foo/Bar.GIT/",
  "  https://example.com/a/b/  ",
  "https://example.com/a",
  "https://example.com",
  "git://host/path/to/repo.git",
  "/local/path/repo.git",
  "../relative/repo",
  "C:\\repos\\x",
  "user@host:repo",
  "user@host:a/b/c.git",
  "ssh://git@ssh.dev.azure.com/v3/org/proj/repo",
  "git@ssh.dev.azure.com:v3/org/proj/repo",
  "org@vs-ssh.visualstudio.com:v3/org/proj/repo",
  "https://dev.azure.com/org/proj/_git/repo",
  "https://user:pw@host.example/a/b.git",
  "https://host.example:8443/a/b",
  "ssh://",
  "https://",
  "http://[::1]/a/b",
  "https://example.com/a%20b/c",
  "https://EXAMPLE.com/A//B",
  "git@github.com:a",
  "file:///tmp/remote.git",
  "",
])
  rows.push({ type: "remoteUrl", url, result: normalizeGitRemoteUrl(url) });

for (const upstream of [
  "origin/main",
  "team/fork/feature",
  "my-org/upstream/effect-atom",
  "origin",
  "/x",
  "x/",
  " a/b ",
  "upstream/release/v1",
  "weird//branch",
]) {
  const names = original.parseRemoteNames("origin\nteam\nteam/fork\nmy-org/upstream\n");
  const viaNames = original.parseUpstreamRefWithRemoteNames(upstream, names);
  const viaSeparator = original.parseUpstreamRefByFirstSeparator(upstream);
  rows.push({ type: "upstream", upstream, names, result: viaNames ?? viaSeparator });
}

for (const name of [
  "origin/main",
  "origin/feature/x",
  "/leading",
  "trailing/",
  "nosep",
  "",
  "a/",
  "a/b",
]) {
  rows.push({ type: "sharedLocalName", name, result: deriveLocalBranchNameFromRemoteRef(name) });
}
const ref = (name, isRemote, extra = {}) => ({
  name,
  current: false,
  isRemote,
  isDefault: false,
  worktreePath: null,
  ...extra,
});
for (const refs of [
  [
    ref("main", false),
    ref("origin/main", true, { remoteName: "origin" }),
    ref("origin/feature/x", true, { remoteName: "origin" }),
    ref("feature/x", false),
  ],
  [
    ref("origin/only", true, { remoteName: "origin" }),
    ref("upstream/main", true, { remoteName: "upstream" }),
    ref("main", false),
  ],
  [
    ref("a", false),
    ref("origin/a", true, { remoteName: "origin" }),
    ref("origin/origin/a", true, { remoteName: "origin" }),
  ],
  [
    ref("x/y", false),
    ref("origin/x/y", true, { remoteName: "origin" }),
    ref("origin/y", true, { remoteName: "origin" }),
  ],
  [ref("noremote", true), ref("noremote", false)],
  [],
])
  rows.push({ type: "dedupe", refs, result: dedupeRemoteBranchesWithLocalMatches(refs) });

for (const [diff, files] of [
  ["", []],
  ["diff --git a/x b/x\n+line\n", [{ path: "x", previousPath: null, additions: 1, deletions: 0 }]],
  [
    'quote " backslash \\ tab \t nl \n ctrl \u0001 \u001f del \u007f',
    [{ path: "dir/é ü.txt", previousPath: "old/ü", additions: 12, deletions: 3 }],
  ],
  [
    "line sep \u2028 \u2029 emoji \u{1F600} astral",
    [{ path: "a\tb\nc", previousPath: null, additions: 0, deletions: 0 }],
  ],
  [
    "x",
    [
      { path: "a", previousPath: null, additions: 1, deletions: 2 },
      { path: "b", previousPath: "c", additions: 3, deletions: 4 },
    ],
  ],
])
  rows.push({
    type: "diffHash",
    diff,
    files,
    result: createHash("sha256")
      .update(new TextEncoder().encode(JSON.stringify([diff, files])))
      .digest("hex"),
  });

for (let failures = 0; failures <= 40; failures++) {
  rows.push({
    type: "cooldown",
    failures,
    ms: Duration.toMillis(original.statusUpstreamRefreshFailureCooldown(failures)),
  });
}

const target = new URL("../crates/vcs/tests/fixtures/git-remote.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
