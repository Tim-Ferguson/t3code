// Runs the original scheduled-task helpers a client uses (`scheduleLabel`, `taskToDraft`,
// `scheduleFromDraft`, `parseMaxDeliveryAge`, `webhookAddress`) and records what they produce. The
// Rust tests in t3-client replay the rows and require identical output. `scheduleLabel` lives in a
// React module node cannot load, so its source is cut out of the file and run as it stands.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const logic = await import(
  pathToFileURL(join(root, "apps/web/src/components/settings/scheduledTasksSettings.logic.ts"))
);
const { parseMaxDeliveryAge } = await import(
  pathToFileURL(join(root, "packages/client-runtime/src/scheduledTaskWebhook.ts"))
);
const { webhookAddress } = await import(
  pathToFileURL(join(root, "packages/client-runtime/src/webhookAddress.ts"))
);

const source = readFileSync(
  join(root, "apps/web/src/components/settings/ScheduledTasksSettings.tsx"),
  "utf8",
);
const labels = source.match(/const WEEKDAY_LABELS = [^;]+;/)[0];
const label = source.match(/export function scheduleLabel[\s\S]*?\n}\n/)[0];
const directory = mkdtempSync(join(tmpdir(), "schedule-label-"));
writeFileSync(
  join(directory, "label.ts"),
  `type ScheduledTaskSchedule = any;\n${labels}\n${label}`,
);
const { scheduleLabel } = await import(pathToFileURL(join(directory, "label.ts")));

const rows = [];
const schedules = [
  { type: "webhook", signature: null, maxDeliveryAgeMinutes: null },
  { type: "interval", everyMs: 60000 },
  { type: "interval", everyMs: 300000 },
  { type: "interval", everyMs: 3600000 },
  { type: "interval", everyMs: 90000 },
  { type: "interval", everyMs: 30000 },
  { type: "interval", everyMs: 1500 },
  { type: "interval", everyMs: 61500 },
  { type: "interval", everyMs: 1 },
  { type: "fixed_time", timeOfDay: "09:00" },
  { type: "fixed_time", timeOfDay: "9:30", weekdays: [] },
  { type: "fixed_time", timeOfDay: "23:59", weekdays: [1, 2, 3, 4, 5] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [5, 4, 3, 2, 1] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [0, 6] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [1, 3, 5] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [0, 1, 2, 3, 4, 5, 6] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [6] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [1, 2, 3, 4, 6] },
];
for (const schedule of schedules)
  rows.push({ type: "label", schedule, label: scheduleLabel(schedule) });

for (const input of [
  "",
  " ",
  "5",
  " 5 ",
  "5.5",
  "0",
  "-1",
  "1440",
  "1441",
  "abc",
  "1e2",
  "0x10",
  "0b11",
  "1,5",
  "+7",
  "Infinity",
  "٣",
  "10 20",
  "60.0",
  ".5e1",
  "99999999999999999999",
]) {
  const result = parseMaxDeliveryAge(input);
  rows.push({ type: "age", input, result: result === undefined ? "invalid" : result });
}

const baseTask = {
  id: "scheduled-task:one",
  title: "Review",
  prompt: "Review the open pull requests.",
  enabled: true,
  projectId: "project",
  threadId: null,
  workspaceStrategy: { type: "root" },
  modelSelection: { instanceId: "codex", model: "fixture-model" },
  runtimeMode: "full-access",
  interactionMode: "default",
  createdBy: "user",
  creationSource: "web",
  createdAt: "2026-09-09T12:00:00.000Z",
  updatedAt: "2026-09-09T12:00:00.000Z",
  nextRunAt: null,
  lastRunAt: null,
  lastRunStatus: "never",
  lastRunError: null,
  runCount: 0,
};
const strategies = [
  { type: "root" },
  { type: "root", branch: "main" },
  { type: "existing_worktree", worktreePath: "/work/tree" },
  { type: "worktree", baseRef: "develop", startFromOrigin: true },
  { type: "worktree", baseRef: "main", startFromOrigin: false },
  { type: "worktree", baseRef: "main" },
];
const taskVariants = [
  ...schedules.map((schedule) => ({ schedule })),
  {
    schedule: {
      type: "webhook",
      signature: { header: "x-sig", encoding: "base64", prefix: "" },
      maxDeliveryAgeMinutes: 90,
    },
  },
  {
    schedule: {
      type: "webhook",
      signature: { header: "x-hub-signature-256", encoding: "hex", prefix: "sha256=" },
      maxDeliveryAgeMinutes: null,
    },
  },
  ...strategies.map((workspaceStrategy) => ({ schedule: schedules[2], workspaceStrategy })),
  {
    schedule: schedules[2],
    threadId: "thread-9",
    enabled: false,
    runtimeMode: "approval-required",
    interactionMode: "plan",
  },
  {
    schedule: schedules[2],
    modelSelection: {
      instanceId: "claudeAgent",
      model: "opus",
      options: [
        { id: "effort", value: "high" },
        { id: "thinking", value: true },
      ],
    },
  },
];
const plainDraft = (draft) => ({
  ...draft,
  weekdays: [...draft.weekdays].toSorted((a, b) => a - b),
});
for (const variant of taskVariants) {
  const task = { ...baseTask, ...variant };
  const draft = logic.taskToDraft(task);
  rows.push({
    type: "draft",
    task,
    draft: plainDraft(draft),
    schedule: logic.scheduleFromDraft(draft),
  });
}

const draftBase = {
  scheduleMode: "fixed",
  intervalMinutes: "15",
  timeOfDay: "09:00",
  weekdays: [1, 2, 3, 4, 5],
  signatureEnabled: false,
  signatureHeader: "x-hub-signature-256",
  signatureEncoding: "hex",
  signaturePrefix: "sha256=",
  signatureSecret: "",
  maxDeliveryAgeMinutes: "",
};
const drafts = [
  {},
  { weekdays: [] },
  { weekdays: [0, 1, 2, 3, 4, 5, 6] },
  { weekdays: [6, 0] },
  { weekdays: [3] },
  { timeOfDay: "" },
  { timeOfDay: "7:05" },
  ...["15", "1", "1.5", "0.5", "", "abc", "-3", "2.25", "0.016667", "1e1", " 7 "].map(
    (intervalMinutes) => ({ scheduleMode: "interval", intervalMinutes }),
  ),
  { scheduleMode: "webhook" },
  { scheduleMode: "webhook", maxDeliveryAgeMinutes: "30" },
  { scheduleMode: "webhook", maxDeliveryAgeMinutes: "0" },
  { scheduleMode: "webhook", signatureEnabled: true, signatureSecret: "s3cret" },
  {
    scheduleMode: "webhook",
    signatureEnabled: true,
    signatureSecret: "  padded  ",
    signatureHeader: "  X-Sig  ",
    signatureEncoding: "base64",
    signaturePrefix: "",
  },
  { scheduleMode: "webhook", signatureEnabled: true },
  { scheduleMode: "webhook", signatureEnabled: false, signatureSecret: "ignored" },
];
for (const override of drafts) {
  const draft = { ...draftBase, ...override };
  const original = {
    editingId: null,
    title: "",
    prompt: "",
    enabled: true,
    ...draft,
    weekdays: new Set(draft.weekdays),
  };
  rows.push({ type: "schedule", draft, schedule: logic.scheduleFromDraft(original) });
}

const path = "/api/hooks/scheduled-task%3Ahook/token";
for (const url of [null, "https://relay.t3.codes/v1/hooks/k/t/x"]) {
  for (const base of [
    null,
    "http://127.0.0.1:3773/",
    "http://127.9.9.9:80",
    "http://localhost:3773",
    "http://[::1]:3773/",
    "https://mac.tail1234.ts.net/",
    "https://example.com/app/",
    "http://192.168.1.5:3773",
    "http://LOCALHOST.:3773/",
    "http://127.0.0.1.example.com/",
  ]) {
    const endpoint = { path, url, hasSecret: false };
    const result = webhookAddress(endpoint, base);
    rows.push({ type: "address", endpoint, base, ...result });
  }
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/scheduled-tasks.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows`);
