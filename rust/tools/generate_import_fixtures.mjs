// Generates the state-directory fixtures for the Rust server's import of the original's state (`state_import*.rs`).
//
// The unchanged original runs recorded provider transcripts (Codex, Claude and Grok, through the replay testkit that the original's
// own tests and this repository's other generators use) into a file database, and the unchanged original *server*
// (`apps/server/src/bin.ts serve`) is then started over it, driven over its own WebSocket (organising threads, projects, scheduled
// tasks, keybindings), stopped, started again, and asked what it serves. The database is vacuumed and committed with that answer
// (`reference.json.gz`); the Rust test imports the state and compares.
//
//   node rust/tools/generate_import_fixtures.mjs [--out DIR] [--scratch DIR] [--only full,inflight,v1,oldest]
//
// Four homes: `full` (every table; settled work), `inflight` (runs held open on an approval or a question when the replay ended),
// `v1` (a V1-only home: `state.sqlite` at the last V1 migration) and `oldest` (V1 data at migration 5, the first with projection
// tables). The V1 homes are made with the original's own migrations plus synthetic rows, and the original server is then run over
// them to record what it makes of them. Nothing here touches ~/.t3.
import { spawn, execFileSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { DatabaseSync } from "node:sqlite";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { randomUUID } from "node:crypto";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as NodeSqliteClient from "../../packages/shared/src/nodeSqliteClient.ts";
import { runMigrations } from "../../apps/server/src/persistence/Migrations.ts";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import { Rpc } from "./browser_fixture/lib/rpc.mjs";
import { ClaudeOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.testkit.ts";
import { CodexOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.testkit.ts";
import { GrokOrchestratorReplayHarness } from "../../apps/server/src/orchestration-v2/Adapters/GrokAdapterV2.testkit.ts";
import * as IdAllocator from "../../apps/server/src/orchestration-v2/IdAllocator.ts";
import { provideDeterministicTestRuntime } from "../../apps/server/src/orchestration-v2/testkit/DeterministicRuntime.ts";
import { ORCHESTRATOR_REPLAY_FIXTURES } from "../../apps/server/src/orchestration-v2/testkit/fixtures/index.ts";
import { materializeFixtureInput } from "../../apps/server/src/orchestration-v2/testkit/fixtures/shared.ts";
import { runOrchestratorV2ProviderReplayScenario } from "../../apps/server/src/orchestration-v2/testkit/ProviderReplayHarness.ts";
import { checkpointWorkspace } from "../../apps/server/src/orchestration-v2/testkit/ReplayFixtureWorkspace.ts";
import {
  materializeReplayTranscriptRuntimeInstructions,
  materializeReplayTranscriptWorkspace,
  readProviderReplayTranscript,
} from "../../apps/server/src/orchestration-v2/testkit/ReplayTranscriptNdjson.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";

const here = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(here, "..", "..");
const SERVER = join(ROOT, "apps", "server", "src", "bin.ts");
const FAKES = join(here, "browser_fixture", "fakes");
const NAME_PREFIX = "t3code-rust-port-test-";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const at = args.indexOf(name);
  return at >= 0 ? args[at + 1] : fallback;
};
const OUT = resolve(
  option("--out", join(ROOT, "rust", "crates", "server", "tests", "fixtures", "state-import")),
);
const scratch = resolve(
  option("--scratch", mkdtempSync(join("/private/tmp/claude-501", `${NAME_PREFIX}import-gen-`))),
);
if (!scratch.split("/").pop().startsWith(NAME_PREFIX))
  throw new Error(`the scratch directory name must start with ${NAME_PREFIX}`);
if (scratch.includes("/.t3")) throw new Error("refusing a scratch directory under ~/.t3");
mkdirSync(scratch, { recursive: true });
const say = (message) => console.log(message);
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

const python = execFileSync("sh", ["-c", "command -v python3"], { encoding: "utf8" }).trim();
const quote = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;

// ---------------------------------------------------------------------------------------------------------------------
// A throwaway environment for the original server.
// ---------------------------------------------------------------------------------------------------------------------

function makeEnvironment(name) {
  const base = join(scratch, name);
  const paths = {
    base,
    home: join(base, "t3home"),
    userdata: join(base, "t3home", "userdata"),
    fakeHome: join(base, "home"),
    bin: join(base, "bin"),
    tmp: join(base, "tmp"),
    projects: join(base, "projects"),
    providers: join(base, "providers"),
    log: join(base, "server.log"),
    calls: join(base, "fake-calls.jsonl"),
  };
  for (const directory of [
    paths.userdata,
    paths.fakeHome,
    paths.bin,
    paths.tmp,
    paths.projects,
    join(paths.providers, "codex-home"),
    join(paths.providers, "claude-home"),
    join(paths.fakeHome, ".config"),
    join(paths.fakeHome, ".cache"),
  ])
    mkdirSync(directory, { recursive: true });
  for (const [tool, script] of [
    ["codex", join(FAKES, "fake_codex.py")],
    ["claude", join(FAKES, "fake_claude.py")],
  ]) {
    writeFileSync(
      join(paths.bin, tool),
      `#!/bin/sh\nexec ${quote(python)} ${quote(script)} "$@"\n`,
    );
    chmodSync(join(paths.bin, tool), 0o755);
  }
  try {
    execFileSync("ln", ["-sf", python, join(paths.bin, "python3")]);
  } catch {}
  return paths;
}

function git(cwd, ...gitArgs) {
  return execFileSync("git", gitArgs, {
    cwd,
    encoding: "utf8",
    env: {
      PATH: "/usr/bin:/bin",
      HOME: scratch,
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_AUTHOR_NAME: "Fixture",
      GIT_AUTHOR_EMAIL: "fixture@example.invalid",
      GIT_COMMITTER_NAME: "Fixture",
      GIT_COMMITTER_EMAIL: "fixture@example.invalid",
    },
  });
}
function makeProject(paths, name) {
  const directory = join(paths.projects, NAME_PREFIX + name);
  mkdirSync(directory, { recursive: true });
  writeFileSync(
    join(directory, "README.md"),
    `Created by the import fixture generator. Safe to delete.\n`,
  );
  git(directory, "init", "-q", "-b", "main");
  git(directory, "add", "-A");
  git(directory, "commit", "-q", "-m", "initial");
  return directory;
}

function providerSettings(paths) {
  const env = (pairs) =>
    Object.entries(pairs).map(([name, value]) => ({ name, value: String(value) }));
  return {
    codex: {
      driver: "codex",
      enabled: true,
      config: {
        binaryPath: join(paths.bin, "codex"),
        homePath: join(paths.providers, "codex-home"),
      },
      environment: env({
        HOME: paths.fakeHome,
        CODEX_HOME: join(paths.providers, "codex-home"),
        FIXTURE_USAGE_RESET_AT: 1_800_003_600,
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
        HOME: paths.fakeHome,
        CLAUDE_CONFIG_DIR: join(paths.providers, "claude-home"),
        CLAUDE_FIXTURE_VERSION: "2.1.200 (Claude Code)",
        T3CODE_CLAUDE_API_BASE: "http://127.0.0.1:1",
      }),
    },
    cursor: { driver: "cursor", enabled: false },
    grok: { driver: "grok", enabled: false },
    opencode: { driver: "opencode", enabled: false },
    pi: { driver: "pi", enabled: false },
    antigravity: { driver: "antigravity", enabled: false },
  };
}

function serverEnvironment(paths) {
  const unroutable = "http://127.0.0.1:1";
  return {
    HOME: paths.fakeHome,
    PATH: `${paths.bin}:${dirname(process.execPath)}:/usr/bin:/bin`,
    TMPDIR: paths.tmp,
    LANG: "C.UTF-8",
    XDG_CONFIG_HOME: join(paths.fakeHome, ".config"),
    XDG_CACHE_HOME: join(paths.fakeHome, ".cache"),
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_TERMINAL_PROMPT: "0",
    T3CODE_USAGE_RATES_URL: unroutable + "/usage-rates.json",
    T3CODE_CLAUDE_API_BASE: unroutable,
    HTTP_PROXY: unroutable,
    HTTPS_PROXY: unroutable,
    NO_PROXY: "127.0.0.1,localhost",
    T3_FIXTURE_CALL_LOG: paths.calls,
  };
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

/** Starts the original server on `paths.home`; resolves once it says it is ready. */
async function startOriginal(paths) {
  const port = await freePort();
  const env = serverEnvironment(paths);
  // The log accumulates across starts; only what this start writes counts.
  const logStart = existsSync(paths.log) ? statSync(paths.log).size : 0;
  const out = (await import("node:fs")).openSync(paths.log, "a");
  const child = spawn(
    process.execPath,
    [
      SERVER,
      "serve",
      "--base-dir",
      paths.home,
      "--port",
      String(port),
      "--host",
      "127.0.0.1",
      "--no-browser",
      paths.projects,
    ],
    { cwd: paths.projects, env, stdio: ["ignore", out, out] },
  );
  let exited = null;
  child.once("exit", (code, signal) => (exited = { code, signal }));
  // Whatever happens next, a server this script started does not outlive it.
  process.once("exit", () => {
    if (exited === null) child.kill("SIGKILL");
  });
  const deadline = Date.now() + 180_000;
  while (
    !readFileSync(paths.log)
      .subarray(logStart)
      .toString("utf8")
      .includes("T3 Code server is ready.")
  ) {
    if (exited)
      throw new Error(
        `the original server exited at startup ${JSON.stringify(exited)}; see ${paths.log}`,
      );
    if (Date.now() > deadline)
      throw new Error(`the original server did not start; see ${paths.log}`);
    await sleep(300);
  }
  const token = execFileSync(
    process.execPath,
    [SERVER, "auth", "session", "issue", "--base-dir", paths.home, "--token-only", "--ttl", "1d"],
    { env, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
  ).trim();
  const rpc = await Rpc.connect(`http://127.0.0.1:${port}`, token);
  const stop = async (signal = "SIGTERM") => {
    rpc.close();
    child.kill(signal);
    for (let waited = 0; exited === null && waited < 60_000; waited += 100) await sleep(100);
    if (exited === null) throw new Error("the original server did not stop");
  };
  return { child, rpc, stop, port };
}

// ---------------------------------------------------------------------------------------------------------------------
// Driving the server.
// ---------------------------------------------------------------------------------------------------------------------

const PNG = Buffer.concat([
  Buffer.from([
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52,
  ]),
  Buffer.from([0, 0, 0, 3, 0, 0, 0, 5]),
  Buffer.alloc(16, 7),
]);

// ---------------------------------------------------------------------------------------------------------------------
// What the original serves for a state: the reference the Rust test compares with.
// ---------------------------------------------------------------------------------------------------------------------

async function firstChunk(rpc, tag, payload) {
  const chunks = [];
  const subscription = rpc.subscribe(tag, payload, (values) => chunks.push(...values));
  for (let waited = 0; chunks.length === 0 && waited < 30_000; waited += 100) await sleep(100);
  subscription.close();
  if (chunks.length === 0) throw new Error(`${tag} sent nothing`);
  return chunks[0];
}

async function capture(rpc) {
  const shell = await firstChunk(rpc, "orchestration.subscribeShell", {});
  const archived = await rpc.call("orchestration.getArchivedShellSnapshot", {});
  const threads = {};
  for (const thread of [...(shell.snapshot?.threads ?? []), ...(archived.threads ?? [])])
    threads[thread.id] = await rpc.call("orchestration.getThreadProjection", {
      threadId: thread.id,
    });
  return {
    shell,
    archived,
    threads,
    settings: await rpc.call("server.getSettings", {}),
    config: await rpc.call("server.getConfig", {}),
    scheduledTasks: await rpc.call("scheduledTasks.list", {}),
  };
}

// ---------------------------------------------------------------------------------------------------------------------
// Writing a fixture.
// ---------------------------------------------------------------------------------------------------------------------

function gzipTo(source, destination) {
  mkdirSync(dirname(destination), { recursive: true });
  writeFileSync(destination, gzipSync(readFileSync(source), { level: 9 }));
}

/** A compact, checkpointed copy of a database: what the original wrote, minus free pages and the write-ahead log. */
function vacuumInto(userdata, name, destination) {
  const work = mkdtempSync(join(scratch, "vacuum-"));
  for (const suffix of ["", "-wal", "-shm"]) {
    const file = join(userdata, name + suffix);
    if (existsSync(file)) copyFileSync(file, join(work, name + suffix));
  }
  const database = new DatabaseSync(join(work, name));
  rmSync(destination, { force: true });
  mkdirSync(dirname(destination), { recursive: true });
  database.exec(`VACUUM INTO '${destination.replace(/'/g, "''")}'`);
  database.close();
  rmSync(work, { recursive: true, force: true });
}

function writeFixture(name, paths, reference, { database = "statev2.sqlite", extras = true } = {}) {
  const directory = join(OUT, name);
  rmSync(directory, { recursive: true, force: true });
  const userdata = join(directory, "userdata");
  mkdirSync(userdata, { recursive: true });
  const plain = join(scratch, `${name}-${database}`);
  vacuumInto(paths.userdata, database, plain);
  gzipTo(plain, join(userdata, `${database}.gz`));
  rmSync(plain, { force: true });
  if (extras) {
    for (const file of ["settings.json", "keybindings.json", "environment-id"])
      if (existsSync(join(paths.userdata, file)))
        copyFileSync(join(paths.userdata, file), join(userdata, file));
    for (const directoryName of ["attachments", "secrets"])
      if (
        existsSync(join(paths.userdata, directoryName)) &&
        readdirSync(join(paths.userdata, directoryName)).length > 0
      )
        cpSync(join(paths.userdata, directoryName), join(userdata, directoryName), {
          recursive: true,
        });
  }
  if (reference)
    writeFileSync(
      join(directory, "reference.json.gz"),
      gzipSync(Buffer.from(JSON.stringify(reference)), { level: 9 }),
    );
  say(`  wrote ${directory}`);
}

// ---------------------------------------------------------------------------------------------------------------------
// The homes.
// ---------------------------------------------------------------------------------------------------------------------

/**
 * Runs recorded provider transcripts through the original's orchestrator and adapters (the replay testkit that the
 * original's own tests and this repository's other generators use), persisting into one file database. `stopAfter`
 * keeps the run in flight: the scenario's declared final steps are replaced by a wait for the named state.
 */
const DRIVERS = {
  codex: {
    harness: CodexOrchestratorReplayHarness,
    prepare: (raw, driver, workspace) => materializeReplayTranscriptWorkspace(raw, workspace),
    needsWorkspace: true,
  },
  claudeAgent: {
    harness: ClaudeOrchestratorReplayHarness,
    prepare: (raw, driver) =>
      materializeReplayTranscriptRuntimeInstructions(raw, {
        driver: driver.driver,
        model: driver.modelSelection.model,
      }),
    needsWorkspace: true,
  },
  grok: {
    harness: GrokOrchestratorReplayHarness,
    prepare: (raw, driver) =>
      materializeReplayTranscriptRuntimeInstructions(raw, {
        driver: driver.driver,
        model: driver.modelSelection.model,
      }),
    needsWorkspace: true,
  },
};

// Each scenario draws its random identifiers from its own seed, so two scenarios in one database never mint the same id (the
// original's projection tables key rows by id alone, so a shared id would make one scenario overwrite the other's rows).
const seedFor = (name, driverKind) =>
  [...`${name}/${driverKind}`].reduce(
    (hash, character) => (hash * 31 + character.charCodeAt(0)) >>> 0,
    7,
  ) || 1;

async function replay(databaseFile, name, driverKind, { holdAfterRequest = null } = {}) {
  const fixture = ORCHESTRATOR_REPLAY_FIXTURES.find((candidate) => candidate.name === name);
  const driver = fixture.providers.find((provider) => provider.driver === driverKind);
  if (driver === undefined) throw new Error(`${name} has no ${driverKind} recording`);
  const kind = DRIVERS[driverKind];
  let input = fixture.buildInput();
  if (holdAfterRequest !== null) {
    // Message, then wait for the run to be running with its request open: the approval is never answered.
    input = {
      ...input,
      steps: [
        input.steps[0],
        holdAfterRequest === "user_input_request"
          ? { type: "answer_next_user_input_request", answers: {} }
          : { type: "approve_next_runtime_request" },
        {
          type: "await_run_status",
          targetRunIndex: 1,
          status: "running",
          waitForTurnItemType: holdAfterRequest,
        },
      ],
    };
  }
  const databaseLayer = SqlitePersistence.layerFromPath(databaseFile).pipe((layer) => layer);
  const program = Effect.gen(function* () {
    const raw = yield* readProviderReplayTranscript(driver.transcriptFile).pipe(
      Effect.provide(NodeServices.layer),
    );
    const workspace = yield* checkpointWorkspace(name, input.workspaceFiles);
    const transcript = yield* kind.harness.decodeTranscript(kind.prepare(raw, driver, workspace));
    const materialized = yield* materializeFixtureInput({
      scenario: name,
      fixtureInput: input,
      driver: driver.driver,
      modelSelection: driver.modelSelection,
    }).pipe(Effect.provide(IdAllocator.layer), provideDeterministicTestRuntime);
    const steps =
      holdAfterRequest === null
        ? materialized.steps
        : materialized.steps.filter(
            (step) =>
              step.type !== "respond_to_next_runtime_request" &&
              step.type !== "await_thread_idle" &&
              step.type !== "await_all",
          );
    const scenario = {
      name: `${name}/${driverKind}`,
      transcript,
      commands: materialized.commands,
      steps,
      projectionThreadIds: materialized.projectionThreadIds,
      runtimePolicyOverride: { ...driver.runtimePolicyOverride, cwd: workspace },
    };
    return yield* runOrchestratorV2ProviderReplayScenario(scenario, kind.harness, {
      databaseLayer: databaseLayer.pipe((layer) => layer),
      ...(driver.runContinuationWorker === true ? { runContinuationWorker: true } : {}),
    }).pipe((effect) =>
      provideDeterministicTestRuntime(effect, { randomSeed: seedFor(name, driverKind) }),
    );
  }).pipe(Effect.provide(NodeServices.layer), Effect.scoped);
  // The replay harness insists a recording was consumed; a run held open on purpose has not consumed its end.
  await Effect.runPromise(program).catch((error) => {
    if (holdAfterRequest === null || !String(error).includes("ReplayIncompleteError")) throw error;
  });
  say(`  replayed ${name} (${driverKind})`);
}

// A scenario name appears once: replays allocate their project, thread and command ids from the name, so a second driver's run
// of the same scenario would be taken for a repeat of the first.
const FULL_SCENARIOS = [
  ["simple", "codex"],
  ["multi_turn", "codex"],
  ["todo_list", "codex"],
  ["proposed_plan", "codex"],
  ["plan_questions", "codex"],
  ["subagent", "codex"],
  ["queued_turn", "codex"],
  ["web_search", "claudeAgent"],
  ["tool_call_read_only", "claudeAgent"],
  ["tool_call_denied_write", "claudeAgent"],
  ["message_steering", "claudeAgent"],
  ["thread_rollback", "claudeAgent"],
  ["claude_result_is_error", "claudeAgent"],
  ["claude_mcp_tool_presentation", "claudeAgent"],
  ["claude_local_bash_task", "claudeAgent"],
  ["claude_background_task_wake", "claudeAgent"],
  // Grok's recordings share fixed ids across scenarios (`...-0000-4000-8000-000000000001`), so one scenario only.
  ["grok_subagent_lineage", "grok"],
];

async function fullHome() {
  say("full home: recorded transcripts through the original's orchestrator ...");
  const paths = makeEnvironment("full");
  const database = join(paths.userdata, "statev2.sqlite");
  for (const [name, driverKind] of FULL_SCENARIOS) await replay(database, name, driverKind);
  const settings = {
    enableProviderUpdateChecks: false,
    textGenerationModelSelection: { instanceId: "codex", model: "fixture-model" },
    defaultModelSelection: { instanceId: "codex", model: "fixture-model" },
    defaultRuntimeMode: "approval-required",
    continueThreadsAfterServerUpdate: true,
    enableAgentBrowserAccess: false,
    defaultAutoPull: true,
    responseStreamingMode: "token",
    sidebarAutoSettleAfterDays: 14,
    snoozeLimitedThreads: true,
    autoResumeLimitedThreads: true,
    sidebarAutoSettleOnMerge: false,
    branchNamePrefix: "fx",
    branchNameInstructions: "Short and lowercase.",
    newWorktreesStartFromOrigin: false,
    addProjectBaseDirectory: paths.projects,
    defaultTheme: "default-dark",
    usageModelAliases: { "fixture-alias": "fixture-model" },
    providerInstances: providerSettings(paths),
  };
  writeFileSync(join(paths.userdata, "settings.json"), JSON.stringify(settings, null, 2) + "\n");
  writeFileSync(
    join(paths.userdata, "keybindings.json"),
    JSON.stringify([{ key: "mod+shift+k", command: "chat.newLocal" }], null, 2) + "\n",
  );
  // Files the server keeps beside the database: an image and a file under attachments/ (their ids are claimed by messages
  // in the legacy homes; here they only need to survive the copy), and a stored secret.
  mkdirSync(join(paths.userdata, "attachments"), { recursive: true });
  writeFileSync(join(paths.userdata, "attachments", "fixture-image.png"), PNG);
  writeFileSync(join(paths.userdata, "attachments", "fixture-report.pdf"), "%PDF-1.4 fixture\n");
  const original = await startOriginal(paths);
  try {
    const { rpc } = original;
    const mutate = async (payload) => {
      const exit = await rpc.exit("projects.mutate", payload);
      if (!exit.ok)
        throw new Error(
          `projects.mutate ${payload.type}: ${JSON.stringify(exit.raw).slice(0, 1500)}`,
        );
      return exit.value;
    };
    const shell = await firstChunk(rpc, "orchestration.subscribeShell", {});
    const threads = shell.snapshot.threads;
    // The replays name a project for each thread but never register it: the server is asked to, as the app would have.
    const projectIds = [...new Set(threads.map((thread) => thread.projectId))].sort();
    for (const [index, projectId] of projectIds.entries())
      await mutate({
        type: "project.create",
        commandId: randomUUID(),
        projectId,
        title: `${NAME_PREFIX}project-${index}`,
        workspaceRoot: makeProject(paths, `project-${index}`),
        createWorkspaceRootIfMissing: false,
        defaultModelSelection: null,
      });
    // Organisation state, as a user would set it: pin, snooze, settle, archive, delete, a pull request link.
    const [first, second, third, fourth, fifth, sixth] = threads;
    const dispatch = (command) =>
      rpc.call("orchestration.dispatchCommand", { commandId: randomUUID(), ...command });
    await dispatch({ type: "thread.pin", threadId: first.id });
    await dispatch({
      type: "thread.snooze",
      threadId: second.id,
      snoozedUntil: new Date(Date.UTC(2099, 0, 1)).toISOString(),
    });
    await dispatch({ type: "thread.settle", threadId: third.id });
    await dispatch({ type: "thread.archive", threadId: fourth.id });
    await dispatch({ type: "thread.delete", threadId: fifth.id });
    await dispatch({
      type: "thread.pull-request.link",
      threadId: sixth.id,
      host: "github.example.invalid",
      repository: "acme/widgets",
      number: 42,
      url: "https://github.example.invalid/acme/widgets/pull/42",
      source: "manual",
    });
    await mutate({
      type: "project.update",
      commandId: randomUUID(),
      projectId: projectIds[0],
      title: NAME_PREFIX + "renamed",
      autoPull: true,
      defaultThreadEnvMode: "worktree",
      defaultModelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-4-6" },
      scripts: [
        {
          id: "test",
          name: "Test",
          command: "echo test",
          icon: "test",
          runOnWorktreeCreate: false,
        },
      ],
    });
    await mutate({
      type: "project.create",
      commandId: randomUUID(),
      projectId: "project-doomed",
      title: NAME_PREFIX + "doomed",
      workspaceRoot: makeProject(paths, "doomed"),
      createWorkspaceRootIfMissing: false,
      defaultModelSelection: null,
    });
    await mutate({ type: "project.delete", commandId: randomUUID(), projectId: "project-doomed" });
    const scheduled = (extraFields) => ({
      commandId: randomUUID(),
      enabled: true,
      projectId: projectIds[0],
      workspaceStrategy: { type: "root" },
      modelSelection: { instanceId: "codex", model: "fixture-model" },
      runtimeMode: "full-access",
      interactionMode: "default",
      ...extraFields,
    });
    await rpc.call(
      "scheduledTasks.upsert",
      scheduled({
        title: "Hourly check",
        prompt: "Check the build.",
        schedule: { type: "interval", everyMs: 3_600_000 },
      }),
    );
    await rpc.call(
      "scheduledTasks.upsert",
      scheduled({
        title: "Weekday morning",
        prompt: "Summarise yesterday.",
        enabled: false,
        schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [1, 2, 3, 4, 5] },
      }),
    );
    await rpc.call(
      "scheduledTasks.upsert",
      scheduled({
        title: "On a webhook",
        prompt: "Handle the event.",
        schedule: { type: "webhook", signature: null, maxDeliveryAgeMinutes: null },
      }),
    );
    await rpc.call("server.upsertKeybinding", { key: "mod+alt+j", command: "chat.new" });
    await sleep(1500);
    await original.stop();
    // What the original serves once it has started again over what it wrote: its load-time settings migrations (the project
    // fold, the providers a user has used) have run, which the import's first start does too.
    const restarted = await startOriginal(paths);
    let reference;
    try {
      await sleep(1500);
      reference = await capture(restarted.rpc);
      await restarted.stop();
    } catch (error) {
      await restarted.stop("SIGKILL").catch(() => {});
      throw error;
    }
    writeFixture("full", paths, reference);
  } catch (error) {
    await original.stop("SIGKILL").catch(() => {});
    throw error;
  }
}

async function inflightHome() {
  say("in-flight home: runs waiting on requests when the process ended ...");
  const paths = makeEnvironment("inflight");
  const database = join(paths.userdata, "statev2.sqlite");
  await replay(database, "simple", "codex");
  await replay(database, "tool_call_denied_write", "claudeAgent", {
    holdAfterRequest: "approval_request",
  });
  await replay(database, "plan_questions", "codex", { holdAfterRequest: "user_input_request" });
  writeFileSync(
    join(paths.userdata, "settings.json"),
    JSON.stringify(
      { enableProviderUpdateChecks: false, providerInstances: providerSettings(paths) },
      null,
      2,
    ) + "\n",
  );
  // What the original serves before it has recovered anything (its startup recovery runs when the server opens the
  // state): the reference is taken after that recovery, and the fixture is the file as the replay left it.
  const copy = join(scratch, "inflight-copy");
  rmSync(copy, { recursive: true, force: true });
  cpSync(paths.base, copy, { recursive: true });
  const served = {
    ...paths,
    base: copy,
    home: join(copy, "t3home"),
    userdata: join(copy, "t3home", "userdata"),
    log: join(copy, "server.log"),
  };
  const original = await startOriginal(served);
  let reference;
  try {
    reference = await capture(original.rpc);
    await original.stop();
  } catch (error) {
    await original.stop("SIGKILL").catch(() => {});
    throw error;
  }
  writeFixture("inflight", paths, reference, { extras: false });
}

/** A V1 home: the original's own migrations up to `through`, then synthetic rows in the shape V1 wrote. */
async function legacyHome(name, through) {
  say(`${name} home: V1 schema at migration ${through}, then the original server over it ...`);
  const paths = makeEnvironment(name);
  const file = join(paths.userdata, "state.sqlite");
  await Effect.runPromise(
    Effect.scoped(
      runMigrations({ toMigrationInclusive: through }).pipe(
        Effect.provide(NodeSqliteClient.layer({ filename: file })),
      ),
    ),
  );
  const database = new DatabaseSync(file);
  const columns = (table) =>
    database
      .prepare(`SELECT name FROM pragma_table_info('${table}')`)
      .all()
      .map((row) => row.name);
  const insert = (table, row) => {
    const present = new Set(columns(table));
    const entries = Object.entries(row).filter(([column]) => present.has(column));
    database
      .prepare(
        `INSERT INTO ${table} (${entries.map(([column]) => column).join(",")}) VALUES (${entries.map(() => "?").join(",")})`,
      )
      .run(...entries.map(([, value]) => value));
  };
  const workspace = makeProject(paths, "v1");
  const t0 = "2025-03-01T10:00:00.000Z";
  insert("projection_projects", {
    project_id: "v1-project",
    title: NAME_PREFIX + "v1",
    workspace_root: workspace,
    default_model: "gpt-5-codex",
    default_model_selection_json: JSON.stringify({
      provider: "codex",
      model: "gpt-5-codex",
      options: { reasoningEffort: "high" },
    }),
    scripts_json: JSON.stringify([
      { id: "build", name: "Build", command: "make", icon: "build", runOnWorktreeCreate: false },
    ]),
    created_at: t0,
    updated_at: t0,
  });
  insert("projection_projects", {
    project_id: "v1-gone",
    title: "Removed project",
    workspace_root: join(paths.projects, "gone"),
    default_model: null,
    default_model_selection_json: null,
    scripts_json: "[]",
    created_at: t0,
    updated_at: "2025-03-02T10:00:00.000Z",
    deleted_at: "2025-03-02T10:00:00.000Z",
  });
  const thread = (id, extra) =>
    insert("projection_threads", {
      thread_id: id,
      project_id: "v1-project",
      title: `V1 thread ${id}`,
      model: "gpt-5-codex",
      model_selection_json: JSON.stringify({ provider: "codex", model: "gpt-5-codex" }),
      runtime_mode: "approval-required",
      interaction_mode: "default",
      branch: null,
      worktree_path: null,
      latest_turn_id: null,
      created_at: t0,
      updated_at: "2025-03-01T11:00:00.000Z",
      ...extra,
    });
  const message = (id, threadId, role, text, minute, extra = {}) =>
    insert("projection_thread_messages", {
      message_id: id,
      thread_id: threadId,
      turn_id: null,
      role,
      text,
      is_streaming: 0,
      created_at: `2025-03-01T10:${String(minute).padStart(2, "0")}:00.000Z`,
      updated_at: `2025-03-01T10:${String(minute).padStart(2, "0")}:30.000Z`,
      ...extra,
    });
  thread("v1-thread-a", { title: "Plain conversation" });
  message("v1-a-1", "v1-thread-a", "user", "What does this repository do?", 1);
  message("v1-a-2", "v1-thread-a", "assistant", "It holds a fixture.", 2);
  message("v1-a-3", "v1-thread-a", "user", "Thanks. Now with an image.", 3, {
    attachments_json: JSON.stringify([
      {
        type: "image",
        id: "v1-image-id",
        name: "shot.png",
        mimeType: "image/png",
        sizeBytes: PNG.length,
      },
    ]),
  });
  message("v1-a-4", "v1-thread-a", "assistant", "partial answer", 4, { is_streaming: 1 });
  thread("v1-thread-b", {
    title: "Archived, pinned and linked",
    archived_at: "2025-03-03T10:00:00.000Z",
    pinned_at: "2025-03-02T10:00:00.000Z",
    pin_order_key: "a0",
    settled_override: "settled",
    settled_at: "2025-03-03T09:00:00.000Z",
    snoozed_until: "2099-01-01T00:00:00.000Z",
    snoozed_at: "2025-03-03T08:00:00.000Z",
    branch: "feature/v1",
    worktree_path: join(paths.base, "worktrees", "v1-thread-b"),
    model_selection_json: JSON.stringify({
      provider: "claudeAgent",
      model: "claude-sonnet-4-5",
      options: { effort: "max", thinking: true },
    }),
    linked_pull_request_json: JSON.stringify({
      projectId: "v1-project",
      repository: "acme/widgets",
      number: 7,
      url: "https://github.com/acme/widgets/pull/7",
    }),
    active_order_key: "b1",
  });
  message("v1-b-1", "v1-thread-b", "user", "Fix the build.", 5);
  message("v1-b-2", "v1-thread-b", "assistant", "Fixed.", 6);
  thread("v1-thread-c", { title: "Deleted", deleted_at: "2025-03-04T10:00:00.000Z" });
  message("v1-c-1", "v1-thread-c", "user", "Never mind.", 7);
  thread("v1-thread-d", {
    title: "No messages",
    runtime_mode: "full-access",
    interaction_mode: "plan",
  });
  database.close();
  // Auto-settling off: the threads are old, and the sweep the original runs at start would settle (and touch) them.
  writeFileSync(
    join(paths.userdata, "settings.json"),
    JSON.stringify(
      { enableProviderUpdateChecks: false, sidebarAutoSettleAfterDays: null },
      null,
      2,
    ) + "\n",
  );
  const original = await startOriginal(paths);
  try {
    // Shells are imported at startup; transcripts are hydrated when a thread is read (and in the background).
    const reference = await capture(original.rpc);
    for (let attempt = 0; attempt < 80; attempt += 1) {
      const counts = new DatabaseSync(join(paths.userdata, "statev2.sqlite"), { readOnly: true });
      const pending = counts
        .prepare(
          "SELECT COUNT(*) AS n FROM orchestration_v2_legacy_imports WHERE transcript_imported_at IS NULL",
        )
        .get().n;
      counts.close();
      if (pending === 0) break;
      await sleep(250);
    }
    const settled = await capture(original.rpc);
    await original.stop();
    // The fixture is the V1 file as it was before the original opened it.
    const v1 = join(scratch, `${name}-state.sqlite`);
    rmSync(v1, { force: true });
    const directory = join(OUT, name);
    rmSync(directory, { recursive: true, force: true });
    mkdirSync(join(directory, "userdata"), { recursive: true });
    gzipTo(file, join(directory, "userdata", "state.sqlite.gz"));
    copyFileSync(
      join(paths.userdata, "settings.json"),
      join(directory, "userdata", "settings.json"),
    );
    writeFileSync(
      join(directory, "reference.json.gz"),
      gzipSync(Buffer.from(JSON.stringify(settled)), { level: 9 }),
    );
    say(
      `  wrote ${directory} (${Object.keys(reference.threads).length} threads served at first read)`,
    );
  } catch (error) {
    await original.stop("SIGKILL").catch(() => {});
    throw error;
  }
}

const homes = option("--only", "full,inflight,v1,oldest").split(",");
mkdirSync(OUT, { recursive: true });
if (homes.includes("full")) await fullHome();
if (homes.includes("inflight")) await inflightHome();
if (homes.includes("v1")) await legacyHome("v1", 54);
if (homes.includes("oldest")) await legacyHome("oldest", 5);
const size = (directory) =>
  readdirSync(directory, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .reduce((total, entry) => total + statSync(join(entry.parentPath, entry.name)).size, 0);
say(`fixtures under ${OUT}: ${size(OUT)} bytes. Scratch directory (not removed): ${scratch}`);
