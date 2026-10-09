// Runs the original ScheduledTaskService (apps/server/src/scheduledTasks/ScheduledTaskService.ts) over an
// in-memory database with a test clock and recording launch services, through scripted scenarios: every
// operation's result, every launch the service asked for, and the task list after each step. A scenario
// runs once per listed time zone (fixed-time schedules read the local zone, so each is a child process
// started with TZ set). The Rust tests replay each scenario against the Rust service with a manual clock
// and a recording launcher and require the same results, launches and lists.
//   node tools/generate_scheduled_task_service_fixtures.mjs
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { fileURLToPath } from "node:url";

const here = NodePath.dirname(fileURLToPath(import.meta.url));
const output = NodePath.join(here, "../crates/server/tests/fixtures/scheduled-task-service.jsonl");

const T0 = Date.UTC(2026, 8, 9, 12, 0, 0); // 2026-09-09T12:00:00Z, a Wednesday
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const base = {
  title: "Review",
  prompt: "Review the open pull requests.",
  enabled: true,
  schedule: { type: "interval", everyMs: 5 * MIN },
  projectId: "project-1",
  workspaceStrategy: { type: "root" },
  modelSelection: { instanceId: "codex", model: "gpt-5.4" },
  runtimeMode: "full-access",
  interactionMode: "default",
};
const task = (id, overrides = {}) => ({ ...base, id: `scheduled-task:${id}`, ...overrides });
const tid = (id) => `scheduled-task:${id}`;
const hook = (overrides = {}) =>
  task("hook", {
    title: "Hook",
    prompt: "PR: {{body.pull_request.url}}",
    schedule: { type: "webhook" },
    ...overrides,
  });
const body = JSON.stringify({ pull_request: { url: "https://github.com/org/repo/pull/45" } });
const delivery = (overrides = {}) => ({
  op: "webhook",
  task: tid("hook"),
  method: "POST",
  query: "",
  headers: { "content-type": "application/json" },
  body,
  ...overrides,
});

const scenarios = [
  {
    name: "interval task: created, not yet due, due, catches up once, then waits a full interval",
    zones: ["UTC", "America/New_York"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: task("a") },
      { op: "setTime", ms: T0 + 5 * MIN - 1 },
      { op: "tick" },
      { op: "setTime", ms: T0 + 5 * MIN },
      { op: "tick" },
      // The server was off for an hour: one run, not twelve, and the next is a full interval after it.
      { op: "setTime", ms: T0 + HOUR + 10 * MIN },
      { op: "tick" },
      { op: "setTime", ms: T0 + HOUR + 15 * MIN + 999 },
      { op: "tick" },
      { op: "setTime", ms: T0 + HOUR + 15 * MIN + 1000 },
      { op: "tick" },
    ],
  },
  {
    name: "fixed-time task: late within the grace window runs, later than it is skipped to the next day",
    zones: ["UTC", "America/New_York", "Europe/London", "Australia/Lord_Howe", "Asia/Kolkata"],
    steps: [
      { op: "setTime", ms: Date.UTC(2026, 6, 1, 3, 0) },
      {
        op: "upsert",
        input: task("morning", { schedule: { type: "fixed_time", timeOfDay: "09:00" } }),
      },
      {
        op: "upsert",
        input: task("weekdays", {
          schedule: { type: "fixed_time", timeOfDay: "9:30", weekdays: [1, 2, 3, 4, 5] },
        }),
      },
      {
        op: "upsert",
        input: task("weekend", {
          schedule: { type: "fixed_time", timeOfDay: "10:00", weekdays: [0, 6] },
        }),
      },
      // Everything is overdue by minutes to days: within ten minutes runs, beyond it is skipped.
      { op: "setTime", ms: Date.UTC(2026, 6, 1, 20, 0) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 6, 4, 12, 9) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 6, 4, 18, 0) },
      { op: "tick" },
    ],
  },
  {
    name: "fixed-time task across the spring-forward gap and the fall-back overlap",
    zones: ["America/New_York", "Europe/London", "Australia/Lord_Howe", "America/St_Johns"],
    steps: [
      { op: "setTime", ms: Date.UTC(2026, 2, 7, 12, 0) },
      {
        op: "upsert",
        input: task("gap", { schedule: { type: "fixed_time", timeOfDay: "02:30" } }),
      },
      {
        op: "upsert",
        input: task("one", { schedule: { type: "fixed_time", timeOfDay: "01:30" } }),
      },
      {
        op: "upsert",
        input: task("noon", { schedule: { type: "fixed_time", timeOfDay: "12:00" } }),
      },
      { op: "setTime", ms: Date.UTC(2026, 2, 8, 6, 31) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 2, 8, 7, 31) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 2, 29, 1, 5) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 9, 25, 0, 40) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 10, 1, 5, 40) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 10, 1, 6, 40) },
      { op: "tick" },
      { op: "setTime", ms: Date.UTC(2026, 10, 2, 8, 0) },
      { op: "tick" },
    ],
  },
  {
    name: "hourly interval task across the fall-back transition adds wall-clock time",
    zones: ["America/New_York", "Europe/London"],
    steps: [
      { op: "setTime", ms: Date.UTC(2026, 9, 24, 23, 30) },
      { op: "upsert", input: task("hourly", { schedule: { type: "interval", everyMs: HOUR } }) },
      ...[0, 1, 2, 3, 4, 5, 6].flatMap((hour) => [
        { op: "setTime", ms: Date.UTC(2026, 9, 25, 0 + hour, 31) },
        { op: "tick" },
        { op: "setTime", ms: Date.UTC(2026, 10, 1, 4 + hour, 31) },
        { op: "tick" },
      ]),
    ],
  },
  {
    name: "editing: a title edit keeps a due run, a schedule edit restarts the clock, a format-only edit keeps it",
    zones: ["UTC", "America/New_York"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: task("e", { schedule: { type: "fixed_time", timeOfDay: "9:00" } }) },
      { op: "setTime", ms: T0 + 30 * MIN },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          schedule: { type: "fixed_time", timeOfDay: "09:00" },
        }),
      },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          schedule: { type: "fixed_time", timeOfDay: "09:00", weekdays: [] },
        }),
      },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          schedule: { type: "fixed_time", timeOfDay: "09:00", weekdays: [0, 1, 2, 3, 4, 5, 6] },
        }),
      },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          schedule: { type: "fixed_time", timeOfDay: "09:30" },
        }),
      },
      {
        op: "upsert",
        input: task("e", { title: "Renamed", schedule: { type: "interval", everyMs: 2 * HOUR } }),
      },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          enabled: false,
          schedule: { type: "interval", everyMs: 2 * HOUR },
        }),
      },
      { op: "setTime", ms: T0 + 3 * HOUR },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          enabled: true,
          schedule: { type: "interval", everyMs: 2 * HOUR },
          createdBy: "agent",
          creationSource: "mcp",
        }),
      },
      {
        op: "upsert",
        input: task("e", {
          title: "Renamed",
          enabled: true,
          schedule: { type: "interval", everyMs: 2 * HOUR },
          threadId: "thread-1",
          prompt: "Another prompt",
          modelSelection: { instanceId: "claudeAgent", model: "sonnet" },
        }),
      },
      {
        op: "upsert",
        input: task("e", {
          requireExisting: true,
          schedule: { type: "interval", everyMs: 2 * HOUR },
          title: "Form edit",
        }),
      },
      { op: "upsert", input: task("missing", { requireExisting: true }) },
      { op: "upsert", input: { ...base, commandId: "create-1" } },
      { op: "setTime", ms: T0 + 3 * HOUR + MIN },
      { op: "upsert", input: { ...base, commandId: "create-1", title: "Replay" } },
    ],
  },
  {
    name: "enable and disable",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: task("s") },
      { op: "setEnabled", id: tid("s"), enabled: true },
      { op: "setTime", ms: T0 + MIN },
      { op: "setEnabled", id: tid("s"), enabled: false },
      { op: "setTime", ms: T0 + 20 * MIN },
      { op: "tick" },
      { op: "setTime", ms: T0 + 21 * MIN },
      { op: "setEnabled", id: tid("s"), enabled: true },
      { op: "setTime", ms: T0 + 26 * MIN },
      { op: "tick" },
      { op: "setEnabled", id: tid("nope"), enabled: true },
    ],
  },
  {
    name: "run now, with a launch that succeeds, then fails, then dies",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: task("m") },
      { op: "upsert", input: hook() },
      { op: "setTime", ms: T0 + MIN },
      { op: "runNow", id: tid("m") },
      { op: "launcher", mode: "fail" },
      { op: "setTime", ms: T0 + 2 * MIN },
      { op: "runNow", id: tid("m") },
      { op: "launcher", mode: "die" },
      { op: "setTime", ms: T0 + 3 * MIN },
      { op: "runNow", id: tid("m") },
      { op: "launcher", mode: "ok" },
      { op: "runNow", id: tid("hook") },
      { op: "runNow", id: tid("nope") },
      { op: "setTime", ms: T0 + 4 * MIN },
      { op: "upsert", input: task("m", { enabled: false }) },
      { op: "runNow", id: tid("m") },
      { op: "tick" },
    ],
  },
  {
    name: "a task bound to a thread queues its prompt in that thread",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      {
        op: "upsert",
        input: task("bound", {
          threadId: "thread-7",
          prompt: "Daily check",
          createdBy: "agent",
          creationSource: "mcp",
          workspaceStrategy: {
            type: "existing_worktree",
            worktreePath: "/work/tree",
            branch: "feature",
          },
        }),
      },
      {
        op: "upsert",
        input: task("fresh", {
          workspaceStrategy: { type: "worktree", baseRef: "main", startFromOrigin: true },
        }),
      },
      { op: "setTime", ms: T0 + 5 * MIN },
      { op: "tick" },
      { op: "launcher", mode: "fail" },
      { op: "setTime", ms: T0 + 11 * MIN },
      { op: "tick" },
    ],
  },
  {
    name: "delete",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: task("d") },
      { op: "upsert", input: hook() },
      { op: "delete", id: tid("d") },
      { op: "delete", id: tid("d") },
      { op: "delete", id: tid("hook") },
      { op: "setTime", ms: T0 + 10 * MIN },
      { op: "tick" },
      { op: "runNow", id: tid("d") },
      { op: "setEnabled", id: tid("d"), enabled: false },
      { op: "upsert", input: task("d") },
    ],
  },
  {
    name: "restart recovery releases runs the previous process left running",
    zones: ["UTC", "America/New_York"],
    steps: [
      { op: "setTime", ms: T0 },
      {
        op: "seed",
        row: {
          id: "stuck-interval",
          next: new Date(T0 - MIN).toISOString(),
          status: "running",
          schedule: { type: "interval", everyMs: 10 * MIN },
          lastRunAt: new Date(T0 - 2 * MIN).toISOString(),
        },
      },
      {
        op: "seed",
        row: {
          id: "stuck-fixed",
          next: new Date(T0 - MIN).toISOString(),
          status: "running",
          schedule: { type: "fixed_time", timeOfDay: "15:00" },
        },
      },
      {
        op: "seed",
        row: {
          id: "stuck-hook",
          next: null,
          status: "running",
          schedule: { type: "webhook", signature: null },
        },
      },
      {
        op: "seed",
        row: {
          id: "stuck-disabled",
          next: null,
          enabled: 0,
          status: "running",
          schedule: { type: "interval", everyMs: 10 * MIN },
        },
      },
      {
        op: "seed",
        row: {
          id: "idle-due",
          next: new Date(T0 - 2 * MIN).toISOString(),
          status: "failed",
          runCount: 4,
          schedule: { type: "interval", everyMs: 10 * MIN },
        },
      },
      { op: "boot" },
      // The interrupted tasks are not run again; the task that was only waiting is.
      { op: "tick" },
    ],
  },
  {
    name: "legacy sub-minute interval rows keep a one-minute floor",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      {
        op: "seed",
        row: {
          id: "legacy",
          next: new Date(T0 - 1000).toISOString(),
          status: "never",
          schedule: { type: "interval", everyMs: 1000 },
        },
      },
      { op: "boot" },
      { op: "tick" },
      { op: "setTime", ms: T0 + 59_999 },
      { op: "tick" },
      { op: "setTime", ms: T0 + MIN },
      { op: "tick" },
      { op: "setEnabled", id: "legacy", enabled: false },
      { op: "setEnabled", id: "legacy", enabled: true },
      { op: "upsert", input: task("legacy", { schedule: { type: "interval", everyMs: 59_999 } }) },
    ],
  },
  {
    name: "webhook tasks: storage, signature secret, token, rotation",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      {
        op: "upsert",
        input: hook({
          schedule: {
            type: "webhook",
            signature: {
              header: "X-Hub-Signature-256",
              encoding: "hex",
              prefix: "sha256=",
              secret: "s3cret",
            },
            maxDeliveryAgeMinutes: 30,
          },
        }),
      },
      {
        op: "upsert",
        input: hook({
          schedule: {
            type: "webhook",
            signature: { header: "X-Hub-Signature-256", encoding: "hex", prefix: "sha256=" },
          },
        }),
      },
      { op: "upsert", input: hook({ schedule: { type: "webhook", signature: null } }) },
      {
        op: "upsert",
        input: hook({
          schedule: {
            type: "webhook",
            signature: { header: "X-Other", encoding: "base64", prefix: "" },
          },
        }),
      },
      {
        op: "upsert",
        input: task("signed-new", {
          schedule: { type: "webhook", signature: { header: "x", encoding: "hex", prefix: "" } },
        }),
      },
      { op: "rotate", id: tid("hook") },
      { op: "rotate", id: tid("hook") },
      { op: "rotate", id: tid("nope") },
      { op: "upsert", input: task("plain") },
      { op: "rotate", id: tid("plain") },
      { op: "setTime", ms: T0 + MIN },
      { op: "setEnabled", id: tid("hook"), enabled: false },
      { op: "setEnabled", id: tid("hook"), enabled: true },
      { op: "upsert", input: task("plain", { schedule: { type: "webhook" } }) },
      { op: "upsert", input: task("plain", { schedule: { type: "interval", everyMs: HOUR } }) },
      { op: "setTime", ms: T0 + HOUR },
      { op: "tick" },
    ],
  },
  {
    name: "webhook deliveries: every outcome, the log and its redaction",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      {
        op: "upsert",
        input: hook({
          schedule: {
            type: "webhook",
            signature: { header: "x-sig", encoding: "hex", prefix: "sha256=", secret: "s3cret" },
            maxDeliveryAgeMinutes: 10,
          },
        }),
      },
      delivery({ token: "wrong" }),
      delivery({ taskId: "scheduled-task:ghost" }),
      delivery({}),
      delivery({ signature: "sha256-bad" }),
      delivery({ signature: "sha256-good" }),
      delivery({
        signature: "sha256-good",
        query: "token=abc&x=1",
        headers: {
          "content-type": "application/json",
          authorization: "Bearer zzz",
          "x-event": "pull_request",
        },
      }),
      delivery({
        signature: "sha256-good",
        relayDeliveryId: "relay-1",
        receivedAt: new Date(T0 - 5 * MIN).toISOString(),
      }),
      delivery({
        signature: "sha256-good",
        relayDeliveryId: "relay-1",
        receivedAt: new Date(T0 - 5 * MIN).toISOString(),
      }),
      delivery({
        signature: "sha256-good",
        relayDeliveryId: "relay-2",
        receivedAt: new Date(T0 - 11 * MIN).toISOString(),
      }),
      delivery({
        signature: "sha256-good",
        relayDeliveryId: "relay-3",
        receivedAt: new Date(T0 + HOUR).toISOString(),
      }),
      delivery({ signature: "sha256-good", body: "{}" }),
      delivery({ signature: "sha256-good", body: "x".repeat(70_000) }),
      { op: "deliveries", id: tid("hook") },
      { op: "delivery", id: tid("hook"), index: 0 },
      { op: "delivery", id: tid("hook"), index: 2 },
      { op: "delivery", id: tid("hook"), index: "missing" },
      { op: "setEnabled", id: tid("hook"), enabled: false },
      delivery({ signature: "sha256-good" }),
      { op: "setEnabled", id: tid("hook"), enabled: true },
      { op: "launcher", mode: "fail" },
      delivery({ signature: "sha256-good" }),
      { op: "upsert", input: hook({ prompt: "x".repeat(121_000), schedule: { type: "webhook" } }) },
      delivery({ body: "{}" }),
      { op: "deliveries", id: tid("hook") },
      { op: "delete", id: tid("hook") },
      { op: "deliveries", id: tid("hook") },
    ],
  },
  {
    // A disabled hook launches nothing, so what each request meets depends only on the count in the window.
    name: "webhook rate limit",
    zones: ["UTC"],
    steps: [
      { op: "setTime", ms: T0 },
      { op: "upsert", input: hook({ enabled: false, schedule: { type: "webhook" } }) },
      { op: "burst", count: 61, request: delivery({}) },
      { op: "setTime", ms: T0 + 59_999 },
      { op: "burst", count: 1, request: delivery({}) },
      { op: "setTime", ms: T0 + 60_001 },
      { op: "burst", count: 2, request: delivery({}) },
      { op: "deliveries", id: tid("hook") },
    ],
  },
];

// ---- worker: runs scenarios under one TZ -------------------------------------------------------------
if (process.argv[2] === "--worker") {
  const { createHmac } = await import("node:crypto");
  const NodePlatformCrypto =
    await import("../../apps/server/node_modules/@effect/platform-node/dist/NodeCrypto.js");
  const Effect = await import("../../apps/server/node_modules/effect/dist/Effect.js");
  const Layer = await import("../../apps/server/node_modules/effect/dist/Layer.js");
  const Logger = await import("../../apps/server/node_modules/effect/dist/Logger.js");
  const Ref = await import("../../apps/server/node_modules/effect/dist/Ref.js");
  const Schema = await import("../../apps/server/node_modules/effect/dist/Schema.js");
  const Scope = await import("../../apps/server/node_modules/effect/dist/Scope.js");
  const TestClock = await import("../../apps/server/node_modules/effect/dist/testing/TestClock.js");
  const SqlClient = await import("../../apps/server/node_modules/effect/dist/sql/SqlClient.js");
  const Contracts = await import("../../packages/contracts/src/scheduledTask.ts");
  const Service = await import("../../apps/server/src/scheduledTasks/ScheduledTaskService.ts");
  const Scheduler = await import("../../apps/server/src/scheduling/Scheduler.ts");
  const ThreadLaunch =
    await import("../../apps/server/src/orchestration-v2/ThreadLaunchService.ts");
  const ThreadManagement =
    await import("../../apps/server/src/orchestration-v2/ThreadManagementService.ts");
  const SecretRequests = await import("../../apps/server/src/secrets/SecretRequests.ts");
  const Sqlite = await import("../../apps/server/src/persistence/Sqlite.ts");

  const decodeUpsert = Schema.decodeUnknownSync(Contracts.ScheduledTaskUpsertInput);
  const input = JSON.parse(readFileSync(process.argv[3], "utf8"));
  const json = (value, replacer) => JSON.parse(JSON.stringify(value, replacer));
  const normalizeError = (error) => ({
    message: error.message,
    taskId: error.taskId ?? null,
    cause:
      error.cause === undefined
        ? null
        : { name: error.cause.name ?? null, message: error.cause.message ?? String(error.cause) },
  });
  const summarizeRunError = (text) =>
    text === null ? null : text.includes("boom") ? "boom" : text;
  const normalizeTask = (value) => {
    const { webhook, lastRunError, ...rest } = json(value);
    const normalized = { ...rest, lastRunError: summarizeRunError(lastRunError) };
    if (webhook !== undefined)
      normalized.webhook = {
        hasSecret: webhook.hasSecret,
        pathPrefix: webhook.path.slice(0, webhook.path.lastIndexOf("/")),
        url: webhook.url,
      };
    return normalized;
  };
  const sign = (secret, text) =>
    `sha256=${createHmac("sha256", secret).update(text).digest("hex")}`;

  const results = [];
  for (const scenario of input.scenarios) {
    const trace = [];
    const program = Effect.gen(function* () {
      const sql = yield* SqlClient.SqlClient;
      const launches = [];
      const mode = yield* Ref.make("ok");
      const captured = yield* Ref.make(Effect.void);
      const tokens = new Map();
      const deliveryNames = new Map();
      const behave = Effect.gen(function* () {
        const current = yield* Ref.get(mode);
        if (current === "fail") return yield* Effect.fail(new Error("boom"));
        if (current === "die") return yield* Effect.die(new Error("boom"));
      });
      const dependencies = Layer.mergeAll(
        NodePlatformCrypto.layer,
        Layer.mock(Scheduler.Scheduler)({ register: (_name, run) => Ref.set(captured, run) }),
        Layer.mock(ThreadLaunch.ThreadLaunchService)({
          launch: (launch) =>
            Effect.sync(() =>
              launches.push(
                json({
                  kind: "launch",
                  commandId: launch.commandId,
                  messageId: launch.initialMessage.messageId,
                  scheduledTaskId: launch.initialMessage.scheduledTaskId,
                  text: launch.initialMessage.text,
                  projectId: launch.projectId,
                  title: launch.title,
                  modelSelection: launch.modelSelection,
                  runtimeMode: launch.runtimeMode,
                  interactionMode: launch.interactionMode,
                  workspaceStrategy: launch.workspaceStrategy,
                  createdBy: launch.createdBy,
                  creationSource: launch.creationSource,
                }),
              ),
            ).pipe(Effect.andThen(behave), Effect.as({})),
        }),
        Layer.mock(ThreadManagement.ThreadManagementService)({
          sendToThread: (send) =>
            Effect.sync(() =>
              launches.push(
                json({
                  kind: "send",
                  commandId: send.commandId,
                  messageId: send.messageId,
                  scheduledTaskId: send.scheduledTaskId,
                  text: send.text,
                  projectId: send.projectId,
                  threadId: send.threadId,
                  modelSelection: send.modelSelection,
                  mode: send.mode,
                  createdBy: send.createdBy,
                  creationSource: send.creationSource,
                }),
              ),
            ).pipe(Effect.andThen(behave), Effect.as({})),
        }),
        Layer.mock(SecretRequests.SecretRequests)({
          consume: () => Effect.fail({ message: "No such secret request." }),
        }),
      );
      let scope = yield* Scope.make();
      let service = null;
      const boot = Effect.gen(function* () {
        yield* Scope.close(scope, { _tag: "Success", value: undefined });
        scope = yield* Scope.make();
        const context = yield* Layer.buildWithScope(
          Service.layer.pipe(Layer.provide(dependencies)),
          scope,
        );
        service = context.mapUnsafe.get(Service.ScheduledTaskService.key);
      });
      yield* Layer.buildWithScope(Service.layer.pipe(Layer.provide(dependencies)), scope).pipe(
        Effect.map((context) => {
          service = context.mapUnsafe.get(Service.ScheduledTaskService.key);
        }),
      );
      const snapshot = () =>
        service.list().pipe(
          Effect.map((result) => result.tasks.map(normalizeTask)),
          Effect.catch((error) => Effect.succeed({ listError: normalizeError(error) })),
        );
      const settle = Effect.forEach(Array.from({ length: 200 }), () => Effect.yieldNow, {
        discard: true,
      });
      const attempt = (effect, map) =>
        effect.pipe(
          Effect.map((value) => ({ ok: map(value) })),
          Effect.catch((error) => Effect.succeed({ error: normalizeError(error) })),
        );
      const nameOf = (id) => {
        if (!deliveryNames.has(id))
          deliveryNames.set(
            id,
            id.startsWith("delivery:relay:") ? id : `delivery:${deliveryNames.size + 1}`,
          );
        return deliveryNames.get(id);
      };
      const normalizeSummary = (summary) => ({ ...json(summary), id: nameOf(summary.id) });
      const request = (step, index) => {
        const bodyText = step.body;
        const raw = new TextEncoder().encode(bodyText);
        const headers = { ...step.headers };
        if (step.signature === "sha256-good") headers["x-sig"] = sign("s3cret", bodyText);
        if (step.signature === "sha256-bad") headers["x-sig"] = sign("other", bodyText);
        const taskId = step.taskId ?? step.task;
        return {
          hookId: taskId,
          token: step.token === "wrong" ? "wrong-token" : (tokens.get(taskId) ?? "no-token"),
          method: step.method,
          path: `/api/hooks/${taskId}`,
          query: step.query,
          headers,
          body: raw,
          bodyText,
          ...(step.relayDeliveryId === undefined ? {} : { relayDeliveryId: step.relayDeliveryId }),
          ...(step.receivedAt === undefined ? {} : { receivedAt: step.receivedAt }),
        };
      };
      const learnTokens = Effect.gen(function* () {
        const listed = yield* service
          .list()
          .pipe(Effect.catch(() => Effect.succeed({ tasks: [] })));
        for (const listedTask of listed.tasks)
          if (listedTask.webhook !== undefined)
            tokens.set(listedTask.id, listedTask.webhook.path.split("/").at(-1));
      });
      for (const step of scenario.steps) {
        const before = launches.length;
        let result = null;
        switch (step.op) {
          case "setTime":
            yield* TestClock.setTime(step.ms);
            break;
          case "launcher":
            yield* Ref.set(mode, step.mode);
            break;
          case "boot":
            yield* boot;
            break;
          case "seed": {
            const row = step.row;
            const iso = new Date(T0).toISOString();
            yield* sql`INSERT INTO scheduled_tasks ${sql.insert({
              task_id: row.id,
              title: "Seeded",
              prompt: "Seeded prompt",
              enabled: row.enabled ?? 1,
              schedule_json: JSON.stringify(row.schedule),
              project_id: "project-1",
              thread_id: null,
              workspace_strategy_json: '{"type":"root"}',
              model_selection_json: '{"instanceId":"codex","model":"gpt-5.4"}',
              runtime_mode: "full-access",
              interaction_mode: "default",
              created_by: "user",
              creation_source: "web",
              created_at: iso,
              updated_at: iso,
              next_run_at: row.next,
              last_run_at: row.lastRunAt ?? null,
              last_run_status: row.status,
              last_run_error: null,
              run_count: row.runCount ?? 0,
            })}`;
            break;
          }
          case "upsert": {
            let decoded = null;
            try {
              decoded = decodeUpsert(step.input);
            } catch {
              result = { decodeError: true };
            }
            if (decoded !== null)
              result = yield* attempt(service.upsert(decoded), (value) =>
                normalizeTask(value.task),
              );
            yield* learnTokens;
            break;
          }
          case "setEnabled":
            result = yield* attempt(
              service.setEnabled({ id: step.id, enabled: step.enabled }),
              (value) => normalizeTask(value.task),
            );
            break;
          case "delete":
            result = yield* attempt(service.delete({ id: step.id }), (value) => json(value));
            break;
          case "runNow":
            result = yield* attempt(service.runNow({ id: step.id }), (value) =>
              normalizeTask(value.task),
            );
            break;
          case "rotate": {
            const previous = tokens.get(step.id);
            result = yield* attempt(service.rotateWebhookToken({ id: step.id }), (value) =>
              normalizeTask(value.task),
            );
            yield* learnTokens;
            if (result.ok !== undefined) result.tokenChanged = previous !== tokens.get(step.id);
            break;
          }
          case "tick": {
            const run = yield* Ref.get(captured);
            yield* run;
            yield* settle;
            break;
          }
          case "webhook": {
            const outcome = yield* service.triggerWebhook(request(step, 0)).pipe(
              Effect.map((value) => ({
                ...json(value),
                ...(value.deliveryId === undefined ? {} : { deliveryId: nameOf(value.deliveryId) }),
              })),
              Effect.catch((error) => Effect.succeed({ error: normalizeError(error) })),
            );
            yield* settle;
            result = outcome;
            break;
          }
          case "burst": {
            const counts = {};
            for (let index = 0; index < step.count; index += 1) {
              const outcome = yield* service
                .triggerWebhook(request(step.request, index))
                .pipe(
                  Effect.map(
                    (value) =>
                      value._tag + (value.outcome === undefined ? "" : `:${value.outcome}`),
                  ),
                );
              counts[outcome] = (counts[outcome] ?? 0) + 1;
            }
            yield* settle;
            result = counts;
            break;
          }
          case "deliveries":
            result = yield* attempt(service.listWebhookDeliveries({ id: step.id }), (value) =>
              value.deliveries.map(normalizeSummary),
            );
            break;
          case "delivery": {
            const listed = yield* service.listWebhookDeliveries({ id: step.id });
            const target =
              step.index === "missing" ? "delivery:nope" : listed.deliveries[step.index]?.id;
            result = yield* attempt(
              service.getWebhookDelivery({ id: step.id, deliveryId: target }),
              (value) => ({ ...json(value.delivery), id: nameOf(value.delivery.id) }),
            );
            break;
          }
          default:
            throw new Error(`unknown op ${step.op}`);
        }
        // A webhook run's command ids carry its random delivery id; name it like the delivery log does.
        const named = launches
          .slice(before)
          .map((launch) =>
            json(launch, (key, value) =>
              typeof value === "string"
                ? value.replace(/delivery:[0-9a-f]{8}-[0-9a-f-]{27}/g, (id) => nameOf(id))
                : value,
            ),
          );
        trace.push({ step: step.op, result, launches: named, list: yield* snapshot() });
      }
      yield* Scope.close(scope, { _tag: "Success", value: undefined });
    });
    const silent = Logger.layer([]);
    await Effect.runPromise(
      program.pipe(
        Effect.provide(Sqlite.layerMemory),
        Effect.provide(TestClock.layer()),
        Effect.provide(silent),
      ),
    );
    results.push({ scenario: scenario.name, trace });
  }
  writeFileSync(process.argv[4], JSON.stringify(results));
} else {
  const rows = [];
  const zones = [...new Set(scenarios.flatMap((scenario) => scenario.zones))];
  for (const zone of zones) {
    const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-service-fixtures-"));
    writeFileSync(
      NodePath.join(scratch, "in.json"),
      JSON.stringify({ scenarios: scenarios.filter((scenario) => scenario.zones.includes(zone)) }),
    );
    const result = spawnSync(
      process.execPath,
      [
        "--no-warnings",
        fileURLToPath(import.meta.url),
        "--worker",
        NodePath.join(scratch, "in.json"),
        NodePath.join(scratch, "out.json"),
      ],
      {
        env: { ...process.env, TZ: zone },
        encoding: "utf8",
        maxBuffer: 256 * 1024 * 1024,
      },
    );
    if (result.status !== 0)
      throw new Error(`worker for ${zone} failed:\n${result.stderr}\n${result.stdout}`);
    const produced = JSON.parse(readFileSync(NodePath.join(scratch, "out.json"), "utf8"));
    rmSync(scratch, { recursive: true, force: true });
    for (const { scenario, trace } of produced) {
      rows.push({
        scenario,
        zone,
        steps: scenarios.find((candidate) => candidate.name === scenario).steps,
        trace,
      });
    }
  }
  mkdirSync(NodePath.dirname(output), { recursive: true });
  const temporary = `${output}.tmp`;
  writeFileSync(temporary, `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`);
  renameSync(temporary, output);
  console.log(`${rows.length} scenario runs -> ${output}`);
}
