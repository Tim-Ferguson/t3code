// Runs the original's usage logic over fixed inputs, for `tests/usage_parity.rs`.
//   mergeUsage, isModelCostUnknown                          packages/shared/src/usageMerge.ts
//   formatUsd .. makeWindow                                  packages/shared/src/usageFormat.ts
//   providersWithLimits .. collectProviderUsageLimits        packages/shared/src/usageLimits.ts
//   modelShare .. speedCostSegments                          apps/web/src/components/usage/usageBreakdown.ts
//   providersWithUsage                                       apps/web/src/components/usage/usageProviders.ts
//   buildPeriodColumns, niceScale, smoothCurve, curvePath    apps/web/src/components/usage/UsageProviderChart.tsx
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const shared = (name) => import(`../../packages/shared/src/${name}.ts`);
const merge = await shared("usageMerge");
const format = await shared("usageFormat");
const limits = await shared("usageLimits");
const breakdown = await import("../../apps/web/src/components/usage/usageBreakdown.ts");
const providers = await import("../../apps/web/src/components/usage/usageProviders.ts");

const chartSource = readFileSync(
  new URL("../../apps/web/src/components/usage/UsageProviderChart.tsx", import.meta.url),
  "utf8",
);
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const chartBody = [
  "const PROVIDER_ORDER = " + JSON.stringify(providers.PROVIDER_ORDER) + ";",
  cut(chartSource, "function valueFor(", "/** Shape-preserving"),
  cut(chartSource, "/** Shape-preserving", "export function UsageProviderChart("),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-usage-"));
const chartFile = NodePath.join(scratch, "chart.mjs");
writeFileSync(
  chartFile,
  `${stripTypeScriptTypes(chartBody.replace(/\bexport /g, "").replace(/\binterface (\w+) \{/g, "interface $1 {"))}\nexport { buildPeriodColumns, niceScale, smoothCurve, curvePath };\n`,
);
const chart = await import(pathToFileURL(chartFile).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
const emit = (row) => rows.push(row);

// ---- number and text formats ------------------------------------------------------------
const numbers = [
  0,
  1,
  9.5,
  12.345,
  999,
  999.5,
  1000,
  1234.5,
  99999,
  999999,
  1e6,
  1.5e6,
  12345678,
  999e6,
  1e9,
  19.9e9,
  1.2e12,
  5e13,
  0.004,
  0.005,
  0.125,
  0.135,
  2.675,
  1e-7,
  -1234.5,
  -0.5,
  1234567.891,
  0.1 + 0.2,
  100,
  99.995,
  10.005,
];
for (const value of numbers) {
  emit({
    type: "number",
    value,
    usd: format.formatUsd(value),
    count: format.formatCount(value),
    tokens: format.formatTokens(value),
  });
}
for (const share of [0, 0.5, 1, 0.00001, 0.0004, 0.00049, 0.001, 0.123456, 0.9999, 1.5, 0.00099]) {
  emit({
    type: "percent",
    share,
    one: format.formatPercent(share),
    zero: format.formatPercent(share, 0),
    two: format.formatPercent(share, 2),
  });
}
for (const day of ["2026-08-07", "2026-12-31", "2026-01-01", "bad", "2026-13-02"]) {
  emit({ type: "dayShort", day, output: format.formatDayShort(day) });
}
for (const [since, until] of [
  ["2026-10-03", "2026-10-09"],
  ["2026-02-27", "2026-03-02"],
  ["2026-10-09", "2026-10-09"],
  ["2026-10-09", "2026-10-03"],
  ["2024-02-27", "2024-03-01"],
  ["bad", "2026-10-03"],
]) {
  emit({ type: "days", since, until, output: format.enumerateDays(since, until) });
}
for (const [since, until] of [
  ["2026-10-08T12:00:00.000Z", "2026-10-09T12:00:00.000Z"],
  ["2026-10-08T12:00:00.000Z", "2026-10-08T14:30:00.000Z"],
  ["2026-10-09T12:00:00.000Z", "2026-10-08T12:00:00.000Z"],
]) {
  emit({ type: "hours", since, until, output: format.enumerateHourStarts(since, until) });
}
for (const [mismatch, label] of [
  [{ direction: "serverBehind" }, "Laptop"],
  [{ direction: "clientBehind" }, "Box"],
]) {
  emit({
    type: "mismatch",
    direction: mismatch.direction,
    label,
    output: format.formatUsageContractMismatch(label, mismatch),
  });
}

// ---- zones: fixed offsets and a daylight-saving zone ------------------------------------
// `rule` is what the Rust side rebuilds the offset function from: minutes east of UTC, or the
// two United States rules for New York.
const zones = [
  { name: "UTC", rule: { fixed: 0 } },
  { name: "Asia/Kolkata", rule: { fixed: 330 } },
  { name: "Asia/Tokyo", rule: { fixed: 540 } },
  { name: "America/Phoenix", rule: { fixed: -420 } },
  { name: "America/New_York", rule: { us: -300 } },
  { name: "America/Los_Angeles", rule: { us: -480 } },
  { name: "Europe/Berlin", rule: { eu: 60 } },
];
const instants = [
  "2026-08-11T14:37:00Z",
  "2026-01-05T00:00:00Z",
  "2026-10-09T23:59:00Z",
  "2026-11-01T04:00:00Z",
  "2026-11-01T05:00:00Z",
  "2026-11-01T06:00:00Z",
  "2026-11-01T07:00:00Z",
  "2026-03-08T06:30:00Z",
  "2026-03-08T07:30:00Z",
  "2026-10-25T00:30:00Z",
  "2026-10-25T01:30:00Z",
  "2026-10-25T02:30:00Z",
  "2026-10-04T23:00:00Z",
];
for (const zone of zones) {
  for (const instant of instants) {
    emit({
      type: "instant",
      zone: zone.name,
      rule: zone.rule,
      instant,
      hour: format.formatHourShort(instant, zone.name),
      dateTime: format.formatDateTimeShort(instant, zone.name),
      relative: ["2026-11-01T18:00:00Z", "2026-11-02T18:00:00Z", "2026-11-04T18:00:00Z"].map(
        (reference) => format.formatRelativeHourShort(instant, reference, zone.name),
      ),
    });
  }
}
emit({
  type: "instant",
  zone: "UTC",
  rule: { fixed: 0 },
  instant: "garbage",
  hour: format.formatHourShort("garbage", "UTC"),
  dateTime: format.formatDateTimeShort("garbage", "UTC"),
  relative: [format.formatRelativeHourShort("garbage", "2026-11-01T18:00:00Z", "UTC")],
});

// ---- windows ---------------------------------------------------------------------------
// `makeWindow` reads the machine's zone, so the zone is pinned through the process: the Rust
// function is given the zone and the instant, the original is given the same two.
for (const zone of zones) {
  for (const now of [
    "2026-10-09T12:00:00Z",
    "2026-03-08T08:30:12Z",
    "2026-11-01T05:30:00Z",
    "2026-01-01T03:00:00Z",
    "2026-12-31T23:59:59Z",
  ]) {
    for (const [days, resolution] of [
      [1, "hour"],
      [1, "day"],
      [7, "day"],
      [30, "day"],
      [90, "day"],
    ]) {
      process.env.TZ = zone.name;
      const output = format.makeWindow(days, new Date(now), resolution);
      emit({ type: "window", zone: zone.name, rule: zone.rule, now, days, resolution, output });
    }
  }
}

// ---- merging ---------------------------------------------------------------------------
const totals = (u = 0, c = 0, w = 0, o = 0, r = 0) => ({
  uncachedInputTokens: u,
  cachedInputTokens: c,
  cacheCreationTokens: w,
  outputTokens: o,
  reasoningTokens: r,
});
const bucket = (day, provider, model, overrides = {}) => ({
  day,
  provider,
  model,
  totals: totals(1000, 5000, 200, 300, 50),
  costUsd: 0.42,
  cacheSavingsUsd: 0.1,
  costSource: "modelPriced",
  records: 10,
  unpricedRecords: 0,
  sessions: 2,
  ...overrides,
});
const source = (provider, path, overrides = {}) => ({
  fingerprint: { hostId: "host-a", provider, resolvedHomePath: path, volumeId: "1:2" },
  status: "ok",
  scannedFiles: 3,
  skippedFiles: 0,
  malformedRecords: 0,
  distinctSessions: 4,
  message: null,
  ...overrides,
});
const pricing = {
  status: "fresh",
  source: "litellm",
  fetchedAt: "2026-10-09T00:00:00Z",
  knownModels: 120,
};
const summary = (overrides) => ({
  contractVersion: 6,
  readAt: "2026-10-09T12:00:00.000Z",
  timeZone: "UTC",
  sinceDay: "2026-10-03",
  untilDay: "2026-10-09",
  buckets: [],
  sources: [],
  pricing,
  scanDurationMs: 40,
  ...overrides,
});
const claudeHome = "/home/a/.claude";
const codexHome = "/home/a/.codex";
const mixed = [
  bucket("2026-10-08", "claude", "claude-sonnet-4-6", {
    sourcePath: claudeHome,
    categoryCostUsd: { input: 0.01, cacheRead: 0.02, cacheWrite: 0.03, output: 0.3 },
    fastCostUsd: 0.1,
    speedPremiumUsd: 0.04,
  }),
  bucket("2026-10-08", "claude", "claude-opus-4-7", {
    sourcePath: claudeHome,
    costUsd: 1.5,
    totals: totals(10, 20, 30, 400000, 1000),
    records: 4,
  }),
  bucket("2026-10-09", "claude", "claude-sonnet-4-6", {
    sourcePath: claudeHome,
    costSource: "providerReported",
    costUsd: 0.8,
    records: 3,
  }),
  bucket("2026-10-09", "codex", "gpt-5", {
    sourcePath: codexHome,
    costUsd: 0,
    costSource: "unpriced",
    unpricedRecords: 10,
    totals: totals(9000, 1000, 0, 2000, 500),
  }),
  bucket("2026-10-09", "codex", "gpt-5", {
    sourcePath: codexHome,
    hourStart: "2026-10-09T10:00:00.000Z",
    costUsd: 0.2,
    unpricedRecords: 3,
    records: 6,
  }),
  bucket("2026-10-09", "codex", "gpt-5-mini", {
    sourcePath: codexHome,
    hourStart: "2026-10-09T11:00:00.000Z",
    costUsd: 0.05,
    ultrafastCostUsd: 0.03,
    speedPremiumUsd: 0.01,
  }),
  bucket("2026-10-07", "claude", "claude-haiku-4-5", {
    sourcePath: claudeHome,
    costUsd: 0,
    records: 0,
    unpricedRecords: 0,
    totals: totals(0, 0, 0, 0, 0),
  }),
];
const mixedSources = [
  source("claude", claudeHome),
  source("codex", codexHome, { distinctSessions: 7 }),
];
const env = (environmentId, label, overrides) => ({
  environmentId,
  label,
  summary: summary(overrides),
});
const scenarios = [
  { name: "empty", environments: [] },
  {
    name: "one environment",
    environments: [env("env-a", "Laptop", { buckets: mixed, sources: mixedSources })],
  },
  {
    name: "two machines",
    environments: [
      env("env-a", "Laptop", { buckets: mixed, sources: mixedSources }),
      env("env-b", "Desktop", {
        buckets: [
          bucket("2026-10-09", "claude", "claude-sonnet-4-6", {
            sourcePath: "/home/b/.claude",
            costUsd: 3,
          }),
        ],
        sources: [
          {
            ...source("claude", "/home/b/.claude"),
            fingerprint: {
              hostId: "host-b",
              provider: "claude",
              resolvedHomePath: "/home/b/.claude",
              volumeId: "9:9",
            },
          },
        ],
      }),
    ],
  },
  {
    name: "same transcript directory twice",
    environments: [
      env("env-a", "Main", {
        readAt: "2026-10-09T12:00:00.000Z",
        buckets: mixed,
        sources: mixedSources,
      }),
      env("env-b", "Worktree", {
        readAt: "2026-10-09T12:05:00.000Z",
        buckets: mixed,
        sources: mixedSources,
      }),
    ],
  },
  {
    name: "a newer partial scan adds cells",
    environments: [
      env("env-a", "Main", {
        readAt: "2026-10-09T12:00:00.000Z",
        buckets: mixed,
        sources: mixedSources,
      }),
      env("env-b", "Worktree", {
        readAt: "2026-10-09T13:00:00.000Z",
        buckets: [
          ...mixed.slice(0, 2),
          bucket("2026-10-09", "claude", "claude-opus-4-7", {
            sourcePath: claudeHome,
            costUsd: 9,
            records: 9,
          }),
        ],
        sources: [source("claude", claudeHome, { status: "partial", distinctSessions: 11 })],
      }),
    ],
  },
  {
    name: "an older server and a newer one",
    environments: [
      env("env-a", "Fresh", { buckets: mixed, sources: mixedSources }),
      env("env-b", "Old", { contractVersion: 3, buckets: mixed, sources: mixedSources }),
      env("env-c", "Ahead", { contractVersion: 7, buckets: mixed, sources: mixedSources }),
      env("env-d", "V4", {
        contractVersion: 4,
        buckets: [bucket("2026-10-05", "codex", "gpt-5", { costUsd: 2 })],
        sources: [source("codex", "/home/d/.codex")],
      }),
    ],
  },
  {
    name: "buckets without a source path",
    environments: [
      env("env-a", "Legacy", {
        buckets: [
          bucket("2026-10-08", "claude", "claude-sonnet-4-6"),
          bucket("2026-10-09", "claude", "claude-sonnet-4-6", { costUsd: 1.25 }),
        ],
        sources: [
          source("claude", claudeHome),
          { ...source("claude", "/missing"), status: "missing", distinctSessions: 0 },
        ],
      }),
    ],
  },
];
for (const scenario of scenarios) {
  const merged = merge.mergeUsage(scenario.environments, 6);
  const mapped = JSON.parse(
    JSON.stringify(merged, (key, value) =>
      value instanceof Map ? Object.fromEntries(value) : value,
    ),
  );
  emit({
    type: "merge",
    name: scenario.name,
    environments: scenario.environments,
    expected: mapped,
    unknown: merged.models.map((model) => merge.isModelCostUnknown(model)),
  });
}
emit({
  type: "compatible",
  checks: [3, 4, 5, 6, 7].map((version) => [
    version,
    merge.isCompatibleUsageContractVersion(version, 6),
  ]),
});

// ---- breakdown and chart data -----------------------------------------------------------
{
  const merged = merge.mergeUsage(scenarios[1].environments, 6);
  const models = merged.models;
  emit({
    type: "breakdown",
    models: JSON.parse(JSON.stringify(models)),
    byTokens: breakdown.sortModelsByTokens(models).map((model) => model.model),
    shares: models.map((model) => [
      breakdown.modelShare(model, "cost"),
      breakdown.modelShare(model, "tokens"),
    ]),
    hit: models.map((model) => breakdown.cacheHitRate(model)),
    perMillion: models.map((model) => breakdown.costPerMillionTokens(model)),
    costSegments: breakdown.costTypeSegments(merged.categoryCost),
    tokenSegments: breakdown.tokenTypeSegments(merged),
    speedSegments: breakdown.speedCostSegments(merged.speedCost),
    withUsage: providers.providersWithUsage(merged.providers),
  });
  const days = format.enumerateDays("2026-10-03", "2026-10-09");
  const hours = format.enumerateHourStarts("2026-10-09T09:00:00.000Z", "2026-10-09T13:00:00.000Z");
  const byDay = new Map(merged.daily.map((entry) => [entry.day, entry]));
  const byHour = new Map(merged.hourly.map((entry) => [entry.hourStart, entry]));
  for (const metric of ["cost", "tokens"]) {
    emit({
      type: "columns",
      resolution: "day",
      metric,
      periods: days,
      daily: merged.daily.map((d) => ({ ...d, byProvider: Object.fromEntries(d.byProvider) })),
      expected: chart.buildPeriodColumns(days, byDay, metric),
    });
    emit({
      type: "columns",
      resolution: "hour",
      metric,
      periods: hours,
      hourly: merged.hourly.map((d) => ({ ...d, byProvider: Object.fromEntries(d.byProvider) })),
      expected: chart.buildPeriodColumns(hours, byHour, metric),
    });
  }
}
for (const peak of [
  0, -1, 0.004, 0.37, 1, 1.9, 2, 2.1, 4.9, 5, 5.1, 9.99, 10, 123.4, 4_500, 1.2e6, 3.4e9,
]) {
  emit({ type: "scale", peak, expected: chart.niceScale(peak, 4) });
}
for (const ys of [
  [100],
  [100, 50],
  [260, 260, 260],
  [260, 100, 200, 10, 10, 120],
  [0, 260, 0],
  [5, 4, 3, 2, 1],
  [1, 1, 5, 9, 9, 2],
]) {
  const step = ys.length === 1 ? 0 : 960 / (ys.length - 1);
  const points = ys.map((y, index) => ({ x: index * step, y }));
  emit({ type: "curve", points, path: chart.curvePath(chart.smoothCurve(points)) });
}

// ---- limits ----------------------------------------------------------------------------
const NOW = Date.parse("2026-10-09T12:00:00Z");
const win = (id, kind, label, usedPercent, resetsAt, mins) => ({
  id,
  kind,
  label,
  usedPercent,
  ...(resetsAt ? { resetsAt } : {}),
  ...(mins ? { windowDurationMins: mins } : {}),
});
const inHours = (hours) => new Date(NOW + hours * 3_600_000).toISOString();
const limitsOf = (windows, extra = {}) => ({
  checkedAt: "2026-10-09T11:58:00.000Z",
  windows,
  ...extra,
});
const provider = (instanceId, driver, extra = {}) => ({
  instanceId,
  driver,
  enabled: true,
  installed: true,
  version: "1.0.0",
  status: "ready",
  auth: { status: "authenticated", type: "oauth", email: "me@example.com", label: "Max" },
  checkedAt: "2026-10-09T11:58:00.000Z",
  models: [],
  slashCommands: [],
  skills: [],
  ...extra,
});
const claudeWindows = [
  win("five_hour", "session", "5 hour", 42, inHours(2.2), 300),
  win("seven_day", "weekly", "Weekly", 71, inHours(80), 10080),
  win("seven_day_opus", "weekly", "Weekly Opus", 12, inHours(80), 10080),
];
const limitCases = [
  {
    name: "one claude account with credits",
    providers: [
      provider("claudeAgent", "claudeAgent", {
        usageLimits: limitsOf(claudeWindows, {
          resetCredits: {
            availableCount: 2,
            nextExpiresAt: inHours(24 * 20 + 5),
            nextCreditId: "g_1",
          },
        }),
      }),
    ],
    sources: [],
  },
  {
    name: "two instances of one account and a codex plan",
    providers: [
      provider("claudeAgent", "claudeAgent", {
        displayName: "Personal",
        usageLimits: limitsOf(claudeWindows),
      }),
      provider("claude_work", "claudeAgent", {
        displayName: "Work",
        usageLimits: limitsOf(
          claudeWindows.map((w) => ({ ...w, usedPercent: w.usedPercent / 2 })),
          {
            checkedAt: "2026-10-09T11:59:30.000Z",
            resetCredits: { availableCount: 1, nextCreditId: "g_2" },
          },
        ),
      }),
      provider("codex", "codex", {
        auth: {
          status: "authenticated",
          type: "chatgpt",
          email: "codex@example.com",
          label: "Pro",
        },
        usageLimits: limitsOf([
          win("primary", "session", "5 hour", 90, inHours(1), 300),
          win("secondary", "weekly", "Weekly", 10, inHours(100), 10080),
        ]),
      }),
    ],
    sources: [],
  },
  {
    name: "two accounts of one provider, ordered by reset",
    providers: [
      provider("a", "claudeAgent", {
        auth: { status: "authenticated", email: "zed@example.com" },
        usageLimits: limitsOf([win("five_hour", "session", "5 hour", 10, inHours(4), 300)]),
      }),
      provider("b", "claudeAgent", {
        auth: { status: "authenticated", email: "amy@example.com" },
        usageLimits: limitsOf([
          win("five_hour", "session", "5 hour", 80, inHours(1), 300),
          win("seven_day", "weekly", "Weekly", 20, inHours(70), 10080),
        ]),
      }),
    ],
    sources: [],
  },
  {
    name: "failures and accounts that cannot report",
    providers: [
      provider("api", "claudeAgent", {
        auth: { status: "authenticated" },
        usageLimits: limitsOf([], {
          unavailable: { reason: "unsupported", message: "API keys have no subscription limits." },
        }),
      }),
      provider("flaky", "codex", {
        usageLimits: limitsOf([], {
          unavailable: { reason: "probeFailed", message: "Could not reach the usage endpoint." },
        }),
      }),
      provider("empty", "cursor", { usageLimits: limitsOf([]) }),
      provider("off", "claudeAgent", { enabled: false, usageLimits: limitsOf(claudeWindows) }),
      provider("gone", "claudeAgent", {
        availability: "unavailable",
        usageLimits: limitsOf(claudeWindows),
      }),
      provider("none", "opencode", {}),
      provider("sharing", "codex", {
        auth: { status: "authenticated", subscriptionSharing: true },
        usageLimits: limitsOf([], {
          externalUsage: { label: "ChatGPT usage", url: "https://chatgpt.com/#settings/Usage" },
          unavailable: { reason: "unsupported" },
        }),
      }),
    ],
    sources: [],
  },
  {
    name: "cursor pools",
    providers: [
      provider("cursor", "cursor", {
        auth: { status: "authenticated", email: "c@example.com" },
        usageLimits: limitsOf([
          win("totalPercentUsed", "monthly", "Overall", 40, inHours(300)),
          win("autoPercentUsed", "monthly", "Auto", 10, inHours(300)),
          win("apiPercentUsed", "monthly", "API", 70, inHours(300)),
        ]),
      }),
    ],
    sources: [],
  },
  {
    name: "a proxy source with accounts and a failing one",
    providers: [
      provider("claudeAgent", "claudeAgent", {
        auth: { status: "authenticated", email: "me@example.com" },
        usageLimits: limitsOf(claudeWindows.slice(0, 1)),
      }),
    ],
    sources: [
      {
        id: "proxy",
        kind: "cliproxy",
        label: "Proxy",
        checkedAt: "2026-10-09T11:59:00.000Z",
        accounts: [
          {
            id: "me.json",
            driver: "claudeAgent",
            email: "me@example.com",
            plan: "Max",
            usageLimits: limitsOf(claudeWindows.slice(0, 1), {
              checkedAt: "2026-10-09T11:59:00.000Z",
              resetCredits: { availableCount: 1, nextCreditId: "hub_1" },
            }),
          },
          {
            id: "other.json",
            driver: "claudeAgent",
            plan: "Pro",
            usageLimits: limitsOf([win("five_hour", "session", "5 hour", 5, inHours(3), 300)]),
          },
        ],
      },
      {
        id: "broken",
        kind: "cliproxy",
        label: "Broken",
        checkedAt: "2026-10-09T11:00:00.000Z",
        accounts: [],
        error: "Connection refused",
      },
      {
        id: "empty",
        kind: "cliproxy",
        label: "Quiet",
        checkedAt: "2026-10-09T11:00:00.000Z",
        accounts: [],
      },
    ],
  },
];
const presentation = (label, entry) => ({ entry: { target: { label } }, serverConfig: entry });
const toObject = (value) => JSON.parse(JSON.stringify(value));
for (const item of limitCases) {
  for (const environments of [1, 2]) {
    const presentations = new Map([
      [
        "env-1",
        presentation("Laptop", { providers: item.providers, usageLimitSources: item.sources }),
      ],
    ]);
    if (environments === 2)
      presentations.set(
        "env-2",
        presentation("Desktop", { providers: item.providers.slice(0, 1), usageLimitSources: [] }),
      );
    const accounts = limits.collectLimitAccounts(presentations);
    const pools = limits.collectLimitPools(accounts, NOW);
    emit({
      type: "limits",
      name: item.name,
      environments: [...presentations].map(([id, value]) => ({
        id,
        label: value.entry.target.label,
        providers: value.serverConfig.providers,
        usageLimitSources: value.serverConfig.usageLimitSources,
      })),
      now: NOW,
      accounts: toObject(accounts),
      pools: toObject(pools),
      displayed: pools.map((pool) =>
        limits
          .displayLimitWindows({ ...pool, windows: [...pool.windows] })
          .map((window) => `${window.kind}:${window.id}`),
      ),
      notices: limits.collectLimitNotices(presentations),
      links: toObject(limits.collectExternalUsageLinks(presentations)),
      withLimits: limits.providersWithLimits(item.providers).map((p) => p.instanceId),
      hasLimits: ["claudeAgent", "codex", "cursor", "opencode"].map((driver) => [
        driver,
        limits.hasProviderUsageLimits(driver, item.providers, item.sources),
      ]),
      reports: item.providers.map((p) => [
        p.instanceId,
        toObject(
          limits.collectProviderUsageLimits(p.instanceId, item.providers, item.sources, NOW),
        ),
      ]),
      sharing: item.providers.map((p) => limits.usesChatGptSharing(p)),
    });
  }
}
for (const window of [
  win("a", "session", "5 hour", 0, inHours(5), 300),
  win("b", "session", "5 hour", 50, inHours(2.5), 300),
  win("c", "weekly", "Weekly", 100, inHours(0.5), 300),
  win("d", "weekly", "Weekly", 33.4, undefined, 300),
  win("e", "weekly", "Weekly", 20, inHours(2), undefined),
  win("f", "weekly", "Weekly", 20, inHours(-1), 300),
  win("g", "other", "Other", 60, inHours(7), 300),
  win("h", "other", "Other", 12.5, inHours(2.4), 0),
  win("i", "monthly", "Monthly", 80, "2026-10-09T12:00:40Z", 60),
  win("j", "monthly", "Monthly", 80, "garbage", 60),
]) {
  emit({
    type: "window-math",
    window,
    now: NOW,
    remaining: limits.remainingPercent(window),
    elapsed: limits.elapsedShare(window, NOW),
    pace: limits.paceOf(window, NOW),
    resetsIn: limits.formatResetsIn(window, NOW),
  });
}
for (const ms of [
  -5,
  0,
  59_999,
  60_000,
  3_599_000,
  3_600_000,
  7_980_000,
  86_399_000,
  86_400_000,
  90_000_000,
  345_600_000 + 14_400_000,
]) {
  emit({ type: "duration", ms, output: limits.formatDuration(ms) });
}
for (const prompt of [
  "/usage-limits",
  "  /Usage-Limits  ",
  "/usage-limits now",
  "usage-limits",
  "",
]) {
  emit({ type: "command", prompt, output: limits.isUsageLimitsCommand(prompt) });
}
for (const [a, b] of [
  [[], []],
  [[{ accounts: [{ driver: "codex" }] }], [{ accounts: [{ driver: "codex" }] }]],
  [[{ accounts: [{ driver: "codex" }] }], [{ accounts: [{ driver: "claudeAgent" }] }]],
  [[{ accounts: [], error: "x" }], [{ accounts: [], error: "y" }]],
]) {
  emit({
    type: "coverage",
    previous: a,
    next: b,
    output: limits.sameUsageLimitCommandCoverage(a, b),
  });
}

// ---- the run a usage limit stopped -----------------------------------------------------
{
  const DateTime = await import(
    new URL("../../packages/shared/node_modules/effect/dist/DateTime.js", import.meta.url).href
  );
  const thread = await shared("orchestrationV2ThreadError");
  const at = (iso) => DateTime.makeUnsafe(iso);
  const run = (
    id,
    ordinal,
    status,
    completedAt,
    startedAt = "2026-10-09T10:00:00.000Z",
    extra = {},
  ) => ({ id, ordinal, status, rootNodeId: `${id}-root`, startedAt, completedAt, ...extra });
  const failure = (runId, id, cls, resetAt, over = {}) => ({
    id,
    type: "error",
    status: "failed",
    runId,
    nodeId: `${runId}-root`,
    ordinal: 3,
    updatedAt: "2026-10-09T10:01:00.000Z",
    failure: { class: cls, message: "stopped", ...(resetAt ? { resetAt } : {}) },
    ...over,
  });
  const cases = [
    { name: "no runs", runs: [], items: [] },
    {
      name: "a limited run",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z")],
      items: [failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z")],
    },
    {
      name: "a limit without a reset time",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z")],
      items: [failure("r1", "e1", "usage_limit")],
    },
    {
      name: "another failure",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z")],
      items: [failure("r1", "e1", "provider_error")],
    },
    {
      name: "a completed run after the limit",
      runs: [
        run("r1", 1, "failed", "2026-10-09T10:01:00.000Z"),
        run("r2", 2, "completed", "2026-10-09T10:05:00.000Z"),
      ],
      items: [failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z")],
    },
    {
      name: "queued and cancelled runs after the limit",
      runs: [
        run("r1", 1, "failed", "2026-10-09T10:01:00.000Z"),
        run("r2", 2, "queued", null, null),
        run("r3", 3, "cancelled", "2026-10-09T10:02:00.000Z", null),
      ],
      items: [failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z")],
    },
    {
      name: "the newest of two failures decides",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z")],
      items: [
        failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z"),
        failure("r1", "e2", "provider_error", null, { updatedAt: "2026-10-09T10:02:00.000Z" }),
      ],
    },
    {
      name: "a failure of a child node",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z")],
      items: [failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z", { nodeId: "child" })],
    },
    {
      name: "a run that is still going",
      runs: [run("r1", 1, "failed", "2026-10-09T10:01:00.000Z"), run("r2", 2, "running", null)],
      items: [failure("r1", "e1", "usage_limit", "2026-10-09T15:00:00.000Z")],
    },
  ];
  for (const item of cases) {
    const decoded = {
      runs: item.runs.map((r) => ({ ...r, completedAt: r.completedAt ? at(r.completedAt) : null })),
      items: item.items.map((i) => ({ ...i, updatedAt: at(i.updatedAt) })),
    };
    const blocked = thread.usageLimitBlockedRun(decoded.runs, decoded.items, null);
    const run = blocked === null ? null : blocked.id;
    const failureOf =
      blocked === null ? null : thread.latestRootProviderFailure(blocked, decoded.items);
    emit({
      type: "blocked",
      name: item.name,
      runs: item.runs,
      items: item.items,
      run,
      resetAt: failureOf?.resetAt ?? null,
      presented:
        thread.usageLimitRunPresentedAsLatest(decoded.runs, decoded.items, null)?.id ?? null,
    });
  }
}

writeFileSync(
  new URL("../crates/client/tests/fixtures/usage.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
