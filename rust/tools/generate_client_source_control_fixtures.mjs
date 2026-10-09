// Runs the original helpers behind the source control settings and the clone step of adding a
// project (`redactedPlaceholder`, `groupGitHubAccounts`, `nextGitHubHosts`, the clone path and URL
// helpers, the source readiness and ordering) and records what they produce. The Rust tests in
// t3-client replay the rows and require identical output. `redactedPlaceholder` lives in a React
// module node cannot load, so its source is cut out of the file and run as it stands.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const load = (path) => import(pathToFileURL(join(root, path)));
const accounts = await load("apps/web/src/components/settings/GitHubAccountSettings.logic.ts");
const projects = await load("packages/client-runtime/src/operations/projects.ts");
const paths = await load("packages/client-runtime/src/state/projects.ts");
const Option = await import(
  pathToFileURL(
    createRequire(join(root, "packages/client-runtime/package.json")).resolve("effect/Option"),
  )
);

const source = readFileSync(
  join(root, "apps/web/src/components/settings/RedactedSensitiveText.tsx"),
  "utf8",
);
const alphabet = source.match(/const REDACTED_TEXT_ALPHABET = [^;]+;/)[0];
const redact = source.match(/export function redactedPlaceholder[\s\S]*?\n}\n/)[0];
const directory = mkdtempSync(join(tmpdir(), "redacted-"));
writeFileSync(join(directory, "redact.ts"), `${alphabet}\n${redact}`);
const { redactedPlaceholder } = await import(pathToFileURL(join(directory, "redact.ts")));

const rows = [];
for (const value of [
  "",
  "octo",
  "tim.r_ferguson@example.com",
  "a-b",
  "UPPER lower 123",
  "ünïcödé",
  "日本語",
  "😀 emoji",
  "x".repeat(40),
  "@",
  "..--__",
  "mixed@Host.example",
]) {
  rows.push({ type: "redacted", value, placeholder: redactedPlaceholder(value) });
}

const entry = (host, account, extra = {}) => ({
  host,
  account,
  active: false,
  authenticated: true,
  ...extra,
});
const accountSets = [
  [],
  [entry("github.com", "octo", { active: true })],
  [
    entry("github.com", "a"),
    entry("github.com", "b", { active: true }),
    entry("ghe.example.com", "corp"),
  ],
  [entry("github.com", "a"), entry("github.com", "b")],
  [
    entry("github.com", "a", { active: true, authenticated: false, error: "token expired" }),
    entry("github.com", "b"),
  ],
  [
    entry("github.com", "env", { environmentVariable: "GH_TOKEN", active: true }),
    entry("github.com", "stored"),
  ],
  [entry("github.com", "x", { authenticated: false, error: "bad" })],
  [entry("ghe.example.com", "corp", { active: true }), entry("github.com", "octo")],
];
for (const set of accountSets)
  rows.push({ type: "groups", accounts: set, groups: accounts.groupGitHubAccounts(set) });

const hostMaps = [
  {},
  { "github.com": { enabled: false } },
  { "github.com": { enabled: true, account: "octo" } },
  { "github.com": { enabled: false, account: "octo" }, "ghe.example.com": { enabled: false } },
];
const changes = [
  { enabled: true },
  { enabled: false },
  { account: "octo" },
  { account: null },
  { enabled: false, account: "b" },
  {},
];
for (const current of hostMaps)
  for (const host of ["github.com", "ghe.example.com"])
    for (const change of changes) {
      rows.push({
        type: "hosts",
        current,
        host,
        change,
        next: accounts.nextGitHubHosts(current, host, change),
      });
    }

const urls = [
  "https://github.com/acme/web.git",
  "https://github.com/acme/web",
  "https://github.com/acme/web/",
  "https://github.com/acme/web.git?ref=x#frag",
  "ssh://git@host:22/owner/repo",
  "ssh://git@host:22/owner/repo.git",
  "git@github.com:acme/web.git",
  "git@host:acme",
  "https://host",
  "https://host/",
  "https://host:8080",
  "https://host:8080/",
  "https://host/acme/123",
  "https://host/123",
  "https://host:123/repo",
  "acme/web",
  "acme/web.git",
  "org/project/repo",
  "web",
  "web.git",
  "",
  "   ",
  "C:\\work\\repo.git",
  "\\\\server\\share\\repo",
  "file:///tmp/seed/repo.git",
  "/srv/git/repo.git",
  "https://dev.azure.com/org/project/_git/repo",
  "  https://host/a/b.git  ",
  ".git",
  "a/.git",
  "https://host/a/b.git.git",
  "ssh://host/",
  "git://host/x/y.git",
  "user@host:path/to/repo",
  "host:repo",
];
for (const input of urls)
  rows.push({ type: "directory", input, name: projects.getCloneDirectoryName(input) });
for (const directoryPath of [
  "/home/me/code",
  "/home/me/code/",
  "~/",
  "~",
  "C:\\Users\\me",
  "C:\\Users\\me\\",
  "relative",
  "",
]) {
  for (const name of ["web", " web ", "", "  ", null])
    rows.push({
      type: "destination",
      directory: directoryPath,
      name,
      path: projects.getCloneDestinationPath(directoryPath, name),
    });
}
for (const input of [
  "acme/web",
  " acme/web ",
  "acme/web.git",
  "a/b",
  "-a/b",
  "acme/web/extra",
  "git@host:acme/web.git",
  "https://github.com/acme/web",
  "acme",
  "o/r.name_x-y",
  `${"a".repeat(39)}/r`,
  `${"a".repeat(40)}/r`,
  "",
]) {
  rows.push({ type: "pasted", input, url: projects.normalizePastedCloneUrl(input) });
}
const info = (provider) => ({
  provider,
  nameWithOwner: "acme/web",
  url: "https://x/acme/web",
  sshUrl: "git@x:acme/web.git",
});
for (const provider of ["github", "forgejo", "gitlab", "bitbucket", "azure-devops", "unknown"]) {
  rows.push({
    type: "default-url",
    repository: info(provider),
    url: projects.getDefaultCloneUrl(info(provider)),
  });
}

// The original decodes Effect `Option`s; its readiness reads them through `Option.getOrNull`.
const some = (value) => ({ _tag: "Some", value });
const none = { _tag: "None" };
const providerItem = (kind, status, auth, label) => ({
  kind,
  label,
  status,
  version: none,
  installHint: `Install ${label}.`,
  detail: none,
  auth,
});
const authItem = (status, detail) => ({
  status,
  account: none,
  host: none,
  detail: detail === undefined ? none : some(detail),
});
const labels = {
  github: "GitHub",
  gitlab: "GitLab",
  bitbucket: "Bitbucket",
  "azure-devops": "Azure DevOps",
  forgejo: "Forgejo",
};
const states = [
  (kind) => providerItem(kind, "available", authItem("authenticated"), labels[kind]),
  (kind) => providerItem(kind, "available", authItem("unauthenticated"), labels[kind]),
  (kind) => providerItem(kind, "available", authItem("unauthenticated", "Run it."), labels[kind]),
  (kind) => providerItem(kind, "available", authItem("unknown", "No idea."), labels[kind]),
  (kind) => providerItem(kind, "missing", authItem("unknown"), labels[kind]),
];
const kinds = ["github", "gitlab", "bitbucket", "azure-devops"];
const scenarios = [null, { versionControlSystems: [], sourceControlProviders: [] }];
for (let shift = 0; shift < states.length; shift += 1) {
  scenarios.push({
    versionControlSystems: [],
    sourceControlProviders: kinds.map((kind, index) =>
      states[(index + shift) % states.length](kind),
    ),
  });
}
scenarios.push({
  versionControlSystems: [],
  sourceControlProviders: [states[0]("github"), states[1]("forgejo")],
});
const pathInputs = [
  "/home/me/code/web",
  "/home/me/code/web/",
  "/home/me/code//",
  "/",
  "//",
  "\\",
  "C:",
  "C:/",
  "C:\\",
  "C:\\work\\repo",
  "C:\\work\\repo\\",
  "C:/work/repo/",
  "\\\\server\\share\\repo",
  "\\\\server\\share",
  "~/code",
  "~",
  "  /padded/path/  ",
  "relative/dir",
  "relative\\dir\\",
  ".",
  "..",
  "./a",
  "../a",
  ".\\a",
  "..\\a",
  "...",
  ".hidden",
  "",
  "   ",
];
for (const input of pathInputs) {
  rows.push({
    type: "path",
    input,
    title: paths.inferProjectTitleFromPath(input),
    normalized: paths.normalizeProjectPathForDispatch(input),
    relative: paths.isExplicitRelativeProjectPath(input.trim()),
  });
  for (const platform of ["linux", "darwin", "win32", "windows"])
    rows.push({
      type: "windows",
      input: input.trim(),
      platform,
      unsupported: paths.isUnsupportedWindowsProjectPath(input.trim(), platform),
    });
}
// The original decodes the wire `Option`s into Effect options; readiness reads them with `Option.getOrNull`.
const toEffect = (scenario) =>
  scenario && {
    ...scenario,
    sourceControlProviders: scenario.sourceControlProviders.map((item) => ({
      ...item,
      auth: {
        ...item.auth,
        detail:
          item.auth.detail._tag === "Some" ? Option.some(item.auth.detail.value) : Option.none(),
      },
    })),
  };
for (const scenario of scenarios) {
  const readiness = projects.buildAddProjectRemoteSourceReadiness(toEffect(scenario));
  const wire = Object.fromEntries(
    Object.entries(readiness).map(([source, value]) => [source, value]),
  );
  rows.push({
    type: "readiness",
    discovery: scenario,
    readiness: wire,
    order: projects.sortAddProjectProviderSources(readiness),
  });
}
for (const source of ["url", "github", "gitlab", "bitbucket", "azure-devops"]) {
  rows.push({
    type: "labels",
    source,
    label: projects.addProjectRemoteSourceLabel(source),
    hint: projects.addProjectRemoteSourcePathHint(source),
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/source-control-settings.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows`);
