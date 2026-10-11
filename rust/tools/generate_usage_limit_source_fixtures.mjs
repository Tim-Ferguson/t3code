// Runs the unchanged CLIProxyAPI hub client (apps/server/src/usage/cliproxyApi.ts) against a scripted hub and writes what it
// read, what it redeemed and every request it made, for the Rust port to replay against the same script served from a
// loopback socket (crates/server/tests/usage_limit_sources_flow.rs). The clock is fixed; no network is used.
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as TestClock from "../../apps/server/node_modules/effect/dist/testing/TestClock.js";
import * as NodeCrypto from "../../apps/server/node_modules/@effect/platform-node/dist/NodeCrypto.js";
import {
  HttpClient,
  HttpClientResponse,
} from "../../apps/server/node_modules/effect/dist/http/index.js";
import { makeCliproxyApi } from "../../apps/server/src/usage/cliproxyApi.ts";
import {
  codexRateLimitsToLimits,
  codexRateLimitsToUpdate,
} from "../../apps/server/src/provider/codexUsageLimits.ts";
import { codexPlanLabel } from "../../apps/server/src/provider/CodexProvider.ts";
import {
  sameUsageLimitCommandCoverage,
  withUsageLimitsCommands,
} from "../../packages/shared/src/usageLimits.ts";

const NOW = 1788710400000;
const config = {
  kind: "cliproxy",
  url: "http://hub.test:8317",
  managementKey: "management-secret",
  enabled: true,
};
const USAGE = "https://chatgpt.com/backend-api/wham/usage";
const CREDITS = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CONSUME = `${CREDITS}/consume`;
const CLAUDE = "https://api.anthropic.com/api/oauth/usage";

const codex = (id, extra = {}) => ({
  id,
  auth_index: id.replace(".json", ""),
  provider: "codex",
  email: `${id.replace(".json", "")}@example.com`,
  id_token: { chatgpt_account_id: `acct-${id.replace(".json", "")}` },
  ...extra,
});
const credit = (id, expires_at = "2099-01-01T00:00:00Z", extra = {}) => ({
  id,
  expires_at,
  status: "available",
  reset_type: "codex_rate_limits",
  ...extra,
});
const window = (used_percent, extra = {}) => ({ used_percent, ...extra });
const usage = (extra = {}) => ({
  plan_type: "pro",
  rate_limit: {
    primary_window: window(12.5, { reset_at: 1_800_000_000, limit_window_seconds: 18000 }),
    secondary_window: window(78, { reset_at: 4070908800, limit_window_seconds: 604800 }),
  },
  ...extra,
});
const ok = (body) => ({ status: 200, body });
const wholeCredits = {
  credits: [
    credit("later", "2099-02-01T00:00:00Z"),
    credit("first"),
    credit("expired", "2000-01-01T00:00:00Z"),
    credit("used", "2099-01-01T00:00:00Z", { status: "redeemed" }),
    credit("other-kind", "2098-01-01T00:00:00Z", { reset_type: "something_else" }),
  ],
};
const upstreamFor = (index, entries) =>
  Object.fromEntries(Object.entries(entries).map(([url, answer]) => [`${index}|${url}`, answer]));

const scenarios = [
  {
    name: "two codex accounts with credits, the earliest unexpired first",
    op: "read",
    files: [codex("a.json"), codex("b.json")],
    upstream: {
      ...upstreamFor("a", { [USAGE]: ok(usage()), [CREDITS]: ok(wholeCredits) }),
      ...upstreamFor("b", {
        [USAGE]: ok(usage({ plan_type: "plus" })),
        [CREDITS]: ok(wholeCredits),
      }),
    },
  },
  {
    name: "claude account with scoped weekly limits",
    op: "read",
    files: [{ id: "c.json", auth_index: "c", provider: "claude", email: "c@example.com" }],
    upstream: upstreamFor("c", {
      [CLAUDE]: ok({
        five_hour: { utilization: 31, resets_at: "2026-09-06T20:00:00Z" },
        seven_day: { utilization: 64.5, resets_at: null },
        limits: [
          {
            kind: "weekly_scoped",
            percent: 12,
            resets_at: "2026-09-10T00:00:00Z",
            scope: { model: { display_name: "Fable 5" } },
          },
          {
            kind: "weekly_scoped",
            percent: null,
            scope: { model: { display_name: "No percent" } },
          },
          { kind: "weekly_scoped", percent: 3, scope: null },
          { kind: "weekly_scoped", percent: 4, scope: { model: null } },
          { kind: "session", percent: 5, scope: { model: { display_name: "Wrong kind" } } },
        ],
      }),
    }),
  },
  {
    name: "claude usage with nothing but the two windows",
    op: "read",
    files: [{ id: "c.json", auth_index: "c", provider: "claude" }],
    upstream: upstreamFor("c", { [CLAUDE]: ok({ five_hour: null }) }),
  },
  {
    name: "claude usage of the wrong shape reads as unavailable",
    op: "read",
    files: [
      { id: "c1.json", auth_index: "c1", provider: "claude" },
      { id: "c2.json", auth_index: "c2", provider: "claude" },
      { id: "c3.json", auth_index: "c3", provider: "claude" },
    ],
    upstream: {
      ...upstreamFor("c1", { [CLAUDE]: ok({ five_hour: { utilization: "x", resets_at: null } }) }),
      ...upstreamFor("c2", { [CLAUDE]: ok({ limits: null }) }),
      ...upstreamFor("c3", { [CLAUDE]: ok("not json") }),
    },
  },
  {
    name: "disabled and other providers are not read",
    op: "read",
    files: [
      codex("a.json", { disabled: true }),
      { id: "g.json", auth_index: "g", provider: "gemini" },
      codex("b.json", { disabled: false }),
    ],
    upstream: upstreamFor("b", { [USAGE]: ok(usage()), [CREDITS]: ok({ credits: [] }) }),
  },
  {
    name: "a provider that refuses, answers badly or is down reads as unavailable",
    op: "read",
    files: [
      codex("a.json"),
      codex("b.json"),
      codex("c.json"),
      codex("d.json"),
      codex("e.json"),
      codex("f.json"),
    ],
    upstream: {
      ...upstreamFor("a", { [USAGE]: { status: 429, body: {} } }),
      ...upstreamFor("b", { [USAGE]: ok("not json at all") }),
      ...upstreamFor("c", { [USAGE]: ok({ plan_type: "pro" }) }),
      ...upstreamFor("d", { [USAGE]: { http: 502 } }),
      ...upstreamFor("e", { [USAGE]: { envelope: { status_code: "200", body: "{}" } } }),
      ...upstreamFor("f", {
        [USAGE]: ok({ rate_limit: { primary_window: { used_percent: "12" } } }),
      }),
    },
  },
  {
    name: "a null rate limit reports no windows, and a credits outage keeps the windows",
    op: "read",
    files: [codex("a.json"), codex("b.json"), codex("c.json")],
    upstream: {
      ...upstreamFor("a", {
        [USAGE]: ok({ plan_type: "team", rate_limit: null }),
        [CREDITS]: ok({ credits: [credit("x")] }),
      }),
      ...upstreamFor("b", { [USAGE]: ok(usage()), [CREDITS]: { status: 500, body: {} } }),
      ...upstreamFor("c", { [USAGE]: ok(usage()), [CREDITS]: ok({ credits: "none" }) }),
    },
  },
  {
    name: "windows: fallbacks, clamping, fractions and plans",
    op: "read",
    files: [
      codex("free.json", { id_token: undefined }),
      codex("go.json", { id_token: { chatgpt_plan_type: "business" } }),
      codex("odd.json", { email: "", id_token: { chatgpt_account_id: "" } }),
      codex("bare.json", { email: undefined }),
    ],
    upstream: {
      ...upstreamFor("free", {
        [USAGE]: ok({
          plan_type: "free",
          rate_limit: { primary_window: window(0), secondary_window: null },
        }),
        [CREDITS]: ok({ credits: [] }),
      }),
      ...upstreamFor("go", {
        [USAGE]: ok({
          rate_limit: {
            primary_window: window(101, { reset_at: null }),
            secondary_window: window(-5, { reset_at: 0, limit_window_seconds: 90 }),
          },
        }),
        [CREDITS]: ok({ credits: [credit("a", "2099-01-01")] }),
      }),
      ...upstreamFor("odd", {
        [USAGE]: ok({
          plan_type: "mystery",
          rate_limit: {
            primary_window: window(50, { reset_at: 9e18, limit_window_seconds: 2592000 }),
          },
        }),
        [CREDITS]: ok({
          credits: [credit("b", "2099-01-01T00:00:00.5Z"), credit("c", "2099-01-01T00:00:00.9Z")],
        }),
      }),
      ...upstreamFor("bare", {
        [USAGE]: ok({ plan_type: "enterprise", rate_limit: { secondary_window: window(7) } }),
        [CREDITS]: ok({ credits: [] }),
      }),
    },
  },
  {
    name: "the hub cannot list accounts",
    op: "read",
    authStatus: 500,
    files: [],
    upstream: {},
  },
  {
    name: "the account list of the wrong shape",
    op: "read",
    authFilesBody: { files: [{ id: "a.json", provider: "codex" }] },
    files: [],
    upstream: {},
  },
  {
    name: "the account list without files",
    op: "read",
    authFilesBody: { accounts: [] },
    files: [],
    upstream: {},
  },
  {
    name: "an empty hub",
    op: "read",
    files: [],
    upstream: {},
  },
  // ---- redemption
  ...[
    ["reset", "reset", 200],
    ["reset with the cooldown left in place", "reset", 500],
    ["already redeemed", "already_redeemed", 200],
    ["already redeemed with the cooldown left in place", "already_redeemed", 503],
    ["nothing to reset", "nothing_to_reset", 200],
    ["no credit", "no_credit", 200],
  ].map(([name, code, cooldownStatus]) => ({
    name: `redeem: ${name}`,
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    cooldownStatus,
    files: [codex("a.json"), codex("b.json")],
    upstream: upstreamFor("a", { [CONSUME]: ok({ code }) }),
  })),
  {
    name: "redeem: an account without a ChatGPT account id uses its file id",
    op: "consume",
    accountId: "plain.json",
    creditId: "c-1",
    files: [codex("plain.json", { id_token: undefined })],
    upstream: upstreamFor("plain", { [CONSUME]: ok({ code: "reset" }) }),
  },
  {
    name: "redeem: an answer in none of the four words",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    files: [codex("a.json")],
    upstream: upstreamFor("a", { [CONSUME]: ok({ code: "maybe" }) }),
  },
  {
    name: "redeem: an answer that is not JSON",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    files: [codex("a.json")],
    upstream: upstreamFor("a", { [CONSUME]: ok("<html>") }),
  },
  {
    name: "redeem: the provider refuses",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    files: [codex("a.json")],
    upstream: upstreamFor("a", { [CONSUME]: { status: 429, body: {} } }),
  },
  {
    name: "redeem: the hub fails the call",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    files: [codex("a.json")],
    upstream: upstreamFor("a", { [CONSUME]: { http: 500 } }),
  },
  {
    name: "redeem: the envelope of the wrong shape",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    files: [codex("a.json")],
    upstream: upstreamFor("a", { [CONSUME]: { envelope: { status_code: 200 } } }),
  },
  {
    name: "redeem: a missing, disabled or claude account",
    op: "consumeMany",
    attempts: [
      ["nope.json", "first"],
      ["off.json", "first"],
      ["claude.json", "first"],
    ],
    files: [
      codex("a.json"),
      codex("off.json", { disabled: true }),
      { id: "claude.json", auth_index: "claude", provider: "claude" },
    ],
    upstream: {},
  },
  {
    name: "redeem: the hub cannot list accounts",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    authStatus: 500,
    files: [],
    upstream: {},
  },
  {
    name: "redeem: the account list of the wrong shape",
    op: "consume",
    accountId: "a.json",
    creditId: "first",
    authFilesBody: { files: "none" },
    files: [],
    upstream: {},
  },
];

const decodeBody = (request) =>
  request.body._tag === "Uint8Array"
    ? JSON.parse(new TextDecoder().decode(request.body.body))
    : undefined;

function fixture(scenario) {
  const requests = [];
  const http = HttpClient.make((request) =>
    Effect.sync(() => {
      if (request.headers.authorization !== "Bearer management-secret")
        throw new Error("the management key was not sent");
      const path = new URL(request.url).pathname;
      const body = decodeBody(request);
      requests.push({
        method: request.method,
        path,
        ...(body
          ? {
              body: {
                ...body,
                ...(body.data === undefined ? {} : { data: JSON.parse(body.data) }),
              },
            }
          : {}),
      });
      const respond = (value, status = 200) =>
        HttpClientResponse.fromWeb(request, Response.json(value, { status }));
      if (path === "/v0/management/auth-files") {
        return scenario.authStatus
          ? respond({}, scenario.authStatus)
          : respond(scenario.authFilesBody ?? { files: scenario.files });
      }
      if (path === "/v0/management/reset-quota") return respond({}, scenario.cooldownStatus ?? 200);
      if (path !== "/v0/management/api-call") return respond({}, 404);
      const answer = scenario.upstream[`${body.auth_index}|${body.url}`];
      if (!answer) return respond({}, 500);
      if (answer.http) return respond({}, answer.http);
      if (answer.envelope !== undefined) return respond(answer.envelope);
      return respond({
        status_code: answer.status,
        body: typeof answer.body === "string" ? answer.body : JSON.stringify(answer.body),
      });
    }),
  );
  return {
    requests,
    api: makeCliproxyApi.pipe(
      Effect.provideService(HttpClient.HttpClient, http),
      Effect.provide(NodeCrypto.layer),
    ),
  };
}

const plain = (value) => JSON.parse(JSON.stringify(value ?? null));
const sorted = (requests) =>
  requests
    .map((request) => JSON.stringify(request))
    .sort()
    .map((text) => JSON.parse(text));

const rows = [];
for (const scenario of scenarios) {
  const test = fixture(scenario);
  const run = Effect.gen(function* () {
    yield* TestClock.setTime(NOW);
    const api = yield* test.api;
    if (scenario.op === "read") {
      const result = yield* api.readAccounts(config).pipe(Effect.result);
      return result._tag === "Success"
        ? { accounts: plain(result.success) }
        : { error: result.failure.detail };
    }
    const attempts =
      scenario.op === "consumeMany" ? scenario.attempts : [[scenario.accountId, scenario.creditId]];
    const results = [];
    for (const [accountId, creditId] of attempts) {
      const result = yield* api.consume(config, accountId, creditId).pipe(Effect.result);
      results.push(
        result._tag === "Success"
          ? { result: plain(result.success) }
          : { error: result.failure.detail },
      );
    }
    return scenario.op === "consumeMany" ? { results } : results[0];
  }).pipe(Effect.provide(TestClock.layer()));
  const output = await Effect.runPromise(run);
  const { name, ...input } = scenario;
  rows.push({
    kind: "hub",
    name,
    now: NOW,
    input: { ...plain(input), config: { url: config.url } },
    output,
    requests: sorted(test.requests),
  });
}

// ---- Codex's mapper, which the hub's answer goes through, and the plan labels
const checkedAt = "2026-09-06T16:00:00.000Z";
const snapshots = [
  {
    primary: { usedPercent: 10, resetsAt: 1_800_000_000, windowDurationMins: 300 },
    secondary: { usedPercent: 99.5, resetsAt: 1_800_500_000, windowDurationMins: 10080 },
  },
  { planType: "free", primary: { usedPercent: 3 } },
  {
    planType: "go",
    primary: { usedPercent: 3, windowDurationMins: 60 },
    secondary: { usedPercent: 4 },
  },
  { planType: "plus", primary: { usedPercent: 120 }, secondary: { usedPercent: -3 } },
  {
    limitId: "codex",
    primary: { usedPercent: 5, resetsAt: -1 },
    secondary: { usedPercent: 6, resetsAt: 9e18 },
  },
  { limitId: "codex_other", primary: { usedPercent: 5 } },
  { limitId: "", primary: { usedPercent: 5 } },
  { primary: { usedPercent: Number.NaN }, secondary: null },
  { primary: null },
  {
    primary: { usedPercent: 40, windowDurationMins: 43200 },
    secondary: { usedPercent: 1, windowDurationMins: 10080.5 },
  },
];
const byLimit = [
  undefined,
  null,
  { codex: { primary: { usedPercent: 77, windowDurationMins: 300 } } },
  { other: { primary: { usedPercent: 77 } } },
];
const credits = [
  undefined,
  null,
  { availableCount: 2 },
  { availableCount: -4, credits: [] },
  {
    availableCount: 3,
    credits: [
      { status: "available", expiresAt: 1_800_000_000 },
      { status: "available", expiresAt: 1_700_000_000 },
      { status: "redeemed", expiresAt: 1 },
      { status: "available" },
      { status: "available", expiresAt: null },
    ],
  },
  { availableCount: 1, credits: null },
];
for (const snapshot of snapshots) {
  rows.push({
    kind: "codexUpdate",
    snapshot,
    output: plain(codexRateLimitsToUpdate(snapshot) ?? null),
  });
  for (const rateLimitsByLimitId of byLimit) {
    for (const resetCredits of credits) {
      const input = {
        snapshot,
        ...(rateLimitsByLimitId === undefined ? {} : { rateLimitsByLimitId }),
        ...(resetCredits === undefined ? {} : { resetCredits }),
        checkedAt,
      };
      rows.push({
        kind: "codexLimits",
        input: plain(input),
        output: plain(codexRateLimitsToLimits(input)),
      });
    }
  }
}
for (const plan of [
  "free",
  "go",
  "plus",
  "pro",
  "prolite",
  "promax",
  "team",
  "self_serve_business_prolite",
  "self_serve_business_usage_based",
  "business",
  "ent26",
  "enterprise_cbp_automation",
  "enterprise_cbp_usage_based",
  "enterprise",
  "edu",
  "edu_plus",
  "edu_pro",
  "unknown",
  "",
  "Pro",
  "other",
  null,
]) {
  rows.push({ kind: "planLabel", plan, output: plain(codexPlanLabel(plan) ?? null) });
}

// ---- the `/usage-limits` command the provider catalogs carry
const provider = (instanceId, driver, extra = {}) => ({
  instanceId,
  driver,
  enabled: true,
  installed: true,
  slashCommands: [
    { name: "compact", description: "Compact" },
    { name: "usage-limits", description: "stale" },
  ],
  ...extra,
});
const limits = { checkedAt, windows: [] };
const source = (id, accounts, error) => ({
  id,
  kind: "cliproxy",
  label: id,
  checkedAt,
  accounts,
  ...(error ? { error } : {}),
});
const account = (driver) => ({ id: `${driver}.json`, driver, usageLimits: limits });
const catalogs = [
  [provider("codex", "codex"), provider("claudeAgent", "claudeAgent"), provider("grok", "grok")],
  [
    provider("codex", "codex", { usageLimits: limits }),
    provider("claudeAgent", "claudeAgent", { usageLimits: limits, availability: "unavailable" }),
    provider("off", "codex", { enabled: false, usageLimits: limits }),
    provider("old", "grok", { installed: false, usageLimits: limits }),
    provider("ws", "cursor", {
      usageLimits: limits,
      workspaceSnapshots: [{ cwd: "/x", slashCommands: [{ name: "one", description: "1" }] }],
    }),
  ],
];
const sourceSets = [
  [],
  [source("hub", [account("codex")])],
  [source("hub", [account("claudeAgent")]), source("other", [account("codex")])],
  [source("hub", [], "No management key configured.")],
  [source("hub", [account("codex")], "partial")],
  [source("hub", [])],
];
for (const providers of catalogs) {
  for (const sources of sourceSets) {
    rows.push({
      kind: "commands",
      providers,
      sources,
      output: plain(withUsageLimitsCommands(providers, sources)),
    });
  }
}
for (const previous of sourceSets) {
  for (const next of sourceSets) {
    rows.push({
      kind: "coverage",
      previous,
      next,
      output: sameUsageLimitCommandCoverage(previous, next),
    });
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/usage-limit-sources.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} scenarios`);
