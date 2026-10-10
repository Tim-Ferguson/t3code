// Runs the original Forgejo / Gitea source control layer and records what it produces, for
// crates/server/tests/source_control_forgejo.rs to replay:
//  - the pure parts of ForgejoCli.ts (login and remote parsing, login matching, where fj keeps its
//    keys) and the pull request schema of forgejoPullRequests.ts;
//  - ForgejoSourceControlProvider over ForgejoCli over the real VcsProcess policy with a scripted
//    process runner, an in-memory file system holding fj's keys, and a scripted fetch.
// The server a scenario talks to is written `{origin}` and `{host}`: this script answers for
// http://forgejo.test, and the test stands a loopback server up in its place. No network is used
// and no process is spawned. The script sets HOME for itself so no real home is read.
import { writeFileSync, renameSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as FileSystem from "../../apps/server/node_modules/effect/dist/FileSystem.js";
import { FetchHttpClient } from "../../apps/server/node_modules/effect/dist/http/index.js";
import * as ForgejoCli from "../../apps/server/src/sourceControl/ForgejoCli.ts";
import * as ForgejoSourceControlProvider from "../../apps/server/src/sourceControl/ForgejoSourceControlProvider.ts";
import * as P from "../../apps/server/src/sourceControl/forgejoPullRequests.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";
import { HostProcessPlatform } from "../../packages/shared/src/hostProcess.ts";
import * as Contracts from "../../packages/contracts/src/sourceControl.ts";

process.env.HOME = "/home/fixture";
delete process.env.XDG_DATA_HOME;
delete process.env.APPDATA;

const ORIGIN = "http://forgejo.test";
const HOST = "forgejo.test";
const HEAD = "abc123";
const rows = [];
const norm = (value) => JSON.parse(JSON.stringify(value));
const jsonOf = (schema, value) =>
  JSON.parse(JSON.stringify(Schema.encodeSync(Schema.toCodecJson(schema))(value)));
const into = (value) =>
  typeof value === "string"
    ? value.replaceAll("{origin}", ORIGIN).replaceAll("{host}", HOST).replaceAll("{head}", HEAD)
    : Array.isArray(value)
      ? value.map(into)
      : value && typeof value === "object"
        ? Object.fromEntries(Object.entries(value).map(([key, v]) => [into(key), into(v)]))
        : value;
const back = (value) =>
  JSON.parse(
    JSON.stringify(value)
      .replaceAll(ORIGIN, "{origin}")
      .replaceAll(HOST, "{host}")
      .replaceAll(HEAD, "{head}"),
  );

// ---- logins ----------------------------------------------------------------------------------------
const login = (overrides = {}) => ({
  name: "main",
  url: "https://forgejo.test",
  user: "maria",
  default: "true",
  ...overrides,
});
for (const value of [
  [login()],
  [login({ ssh_host: "git.forgejo.test", valid: "true" })],
  [login(), login({ name: "second", url: "https://other.test", default: "false" })],
  [],
  [login({ name: undefined })],
  [login({ ssh_host: null })],
  [login({ valid: null })],
  [login({ default: true })],
  [login({ user: 4 })],
  [login(), { bad: true }],
  {},
  "nope",
  null,
  [login({ extra: 1 })],
]) {
  const raw = typeof value === "string" ? value : JSON.stringify(value);
  rows.push({ type: "logins", raw, result: norm(ForgejoCli.parseForgejoLogins(raw)) });
}

// ---- remotes ---------------------------------------------------------------------------------------
for (const value of [
  "https://forgejo.test/maria/project.git",
  "https://Forgejo.Test/maria/project",
  "https://forgejo.test:3000/maria/project.git",
  "http://forgejo.test/mount/maria/project/",
  "https://forgejo.test//maria/project//",
  "https://forgejo.test",
  "https://forgejo.test/",
  "HTTPS://forgejo.test/a/b.git",
  "https://user:pass@forgejo.test/a/b.git",
  "https://forgejo.test/a/b.git.git",
  "https://forgejo.test/a/b.GIT",
  "ssh://git@forgejo.test/maria/project.git",
  "ssh://git@forgejo.test:2222/maria/project.git",
  "ssh://Forgejo.Test/maria/project",
  "git@forgejo.test:maria/project.git",
  "forgejo.test:maria/project",
  "git@Forgejo.Test:maria/project.git",
  "git@forgejo.test:/maria/project",
  "git@forgejo.test:",
  "forgejo.test",
  "",
  "not a remote",
  "/local/path/repo.git",
  "file:///repo.git",
  "https://",
  "ssh://",
  "https://[::1]:3000/a/b",
  "https://forgejo.test/a%20b/c",
  "https://forgejo.test/a/b?x=1#frag",
]) {
  rows.push({ type: "remote", value, result: norm(ForgejoCli.parseForgejoRemote(value)) });
}

// ---- matching a login to a remote -------------------------------------------------------------------
const logins = {
  one: [login()],
  mount: [login({ url: "https://forgejo.test/git" })],
  two: [login(), login({ name: "work", user: "wm", default: "false" })],
  twoDefaultLast: [
    login({ default: "false" }),
    login({ name: "work", user: "wm", default: "true" }),
  ],
  twoNoDefault: [
    login({ default: "false" }),
    login({ name: "work", user: "wm", default: "false" }),
  ],
  differentServers: [
    login(),
    login({ name: "other", url: "https://other.test", default: "false" }),
  ],
  ssh: [login({ ssh_host: "git.forgejo.test" })],
  sshSame: [login({ ssh_host: "forgejo.test" })],
  sshCase: [login({ ssh_host: "GIT.Forgejo.Test" })],
  withPort: [login({ url: "http://forgejo.test:3000" })],
  badUrl: [login({ url: "nonsense" })],
  duplicateName: [login(), login({ user: "again" })],
  empty: [],
};
const remotes = [
  "https://forgejo.test/maria/project.git",
  "https://forgejo.test/git/maria/project.git",
  "https://forgejo.test/gitx/maria/project.git",
  "https://forgejo.test",
  "https://other.test/a/b",
  "http://forgejo.test:3000/a/b",
  "ssh://git@forgejo.test/maria/project.git",
  "git@git.forgejo.test:maria/project.git",
  "git@forgejo.test:maria/project.git",
  "ssh://git@forgejo.test:2222/a/b",
];
for (const [name, list] of Object.entries(logins)) {
  for (const remoteText of remotes) {
    for (const requestedHost of [undefined, "forgejo.test", "FORGEJO.test", "other.test"]) {
      for (const hostOnly of [false, true]) {
        const remote = ForgejoCli.parseForgejoRemote(remoteText);
        rows.push({
          type: "match",
          logins: list,
          remote: remoteText,
          requestedHost: requestedHost ?? null,
          hostOnly,
          result: norm(ForgejoCli.matchForgejoLogin(list, remote, requestedHost, hostOnly) ?? null),
          name,
        });
      }
    }
  }
}

// ---- where fj keeps its keys ---------------------------------------------------------------------------
async function keyPaths(platform, home, dataHome, appData) {
  const saved = {
    HOME: process.env.HOME,
    XDG_DATA_HOME: process.env.XDG_DATA_HOME,
    APPDATA: process.env.APPDATA,
  };
  process.env.HOME = home;
  if (dataHome === null) delete process.env.XDG_DATA_HOME;
  else process.env.XDG_DATA_HOME = dataHome;
  if (appData === null) delete process.env.APPDATA;
  else process.env.APPDATA = appData;
  const checked = [];
  const runner = ProcessRunner.ProcessRunner.of({ run: () => Effect.die("no process") });
  const processLayer = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  await Effect.runPromise(
    Effect.gen(function* () {
      const cli = yield* ForgejoCli.make;
      yield* Effect.result(cli.listLogins({ cwd: "/repo", command: "fj" }));
    }).pipe(
      Effect.provide(processLayer),
      Effect.provideService(
        FileSystem.FileSystem,
        FileSystem.makeNoop({
          exists: (path) => {
            checked.push(path);
            return Effect.succeed(false);
          },
        }),
      ),
      Effect.provide(FetchHttpClient.layer),
      Effect.provideService(HostProcessPlatform, platform),
    ),
  );
  Object.assign(process.env, saved);
  for (const [key, value] of Object.entries(saved))
    if (value === undefined) delete process.env[key];
  return checked;
}
for (const [platform, home, dataHome, appData] of [
  ["linux", "/home/u", null, null],
  ["linux", "/home/u", "/data/home", null],
  ["linux", "/home/u", "relative/data", null],
  ["linux", "/home/u", "", null],
  ["linux", "/home/u/", "/data//x/../home/", null],
  ["darwin", "/Users/u", null, null],
  ["darwin", "/Users/u", "/ignored", "/ignored"],
  ["win32", "C:\\Users\\u", null, null],
  ["win32", "C:\\Users\\u", null, "D:\\Roaming"],
  ["win32", "C:\\Users\\u", null, ""],
  ["win32", "C:\\Users\\u", null, "D:\\Roaming\\"],
]) {
  rows.push({
    type: "keys",
    platform,
    home,
    dataHome,
    appData,
    result: await keyPaths(platform, home, dataHome, appData),
  });
}

// ---- the pull request as a change request ----------------------------------------------------------------
const repo = (name = "maria/project", owner = "maria") => ({
  full_name: name,
  owner: { login: owner },
});
const pr = (overrides = {}) => ({
  number: 42,
  title: "Add it",
  html_url: "https://forgejo.test/maria/project/pulls/42",
  state: "open",
  merged: false,
  base: { ref: "main", sha: "b", repo: repo() },
  head: { ref: "feature", sha: "h", repo: repo() },
  closed_at: null,
  merged_at: null,
  updated_at: "2026-09-16T00:00:00Z",
  ...overrides,
});
for (const value of [
  pr(),
  pr({ merged: true, state: "closed", merged_at: "2026-09-17T00:00:00Z" }),
  pr({ state: "closed", closed_at: "2026-09-17T00:00:00Z" }),
  pr({ state: "weird" }),
  pr({ draft: true }),
  pr({ draft: false, title: "WIP: not a draft" }),
  pr({ title: "WIP: draft by title" }),
  pr({ title: "[wip] draft by title" }),
  pr({ title: "wip:draft" }),
  pr({ title: "WIP draft" }),
  pr({ draft: null }),
  pr({ head: { ref: "f", sha: "h", repo: null } }),
  pr({ base: { ref: "m", sha: "b", repo: null } }),
  pr({ head: { ref: "f", sha: "h", repo: repo("fork/project", "fork") } }),
  pr({ head: { ref: "f", sha: "h" } }),
  pr({ head: { ref: "f", repo: repo() } }),
  pr({ updated_at: null }),
  pr({ updated_at: undefined }),
  pr({ updated_at: "2026-09-16T10:00:00+02:00" }),
  pr({ updated_at: "garbage" }),
  pr({ updated_at: 5 }),
  pr({ closed_at: undefined, merged_at: undefined }),
  pr({ closed_at: 5 }),
  pr({ number: 1.5 }),
  pr({ number: 0 }),
  pr({ number: "42" }),
  pr({ merged: "yes" }),
  pr({ merged: undefined }),
  pr({ html_url: undefined }),
  pr({ extra: "ignored" }),
  pr({ head: { ref: "f", sha: "h", repo: { full_name: "a/b" } } }),
]) {
  const decoded = Schema.decodeUnknownExit(P.ForgejoPullRequestSchema)(value);
  // A row that reads but cannot be carried on the wire (a pull request numbered zero) is no row.
  let result = { error: true };
  if (Exit.isSuccess(decoded)) {
    try {
      result = { ok: jsonOf(Contracts.ChangeRequest, P.toForgejoChangeRequest(decoded.value)) };
    } catch {
      result = { error: true };
    }
  }
  rows.push({ type: "changeRequest", value: JSON.stringify(value), result });
}

// ---- the provider over the tool over scripted processes, files and fetches -------------------------------
const output = (route) => ({
  stdout: route.stdout ?? "",
  stderr: route.stderr ?? "",
  code: route.code ?? 0,
  timedOut: false,
  stdoutTruncated: false,
  stderrTruncated: false,
  stdoutInvalidUtf8: false,
  stderrInvalidUtf8: false,
});
const KEYS_PATH = "/home/fixture/.local/share/forgejo-cli/keys.json";
const BODY_FILE = "/tmp/body.md";
function harness(scenario) {
  const processes = [];
  const requests = [];
  const runner = ProcessRunner.ProcessRunner.of({
    run: (input) => {
      const joined = input.args.join(" ");
      processes.push({ command: input.command, args: input.args, stdin: input.stdin ?? null });
      const route = (scenario.processes ?? []).find(
        (candidate) =>
          candidate.command === input.command &&
          (candidate.match === undefined || joined.includes(into(candidate.match))),
      );
      const fields = { command: input.command, argumentCount: input.args.length, cwd: input.cwd };
      if (route === undefined)
        return Effect.succeed(output({ code: 97, stderr: `no route: ${input.command} ${joined}` }));
      if (route.spawn)
        return Effect.fail(
          new ProcessRunner.ProcessSpawnError({
            ...fields,
            cause: new Error(`spawn ${input.command} ENOENT`),
          }),
        );
      return Effect.succeed(
        output({ ...route, stdout: into(route.stdout ?? ""), stderr: into(route.stderr ?? "") }),
      );
    },
  });
  const process_ = Layer.effect(VcsProcess.VcsProcess, VcsProcess.make).pipe(
    Layer.provide(Layer.succeed(ProcessRunner.ProcessRunner, runner)),
  );
  const keysText =
    scenario.keys === undefined || scenario.keys === null
      ? null
      : typeof scenario.keys === "string"
        ? scenario.keys
        : JSON.stringify(into(scenario.keys));
  const fileSystem = FileSystem.makeNoop({
    exists: (path) => Effect.succeed(path === KEYS_PATH && keysText !== null),
    readFileString: (path) =>
      path === KEYS_PATH && keysText !== null
        ? Effect.succeed(keysText)
        : path === BODY_FILE
          ? Effect.succeed("Body from file\n")
          : Effect.fail(new Error(`unexpected read ${path}`)),
  });
  const fetch = async (input, init) => {
    const request = new Request(input instanceof Request ? input.url : String(input), {
      ...(init?.method === undefined ? {} : { method: init.method }),
      ...(init?.headers === undefined ? {} : { headers: init.headers }),
      ...(init?.body === undefined ? {} : { body: init.body }),
      redirect: "manual",
    });
    const text = request.method === "GET" || request.method === "HEAD" ? "" : await request.text();
    const url = new URL(request.url);
    requests.push({
      method: request.method,
      path: `${url.pathname}${url.search}`,
      authorization: request.headers.get("authorization"),
      body: text === "" ? null : JSON.parse(text),
    });
    const route = (scenario.http ?? []).find(
      (candidate) =>
        (candidate.method ?? "GET") === request.method &&
        `${url.pathname}${url.search}`.includes(candidate.match),
    );
    if (route === undefined) return new Response("no route", { status: 599 });
    return new Response(route.body === undefined ? "" : into(route.body), {
      status: route.status ?? 200,
      headers: route.headers ?? {},
    });
  };
  return {
    process_,
    fileSystem,
    fetch: Object.assign(fetch, { preconnect: () => undefined }),
    processes,
    requests,
  };
}

const context = (overrides = {}) => ({
  provider: { kind: "forgejo", name: "Forgejo / Gitea", baseUrl: "{origin}" },
  remoteName: "origin",
  remoteUrl: "{origin}/maria/project.git",
  ...overrides,
});
const keys = (extra = {}) => ({
  hosts: { "{host}": { type: "Application", token: "test-token" }, ...extra },
});
const repoJson = (overrides = {}) =>
  JSON.stringify({
    full_name: "maria/project",
    clone_url: "{origin}/maria/project.git",
    ssh_url: "git@ssh.example.invalid:maria/project.git",
    default_branch: "main",
    ...overrides,
  });
const pullJson = (number, overrides = {}) =>
  JSON.stringify(
    pr({
      number,
      html_url: `{origin}/maria/project/pulls/${number}`,
      ...overrides,
    }),
  );
const FJ = [
  { command: "fj", match: "whoami", stdout: "" },
  { command: "fj", match: "version", stdout: "fj 0.6.0\n" },
];
const teaLogins = (list) => ({
  command: "tea",
  match: "login list",
  stdout: JSON.stringify(list),
});
const origin = { command: "git", match: "get-url origin", stdout: "{origin}/maria/project.git\n" };
const scenarios = [
  {
    name: "clone urls through fj",
    keys: keys(),
    processes: [...FJ, origin],
    http: [{ match: "/api/v1/repos/maria/project", body: repoJson() }],
    calls: [
      { op: "cloneUrls", input: { repository: "maria/project" }, context: context() },
      { op: "cloneUrls", input: { repository: "maria/project" }, context: context() },
    ],
  },
  {
    name: "default branch through fj",
    keys: keys(),
    processes: [...FJ, origin],
    http: [{ match: "/api/v1/repos/maria/project", body: repoJson({ default_branch: null }) }],
    calls: [{ op: "defaultBranch", input: {}, context: context() }],
  },
  {
    name: "the repository comes from the remote when none is named",
    keys: keys(),
    processes: [...FJ, origin],
    http: [
      { match: "/api/v1/repos/maria/project/pulls/42", body: pullJson(42) },
      { match: "/pulls?state=open&sort=recentupdate&limit=50&page=1", body: `[${pullJson(42)}]` },
      { match: "/pulls?", body: "[]" },
    ],
    calls: [
      { op: "get", input: { reference: "42" }, context: context() },
      { op: "get", input: { reference: "#42" }, context: context() },
      { op: "get", input: { reference: "{origin}/maria/project/pulls/42" }, context: context() },
      {
        op: "get",
        input: { reference: "{origin}/maria/project/pulls/42/files?x=1" },
        context: context(),
      },
      { op: "list", input: { headSelector: "feature", state: "open" }, context: context() },
    ],
  },
  {
    name: "a pull request reference that names no number",
    keys: keys(),
    processes: [...FJ, origin],
    calls: [
      { op: "get", input: { reference: "feature" }, context: context() },
      { op: "get", input: { reference: "{origin}/maria/project" }, context: context() },
    ],
  },
  {
    name: "list filters by branch, source and state",
    keys: keys(),
    processes: [...FJ, origin],
    http: [
      {
        match: "/pulls?state=all&sort=recentupdate&limit=50&page=1",
        body: `[${[
          pullJson(1, { head: { ref: "feature", sha: "h", repo: repo() } }),
          pullJson(2, { head: { ref: "feature", sha: "h", repo: repo("fork/project", "fork") } }),
          pullJson(3, {
            merged: true,
            state: "closed",
            head: { ref: "feature", sha: "h", repo: repo() },
          }),
          pullJson(4, { state: "closed", head: { ref: "feature", sha: "h", repo: repo() } }),
          pullJson(5, { head: { ref: "other", sha: "h", repo: repo() } }),
          pullJson(6, { head: { ref: "feature", sha: "h", repo: null } }),
        ].join(",")}]`,
      },
      {
        match: "/pulls?state=closed&sort=recentupdate&limit=50&page=1",
        body: `[${[pullJson(3, { merged: true, state: "closed" }), pullJson(4, { state: "closed" })].join(",")}]`,
      },
      {
        match: "/pulls?state=open&sort=recentupdate&limit=50&page=1",
        body: `[${[pullJson(1), pullJson(2, { head: { ref: "feature", sha: "h", repo: repo("fork/project", "fork") } })].join(",")}]`,
      },
      { match: "/pulls?", body: "[]" },
    ],
    calls: [
      { op: "list", input: { headSelector: "feature", state: "all" }, context: context() },
      { op: "list", input: { headSelector: "feature", state: "closed" }, context: context() },
      { op: "list", input: { headSelector: "feature", state: "merged" }, context: context() },
      { op: "list", input: { headSelector: "fork:feature", state: "open" }, context: context() },
      {
        op: "list",
        input: { headSelector: "feature", state: "open", limit: 1 },
        context: context(),
      },
      {
        op: "list",
        input: {
          headSelector: "x",
          state: "open",
          source: { refName: "feature", repository: "fork/project" },
        },
        context: context(),
      },
    ],
  },
  {
    name: "list with nothing to show",
    keys: keys(),
    processes: [...FJ, origin],
    http: [{ match: "/pulls?", body: "[]" }],
    calls: [{ op: "list", input: { headSelector: "feature", state: "open" }, context: context() }],
  },
  {
    name: "responses that do not read",
    keys: keys(),
    processes: [...FJ, origin],
    http: [
      { match: "/pulls/1", body: "nope" },
      { match: "/pulls/2", body: "{}" },
      { match: "/pulls?", body: "{}" },
      { match: "/api/v1/repos/maria/project", body: '{"full_name":"maria/project"}' },
    ],
    calls: [
      { op: "get", input: { reference: "1" }, context: context() },
      { op: "get", input: { reference: "2" }, context: context() },
      { op: "list", input: { headSelector: "f", state: "open" }, context: context() },
      { op: "cloneUrls", input: { repository: "maria/project" }, context: context() },
    ],
  },
  {
    name: "http failures",
    keys: keys(),
    processes: [...FJ, origin],
    http: [
      { match: "/pulls/401", status: 401, body: "bad credentials" },
      { match: "/pulls/403", status: 403, body: "" },
      { match: "/pulls/404", status: 404, body: "gone" },
      { match: "/pulls/429", status: 429, body: "slow down" },
      { match: "/pulls/500", status: 500, body: "boom" },
      { match: "/pulls/502", status: 502, body: "" },
      {
        match: "/pulls/302",
        status: 302,
        body: "",
        headers: { location: "https://elsewhere.test/" },
      },
    ],
    calls: ["401", "403", "404", "429", "500", "502", "302"].map((reference) => ({
      op: "get",
      input: { reference },
      context: context(),
    })),
  },
  {
    name: "no credentials",
    keys: null,
    processes: [{ command: "fj", match: "version", spawn: true }, teaLogins([]), origin],
    calls: [
      { op: "get", input: { reference: "1" }, context: context() },
      { op: "cloneUrls", input: { repository: "maria/project" }, context: context() },
    ],
  },
  {
    name: "unreadable credentials",
    keys: "not json",
    processes: [...FJ, teaLogins([]), origin],
    calls: [{ op: "get", input: { reference: "1" }, context: context() }],
  },
  {
    name: "credentials for another server",
    keys: { hosts: { "other.test": { type: "Application", token: "elsewhere" } } },
    processes: [...FJ, teaLogins([]), origin],
    calls: [{ op: "get", input: { reference: "1" }, context: context() }],
  },
  {
    name: "fj is not installed, tea is",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      { command: "tea", match: "api", stdout: pullJson(7), stderr: "HTTP/1.1 200 OK\n" },
      origin,
    ],
    calls: [{ op: "get", input: { reference: "7" }, context: context() }],
  },
  {
    name: "tea answers for fj when fj holds no login",
    keys: null,
    processes: [
      { command: "fj", match: "version", stdout: "fj 0.6.0\n" },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      { command: "tea", match: "page=1", stdout: `[${pullJson(7)}]`, stderr: "HTTP/1.1 200 OK\n" },
      { command: "tea", match: "/pulls?", stdout: "[]", stderr: "HTTP/1.1 200 OK\n" },
      {
        command: "tea",
        match: "/api/v1/repos/maria/project/pulls/7",
        stdout: pullJson(7),
        stderr: "HTTP/1.1 200 OK\n",
      },
      {
        command: "tea",
        match: "/api/v1/repos/maria/project",
        stdout: repoJson(),
        stderr: "HTTP/1.1 200 OK\n",
      },
      origin,
    ],
    calls: [
      { op: "get", input: { reference: "7" }, context: context() },
      { op: "list", input: { headSelector: "feature", state: "open" }, context: context() },
      { op: "cloneUrls", input: { repository: "maria/project" }, context: context() },
      { op: "defaultBranch", input: {}, context: context() },
    ],
  },
  {
    name: "tea reports failures with a zero exit",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      { command: "tea", match: "/pulls/401", stdout: "", stderr: "HTTP/1.1 401 Unauthorized\n" },
      { command: "tea", match: "/pulls/403", stdout: "", stderr: "HTTP/1.1 403 Forbidden\n" },
      { command: "tea", match: "/pulls/404", stdout: "", stderr: "HTTP/1.1 404 Not Found\n" },
      { command: "tea", match: "/pulls/429", stdout: "", stderr: "HTTP/1.1 429 Too Many\n" },
      { command: "tea", match: "/pulls/500", stdout: "", stderr: "HTTP/1.1 500 Server Error\n" },
      { command: "tea", match: "/pulls/none", stdout: "", stderr: "" },
      {
        command: "tea",
        match: "/pulls/auth",
        code: 1,
        stderr: "Please run tea login add: unauthorized",
      },
      { command: "tea", match: "/pulls/crash", code: 2, stderr: "boom" },
      origin,
    ],
    calls: ["401", "403", "404", "429", "500", "none", "auth", "crash"].map((reference) => ({
      op: "get",
      input: { reference },
      context: context(),
    })),
  },
  {
    name: "several tea logins",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([
        { name: "a", url: "{origin}", user: "maria", default: "false" },
        { name: "b", url: "{origin}", user: "other", default: "true" },
      ]),
      { command: "tea", match: "/pulls/7", stdout: pullJson(7), stderr: "HTTP/1.1 200 OK\n" },
      origin,
    ],
    calls: [{ op: "get", input: { reference: "7" }, context: context() }],
  },
  {
    name: "ambiguous tea logins",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([
        { name: "a", url: "{origin}", user: "maria", default: "false" },
        { name: "b", url: "https://{host}", user: "other", default: "false" },
      ]),
      origin,
    ],
    calls: [{ op: "get", input: { reference: "7" }, context: context() }],
  },
  {
    name: "a bare repository name takes its owner from the account",
    keys: keys(),
    processes: [...FJ, origin],
    http: [
      { match: "/api/v1/user", body: '{"login":"maria"}' },
      { match: "/api/v1/repos/maria/project", body: repoJson() },
    ],
    calls: [
      {
        op: "cloneUrls",
        input: { repository: "project" },
        context: context({ remoteUrl: "{origin}" }),
      },
    ],
  },
  {
    name: "the account cannot be read",
    keys: keys(),
    processes: [...FJ, origin],
    http: [{ match: "/api/v1/user", body: '{"login":"  "}' }],
    calls: [
      {
        op: "cloneUrls",
        input: { repository: "project" },
        context: context({ remoteUrl: "{origin}" }),
      },
    ],
  },
  {
    name: "a mount path is not part of the repository",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}/git", user: "maria", default: "true" }]),
      {
        command: "tea",
        match: "/api/v1/repos/maria/project",
        stdout: repoJson(),
        stderr: "HTTP/1.1 200 OK\n",
      },
      { command: "git", match: "get-url origin", stdout: "{origin}/git/maria/project.git\n" },
    ],
    calls: [
      {
        op: "cloneUrls",
        input: { repository: "git/maria/project" },
        context: context({ remoteUrl: "{origin}/git/maria/project.git" }),
      },
    ],
  },
  {
    name: "create repository for the account and for an organisation",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      {
        command: "tea",
        match: "--method GET {origin}/api/v1/user",
        stdout: '{"login":"maria"}',
        stderr: "HTTP/1.1 200 OK\n",
      },
      {
        command: "tea",
        match: "{origin}/api/v1/user/repos",
        stdout: repoJson(),
        stderr: "HTTP/1.1 201 Created\n",
      },
      {
        command: "tea",
        match: "{origin}/api/v1/orgs/acme/repos",
        stdout: repoJson({ full_name: "acme/tools" }),
        stderr: "HTTP/1.1 201 Created\n",
      },
    ],
    calls: [
      { op: "createRepo", input: { repository: "maria/project", visibility: "private" } },
      { op: "createRepo", input: { repository: "acme/tools", visibility: "public" } },
    ],
  },
  {
    name: "create a pull request",
    keys: keys(),
    processes: [...FJ, origin],
    http: [{ method: "POST", match: "/pulls", body: pullJson(9) }],
    calls: [
      {
        op: "createPr",
        input: { baseRefName: "main", headSelector: "feature", title: "Add it" },
        context: context(),
      },
      {
        op: "createPr",
        input: { baseRefName: "main", headSelector: "fork:feature", title: "From a fork" },
        context: context(),
      },
      {
        op: "createPr",
        input: {
          baseRefName: "main",
          headSelector: "feature",
          title: "Other target",
          target: { refName: "release", repository: "acme/upstream" },
          source: { refName: "feature", repository: "maria/project" },
        },
        context: context(),
      },
    ],
  },
  {
    name: "checkout through tea",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      {
        command: "tea",
        match: "/api/v1/repos/maria/project/pulls/7",
        stdout: pullJson(7, { head: { ref: "feature", sha: "{head}", repo: repo() } }),
        stderr: "HTTP/1.1 200 OK\n",
      },
      { command: "tea", match: "pulls checkout", stdout: "" },
      { command: "git", match: "reset", stdout: "" },
      origin,
    ],
    calls: [
      { op: "checkout", input: { reference: "7", force: false }, context: context() },
      { op: "checkout", input: { reference: "7", force: true }, context: context() },
    ],
  },
  {
    name: "checkout failures",
    keys: null,
    processes: [
      { command: "fj", spawn: true },
      teaLogins([{ name: "main", url: "{origin}", user: "maria", default: "true" }]),
      {
        command: "tea",
        match: "/api/v1/repos/maria/project/pulls/7",
        stdout: pullJson(7),
        stderr: "HTTP/1.1 200 OK\n",
      },
      { command: "tea", match: "pulls checkout", code: 1, stderr: "boom" },
      origin,
    ],
    calls: [{ op: "checkout", input: { reference: "7", force: false }, context: context() }],
  },
];

const scenarioSpecs = [];
for (const scenario of scenarios) {
  const { process_, fileSystem, fetch, processes, requests } = harness(scenario);
  const results = [];
  const callLog = [];
  await Effect.runPromise(
    Effect.gen(function* () {
      const cli = yield* ForgejoCli.make;
      const provider = yield* ForgejoSourceControlProvider.make.pipe(
        Effect.provideService(ForgejoCli.ForgejoCli, cli),
      );
      for (const call of scenario.calls) {
        const processesBefore = processes.length;
        const requestsBefore = requests.length;
        const base = { cwd: "/repo" };
        const input = into(call.input);
        const ctx = call.context === undefined ? {} : { context: into(call.context) };
        const effect =
          call.op === "list"
            ? provider
                .listChangeRequests({ ...base, ...ctx, ...input })
                .pipe(
                  Effect.map((items) => items.map((item) => jsonOf(Contracts.ChangeRequest, item))),
                )
            : call.op === "get"
              ? provider
                  .getChangeRequest({ ...base, ...ctx, ...input })
                  .pipe(Effect.map((item) => jsonOf(Contracts.ChangeRequest, item)))
              : call.op === "cloneUrls"
                ? provider.getRepositoryCloneUrls({ ...base, ...ctx, ...input })
                : call.op === "createRepo"
                  ? provider.createRepository({ ...base, ...input })
                  : call.op === "createPr"
                    ? provider.createChangeRequest({
                        ...base,
                        ...ctx,
                        ...input,
                        bodyFile: BODY_FILE,
                      })
                    : call.op === "checkout"
                      ? provider.checkoutChangeRequest({ ...base, ...ctx, ...input })
                      : provider.getDefaultBranch({ ...base, ...ctx });
        const exit = yield* Effect.exit(effect);
        results.push(
          exit._tag === "Success"
            ? { ok: back(exit.value ?? null) }
            : {
                error: back(
                  jsonOf(Contracts.SourceControlProviderError, exit.cause.reasons[0].error),
                ),
              },
        );
        callLog.push({
          processes: back(processes.slice(processesBefore)),
          requests: back(requests.slice(requestsBefore)),
        });
      }
    }).pipe(
      Effect.provide(process_),
      Effect.provideService(FileSystem.FileSystem, fileSystem),
      Effect.provide(FetchHttpClient.layer),
      Effect.provideService(FetchHttpClient.Fetch, fetch),
      Effect.provideService(HostProcessPlatform, "linux"),
    ),
  );
  rows.push({
    type: "provider",
    name: scenario.name,
    keys: scenario.keys ?? null,
    processes: scenario.processes ?? [],
    http: scenario.http ?? [],
    calls: scenario.calls,
    results,
    log: callLog,
  });
  scenarioSpecs.push(scenario.name);
}

const target = new URL(
  "../crates/server/tests/fixtures/source-control-forgejo.jsonl",
  import.meta.url,
);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(`wrote ${rows.length} rows to ${target.pathname}`);
