// Runs the original's thread-row rules over many shells, for `tests/sidebar_parity.rs`.
//   presentThreadShell / shellRuntime      packages/client-runtime/src/state/models.ts
//   resolveSidebarThreadStatus, resolveThreadStatusPill, hasUnseenCompletion,
//   resolveProjectStatusIndicator,
//   formatWorkingDurationLabel             apps/web/src/components/Sidebar.logic.ts
//   isLatestRunSettled                     apps/web/src/session-logic.ts
//   backgroundWorkHoldsCompletion          packages/shared/src/orchestrationV2PendingBackgroundWork.ts
//   resolveSnoozePresets, snoozeWakeLabel  packages/client-runtime/src/state/threadSettled.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const models = read("packages/client-runtime/src/state/models.ts");
const logic = read("apps/web/src/components/Sidebar.logic.ts");
const session = read("apps/web/src/session-logic.ts");
const background = read("packages/shared/src/orchestrationV2PendingBackgroundWork.ts");
const settled = read("packages/client-runtime/src/state/threadSettled.ts");
const body = [
  cut(background, "export function backgroundWorkHoldsCompletion", "\n}\n") + "\n}\n",
  cut(background, "function backgroundWorkKindHoldsCompletion", "\n}\n") + "\n}\n",
  cut(models, "function iso(", "export function scopeProject("),
  cut(models, "export function presentThreadShell", "export function scopeThreadShell"),
  cut(session, "export function isLatestRunSettled", "export function deriveActiveWorkStartedAt"),
  cut(
    logic,
    "export function hasUnseenCompletion",
    "export function shouldClearThreadSelectionOnMouseDown",
  ),
  cut(logic, "export function resolveSidebarThreadStatus", "export type SidebarV2TopStatusKind"),
  cut(
    logic,
    "export function formatWorkingDurationLabel",
    "export function getFallbackThreadIdAfterDelete",
  ),
  cut(logic, "const THREAD_STATUS_PRIORITY", "type ThreadStatusInput"),
  cut(settled, "const HOUR_MS", "export type CustomSnoozeInput"),
].join("\n");
const header = `
const DateTime = { formatIso: (v) => v, toDate: (v) => v, makeUnsafe: (v) => new Date(v) };
const formatSubagentDisplayTitle = (title) => title;
const threadPullRequestsOf = () => [];
const DAY_MS = 24 * 60 * 60 * 1000;
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-sidebar-"));
const file = NodePath.join(scratch, "sidebar.mjs");
writeFileSync(
  file,
  `${header}\n${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { presentThreadShell, resolveSidebarThreadStatus, resolveThreadStatusPill, hasUnseenCompletion, resolveProjectStatusIndicator, formatWorkingDurationLabel, resolveSnoozePresets, snoozeWakeLabel };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

let seed = 20261010;
const random = () => (seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32;
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (p) => random() < p;
const at = (minutes) => new Date(Date.UTC(2026, 9, 1, 8, 0, 0) + minutes * 60_000).toISOString();

const rows = [];
const statuses = [
  "idle",
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
  "completed",
  "interrupted",
  "failed",
  "cancelled",
  "rolled_back",
];
for (let index = 0; index < 600; index += 1) {
  const status = pick(statuses);
  const shell = {
    createdBy: "user",
    creationSource: "web",
    id: `thread-${index}`,
    projectId: "p",
    title: "T",
    providerInstanceId: "codex",
    modelSelection: { instanceId: "codex", model: "m" },
    runtimeMode: "full-access",
    interactionMode: pick(["default", "plan"]),
    branch: null,
    worktreePath: null,
    lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: `thread-${index}` },
    forkedFrom: null,
    activeProviderThreadId: chance(0.8) ? "pt" : null,
    latestRunId: chance(0.85) ? "run-1" : null,
    activeRunId: chance(0.4) ? "run-1" : null,
    status,
    pendingRuntimeRequest: chance(0.2)
      ? {
          id: "r",
          kind: pick(["command", "user_input", "auth_refresh", "permission"]),
          createdAt: at(1),
        }
      : null,
    latestVisibleMessage: null,
    latestUserMessageAt: at(2),
    hasActionableProposedPlan: chance(0.4),
    pendingBackgroundTasks: chance(0.3)
      ? [{ kind: pick(["command", "subagent", "monitor", "background_task"]), taskId: "t" }]
      : [],
    itemCount: 1,
    visibleItemCount: 1,
    createdAt: at(0),
    updatedAt: at(chance(0.5) ? 30 : 10),
    archivedAt: null,
    settledOverride: null,
    settledAt: null,
    deletedAt: null,
  };
  if (chance(0.6))
    shell.activityRunStatus = pick(["preparing", "starting", "running", "waiting", null]);
  if (chance(0.3)) shell.lastErrorClass = pick(["usage_limit", "provider_error", null]);
  if (chance(0.5)) shell.lastVisitedAt = chance(0.1) ? null : at(pick([5, 20, 40]));
  if (chance(0.4)) shell.latestRunCompletedAt = chance(0.2) ? null : at(pick([15, 25, 35]));
  const presented = original.presentThreadShell("env", structuredClone(shell));
  rows.push({
    type: "thread",
    shell,
    status: original.resolveSidebarThreadStatus(presented),
    pill: original.resolveThreadStatusPill({ thread: presented })?.label ?? null,
    unseen: original.hasUnseenCompletion(presented),
    runtime: presented.runtime?.status ?? null,
  });
}
const labels = [
  "Pending Approval",
  "Awaiting Input",
  "Working",
  "Connecting",
  "Waiting",
  "Plan Ready",
  "Completed",
];
const pill = (label) => (label === null ? null : { label });
for (let index = 0; index < 60; index += 1) {
  const list = Array.from({ length: Math.floor(random() * 5) }, () =>
    chance(0.3) ? null : pick(labels),
  );
  rows.push({
    type: "highest",
    labels: list,
    output: original.resolveProjectStatusIndicator(list.map(pill))?.label ?? null,
  });
}
for (const ms of [
  0, 999, 1000, 59_999, 60_000, 61_000, 3_599_000, 3_600_000, 3_660_000, 7_322_000, -5, 90_000_000,
])
  rows.push({ type: "working", ms, output: original.formatWorkingDurationLabel(ms) });
const zones = [
  ["UTC", 0],
  ["Asia/Kolkata", 330],
  ["America/Sao_Paulo", -180],
  ["Asia/Tokyo", 540],
];
for (const [zone, offset] of zones) {
  process.env.TZ = zone;
  for (let day = 0; day < 9; day += 1) {
    for (const hour of [0, 6, 9, 16, 17, 18, 22, 23]) {
      const now = new Date(Date.UTC(2026, 9, 4 + day, hour, 17, 0));
      rows.push({
        type: "presets",
        now: now.toISOString(),
        offset,
        output: original
          .resolveSnoozePresets(now)
          .map(({ id, label, snoozedUntil }) => ({ id, label, until: snoozedUntil })),
      });
    }
  }
}
process.env.TZ = "UTC";
const now = "2026-10-01T12:00:00.000Z";
for (const minutes of [
  -5, 0, 0.5, 1, 30, 59, 59.5, 60, 61, 119, 120, 600, 1439, 1440, 1441, 2879, 4320, 10000,
]) {
  const until = new Date(Date.parse(now) + minutes * 60_000).toISOString();
  rows.push({ type: "wake", now, until, output: original.snoozeWakeLabel(until, { now }) });
}
rows.push({
  type: "wake",
  now,
  until: "garbage",
  output: original.snoozeWakeLabel("garbage", { now }),
});
writeFileSync(
  new URL("../crates/client/tests/fixtures/sidebar.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
