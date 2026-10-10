// Runs the original's sidebar sections over many shells, for `tests/sidebar_sections_parity.rs`.
//   presentThreadShell                                       packages/client-runtime/src/state/models.ts
//   effectiveSnoozed, threadWokeAt, threadRaisedHandWhileSnoozed   packages/client-runtime/src/state/threadSettled.ts
//   sort*Threads, planPinnedReorder                          packages/client-runtime/src/state/threadSort.ts
//   resolveSidebarThreadSection, resolveSidebarDropTarget,
//   resolveSidebarDropVerb, planSidebarThreadDrop            apps/web/src/components/Sidebar.logic.ts
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
const background = read("packages/shared/src/orchestrationV2PendingBackgroundWork.ts");
const settled = read("packages/client-runtime/src/state/threadSettled.ts");
const sort = read("packages/client-runtime/src/state/threadSort.ts");
const body = [
  cut(background, "export function backgroundWorkHoldsCompletion", "\n}\n") + "\n}\n",
  cut(background, "function backgroundWorkKindHoldsCompletion", "\n}\n") + "\n}\n",
  cut(models, "function iso(", "export function scopeProject("),
  cut(models, "export function presentThreadShell", "export function scopeThreadShell"),
  cut(settled, "export function threadRaisedHandWhileSnoozed", "/**\n * A thread may be snoozed"),
  cut(settled, "export function effectiveSnoozed", "const HOUR_MS"),
  cut(sort, "export function toSortableTimestamp", "function getFirstSortableTimestamp"),
  cut(sort, "export function activeThreadAnchorTimestampMs", "export function sortThreads"),
  cut(sort, "const PIN_ORDER_DIGITS", "/**\n * planPinnedReorder specialized for mobile"),
  cut(logic, "export function resolveSidebarThreadSection", "/** Sortable ids"),
  cut(logic, "const SIDEBAR_MARKER_PREFIX", "export type SidebarListMarker"),
  cut(logic, "export function sidebarMarkerId", "export type SidebarListItem"),
  cut(logic, "export function sidebarListItemId", "export type SidebarDropTarget"),
  cut(logic, "export function resolveSidebarDropTarget", "export type SidebarThreadDropPlan"),
  cut(logic, "export function resolveSidebarDropVerb", "/** Eligible rows between"),
  cut(logic, "export function planSidebarThreadDrop", "/** Project a drop's lifecycle"),
].join("\n");
const header = `
const DateTime = { formatIso: (v) => v, toDate: (v) => v, makeUnsafe: (v) => new Date(v) };
const formatSubagentDisplayTitle = (title) => title;
const threadPullRequestsOf = () => [];
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-sidebar-sections-"));
const file = NodePath.join(scratch, "sections.mjs");
writeFileSync(
  file,
  `${header}\n${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { presentThreadShell, effectiveSnoozed, threadWokeAt, sortSettledThreads, sortPinnedThreadsByOrderKey, sortActiveThreadsByOrderKey, resolveSidebarThreadSection, resolveSidebarDropTarget, resolveSidebarDropVerb, planSidebarThreadDrop };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

let seed = 20261011;
const random = () => (seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32;
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (p) => random() < p;
const at = (minutes) => new Date(Date.UTC(2026, 9, 1, 8, 0, 0) + minutes * 60_000).toISOString();
const NOW = at(600);
const statuses = ["idle", "running", "waiting", "completed", "failed", "interrupted", "cancelled"];
const keys = ["b", "c", "f", "m", "mm", "n", "t", "tb", "z"];

const makeShell = (id) => {
  const status = pick(statuses);
  const ran = chance(0.8);
  const shell = {
    createdBy: "user",
    creationSource: "web",
    id,
    projectId: "p",
    title: id,
    providerInstanceId: "codex",
    modelSelection: { instanceId: "codex", model: "m" },
    runtimeMode: "full-access",
    interactionMode: "default",
    branch: null,
    worktreePath: null,
    lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: id },
    forkedFrom: null,
    activeProviderThreadId: ran ? "pt" : null,
    latestRunId: ran ? "run-1" : null,
    activeRunId: null,
    status,
    pendingRuntimeRequest: chance(0.1)
      ? { id: "r", kind: pick(["command", "user_input", "auth_refresh"]), createdAt: at(1) }
      : null,
    latestVisibleMessage: null,
    latestUserMessageAt: chance(0.7) ? at(pick([1, 50, 120, 300])) : null,
    hasActionableProposedPlan: false,
    pendingBackgroundTasks: [],
    itemCount: 1,
    visibleItemCount: 1,
    createdAt: at(pick([0, 10, 10, 30, 90, 200])),
    updatedAt: at(pick([100, 250, 400, 590])),
    archivedAt: null,
    settledOverride: pick([null, "settled", "active", "settled"]),
    settledAt: chance(0.5) ? at(pick([20, 60, 60, 300])) : null,
    deletedAt: null,
  };
  if (chance(0.5)) shell.pinnedAt = at(pick([5, 40, 90]));
  if (shell.pinnedAt && chance(0.7)) shell.pinOrderKey = pick(keys);
  if (chance(0.4)) shell.activeOrderKey = pick(keys);
  if (chance(0.3)) shell.unsettledAt = at(pick([20, 150, 500]));
  if (chance(0.4)) {
    shell.snoozedUntil = at(pick([300, 590, 610, 700, 900]));
    shell.snoozedAt = at(pick([100, 280, 500]));
  }
  if (ran && chance(0.5)) shell.latestRunRequestedAt = at(pick([5, 70, 400]));
  if (ran && chance(0.5)) shell.latestRunStartedAt = at(pick([6, 80, 410]));
  if (ran && chance(0.5))
    shell.latestRunCompletedAt = chance(0.2) ? null : at(pick([10, 90, 450, 580]));
  return shell;
};
const present = (shell) => original.presentThreadShell("env", structuredClone(shell));
const ids = (list) => list.map((thread) => thread.id);

const rows = [];
for (let index = 0; index < 70; index += 1) {
  const shells = Array.from({ length: 2 + Math.floor(random() * 9) }, (_, n) =>
    makeShell(`t${index}-${n}`),
  );
  const support = { settlement: chance(0.7), snooze: chance(0.8) };
  const threads = shells.map(present);
  const sections = { pinned: [], active: [], snoozed: [], settled: [] };
  for (const thread of threads) {
    const section = original.resolveSidebarThreadSection({
      snoozed: support.snooze && original.effectiveSnoozed(thread, { now: NOW }),
      settled: support.settlement && thread.settledOverride === "settled",
      pinned: thread.pinnedAt != null,
    });
    sections[section].push(thread);
  }
  const output = {
    pinned: ids(original.sortPinnedThreadsByOrderKey(sections.pinned)),
    active: ids(original.sortActiveThreadsByOrderKey(sections.active)),
    snoozed: ids(
      sections.snoozed.toSorted((a, b) => Date.parse(a.snoozedUntil) - Date.parse(b.snoozedUntil)),
    ),
    settled: ids(original.sortSettledThreads(sections.settled)),
  };
  rows.push({
    type: "sections",
    now: NOW,
    support,
    shells,
    output,
    woke: threads.map((thread) => ({
      snoozed: original.effectiveSnoozed(thread, { now: NOW }),
      wokeAt: original.threadWokeAt(thread, { now: NOW }),
    })),
  });
}

// Drops: a project's sections laid out as the original's sortable list, dropped at a random slot.
const markers = ["pinned-header", "pinned-divider", "snoozed-header", "settled-header"];
for (let index = 0; index < 130; index += 1) {
  const shells = Array.from({ length: 3 + Math.floor(random() * 6) }, (_, n) =>
    makeShell(`d${index}-${n}`),
  );
  const support = { settlement: chance(0.75), snooze: true };
  const threads = shells.map(present);
  const sectionOf = (thread) =>
    original.resolveSidebarThreadSection({
      snoozed: support.snooze && original.effectiveSnoozed(thread, { now: NOW }),
      settled: support.settlement && thread.settledOverride === "settled",
      pinned: thread.pinnedAt != null,
    });
  const groups = { pinned: [], active: [], snoozed: [], settled: [] };
  for (const thread of threads) groups[sectionOf(thread)].push(thread);
  groups.pinned = original.sortPinnedThreadsByOrderKey(groups.pinned);
  groups.active = original.sortActiveThreadsByOrderKey(groups.active);
  groups.snoozed.sort((a, b) => Date.parse(a.snoozedUntil) - Date.parse(b.snoozedUntil));
  groups.settled = original.sortSettledThreads(groups.settled);
  const thread = (section) =>
    groups[section].map((item) => ({ kind: "thread", key: item.id, section }));
  const list = [
    { kind: "marker", marker: "pinned-header" },
    ...thread("pinned"),
    { kind: "marker", marker: "pinned-divider" },
    ...thread("active"),
    ...(groups.snoozed.length > 0 ? [{ kind: "marker", marker: "snoozed-header" }] : []),
    ...thread("snoozed"),
    { kind: "marker", marker: "settled-header" },
    ...thread("settled"),
  ];
  const itemId = (item) => (item.kind === "thread" ? item.key : `sidebar-marker-${item.marker}`);
  const movable = list.filter((item) => item.kind === "thread");
  const active = pick(movable);
  const over = pick(list);
  const target = original.resolveSidebarDropTarget(list, active.key, itemId(over));
  const activeThread = threads.find((item) => item.id === active.key);
  const keysById = (field) =>
    Object.fromEntries(threads.map((item) => [item.id, item[field] ?? null]));
  const writable = (flag) => (flag ? threads.map((item) => item.id) : null);
  const pinWritable = chance(0.8);
  const activeWritable = chance(0.8);
  let plan = null;
  if (target !== null) {
    plan = original.planSidebarThreadDrop({
      activeKey: active.key,
      activeSection: active.section,
      activePinned: activeThread.pinnedAt != null,
      activeSettled: activeThread.settledOverride === "settled",
      supportsSettlement: support.settlement,
      target,
      pinnedOrder: ids(groups.pinned),
      pinnedKeysById: new Map(Object.entries(keysById("pinOrderKey"))),
      reorderableKeys: pinWritable ? new Set(writable(true)) : new Set(),
      activeOrder: ids(groups.active),
      activeKeysById: new Map(Object.entries(keysById("activeOrderKey"))),
      activeReorderableKeys: activeWritable ? new Set(writable(true)) : new Set(),
    });
  }
  rows.push({
    type: "drop",
    now: NOW,
    support,
    shells,
    moved: active.key,
    from: active.section,
    over: itemId(over),
    pinWritable,
    activeWritable,
    target,
    plan,
    verb: target ? original.resolveSidebarDropVerb(active.section, target.section) : null,
  });
}
void markers;
writeFileSync(
  new URL("../crates/client/tests/fixtures/sidebar-sections.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
