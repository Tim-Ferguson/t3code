// Decodes payloads with the original scheduled-task contracts (packages/contracts/src/scheduledTask.ts)
// and records what each one becomes, or that it fails; also records the error envelope and the
// read model. The Rust tests decode the same payloads and require the same accept/reject decision and
// the same value, so a client that works against the original works against the Rust server.
//   node tools/generate_scheduled_task_contract_fixtures.mjs
import { mkdirSync, renameSync, writeFileSync } from "node:fs";
import * as NodePath from "node:path";
import { fileURLToPath } from "node:url";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Contracts from "../../packages/contracts/src/scheduledTask.ts";

const here = NodePath.dirname(fileURLToPath(import.meta.url));
const output = NodePath.join(
  here,
  "../crates/server/tests/fixtures/scheduled-task-contracts.jsonl",
);
const json = (value) => JSON.parse(JSON.stringify(value));
const rows = [];

const codec = (schema) => Schema.toCodecJson(schema);
const decoders = {
  upsert: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskUpsertInput)),
  setEnabled: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskSetEnabledInput)),
  delete: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskDeleteInput)),
  runNow: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskRunNowInput)),
  rotate: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskRotateWebhookTokenInput)),
  list: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskListInput)),
  listDeliveries: Schema.decodeUnknownExit(
    codec(Contracts.ScheduledTaskListWebhookDeliveriesInput),
  ),
  getDelivery: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskGetWebhookDeliveryInput)),
  task: Schema.decodeUnknownExit(codec(Contracts.ScheduledTask)),
  schedule: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskSchedule)),
  listResult: Schema.decodeUnknownExit(codec(Contracts.ScheduledTaskListResult)),
};
const record = (kind, payload) => {
  const exit = decoders[kind](json(payload));
  rows.push({
    type: "decode",
    kind,
    payload: json(payload),
    ok: exit._tag === "Success" ? json(exit.value) : undefined,
  });
};

// ---- upsert ------------------------------------------------------------------------------------
const base = {
  title: "Review",
  prompt: "Review the open pull requests.",
  enabled: true,
  schedule: { type: "interval", everyMs: 60_000 },
  projectId: "project-1",
  workspaceStrategy: { type: "root" },
  modelSelection: { instanceId: "codex", model: "gpt-5.4" },
  runtimeMode: "full-access",
  interactionMode: "default",
};
const without = (object, key) => {
  const { [key]: _removed, ...rest } = object;
  return rest;
};
const upserts = [
  base,
  {
    ...base,
    id: "scheduled-task:abc",
    commandId: "cmd-1",
    requireExisting: true,
    threadId: "thread-1",
    createdBy: "agent",
    creationSource: "mcp",
  },
  {
    ...base,
    id: "  padded  ",
    commandId: "  cmd  ",
    title: "  Padded title  ",
    prompt: "  Padded prompt\n",
    projectId: " p ",
  },
  { ...base, id: "" },
  { ...base, id: "   " },
  { ...base, id: null },
  { ...base, id: 5 },
  { ...base, commandId: "" },
  { ...base, commandId: null },
  { ...base, requireExisting: false },
  { ...base, requireExisting: null },
  { ...base, requireExisting: "true" },
  { ...base, title: "" },
  { ...base, title: "   " },
  { ...base, title: 5 },
  without(base, "title"),
  { ...base, prompt: "" },
  { ...base, prompt: " \n " },
  without(base, "prompt"),
  { ...base, enabled: false },
  { ...base, enabled: "yes" },
  { ...base, enabled: 1 },
  without(base, "enabled"),
  { ...base, projectId: "" },
  without(base, "projectId"),
  { ...base, projectId: 5 },
  { ...base, threadId: null },
  { ...base, threadId: "" },
  { ...base, threadId: "t" },
  { ...base, threadId: 5 },
  { ...base, schedule: { type: "interval", everyMs: 59_999 } },
  { ...base, schedule: { type: "interval", everyMs: 60_000.5 } },
  { ...base, schedule: { type: "interval", everyMs: 1 } },
  { ...base, schedule: { type: "interval", everyMs: 0 } },
  { ...base, schedule: { type: "interval", everyMs: -60_000 } },
  { ...base, schedule: { type: "interval", everyMs: "60000" } },
  { ...base, schedule: { type: "interval" } },
  { ...base, schedule: { type: "interval", everyMs: 9_007_199_254_740_991 } },
  { ...base, schedule: { type: "interval", everyMs: 1e20 } },
  { ...base, schedule: { type: "interval", everyMs: 120_000, extra: true } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: " 9:30 " } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "24:00" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:60" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "9:5" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "" } },
  { ...base, schedule: { type: "fixed_time" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30:00" } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [1, 2, 3] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [7] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [-1] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [1.5] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: ["1"] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: [0, 6, 0] } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: null } },
  { ...base, schedule: { type: "fixed_time", timeOfDay: "09:30", weekdays: "1" } },
  { ...base, schedule: { type: "webhook" } },
  { ...base, schedule: { type: "webhook", signature: null } },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "X-Hub-Signature-256", encoding: "hex", prefix: "sha256=", secret: "s" },
    },
  },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "x-sig", encoding: "base64", prefix: "", secretRef: "ref-1" },
    },
  },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: {
        header: "x-sig",
        encoding: "base64",
        prefix: "",
        secret: "s",
        secretRef: "ref-1",
      },
    },
  },
  {
    ...base,
    schedule: { type: "webhook", signature: { header: "x-sig", encoding: "base64", prefix: "" } },
  },
  {
    ...base,
    schedule: { type: "webhook", signature: { header: "x-sig", encoding: "sha1", prefix: "" } },
  },
  {
    ...base,
    schedule: { type: "webhook", signature: { header: "", encoding: "hex", prefix: "" } },
  },
  { ...base, schedule: { type: "webhook", signature: { header: "x", encoding: "hex" } } },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "x", encoding: "hex", prefix: "", secret: "" },
    },
  },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "x", encoding: "hex", prefix: "", secret: "  s  " },
    },
  },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "x", encoding: "hex", prefix: "", secretRef: "" },
    },
  },
  {
    ...base,
    schedule: {
      type: "webhook",
      signature: { header: "x", encoding: "hex", prefix: "", secret: null },
    },
  },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: 1 } },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: 1440 } },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: 0 } },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: 1441 } },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: null } },
  { ...base, schedule: { type: "webhook", maxDeliveryAgeMinutes: 1.5 } },
  { ...base, schedule: { type: "cron", expression: "* * * * *" } },
  { ...base, schedule: {} },
  { ...base, schedule: null },
  { ...base, schedule: "interval" },
  without(base, "schedule"),
  { ...base, workspaceStrategy: { type: "existing_worktree", worktreePath: "/tmp/w" } },
  {
    ...base,
    workspaceStrategy: { type: "existing_worktree", worktreePath: "/tmp/w", branch: "b" },
  },
  { ...base, workspaceStrategy: { type: "existing_worktree" } },
  { ...base, workspaceStrategy: { type: "worktree", baseRef: "main" } },
  {
    ...base,
    workspaceStrategy: {
      type: "worktree",
      baseRef: "main",
      branch: "feature",
      startFromOrigin: true,
    },
  },
  { ...base, workspaceStrategy: { type: "worktree" } },
  { ...base, workspaceStrategy: { type: "root", branch: "main" } },
  { ...base, workspaceStrategy: { type: "root", branch: null } },
  { ...base, workspaceStrategy: { type: "other" } },
  without(base, "workspaceStrategy"),
  {
    ...base,
    modelSelection: {
      instanceId: "codex",
      model: "gpt-5",
      options: [
        { id: "effort", value: "high" },
        { id: "fast", value: true },
      ],
    },
  },
  { ...base, modelSelection: { provider: "codex", model: "gpt-5" } },
  { ...base, modelSelection: { instanceId: "", model: "gpt-5" } },
  { ...base, modelSelection: { instanceId: "codex", model: "" } },
  { ...base, modelSelection: { instanceId: "codex" } },
  { ...base, modelSelection: { model: "x" } },
  { ...base, modelSelection: { instanceId: "codex", model: " gpt-5 " } },
  { ...base, modelSelection: { instanceId: "Bad Id!", model: "gpt-5" } },
  { ...base, modelSelection: { instanceId: "codex", model: "gpt-5", options: { effort: "high" } } },
  without(base, "modelSelection"),
  ...["approval-required", "auto-accept-edits", "auto", "full-access", "nope", ""].map(
    (runtimeMode) => ({ ...base, runtimeMode }),
  ),
  without(base, "runtimeMode"),
  ...["default", "plan", "chat", ""].map((interactionMode) => ({ ...base, interactionMode })),
  without(base, "interactionMode"),
  ...["user", "agent", "system", "robot", null].map((createdBy) => ({ ...base, createdBy })),
  ...["web", "mobile", "mcp", "provider", "server", "cli", null].map((creationSource) => ({
    ...base,
    creationSource,
  })),
  { ...base, unknownField: 1 },
  {},
  [],
  null,
  "x",
  5,
  true,
];
for (const payload of upserts) record("upsert", payload);

// ---- the small inputs --------------------------------------------------------------------------
for (const kind of ["setEnabled", "delete", "runNow", "rotate"]) {
  const extra = kind === "setEnabled" ? { enabled: true } : {};
  for (const payload of [
    { id: "t", ...extra },
    { id: "  t  ", ...extra },
    { id: "", ...extra },
    { id: "  ", ...extra },
    { id: 5, ...extra },
    { id: null, ...extra },
    extra,
    { id: "t", ...extra, more: 1 },
    null,
    [],
    "t",
    ...(kind === "setEnabled"
      ? [
          { id: "t", enabled: false },
          { id: "t", enabled: "true" },
          { id: "t" },
          { id: "t", enabled: null },
          { id: "t", enabled: 1 },
        ]
      : []),
  ])
    record(kind, payload);
}
for (const payload of [{}, [], null, 5, "x", { a: 1 }, true, undefined])
  record("list", payload ?? null);
for (const payload of [{ id: "t" }, { id: "" }, {}, null, { id: " t " }])
  record("listDeliveries", payload);
for (const payload of [
  { id: "t", deliveryId: "d" },
  { id: "t", deliveryId: "" },
  { id: "t" },
  { deliveryId: "d" },
  { id: " t ", deliveryId: " delivery:relay:x " },
  null,
])
  record("getDelivery", payload);

// ---- read model --------------------------------------------------------------------------------
const task = {
  id: "scheduled-task:1",
  title: "T",
  prompt: "P",
  enabled: true,
  schedule: { type: "interval", everyMs: 60_000 },
  projectId: "p",
  threadId: null,
  workspaceStrategy: { type: "root" },
  modelSelection: { instanceId: "codex", model: "gpt-5" },
  runtimeMode: "full-access",
  interactionMode: "default",
  createdBy: "user",
  creationSource: "web",
  createdAt: "2026-09-09T12:00:00.000Z",
  updatedAt: "2026-09-09T12:00:00.000Z",
  nextRunAt: "2026-09-09T12:01:00.000Z",
  lastRunAt: null,
  lastRunStatus: "never",
  lastRunError: null,
  runCount: 0,
};
const tasks = [
  task,
  { ...task, schedule: { type: "interval", everyMs: 1_000 } },
  { ...task, schedule: { type: "interval", everyMs: 0 } },
  { ...task, schedule: { type: "interval", everyMs: -5 } },
  { ...task, schedule: { type: "fixed_time", timeOfDay: "9:00", weekdays: [1] } },
  { ...task, schedule: { type: "fixed_time", timeOfDay: "25:00" } },
  {
    ...task,
    schedule: { type: "webhook", signature: null },
    webhook: { path: "/api/hooks/x/y", url: null, hasSecret: false },
  },
  {
    ...task,
    schedule: {
      type: "webhook",
      signature: { header: "x-sig", encoding: "hex", prefix: "sha256=" },
      maxDeliveryAgeMinutes: 30,
    },
    webhook: {
      path: "/api/hooks/x/y",
      url: "https://relay.test/v1/hooks/abc/x/y",
      hasSecret: true,
    },
  },
  { ...task, schedule: { type: "webhook", signature: null, maxDeliveryAgeMinutes: null } },
  { ...task, schedule: { type: "webhook" } },
  {
    ...task,
    schedule: {
      type: "webhook",
      signature: { header: "x", encoding: "hex", prefix: "", secret: "leak" },
    },
  },
  { ...task, schedule: { type: "cron" } },
  { ...task, threadId: "thread-1" },
  { ...task, threadId: "" },
  { ...task, lastRunStatus: "running", lastRunAt: "2026-09-09T12:00:00.000Z" },
  { ...task, lastRunStatus: "failed", lastRunError: "boom", runCount: 3 },
  { ...task, lastRunStatus: "weird" },
  { ...task, runCount: -1 },
  { ...task, runCount: 1.5 },
  { ...task, nextRunAt: null },
  { ...task, nextRunAt: "not a date" },
  { ...task, createdAt: 5 },
  { ...task, createdBy: "robot" },
  { ...task, creationSource: "cli" },
  without(task, "title"),
  without(task, "lastRunError"),
  without(task, "nextRunAt"),
  { ...task, extra: 1 },
  { ...task, title: "  padded  ", id: "  id  " },
];
for (const payload of tasks) record("task", payload);
for (const payload of [
  { type: "interval", everyMs: 1 },
  { type: "interval", everyMs: 0 },
  { type: "fixed_time", timeOfDay: "00:00" },
  { type: "webhook", signature: null },
  { type: "interval", everyMs: 1.5 },
])
  record("schedule", payload);
record("listResult", {
  tasks: [
    task,
    { ...task, schedule: { type: "cron" } },
    { ...task, id: "" },
    { ...task, id: "two" },
  ],
});
record("listResult", { tasks: [] });
record("listResult", { tasks: [5, null, "x"] });
record("listResult", { tasks: "no" });
record("listResult", {});

// ---- error envelope ----------------------------------------------------------------------------
const encodeError = Schema.encodeSync(codec(Contracts.ScheduledTaskError));
const E = Contracts.ScheduledTaskError;
const errors = [
  new E({ message: "Schedule task not found.", taskId: "t1" }),
  new E({ message: "Schedule task not found." }),
  new E({
    message: "Could not run schedule task.",
    taskId: "t1",
    cause: new E({ message: "Schedule task is already running.", taskId: "t1" }),
  }),
  new E({
    message: "Could not save schedule task.",
    taskId: "t1",
    cause: new Error("SQLITE_BUSY: database is locked"),
  }),
  new E({ message: "x", cause: "plain string" }),
  new E({ message: "x", cause: { code: 5 } }),
  new E({ message: "x", cause: 42 }),
];
for (const error of errors) rows.push({ type: "error", wire: json(encodeError(error)) });

mkdirSync(NodePath.dirname(output), { recursive: true });
const temporary = `${output}.tmp`;
writeFileSync(temporary, `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`);
renameSync(temporary, output);
console.log(`${rows.length} rows -> ${output}`);
