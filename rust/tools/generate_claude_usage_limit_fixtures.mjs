// Executes the unchanged Claude usage-limit functions (window mapping, sparse updates, probe
// resolution, banked reset credits) and records witnesses for the Rust port. No provider runs.
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Ref from "../../apps/server/node_modules/effect/dist/Ref.js";
import {
  claudeRateLimitEventToUpdate,
  claudeUsageResponseToLimits,
} from "../../apps/server/src/provider/claudeUsageLimits.ts";
import {
  applyUsageLimitsUpdate,
  resolveUsageLimitsAfterProbe,
} from "../../apps/server/src/provider/providerUsageLimits.ts";
import { claudeResetCreditsToContract } from "../../apps/server/src/provider/claudeResetCredits.ts";

const rows = [];
const names = { overageIncluded: "Fable" };
const events = [
  { rateLimitType: "five_hour", utilization: 0.42, resetsAt: 1_800_000_000 },
  { rateLimitType: "seven_day", utilization: 1.5, resetsAt: 1_800_100_000 },
  { rateLimitType: "seven_day", utilization: 0.1 },
  { rateLimitType: "seven_day_overage_included", utilization: 0.5, resetsAt: 1_800_000_000 },
  { rateLimitType: "seven_day_opus", utilization: 0.5, resetsAt: 1_800_000_000 },
  { rateLimitType: "five_hour" },
  { utilization: 0.2 },
  { rateLimitType: "five_hour", utilization: 0.2, resetsAt: -4 },
  { rateLimitType: "five_hour", utilization: 0.2, resetsAt: 9e18 },
];
for (const info of events) {
  rows.push({
    operation: "event",
    info,
    names,
    output: claudeRateLimitEventToUpdate(info, names) ?? null,
  });
  rows.push({
    operation: "event",
    info,
    names: { overageIncluded: null },
    output: claudeRateLimitEventToUpdate(info, { overageIncluded: undefined }) ?? null,
  });
}
const responses = [
  { rate_limits_available: false },
  { rate_limits_available: true },
  {
    rate_limits_available: true,
    rate_limits: {
      five_hour: { utilization: 12, resets_at: "2027-01-01T05:00:00Z" },
      seven_day: { utilization: 130, resets_at: null },
      model_scoped: [
        { display_name: "Fable 5", utilization: 33, resets_at: "2027-01-05T00:00:00.000Z" },
        { display_name: "Skipped", utilization: null, resets_at: null },
        { display_name: "Second", utilization: 1, resets_at: "bad" },
        { nope: 1 },
      ],
    },
  },
  { rate_limits_available: true, rate_limits: { seven_day: { utilization: "x" } } },
];
const checkedAt = "2026-10-09T12:00:00.000Z";
for (const response of responses) {
  const { limits, names } = claudeUsageResponseToLimits({ response, checkedAt });
  rows.push({
    operation: "response",
    response,
    output: limits,
    names: { overageIncluded: names.overageIncluded ?? null },
  });
}
const previous = claudeUsageResponseToLimits({ response: responses[2], checkedAt }).limits;
const updates = [
  { windows: [] },
  {
    windows: [
      {
        id: "five_hour",
        kind: "session",
        label: "Session",
        usedPercent: 12,
        windowDurationMins: 300,
      },
    ],
  },
  { windows: [{ id: "five_hour", kind: "session", label: "Session", usedPercent: 80 }] },
  {
    windows: [
      {
        id: "seven_day_new",
        kind: "weekly",
        label: "Weekly · New",
        usedPercent: 140,
        resetsAt: "2027-02-01T00:00:00.000Z",
      },
    ],
  },
];
const later = "2026-10-09T12:05:00.000Z";
for (const base of [
  undefined,
  previous,
  { ...previous, resetCredits: { availableCount: 2, nextCreditId: "g1" } },
  { checkedAt, windows: [], unavailable: { reason: "unsupported" } },
  { checkedAt, windows: [], unavailable: { reason: "probeFailed", message: "x" } },
]) {
  for (const update of updates) {
    rows.push({
      operation: "apply",
      previous: base ?? null,
      update,
      checkedAt: later,
      output: applyUsageLimitsUpdate({ previous: base, update, checkedAt: later }) ?? null,
    });
  }
}
const failed = { checkedAt: later, windows: [], unavailable: { reason: "probeFailed" } };
for (const [published, probed] of [
  [previous, failed],
  [undefined, failed],
  [{ checkedAt, windows: [], unavailable: { reason: "unsupported" } }, failed],
  [previous, undefined],
  [previous, previous],
]) {
  rows.push({
    operation: "resolve",
    published: published ?? null,
    probed: probed ?? null,
    output: resolveUsageLimitsAfterProbe({ published, probed }) ?? null,
  });
}
const now = Date.parse("2026-10-09T12:00:00.000Z");
const grant = (extra) => ({
  id: "g_1",
  resets_left: 1,
  usable_now: true,
  ends_at: "2027-01-01T00:00:00Z",
  ...extra,
});
const blocks = [
  null,
  "x",
  { eligible: false },
  { eligible: true },
  { eligible: true, grants: [grant({})], next_grant_id: "g_1" },
  {
    eligible: true,
    grants: [grant({}), grant({ id: "g_2", resets_left: 2, ends_at: null })],
    next_grant_id: "g_2",
  },
  { eligible: true, grants: [grant({ paused: true })], next_grant_id: "g_1" },
  { eligible: true, grants: [grant({ ends_at: "2020-01-01T00:00:00Z" })], next_grant_id: "g_1" },
  { eligible: true, grants: [grant({ ends_at: "2027-02-30T00:00:00Z" })], next_grant_id: "g_1" },
  { eligible: true, grants: [grant({ usable_now: false })], next_grant_id: "g_1" },
  { eligible: true, grants: [grant({ id: "BAD ID" })], next_grant_id: "BAD ID" },
  { eligible: true, grants: [grant({})], next_grant_id: "other" },
  { eligible: true, grants: [grant({ ends_at: "2027-01-01 00:00:00" })], next_grant_id: "g_1" },
];
for (const block of blocks)
  rows.push({
    operation: "credits",
    block,
    nowMs: now,
    output: claudeResetCreditsToContract(block, now) ?? null,
  });
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-usage-limits.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} usage-limit witnesses`);
process.exit(0);
