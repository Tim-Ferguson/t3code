// Starts an isolated server in a rich state. See README.md.
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import { layout, validateScratch } from "./layout.mjs";
import { buildBranchProject, buildDirtyProject, buildPlainProject, makeGit } from "./repos.mjs";
import {
  TOOL_DIR,
  buildSettings,
  grokScript,
  launchServer,
  mintCredential,
  resolvePython,
  sandboxed,
  serverEnvironment,
  waitForRoot,
  writeBin,
} from "./server.mjs";
import { exchangeCredential, Rpc } from "./rpc.mjs";
import { seed } from "./seed.mjs";

export const GITHUB_HOST = "github.t3code-test.invalid";
export const GITLAB_HOST = "gitlab.t3code-test.invalid";

function usage(message) {
  if (message) console.error(`error: ${message}\n`);
  console.error(`usage: start.sh --port PORT --scratch DIR --server T3_SERVER_BINARY --assets BUILT_WEB_DIR [--reuse] [--no-seed] [--no-github-api]

  --port      loopback port for the server (47840-47849 for agent runs)
  --scratch   absolute path of an empty directory named t3code-rust-port-test-...; everything is created under it
  --server    the t3-server binary (this tool builds nothing)
  --assets    the built web UI directory (dx output, .../web/public)
  --reuse     allow a scratch directory that already has content (an earlier run's)
  --no-seed   start the server and the projects but create no conversations
  --no-github-api  do not start the fake GitHub API process
  --no-sandbox     do not wrap the server in the macOS sandbox profile (loopback-only network, no editor launches)
  --hook      pass --fixture to the server and point it at the fake GitHub API (README, "Server hooks")
  --with-grok enable the fake Grok and seed two Grok threads`);
  process.exit(2);
}

function parse(argv) {
  const options = {
    reuse: false,
    seed: true,
    githubApi: true,
    grok: false,
    sandbox: true,
    hook: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => argv[++i] ?? usage(`${arg} needs a value`);
    if (arg === "--port") options.port = Number(value());
    else if (arg === "--scratch") options.scratch = value();
    else if (arg === "--server") options.server = value();
    else if (arg === "--assets") options.assets = value();
    else if (arg === "--reuse") options.reuse = true;
    else if (arg === "--no-seed") options.seed = false;
    else if (arg === "--no-github-api") options.githubApi = false;
    else if (arg === "--with-grok") options.grok = true;
    else if (arg === "--no-sandbox") options.sandbox = false;
    else if (arg === "--hook") options.hook = true;
    else if (arg === "--help" || arg === "-h") usage();
    else usage(`unknown argument ${arg}`);
  }
  if (!Number.isInteger(options.port) || options.port < 1024 || options.port > 65535)
    usage("--port is required (1024-65535)");
  if (!options.scratch) usage("--scratch is required");
  if (!options.server) usage("--server is required");
  if (!options.assets) usage("--assets is required");
  return options;
}

async function portFree(port) {
  return new Promise((done) => {
    const probe = createServer();
    probe.once("error", () => done(false));
    probe.listen(port, "127.0.0.1", () => probe.close(() => done(true)));
  });
}

async function freePort() {
  return new Promise((done, fail) => {
    const probe = createServer();
    probe.once("error", fail);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => done(port));
    });
  });
}

function say(message) {
  console.log(message);
}

export async function main(argv) {
  const options = parse(argv);
  let scratch;
  try {
    scratch = validateScratch(options.scratch, { reuse: options.reuse });
  } catch (error) {
    usage(error.message);
  }
  const server = resolve(options.server);
  const assets = resolve(options.assets);
  if (!existsSync(server)) usage(`--server ${server} does not exist`);
  if (!existsSync(join(assets, "index.html"))) usage(`--assets ${assets} has no index.html`);
  if (!(await portFree(options.port))) usage(`port ${options.port} is already in use`);
  const paths = layout(scratch);
  if (existsSync(paths.pid)) {
    const recorded = Number(readFileSync(paths.pid, "utf8").trim());
    let alive = false;
    try {
      process.kill(recorded, 0);
      alive = true;
    } catch {}
    if (alive)
      usage(
        `pid ${recorded} from ${paths.pid} is still running: stop it first (stop.sh --scratch ${scratch})`,
      );
  }

  for (const directory of [
    paths.state,
    paths.home,
    paths.tmp,
    paths.logs,
    paths.remotes,
    paths.serverCwd,
    join(paths.providers, "codex-home"),
    join(paths.providers, "claude-home"),
    join(paths.providers, "grok-home", ".grok"),
    join(paths.home, ".config"),
    join(paths.home, ".cache"),
  ]) {
    mkdirSync(directory, { recursive: true });
  }
  const python = resolvePython();
  writeBin(paths, python);

  const again = options.reuse && existsSync(paths.meta) && existsSync(paths.projects.dirty);
  if (again) say("reusing the projects and server state of the earlier run ...");
  else say("creating the git projects ...");
  const git = makeGit(paths.home);
  if (!again) buildDirtyProject(git, paths.projects.dirty);
  if (!again)
    buildBranchProject(
      git,
      paths.projects.github,
      join(paths.remotes, "widgets.git"),
      `https://${GITHUB_HOST}/acme/widgets.git`,
      {
        title: "t3code-rust-port-test-github",
        branch: "feature/pr-panel-demo",
        description:
          "A repository whose remote is read as a GitHub host. Pull requests come from the fake GitHub API.",
      },
    );
  if (!again)
    buildBranchProject(
      git,
      paths.projects.gitlab,
      join(paths.remotes, "lab.git"),
      `https://${GITLAB_HOST}/acme/lab.git`,
      {
        title: "t3code-rust-port-test-gitlab",
        branch: "feature/mr-panel-demo",
        description:
          "A repository whose remote is read as a GitLab host. Merge requests come from the fake glab.",
      },
    );
  if (!again)
    buildPlainProject(
      git,
      paths.projects.conversations,
      "t3code-rust-port-test-conversations",
      "The seeded conversations run here. The working tree stays clean.",
    );
  if (!again)
    buildPlainProject(
      git,
      paths.projects.commitcheck,
      "t3code-rust-port-test-commitcheck",
      "check.mjs writes a file here and commits it, to prove that commit messages can be written. Nothing else uses it.",
    );

  const grokFiles = {
    script: join(paths.logs, "grok-script.json"),
    log: join(paths.logs, "grok-agent.jsonl"),
  };
  writeFileSync(grokFiles.script, JSON.stringify(grokScript()));
  // The scripted Codex stamps its turns at a fixed instant (2027-01-15T08:00:01Z) and reports a limit that resets an hour later;
  // a recovery can only be armed for a reset after the run it recovers, so the fixture uses that instant.
  const resetAtSeconds = 1_800_003_600;
  writeFileSync(
    paths.settings,
    JSON.stringify(
      buildSettings(paths, { resetAtSeconds, grokFiles, withGrok: options.grok }),
      null,
      2,
    ) + "\n",
  );

  const wrap = sandboxed(paths, options.sandbox);
  let githubApi = null;
  if (options.githubApi) {
    const apiPort = await freePort();
    const [apiCommand, apiArgs] = wrap(python, [
      join(TOOL_DIR, "fakes", "fake_github_api.py"),
      "--port",
      String(apiPort),
    ]);
    const child = spawn(apiCommand, apiArgs, {
      cwd: paths.serverCwd,
      env: { PATH: "/usr/bin:/bin", T3_FIXTURE_CALL_LOG: paths.calls },
      detached: true,
      stdio: "ignore",
    });
    child.unref();
    writeFileSync(join(scratch, "github-api.pid"), String(child.pid) + "\n");
    githubApi = { origin: `http://127.0.0.1:${apiPort}`, pid: child.pid, port: apiPort };
  }

  // The server refuses the variable without --fixture, so it is set only with the hook.
  const env = serverEnvironment(paths, {
    githubApiOrigin: options.hook ? githubApi?.origin : undefined,
  });
  const origin = `http://127.0.0.1:${options.port}`;
  const child = launchServer({
    server,
    assets,
    port: options.port,
    paths,
    env,
    wrap,
    hook: options.hook,
  });
  writeFileSync(paths.pid, String(child.pid) + "\n");
  writeFileSync(
    paths.meta,
    JSON.stringify(
      {
        server,
        assets,
        port: options.port,
        cwd: paths.serverCwd,
        pid: child.pid,
        githubApi,
        startedAt: new Date().toISOString(),
      },
      null,
      2,
    ) + "\n",
  );
  writeFileSync(
    join(scratch, "meta.txt"),
    [
      `server=${server}`,
      `cwd=${paths.serverCwd}`,
      `port=${options.port}`,
      githubApi ? `githubApiPid=${githubApi.pid}` : "githubApiPid=",
      githubApi ? `githubApiPort=${githubApi.port}` : "githubApiPort=",
    ].join("\n") + "\n",
  );
  say(`server started: pid ${child.pid}, waiting for ${origin}/ ...`);
  try {
    await waitForRoot(origin, child);
  } catch (error) {
    console.error(`error: ${error.message}`);
    console.error(
      `the server (pid ${child.pid}) is left for inspection; stop it with: kill "$(cat ${paths.pid})"`,
    );
    process.exit(1);
  }

  let summary = null;
  if (options.seed && !again) {
    say("seeding conversations over the server's own protocol ...");
    const credential = mintCredential({ server, paths, env });
    const token = await exchangeCredential(origin, credential);
    const rpc = await Rpc.connect(origin, token);
    try {
      summary = await seed(rpc, {
        paths,
        say,
        resetAtSeconds,
        withGrok: options.grok,
        hosts: { github: GITHUB_HOST, gitlab: GITLAB_HOST },
      });
    } finally {
      rpc.close();
    }
    writeFileSync(join(scratch, "seeded.json"), JSON.stringify(summary, null, 2) + "\n");
  }

  const credential = mintCredential({ server, paths, env });
  say("");
  say("READY");
  say(`  url:         ${origin}/`);
  say(
    `  credential:  ${credential}   (one use, valid five minutes; mint another with: ${join(TOOL_DIR, "pair.sh")} --scratch ${scratch} --server ${server})`,
  );
  say(`  pid:         ${child.pid}`);
  if (githubApi) say(`  fake github api: ${githubApi.origin} (pid ${githubApi.pid})`);
  say(`  scratch:     ${scratch}`);
  say(
    `  stop with:   kill "$(cat ${paths.pid})"   (or ${join(TOOL_DIR, "stop.sh")} --scratch ${scratch}, which also stops the fake GitHub API)`,
  );
  say(`  check with:  node ${join(TOOL_DIR, "check.mjs")} ${origin} <credential>`);
  say(`  projects:    ${Object.values(paths.projects).join("\n               ")}`);
}
