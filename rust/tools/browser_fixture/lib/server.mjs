// The fake CLIs on the server's PATH, the settings file, and the server process itself.
import { execFileSync, spawn } from "node:child_process";
import {
  chmodSync,
  closeSync,
  mkdirSync,
  openSync,
  symlinkSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
export const TOOL_DIR = resolve(here, "..");
export const RUST_DIR = resolve(TOOL_DIR, "..", "..");
export const FIXTURES = join(RUST_DIR, "crates", "server", "tests", "fixtures");

/** The loopback address nothing listens on: every network base the server knows is pointed here. */
export const UNROUTABLE = "http://127.0.0.1:1";

export function resolvePython() {
  const found = execFileSync("sh", ["-c", "command -v python3"], { encoding: "utf8" }).trim();
  if (!found) throw new Error("python3 is required");
  return found;
}

function shellQuote(text) {
  return `'${String(text).replace(/'/g, `'\\''`)}'`;
}

/** Writes bin/: the fake CLIs, each a two-line shell wrapper around a script that is part of this tool or the repository's fixtures. */
export function writeBin(paths, python) {
  mkdirSync(paths.bin, { recursive: true });
  const wrapper = (name, script, env = {}) => {
    const exports = Object.entries(env)
      .map(([key, value]) => `${key}=${shellQuote(value)}; export ${key}\n`)
      .join("");
    writeFileSync(
      join(paths.bin, name),
      `#!/bin/sh\n${exports}exec ${shellQuote(python)} ${shellQuote(script)} "$@"\n`,
    );
    chmodSync(join(paths.bin, name), 0o755);
  };
  wrapper("codex", join(TOOL_DIR, "fakes", "fake_codex.py"));
  wrapper("claude", join(TOOL_DIR, "fakes", "fake_claude.py"));
  wrapper("glab", join(TOOL_DIR, "fakes", "fake_glab.py"));
  wrapper("gh", join(TOOL_DIR, "fakes", "fake_gh.py"));
  wrapper("grok", join(FIXTURES, "grok-fake-cli.py"));
  // The server opens editors and the file manager through `open` on macOS (the probe that lists them scans /Applications, which
  // no setting changes); this one refuses, so nothing of the developer's is launched.
  wrapper("open", join(TOOL_DIR, "fakes", "fake_refuse.py"));
  // `#!/usr/bin/env python3` in the repository's fixtures finds this one rather than whatever the machine has.
  try {
    symlinkSync(python, join(paths.bin, "python3"));
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
  }
}

/** The scripted prompts the fake Grok plays, in order, one per `session/prompt` across all Grok threads. */
export function grokScript() {
  const update = (value) => ({ update: value });
  const text = (body) =>
    update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: body } });
  const background = (call, task, command) => [
    update({
      sessionUpdate: "tool_call",
      toolCallId: call,
      title: "run_terminal_command",
      kind: "execute",
      rawInput: { command, description: "Build" },
    }),
    update({
      sessionUpdate: "tool_call_update",
      toolCallId: call,
      status: "completed",
      title: "[bg] " + command,
      rawOutput: {
        type: "BackgroundTaskStarted",
        task_id: task,
        task_type: "bash",
        command,
        status: "running",
      },
    }),
    {
      ext: {
        method: "_x.ai/task_backgrounded",
        params: {
          update: {
            sessionUpdate: "task_backgrounded",
            tool_call_id: call,
            task_id: task,
            command,
          },
        },
      },
    },
  ];
  const prompts = [
    // 1: a finished turn with a tool call.
    [
      text("Listing the project files."),
      update({
        sessionUpdate: "tool_call",
        toolCallId: "ls-1",
        title: "Run ls",
        kind: "execute",
        status: "in_progress",
        rawInput: { command: "ls" },
      }),
      update({
        sessionUpdate: "tool_call_update",
        toolCallId: "ls-1",
        status: "completed",
        rawOutput: { output: "README.md\nsrc\n" },
      }),
      text("The project has a README and a src directory."),
      { end: "end_turn" },
    ],
    // 2: a command moved to the background (shows as background work).
    [
      text("Starting the build in the background."),
      ...background("bg-1", "task-1", "sleep 600"),
      { end: "end_turn" },
    ],
  ];
  // Anything beyond the seeded turns still answers, so a person can keep talking to a Grok thread.
  for (let i = 0; i < 40; i += 1) prompts.push([text("Fixture Grok reply."), { end: "end_turn" }]);
  return { prompts };
}

export function buildSettings(paths, { resetAtSeconds, grokFiles, withGrok }) {
  const env = (pairs) =>
    Object.entries(pairs).map(([name, value]) => ({ name, value: String(value) }));
  const disabled = (driver) => ({ driver, enabled: false });
  return {
    enableProviderUpdateChecks: false,
    textGenerationModelSelection: { instanceId: "codex", model: "gpt-6-luna" },
    providerInstances: {
      codex: {
        driver: "codex",
        enabled: true,
        config: {
          binaryPath: join(paths.bin, "codex"),
          homePath: join(paths.providers, "codex-home"),
        },
        environment: env({
          HOME: paths.home,
          CODEX_HOME: join(paths.providers, "codex-home"),
          FIXTURE_USAGE_RESET_AT: resetAtSeconds,
          FIXTURE_SECOND_MODEL: "1",
        }),
      },
      claudeAgent: {
        driver: "claudeAgent",
        enabled: true,
        config: {
          binaryPath: join(paths.bin, "claude"),
          homePath: join(paths.providers, "claude-home"),
        },
        environment: env({
          HOME: paths.home,
          CLAUDE_CONFIG_DIR: join(paths.providers, "claude-home"),
          CLAUDE_FIXTURE_VERSION: "2.1.200 (Claude Code)",
          T3CODE_CLAUDE_API_BASE: UNROUTABLE,
        }),
      },
      // Off unless asked for: see README, "Grok". The server cannot run a Grok session when started by hand.
      grok: {
        driver: "grok",
        enabled: withGrok,
        config: { enabled: withGrok, binaryPath: join(paths.bin, "grok") },
        environment: env({
          HOME: paths.home,
          GROK_HOME: join(paths.providers, "grok-home"),
          GROK_FIXTURE_AGENT: join(FIXTURES, "grok-turn-agent.py"),
          GROK_FIXTURE_SCRIPT: grokFiles.script,
          GROK_FIXTURE_LOG: grokFiles.log,
        }),
      },
      cursor: disabled("cursor"),
      opencode: disabled("opencode"),
      pi: disabled("pi"),
      antigravity: disabled("antigravity"),
    },
  };
}

/** The server's whole environment. Nothing is inherited: the real HOME, PATH and tokens are not visible to it. */
export function serverEnvironment(paths, { githubApiOrigin }) {
  return {
    HOME: paths.home,
    PATH: `${paths.bin}:/usr/bin:/bin`,
    TMPDIR: paths.tmp,
    LANG: "C.UTF-8",
    XDG_CONFIG_HOME: join(paths.home, ".config"),
    XDG_DATA_HOME: join(paths.home, ".local", "share"),
    XDG_CACHE_HOME: join(paths.home, ".cache"),
    // Git works on local repositories and nothing else: no global config, no prompts, file:// only.
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_TERMINAL_PROMPT: "0",
    GIT_ALLOW_PROTOCOL: "file",
    // Every network base the server knows points at an address nothing listens on.
    T3CODE_BITBUCKET_API_BASE_URL: UNROUTABLE,
    T3CODE_USAGE_RATES_URL: UNROUTABLE + "/usage-rates.json",
    T3CODE_CLAUDE_API_BASE: UNROUTABLE,
    T3CODE_OPENCODE_GO_USAGE_URL: UNROUTABLE + "/usage",
    // Any other HTTP(S) request the server makes through its client is sent to a proxy that is not there.
    HTTP_PROXY: UNROUTABLE,
    HTTPS_PROXY: UNROUTABLE,
    ALL_PROXY: UNROUTABLE,
    http_proxy: UNROUTABLE,
    https_proxy: UNROUTABLE,
    all_proxy: UNROUTABLE,
    NO_PROXY: "127.0.0.1,localhost,::1",
    no_proxy: "127.0.0.1,localhost,::1",
    // Read by the fake CLIs.
    T3_FIXTURE_CALL_LOG: paths.calls,
    // Read only by `serve --fixture`, which the server insists on (the variable alone is an error). Set only with --hook.
    ...(githubApiOrigin ? { T3CODE_FIXTURE_GITHUB_API_ORIGIN: githubApiOrigin } : {}),
    RUST_LOG: "warn,t3_server=info",
  };
}

/**
 * macOS: a sandbox profile, applied by `sandbox-exec`, which then execs the program itself (the process keeps its PID and
 * is the program in `ps`). It refuses every network connection that is not to this machine, and refuses to start
 * anything inside /Applications except Xcode (the `git` on the PATH is Xcode's): no editor can be launched from the
 * server, however the UI asks. The settings and PATH already keep the server away from real CLIs; this is the net under them.
 */
export const SANDBOX_PROFILE = `(version 1)
(allow default)
(deny network-outbound (remote ip))
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (remote unix-socket))
(deny process-exec (subpath "/Applications"))
(allow process-exec (subpath "/Applications/Xcode.app"))
(allow process-exec (subpath "/Library/Developer/CommandLineTools"))
`;

export function sandboxed(paths, enabled) {
  if (!enabled || process.platform !== "darwin" || !existsSync("/usr/bin/sandbox-exec"))
    return (command, args) => [command, args];
  writeFileSync(paths.sandbox, SANDBOX_PROFILE);
  return (command, args) => ["/usr/bin/sandbox-exec", ["-f", paths.sandbox, command, ...args]];
}

export function launchServer({ server, assets, port, paths, env, wrap, hook }) {
  mkdirSync(paths.serverCwd, { recursive: true });
  const out = openSync(paths.log, "a");
  const args = [
    "serve",
    "--state-dir",
    paths.state,
    "--host",
    "127.0.0.1",
    "--port",
    String(port),
    "--settings",
    paths.settings,
    "--assets",
    assets,
    ...(hook ? ["--fixture"] : []),
  ];
  const [command, commandArgs] = wrap(server, args);
  // The server is the child itself (no shell), in its own session so closing a terminal does not end it.
  const child = spawn(command, commandArgs, {
    cwd: paths.serverCwd,
    env,
    detached: true,
    stdio: ["ignore", out, out],
  });
  child.unref();
  closeSync(out);
  return child;
}

export async function waitForRoot(origin, child, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  let exited = null;
  child.once("exit", (code, signal) => (exited = { code, signal }));
  while (Date.now() < deadline) {
    if (exited)
      throw new Error(
        `the server exited at startup (code ${exited.code}, signal ${exited.signal}); see server.log`,
      );
    try {
      const response = await fetch(origin + "/", { signal: AbortSignal.timeout(2000) });
      if (response.status < 500) return;
    } catch {}
    await new Promise((done) => setTimeout(done, 200));
  }
  throw new Error(
    `the server did not answer ${origin}/ within ${timeoutMs / 1000}s; see server.log`,
  );
}

export function mintCredential({ server, paths, env }) {
  // `pair` is not a fixture server: it refuses the GitHub origin variable, which only `serve --fixture` reads.
  const { T3CODE_FIXTURE_GITHUB_API_ORIGIN: _origin, ...pairEnv } = env;
  return execFileSync(server, ["pair", "--state-dir", paths.state], {
    cwd: paths.serverCwd,
    env: pairEnv,
    encoding: "utf8",
  }).trim();
}

export function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

export { existsSync, rmSync };
