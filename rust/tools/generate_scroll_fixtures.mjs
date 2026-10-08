// Execute unchanged original pure helpers; development oracle only.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const base = new URL("../../", import.meta.url);
const anchoring = readFileSync(
  new URL("apps/web/src/components/chat/timelineScrollAnchoring.ts", base),
  "utf8",
);
const work = readFileSync(
  new URL("packages/client-runtime/src/work-log/scrollAnchor.ts", base),
  "utf8",
);
const logic = readFileSync(
  new URL("apps/web/src/components/chat/MessagesTimeline.logic.ts", base),
  "utf8",
);
function original(source, name) {
  const start = source.indexOf(`export function ${name}(`);
  if (start < 0) throw new Error(`missing ${name}`);
  const tail = source.slice(start),
    end = /^}\s*$/m.exec(tail);
  if (!end) throw new Error(`missing end ${name}`);
  return stripTypeScriptTypes(tail.slice(0, end.index + 1).replace(/^export /, ""));
}
const helpers = new Function(
  "const TIMELINE_FOLLOW_REARM_THRESHOLD_PX=40;" +
    [
      "observeTimelineRun",
      "getRowBottom",
      "timelineContentOverflowsViewport",
      "getAnchoredTurnMetrics",
    ]
      .map((name) => original(anchoring, name))
      .join("\n") +
    original(work, "resolveWorkGroupScrollAnchor") +
    original(logic, "resolveTimelineIsAtEnd") +
    ";return {observeTimelineRun,getRowBottom,timelineContentOverflowsViewport,getAnchoredTurnMetrics,resolveWorkGroupScrollAnchor,resolveTimelineIsAtEnd};",
)();
const number = (value) =>
  value === "NaN"
    ? NaN
    : value === "Infinity"
      ? Infinity
      : value === "-Infinity"
        ? -Infinity
        : value;
const rows = [];
for (const previous of [
  null,
  ...[null, "a:thread", "b:thread"].flatMap((threadKey) =>
    [false, true].flatMap((hydrated) =>
      [null, "run1", "run2"].map((runId) => ({ threadKey, hydrated, runId })),
    ),
  ),
])
  for (const threadKey of [null, "a:thread", "b:thread"])
    for (const hydrated of [false, true])
      for (const runId of [null, "run1", "run2"])
        for (const queued of [false, true])
          for (const messageId of [null, "message"]) {
            const input = { threadKey, hydrated, runId, queued, messageId };
            rows.push({
              kind: "observe",
              previous,
              input,
              expected: helpers.observeTimelineRun(previous, input),
            });
          }
const measurements = [
  { positions: [], sizes: [], scroll: 0, scrollLength: 700 },
  { positions: [0, 120], sizes: [80, 40], scroll: 0, scrollLength: 700 },
  { positions: [0, 300, 460], sizes: [240, 80, 140], scroll: 0, scrollLength: 760 },
  { positions: [0, 1720, 1880], sizes: [1600, 80, 120], scroll: 1900, scrollLength: 760 },
  { positions: [0, 900, 1180], sizes: [800, 220, 360], scroll: 900, scrollLength: 760 },
  { positions: [0, 200], sizes: [200, 400], scroll: 0, scrollLength: 0 },
  { positions: [0, 200], sizes: [200, "NaN"], scroll: 0, scrollLength: 700 },
  { positions: [0, null], sizes: [200, 400], scroll: 0, scrollLength: 700 },
  { positions: [0, "Infinity"], sizes: [200, 400], scroll: 0, scrollLength: 700 },
  { positions: [0, 200], sizes: [200, "Infinity"], scroll: 0, scrollLength: 700 },
  { positions: [0, 200], sizes: [-20, 0], scroll: -30, scrollLength: 200 },
  { positions: [0, 33, 600], sizes: [33, 567, 90], scroll: 153, scrollLength: 700 },
  { positions: [0, 1e100], sizes: [1, 1e100], scroll: 1e100, scrollLength: 700 },
  { positions: [0, 1e308], sizes: [1, 1e308], scroll: 1e308, scrollLength: 700 },
  { positions: [0, 33, 66], sizes: [33, 33, 33], scroll: "NaN", scrollLength: 700 },
  { positions: [0, 33, 66], sizes: [33, 33, 33], scroll: 66, scrollLength: "Infinity" },
];
for (const measurement of measurements) {
  const state = {
    data: measurement.positions.map((_, index) => ({ id: `row-${index}` })),
    scroll: number(measurement.scroll),
    scrollLength: number(measurement.scrollLength),
    positionAtIndex: (index) => number(measurement.positions[index]),
    sizeAtIndex: (index) => number(measurement.sizes[index]),
  };
  for (const index of [-1, 0, 1, 1.5, 2, 20, "NaN"])
    rows.push({
      kind: "bottom",
      state: measurement,
      index,
      expected: helpers.getRowBottom(state, number(index)),
    });
  for (const composerInset of [0, 100, 900, "NaN"])
    for (const anchorOffset of [0, 24])
      rows.push({
        kind: "overflow",
        state: measurement,
        input: { composerInset, anchorOffset },
        expected: helpers.timelineContentOverflowsViewport(state, {
          composerInset: number(composerInset),
          anchorOffset,
        }),
      });
  for (const anchorIndex of [-20, 0, 1, 1.5, 30, "NaN"])
    for (const composerOverlayHeight of [0, 180, 900, "NaN"])
      for (const anchorOffset of [16, 24])
        rows.push({
          kind: "metrics",
          state: measurement,
          input: { anchorIndex, composerOverlayHeight, anchorOffset },
          expected: helpers.getAnchoredTurnMetrics({
            state,
            anchorIndex: number(anchorIndex),
            composerOverlayHeight: number(composerOverlayHeight),
            anchorOffset,
          }),
        });
  rows.push({
    kind: "anchor",
    state: measurement,
    expected: helpers.resolveWorkGroupScrollAnchor(state) ?? null,
  });
}
for (const scroll of [-20, 0, 5, 40, 165, 302, 599, 600, 1e100, "Infinity", "NaN"]) {
  const positions = Array.from({ length: 18 }, (_, index) => index * 33),
    sizes = positions.map(() => 33);
  const state = {
    data: positions.map((_, index) => ({ id: `row-${index}` })),
    scroll: number(scroll),
    positionAtIndex: (index) => positions[index],
  };
  rows.push({
    kind: "anchor",
    state: { positions, sizes, scroll, scrollLength: 700 },
    expected: helpers.resolveWorkGroupScrollAnchor(state) ?? null,
  });
}
for (const isAtEnd of [undefined, false, true])
  for (const isNearEnd of [undefined, false, true])
    for (const contentLength of [undefined, 600, 840, 841, "NaN", "Infinity"])
      for (const scroll of [undefined, 0, 100])
        for (const scrollLength of [undefined, 700]) {
          const input = { isAtEnd, isNearEnd, contentLength, scroll, scrollLength };
          const state = Object.fromEntries(
            Object.entries(input)
              .filter(([, value]) => value !== undefined)
              .map(([key, value]) => [key, number(value)]),
          );
          rows.push({
            kind: "end",
            input,
            expected: helpers.resolveTimelineIsAtEnd(state) ?? null,
          });
        }
rows.push({ kind: "end", input: null, expected: null });
const chatLogic = readFileSync(new URL("apps/web/src/components/ChatView.logic.ts", base), "utf8");
const release = new Function(
  original(chatLogic, "shouldReleaseTimelineAnchorForToolActivity") +
    ";return shouldReleaseTimelineAnchorForToolActivity;",
)();
for (const anchorMessageId of [null, "authored"])
  for (const liveFollowEnabled of [false, true])
    for (const runningTurnId of [null, "running"])
      for (const kind of ["message", "event", "proposed-plan", "work"])
        for (const runId of [null, "old", "running"])
          for (const metadata of [
            {},
            { tone: "info" },
            { tone: "tool" },
            { itemType: "command_execution" },
            { itemType: null },
            { requestKind: "approval" },
            { requestKind: null },
            { command: "" },
            { command: " \t" },
            { command: "\ufeff" },
            { command: "\u0085" },
            { command: "pwd" },
          ]) {
            const input = {
              anchorMessageId,
              liveFollowEnabled,
              runningTurnId,
              timelineEntries: [{ kind, entry: { runId, ...metadata } }],
            };
            rows.push({ kind: "release", input, expected: release(input) });
          }
writeFileSync(
  new URL("../crates/client/tests/fixtures/timeline-scroll.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${rows.length} unchanged original scrolling cases\n`);
