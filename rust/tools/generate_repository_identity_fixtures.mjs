// Development-only witnesses of `RepositoryIdentityResolver` (what a workspace's git remotes say
// about its repository). The unchanged resolver runs over a stand-in process runner that answers
// `git rev-parse --show-toplevel` and `git remote -v` with the recorded text, so the Rust test feeds
// the same text to its resolver and compares the identity, field for field.
import { writeFileSync } from "node:fs";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Layer from "../../packages/contracts/node_modules/effect/dist/Layer.js";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";
import * as Resolver from "../../apps/server/src/project/RepositoryIdentityResolver.ts";

const fetch = (name, url) => `${name}\t${url} (fetch)\n${name}\t${url} (push)\n`;
const cases = [
  { name: "https github", remotes: fetch("origin", "https://github.com/T3Tools/t3code.git") },
  { name: "scp-style github", remotes: fetch("origin", "git@github.com:T3Tools/t3code.git") },
  {
    name: "a fork: upstream is the repository, origin the fork",
    remotes:
      fetch("origin", "git@github.com:someone/t3code.git") +
      fetch("upstream", "https://github.com/T3Tools/t3code.git"),
  },
  {
    name: "upstream the same repository as origin",
    remotes:
      fetch("origin", "git@github.com:T3Tools/t3code.git") +
      fetch("upstream", "https://github.com/T3Tools/t3code"),
  },
  {
    name: "neither upstream nor origin: the first by name",
    remotes:
      fetch("zeta", "https://github.com/z/z.git") + fetch("alpha", "https://github.com/a/a.git"),
  },
  {
    name: "names that differ in case",
    remotes:
      fetch("b", "https://github.com/lower-b/x.git") +
      fetch("A", "https://github.com/upper-a/x.git") +
      fetch("B", "https://github.com/upper-b/x.git") +
      fetch("a", "https://github.com/lower-a/x.git"),
  },
  {
    name: "origin beside others",
    remotes:
      fetch("mirror", "https://github.com/m/m.git") + fetch("origin", "https://gitlab.com/o/o.git"),
  },
  {
    name: "gitlab nested group",
    remotes: fetch("origin", "https://gitlab.com/group/sub/deep/repo.git"),
  },
  { name: "self-hosted gitlab", remotes: fetch("origin", "git@gitlab.example.com:team/app.git") },
  {
    name: "azure devops https",
    remotes: fetch("origin", "https://dev.azure.com/org/project/_git/repo"),
  },
  {
    name: "azure devops scp",
    remotes: fetch("origin", "git@ssh.dev.azure.com:v3/org/project/repo"),
  },
  { name: "bitbucket", remotes: fetch("origin", "https://bitbucket.org/team/repo.git") },
  { name: "codeberg", remotes: fetch("origin", "https://codeberg.org/owner/repo.git") },
  {
    name: "unknown host over ssh with a port",
    remotes: fetch("origin", "ssh://git@git.example.com:2222/owner/repo.git"),
  },
  { name: "upper case everywhere", remotes: fetch("origin", "HTTPS://GitHub.com/Foo/Bar.GIT") },
  { name: "trailing slashes", remotes: fetch("origin", "https://github.com/foo/bar.git/") },
  { name: "git protocol", remotes: fetch("origin", "git://github.com/foo/bar.git") },
  { name: "a path with a single segment", remotes: fetch("origin", "https://example.com/repo") },
  { name: "a local path", remotes: fetch("origin", "/srv/git/repo.git") },
  { name: "a relative path", remotes: fetch("origin", "../sibling/repo.git") },
  { name: "windows path", remotes: fetch("origin", "C:\\\\Users\\\\me\\\\repo") },
  { name: "no remotes", remotes: "" },
  { name: "only push lines", remotes: "origin\thttps://github.com/foo/bar.git (push)\n" },
  { name: "space separated fields", remotes: "origin https://github.com/foo/bar.git (fetch)\n" },
  {
    name: "crlf line ends",
    remotes:
      "origin\thttps://github.com/foo/bar.git (fetch)\r\norigin\thttps://github.com/foo/bar.git (push)\r\n",
  },
  {
    name: "blank and unrelated lines",
    remotes: "\nwarning: something\n  origin\thttps://github.com/foo/bar.git (fetch)  \n\n",
  },
  {
    name: "a fetch url that differs from its push url",
    remotes:
      "origin\thttps://github.com/foo/bar.git (fetch)\norigin\tgit@example.com:other/push.git (push)\n",
  },
  {
    name: "the same remote twice",
    remotes:
      fetch("origin", "https://github.com/first/one.git") +
      fetch("origin", "https://github.com/second/two.git"),
  },
  { name: "unicode owner", remotes: fetch("origin", "https://github.com/Müller/réPo.git") },
  { name: "an empty path", remotes: fetch("origin", "https://github.com/") },
  {
    name: "a root with spaces",
    cwd: "/work/my project",
    top: "/work/my project\n",
    remotes: fetch("origin", "https://github.com/foo/bar.git"),
  },
  {
    name: "toplevel with padding",
    top: "  /repo/root  \n\n",
    remotes: fetch("origin", "https://github.com/foo/bar.git"),
  },
  { name: "toplevel empty", top: "\n", remotes: fetch("origin", "https://github.com/foo/bar.git") },
  {
    name: "toplevel fails",
    topCode: 128,
    remotes: fetch("origin", "https://github.com/foo/bar.git"),
  },
  {
    name: "remote fails",
    remoteCode: 1,
    remotes: fetch("origin", "https://github.com/foo/bar.git"),
  },
];

const rows = [];
for (const entry of cases) {
  const cwd = entry.cwd ?? "/work/repo/sub";
  const top = entry.top ?? "/work/repo\n";
  const runner = Layer.succeed(ProcessRunner.ProcessRunner, {
    run: (input) =>
      Effect.sync(() => {
        const isTop = input.args.includes("rev-parse");
        return {
          stdout: isTop ? top : entry.remotes,
          stderr: "",
          code: isTop ? (entry.topCode ?? 0) : (entry.remoteCode ?? 0),
          timedOut: false,
          stdoutTruncated: false,
          stderrTruncated: false,
        };
      }),
  });
  const identity = await Effect.runPromise(
    Effect.gen(function* () {
      const resolver = yield* Resolver.make({ cacheCapacity: 4 });
      return yield* resolver.resolve(cwd);
    }).pipe(Effect.provide(runner), Effect.scoped),
  );
  rows.push({
    name: entry.name,
    cwd,
    topLevel: { stdout: top, code: entry.topCode ?? 0 },
    remotes: { stdout: entry.remotes, code: entry.remoteCode ?? 0 },
    identity,
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/repository-identity.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({ cases: rows.length, resolved: rows.filter((row) => row.identity).length }),
);
