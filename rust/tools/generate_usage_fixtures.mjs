// Runs the unchanged usage parsers, rate table, pricing and aggregation over fixed inputs and
// records the results for the Rust port. Nothing reads a transcript directory or the network.
import { writeFileSync } from "node:fs";
import {
  initialCodexScanState,
  parseClaudeLine,
  parseCodexLine,
} from "../../apps/server/src/usage/usageTranscripts.ts";
import {
  cacheSavingsUsd,
  createOverrideRateTable,
  lookupRate,
  parseRateTable,
  priceUsage,
} from "../../apps/server/src/usage/usagePricing.ts";
import {
  UsageAggregator,
  resolveModelAliases,
} from "../../apps/server/src/usage/usageAggregation.ts";

const claudeLines = [
  {
    type: "assistant",
    timestamp: "2026-10-08T10:00:00.000Z",
    requestId: "req_1",
    sessionId: "s1",
    message: {
      id: "msg_1",
      model: "claude-sonnet-4-6",
      usage: {
        input_tokens: 10,
        cache_read_input_tokens: 100,
        cache_creation_input_tokens: 50,
        output_tokens: 20,
      },
    },
  },
  {
    type: "assistant",
    timestamp: "2026-10-08T10:00:01.000Z",
    requestId: "req_1",
    sessionId: "s1",
    message: {
      id: "msg_1",
      model: "claude-sonnet-4-6",
      usage: {
        input_tokens: 10,
        cache_read_input_tokens: 100,
        cache_creation_input_tokens: 50,
        output_tokens: 20,
      },
    },
  },
  {
    type: "assistant",
    timestamp: "2026-10-08T23:30:00.000Z",
    sessionId: "s2",
    costUSD: 0.5,
    message: {
      id: "msg_2",
      model: "claude-opus-4-1",
      usage: { input_tokens: 1000, output_tokens: 2000, speed: "fast" },
    },
  },
  {
    type: "assistant",
    timestamp: "2026-10-09T00:30:00.000Z",
    sessionId: "s2",
    message: { model: "<synthetic>", usage: { input_tokens: 5 } },
  },
  {
    type: "assistant",
    timestamp: "2026-10-09T01:00:00.000Z",
    message: { model: "claude-haiku-4-5[1m]", usage: { input_tokens: 7.9, output_tokens: -3 } },
  },
  { type: "assistant", timestamp: "bad", message: { id: "x", model: "m", usage: {} } },
  {
    type: "assistant",
    timestamp: "2026-10-09T01:00:00Z",
    message: { id: "x", model: "", usage: {} },
  },
  { type: "user", timestamp: "2026-10-09T01:00:00Z", message: { usage: {} } },
  { type: "assistant", timestamp: "2026-10-09T01:00:00Z", message: { model: "m" } },
];
const rows = [];
for (const record of claudeLines)
  rows.push({
    operation: "claude",
    line: JSON.stringify(record),
    output: parseClaudeLine(JSON.stringify(record)) ?? null,
  });
rows.push({ operation: "claude", line: "not json", output: parseClaudeLine("not json") ?? null });

const codexLines = [
  {
    type: "token_count",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 5 } } },
    timestamp: "2026-10-08T09:59:00Z",
  },
  { type: "session_meta", timestamp: "2026-10-08T10:00:00Z", payload: { id: "rollout-1" } },
  { type: "session_meta", timestamp: "2026-10-08T10:00:00Z", payload: { id: "other" } },
  { type: "turn_context", timestamp: "2026-10-08T10:00:01Z", payload: { model: "gpt-5" } },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:02Z",
    payload: { type: "thread_settings_applied", thread_settings: { service_tier: "priority" } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:03Z",
    payload: {
      type: "token_count",
      info: {
        last_token_usage: {
          input_tokens: 1000,
          cached_input_tokens: 400,
          cache_write_input_tokens: 100,
          output_tokens: 50,
          reasoning_output_tokens: 80,
        },
      },
    },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:04Z",
    payload: {
      type: "token_count",
      info: {
        last_token_usage: {
          input_tokens: 1000,
          cached_input_tokens: 400,
          cache_write_input_tokens: 100,
          output_tokens: 50,
          reasoning_output_tokens: 80,
        },
      },
    },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:05Z",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 10 } } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:06Z",
    payload: { type: "token_count", info: { last_token_usage: {} } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:07Z",
    payload: { type: "token_count", info: null },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:08Z",
    payload: { type: "thread_settings_applied", thread_settings: { service_tier: "ultrafast" } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:09Z",
    payload: {
      type: "token_count",
      info: { last_token_usage: { input_tokens: 20, output_tokens: 5 } },
    },
  },
];
const state = initialCodexScanState();
for (const record of codexLines) {
  const line = JSON.stringify(record);
  rows.push({
    operation: "codex",
    scenario: "plain",
    line,
    output: parseCodexLine(line, state) ?? null,
  });
}
const fork = initialCodexScanState();
for (const record of [
  {
    type: "session_meta",
    timestamp: "2026-10-08T10:00:00.000Z",
    payload: { id: "child", forked_from_id: "parent" },
  },
  { type: "turn_context", timestamp: "2026-10-08T10:00:00.100Z", payload: { model: "gpt-5" } },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:00.200Z",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 100 } } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:00.500Z",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 200 } } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:07.000Z",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 300 } } },
  },
  {
    type: "event_msg",
    timestamp: "2026-10-08T10:00:08.000Z",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: 400 } } },
  },
]) {
  const line = JSON.stringify(record);
  rows.push({
    operation: "codex",
    scenario: "fork",
    line,
    output: parseCodexLine(line, fork) ?? null,
  });
}

const document = {
  "claude-sonnet-4-6": {
    input_cost_per_token: 3e-6,
    output_cost_per_token: 15e-6,
    cache_read_input_token_cost: 3e-7,
    cache_creation_input_token_cost: 3.75e-6,
  },
  "anthropic/claude-opus-4-1": {
    input_cost_per_token: 15e-6,
    output_cost_per_token: 75e-6,
    provider_specific_entry: { fast: 2 },
  },
  "bedrock/claude-haiku-4-5": { input_cost_per_token: 1e-6, output_cost_per_token: 5e-6 },
  "vertex/claude-haiku-4-5": { input_cost_per_token: 1e-6, output_cost_per_token: 5e-6 },
  "gpt-5": {
    input_cost_per_token: 1.25e-6,
    output_cost_per_token: 10e-6,
    cache_read_input_token_cost: 1.25e-7,
    input_cost_per_token_priority: 2.5e-6,
    output_cost_per_token_priority: 20e-6,
    input_cost_per_token_ultrafast: 5e-6,
    output_cost_per_token_ultrafast: 40e-6,
  },
  "openai/dual": { input_cost_per_token: 1e-6, output_cost_per_token: 2e-6 },
  "azure/dual": { input_cost_per_token: 9e-6, output_cost_per_token: 2e-6 },
  "half-priced": { input_cost_per_token: 1e-6 },
  "not-an-object": 4,
  "  ": { input_cost_per_token: 1, output_cost_per_token: 1 },
};
const table = parseRateTable(document);
rows.push({
  operation: "table",
  document,
  output: [...table.entries()].sort(([a], [b]) => (a < b ? -1 : 1)),
});
const overrides = {
  "Custom/Model-X": { inputCostPerMillionTokens: 2, outputCostPerMillionTokens: 8 },
  cached: {
    inputCostPerMillionTokens: 1,
    outputCostPerMillionTokens: 2,
    cacheReadCostPerMillionTokens: 0.1,
    cacheWriteCostPerMillionTokens: 1.5,
  },
};
const overrideTable = createOverrideRateTable(overrides);
rows.push({ operation: "overrides", overrides, output: [...overrideTable.entries()] });
const totals = (u, c, w, o, r = 0) => ({
  uncachedInputTokens: u,
  cachedInputTokens: c,
  cacheCreationTokens: w,
  outputTokens: o,
  reasoningTokens: r,
});
const cases = [
  ["claude-sonnet-4-6", totals(10, 100, 50, 20), null, "standard"],
  ["claude-sonnet-4-6", totals(10, 100, 50, 20), 0.5, "standard"],
  ["claude-sonnet-4-6", totals(0, 0, 0, 0), 0.5, "standard"],
  ["claude-opus-4-1", totals(1000, 0, 0, 2000), null, "fast"],
  ["claude-opus-4-1", totals(1000, 0, 0, 2000), 0.5, "fast"],
  ["gpt-5", totals(500, 400, 100, 50, 80), null, "fast"],
  ["gpt-5", totals(500, 400, 100, 50, 80), null, "ultrafast"],
  ["gpt-5", totals(1, 1, 1, 1), null, "standard"],
  ["unknown-model", totals(1, 1, 1, 1), null, "standard"],
  ["unknown-model", totals(1, 1, 1, 1), 1.25, "standard"],
  ["<synthetic>", totals(1, 1, 1, 1), 2, "standard"],
  ["opus", totals(1, 1, 1, 1), null, "standard"],
  ["claude-haiku-4-5[1m]", totals(10, 0, 0, 10), null, "standard"],
  ["CLAUDE-SONNET-4-6", totals(1, 2, 3, 4), null, "standard"],
  ["Custom/Model-X", totals(1000, 0, 0, 1000), 9, "standard"],
  ["cached", totals(1000, 1000, 1000, 1000), null, "fast"],
  ["anthropic/claude-opus-4-1", totals(1, 0, 0, 1), null, "standard"],
  ["dual", totals(1, 0, 0, 1), null, "standard"],
];
for (const [model, tot, reported, speed] of cases) {
  const record = { model, totals: tot, speed, reportedCostUsd: reported };
  rows.push({
    operation: "price",
    model,
    totals: tot,
    speed,
    reported,
    output: priceUsage(table, record, overrideTable),
    savings: cacheSavingsUsd(table, record, overrideTable),
  });
}
const aliases = { a: "b", b: "c", x: "y", y: "x", solo: "target" };
rows.push({
  operation: "aliases",
  aliases,
  output: [...resolveModelAliases(aliases).entries()].sort(([a], [b]) => (a < b ? -1 : 1)),
});

// Aggregation: the records the parsers above produced, plus a few more, in several zones.
const records = [];
const s = initialCodexScanState();
for (const record of claudeLines) {
  const r = parseClaudeLine(JSON.stringify(record));
  if (r) records.push(r);
}
for (const record of codexLines) {
  const r = parseCodexLine(JSON.stringify(record), s);
  if (r) records.push(r);
}
const aggregate = (options, sourcePath) => {
  const aggregator = new UsageAggregator({
    rates: table,
    priceOverrides: overrideTable,
    modelAliases: resolveModelAliases({ "claude-haiku-4-5[1m]": "claude-haiku-4-5" }),
    ...options,
  });
  const contributed = records.map((record) => aggregator.add(record, sourcePath));
  const finished = aggregator.finish();
  return { contributed, ...finished };
};
for (const timeZone of ["UTC", "America/Los_Angeles", "Asia/Kolkata", "+05:30", "Not/AZone"]) {
  const options = { timeZone, sinceDay: "2026-10-08", untilDay: "2026-10-09", resolution: "day" };
  rows.push({
    operation: "aggregate",
    options,
    sourcePath: "/h/.claude/projects",
    records,
    output: aggregate(options, "/h/.claude/projects"),
  });
}
{
  const options = {
    timeZone: "UTC",
    sinceDay: "2026-10-08",
    untilDay: "2026-10-09",
    resolution: "hour",
    sinceTimeMs: Date.parse("2026-10-08T09:30:00Z"),
    untilTimeMs: Date.parse("2026-10-08T11:00:00Z"),
  };
  rows.push({
    operation: "aggregate",
    options,
    sourcePath: "",
    records,
    output: aggregate(options, ""),
  });
}
{
  const options = {
    timeZone: "UTC",
    sinceDay: "2026-10-09",
    untilDay: "2026-10-09",
    resolution: "day",
  };
  rows.push({
    operation: "aggregate",
    options,
    sourcePath: "",
    records,
    output: aggregate(options, ""),
  });
}
const replacer = (key, value) => (value instanceof Map ? [...value.entries()] : value);
writeFileSync(
  new URL("../crates/server/tests/fixtures/usage-parity.json", import.meta.url),
  JSON.stringify(rows, replacer, 1) + "\n",
);
console.log(`${rows.length} usage witnesses`);
process.exit(0);
