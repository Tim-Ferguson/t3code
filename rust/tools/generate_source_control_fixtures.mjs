// Runs the original host detection (packages/shared/src/sourceControl.ts), the identifier
// helpers of SourceControlProvider.ts and the auth-output readers of
// SourceControlProviderDiscovery.ts, and records what they produce. The Rust tests replay the
// rows and require identical results. No network, no process spawned.
import { writeFileSync, renameSync } from "node:fs";
import * as Option from "../../apps/server/node_modules/effect/dist/Option.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Shared from "../../packages/shared/src/sourceControl.ts";
import * as Provider from "../../apps/server/src/sourceControl/SourceControlProvider.ts";
import * as Discovery from "../../apps/server/src/sourceControl/SourceControlProviderDiscovery.ts";
import * as Contracts from "../../packages/contracts/src/sourceControl.ts";
import * as RateLimit from "../../apps/server/src/sourceControl/SourceControlRateLimit.ts";

const rows = [];
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));

// ---- host detection ------------------------------------------------------------------------
const remoteUrls = [
  "https://github.com/owner/repo.git",
  "git@github.com:owner/repo.git",
  "ssh://git@github.com/owner/repo.git",
  "ssh://git@github.com:22/owner/repo.git",
  "https://GitHub.com/Owner/Repo",
  "  https://github.com/owner/repo  ",
  "https://github.example.com/owner/repo",
  "git@github.acme.corp:team/repo.git",
  "https://git.github.acme.test:8443/o/r",
  "https://githubusercontent.com/o/r",
  "https://notgithub.com/o/r",
  "https://github.attacker.test/o/r",
  "https://my-github.com/o/r",
  "https://gitlab.com/group/sub/project.git",
  "git@gitlab.com:group/project.git",
  "https://gitlab.example.org/g/p",
  "https://git.gitlab.internal:8443/g/p",
  "ssh://git@gitlab.internal:2222/g/p.git",
  "https://selfhosted.example/group/project",
  "https://bitbucket.org/ws/repo.git",
  "git@bitbucket.org:ws/repo.git",
  "https://bitbucket.acme.test/scm/proj/repo.git",
  "https://dev.azure.com/org/project/_git/repo",
  "git@ssh.dev.azure.com:v3/org/project/repo",
  "https://org.visualstudio.com/project/_git/repo",
  "org@vs-ssh.visualstudio.com:v3/org/project/repo",
  "https://org.visualstudio.com/DefaultCollection/project/_git/repo",
  "https://codeberg.org/owner/repo.git",
  "git@codeberg.org:owner/repo.git",
  "https://forgejo.example.com/owner/repo",
  "https://git.gitea.example/owner/repo",
  "https://gitea.example:3000/owner/repo",
  "http://gitea.example:3000/sub/path/owner/repo",
  "git@forgejo.internal:owner/repo.git",
  "ssh://git@gitea.home/owner/repo.git",
  "https://forgejo.example.com:8443/base/owner/repo",
  "HTTP://FORGEJO.EXAMPLE.COM/o/r",
  "https://code.example.com/owner/repo",
  "git@code.example.com:owner/repo.git",
  "ssh://git@code.example.com:2200/owner/repo.git",
  "https://code.example.com:8443/owner/repo",
  "git@alias:owner/repo.git",
  "ssh://alias/owner/repo",
  "file:///srv/git/repo.git",
  "/srv/git/repo.git",
  "../relative/repo",
  "",
  "   ",
  "github.com/owner/repo",
  "https://",
  "https://:80/x",
  "not a url",
  "mailto:person@example.com",
  "git://github.com/owner/repo.git",
  "https://user:pass@github.com/owner/repo.git",
  "https://[::1]:3000/o/r",
  "https://192.168.1.10:3000/o/r",
  "http://localhost:3000/o/r",
  "https://github.com./o/r",
  "https://GITHUB.COM:443/o/r",
  "http://github.com:80/o/r",
  "https://github.com:8080/o/r",
  "https://münchen.example/o/r",
  "https://exämple.github.test/o/r",
  "ssh://git@github.com:notaport/o/r",
  "git@host:with:colons/repo.git",
  "user@host.example:repo.git",
  "foo.bar@github.com:o/r.git",
];
for (const remoteUrl of remoteUrls) {
  rows.push({
    type: "detect",
    remoteUrl,
    result: Shared.detectSourceControlProviderFromRemoteUrl(remoteUrl),
    ssh: Shared.isSshRemoteUrl(remoteUrl),
  });
}

// ---- repository selector and canonical key -------------------------------------------------
const identities = [
  { provider: "github", displayName: "owner/repo", owner: "owner", name: "repo" },
  { provider: "gitlab", displayName: "group/sub/project", owner: "group", name: "project" },
  { provider: "gitlab", owner: "group", name: "project" },
  { provider: "gitlab", displayName: "", owner: "group", name: "project" },
  { provider: "gitlab", owner: "group" },
  { provider: "github" },
  { provider: "azure-devops", displayName: "org/project/_git/repo", owner: "org", name: "repo" },
  { provider: "azure-devops", displayName: "org/project/_git/repo" },
  { provider: "azure-devops", displayName: "org/project/_git/repo", name: "" },
  { provider: "azure-devops", displayName: "_git" },
  { provider: "azure-devops" },
  { provider: "bitbucket", displayName: "ws/repo", owner: "ws", name: "repo" },
  { displayName: "x/y" },
  { owner: "o", name: "n" },
  {},
];
for (const identity of identities)
  rows.push({
    type: "selector",
    identity,
    result: Shared.sourceControlRepositorySelector(identity),
  });
rows.push({
  type: "selector",
  identity: null,
  result: Shared.sourceControlRepositorySelector(null),
});
const keys = [
  "github.com/o/r",
  "ssh.dev.azure.com/v3/org/project/repo",
  "vs-ssh.visualstudio.com/v3/org/project/repo",
  "org.visualstudio.com/project/_git/repo",
  "org.visualstudio.com/defaultcollection/project/_git/repo",
  "org.visualstudio.com/other/project/_git/repo",
  "dev.azure.com/org/project/_git/repo",
  "a.b.visualstudio.com/p/_git/r",
  "ssh.dev.azure.com/v3/org/project",
  "ssh.dev.azure.com/v3/org/project/repo/extra",
];
for (const key of keys)
  rows.push({ type: "canonical", key, result: Shared.canonicalRepositoryKey(key) });

// ---- identifiers ---------------------------------------------------------------------------
const values = [
  "owner/repo",
  "https://user:secret@github.com/o/r.git?token=abc#frag",
  "https://github.com/o/r",
  "  spaced   out\tvalue\n ",
  "line1\nline2\u0007bell",
  "x".repeat(300),
  "😀".repeat(200),
  "https://example.com/" + "p".repeat(400),
  "feature/x",
  "",
  "   ",
  "github:foo",
  "ssh://git@host/o/r",
  "http://[::1]:3000/path?x=1",
  "c:\\path\\to",
  "\ufeffbom value\ufeff",
];
for (const value of values)
  rows.push({
    type: "transportSafe",
    value,
    result: Provider.transportSafeSourceControlErrorValue(value),
  });
const heads = [
  "feature",
  "owner:feature",
  " owner : feature ",
  "owner:feature/x",
  "own er:feature",
  "owner/x:feature",
  "owner:",
  ":feature",
  "a:b:c",
  "owner:fea ture",
  "  ",
  "",
  "o:\nb",
  "o:b\r",
  "o:\u2028b",
  "\u00a0o:b\u00a0",
  "o\u00a0:b",
  "o:\u00a0b",
];
const sources = [
  undefined,
  { refName: "explicit" },
  { refName: "explicit", owner: "own", repository: "own/repo" },
];
for (const headSelector of heads) {
  rows.push({
    type: "ownerRef",
    headSelector,
    result: Provider.parseSourceControlOwnerRef(headSelector) ?? null,
  });
  for (const source of sources) {
    rows.push({
      type: "sourceRef",
      headSelector,
      source: source ?? null,
      branch: Provider.sourceBranch({ headSelector, ...(source ? { source } : {}) }),
      ref:
        Provider.sourceControlRefFromInput({ headSelector, ...(source ? { source } : {}) }) ?? null,
    });
  }
}

// ---- discovery output readers --------------------------------------------------------------
const texts = [
  "gh version 2.81.0 (2025-01-01)\nhttps://github.com/cli/cli/releases/tag/v2.81.0\n",
  "\n\n  \nfirst\nsecond",
  "\u001b[1mbold\u001b[0m\nnext",
  "\u001b[31mred\u001b[39m",
  "\u001b]0;title\u0007visible",
  "glab 1.40 (abc)\r\nmore",
  "",
  "  \r\n \r\n",
  "\u009b1;31mC1 style\u009b0m",
  "github.com\n  ✓ Logged in to github.com account octo (keyring)\n  - Active account: true\n  - Token: gho_abcdef\n  - Token scopes: 'repo'\n",
  "Token: secret\n- Token scopes: repo\nreal line",
  "token scopes: x\nTOKEN: y",
  "  - Token: x",
  "gitlab.com\n  ✓ Logged in to gitlab.com as tim (token)\n",
  "✓ gitlab.example.com:8443\nmore",
  "- host.example\n",
  "a b\nhost.example:99",
  "!! 192.168.0.1",
  "[::1]:80",
  "host-with-dash.example",
  "-.bad",
  "user@host",
];
for (const text of texts) {
  rows.push({
    type: "readers",
    text,
    firstNonEmptyLine: Option.getOrNull(Discovery.firstNonEmptyLine(text)),
    firstSafeAuthLine: Discovery.firstSafeAuthLine(text) ?? null,
    parseCliHost: Discovery.parseCliHost(text) ?? null,
  });
}
for (const [stdout, stderr] of [
  ["out", "err"],
  ["", "err"],
  ["out", ""],
  ["  ", "\n"],
  ["", ""],
  ["out\n", "err\n"],
]) {
  rows.push({
    type: "combined",
    stdout,
    stderr,
    exitCode: 0,
    result: Discovery.combinedAuthOutput({ stdout, stderr, exitCode: 0 }),
  });
}
const patterns = [/Logged in to .* as\s+([^\s(]+)/iu, /account:\s*([^\s(]+)/iu];
for (const text of [
  "Logged in to gitlab.com as tim (token)",
  "account: bob (x)",
  "nothing",
  "Logged in to x as   ",
  "account:  \n",
]) {
  rows.push({ type: "matchFirst", text, result: Discovery.matchFirst(text, patterns) ?? null });
}
for (const [status, account, host, detail] of [
  ["authenticated", "tim", "github.com", undefined],
  ["unknown", undefined, undefined, "  detail  "],
  ["unauthenticated", "  ", "", ""],
])
  rows.push({
    type: "providerAuth",
    input: { status, account: account ?? null, host: host ?? null, detail: detail ?? null },
    result: jsonOf(
      Contracts.SourceControlProviderAuth,
      Discovery.providerAuth({ status, account, host, detail }),
    ),
  });

// ---- rate limit headers --------------------------------------------------------------------
const NOW = Date.parse("2026-10-05T12:00:00.000Z");
for (const value of [
  undefined,
  "0",
  "30",
  " 30 ",
  "-5",
  "1.5",
  "abc",
  "",
  "Mon, 05 Oct 2026 12:05:00 GMT",
  "Mon, 05 Oct 2026 11:00:00 GMT",
  "2026-10-05T13:00:00Z",
  "9007199254740991",
  "99999999999999999999",
]) {
  rows.push({
    type: "retryAt",
    value: value ?? null,
    now: NOW,
    result: RateLimit.retryAtFromHeader(value, NOW) ?? null,
  });
}

const target = new URL(
  "../crates/server/tests/fixtures/source-control-core.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
