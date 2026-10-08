// Execute the unchanged original background contracts and policy decisions.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as DateTime from "../../packages/contracts/node_modules/effect/dist/DateTime.js";
import * as Background from "../../packages/contracts/src/background.ts";
import { getBackgroundActivityPresetSettings } from "../../packages/shared/src/backgroundActivitySettings.ts";
const source = readFileSync(
  new URL("../../apps/server/src/background/BackgroundPolicy.ts", import.meta.url),
  "utf8",
);
const helpers = new Function(
  "DateTime",
  stripTypeScriptTypes(
    source.slice(
      source.indexOf("function scopeKey("),
      source.indexOf("/** @public Service construction"),
    ),
    { mode: "strip" },
  ) +
    ";return {compute:computeSnapshot,upsert:upsertClientActivityLease,run:leaseMayRunScopedWork,host:isHostConstrained};",
)(DateTime);
// upsert references this source constant, copied from its literal declaration.
// Give that helper a lexical binding instead of rewriting its function body.
const upsert = new Function(
  "DateTime",
  source
    .match(/export const MAX_CLIENT_ACTIVITY_LEASES_PER_RPC_CLIENT = \d+;/)[0]
    .replace("export ", "") +
    stripTypeScriptTypes(
      source.slice(
        source.indexOf("function scopeKey("),
        source.indexOf("/** @public Service construction"),
      ),
      { mode: "strip" },
    ) +
    ";return upsertClientActivityLease;",
)(DateTime);
const codec = (name) => Schema.toCodecJson(Background[name]),
  decode = (name, value) => Schema.decodeUnknownSync(codec(name))(value),
  wire = (name, value) => Schema.encodeSync(codec(name))(value);
const at = "2026-10-08T00:00:00.000Z",
  later = "2026-10-08T00:01:00.000Z",
  now = DateTime.makeUnsafe(at);
const power = {
  source: "electron-main",
  idle: "false",
  idleSeconds: 0,
  locked: "false",
  suspended: false,
  onBattery: "false",
  lowPowerMode: "false",
  thermalState: "nominal",
  stale: false,
  updatedAt: at,
};
const report = {
  clientId: "client",
  clientKind: "web",
  visible: true,
  focused: true,
  recentlyInteracted: false,
  scopes: [{ type: "diagnostics" }],
  observedAt: at,
};
const lease = { sessionId: "session", rpcClientId: 0, ...report, updatedAt: at, expiresAt: later };
delete lease.observedAt;
const snapshot = {
  hostPower: power,
  leases: [lease],
  activeForegroundLeaseCount: 1,
  activeScopeKeys: ["diagnostics"],
  shouldRunOpportunisticWork: true,
  updatedAt: at,
};
const templates = {
  BackgroundScope: { type: "thread", threadId: "thread" },
  ClientKind: "web",
  ClientActivityClientId: "client",
  ClientActivityReportInput: report,
  ClientActivityLease: lease,
  BackgroundPolicySnapshot: snapshot,
};
const codecs = [];
for (const [name, input] of Object.entries(templates)) {
  const candidates = [
    input,
    null,
    [],
    false,
    0,
    "",
    {},
    [input],
    ...(typeof input === "object"
      ? Object.keys(input).flatMap((key) =>
          [null, [], false, 0, {}, ""].map((value) => ({ ...input, [key]: value })),
        )
      : []),
  ];
  if (typeof input === "object")
    for (const key of Object.keys(input)) {
      const copy = { ...input };
      delete copy[key];
      candidates.push(copy);
    }
  if (name === "BackgroundScope")
    candidates.push(
      ...["server-config", "provider-status", "vcs-status", "git-refs", "diagnostics"].flatMap(
        (type) => [
          { type },
          { type, instanceId: null, cwd: "" },
          { type, instanceId: "work", cwd: "", threadId: "thread" },
        ],
      ),
    );
  if (name === "ClientActivityReportInput")
    candidates.push(
      ...[
        "environmentId",
        "appState",
        "lowPowerMode",
        "batteryState",
        "networkType",
        "ttlMs",
      ].flatMap((key) => [null, [], false, 0, {}, ""].map((value) => ({ ...input, [key]: value }))),
      { ...input, clientId: "😀".repeat(64) },
      { ...input, clientId: "😀".repeat(65) },
      { ...input, ttlMs: 1000.5 },
    );
  if (name === "ClientActivityClientId")
    candidates.push("  client  ", "\uFEFFclient\uFEFF", "😀".repeat(64), "😀".repeat(65));
  for (const input of candidates) {
    try {
      codecs.push({ name, input, accepted: true, output: wire(name, decode(name, input)) });
    } catch {
      codecs.push({ name, input, accepted: false });
    }
  }
}
const policies = [];
const powers = [
  power,
  ...["suspended", "stale"].map((key) => ({ ...power, [key]: true })),
  ...["idle", "locked", "onBattery", "lowPowerMode"].flatMap((key) =>
    ["true", "unknown"].map((value) => ({ ...power, [key]: value })),
  ),
  ...["serious", "critical", "unknown"].map((thermalState) => ({ ...power, thermalState })),
];
const leases = [
  [],
  [lease],
  ...[
    { visible: false },
    { focused: false },
    { focused: false, recentlyInteracted: true },
    { expiresAt: at },
    { expiresAt: "2026-10-07T23:59:59.999Z" },
    { lowPowerMode: "true" },
    { batteryState: "unplugged" },
    {
      scopes: [
        { type: "provider-status" },
        { type: "provider-status", instanceId: "work" },
        { type: "vcs-status", cwd: "" },
        { type: "git-refs", cwd: "😀" },
        { type: "git-refs", cwd: "\uFFFF" },
      ],
    },
  ].map((patch) => [{ ...lease, ...patch }]),
];
for (const profile of ["balanced", "performance", "battery-saver"])
  for (const host of powers)
    for (const rows of leases) {
      const settings = getBackgroundActivityPresetSettings(profile),
        hostPower = decode("HostPowerSnapshot", host),
        map = new Map(
          rows.map((value, index) => [String(index), decode("ClientActivityLease", value)]),
        );
      const output = wire(
        "BackgroundPolicySnapshot",
        helpers.compute({ hostPower, leases: map, now, settings, updatedAt: now }),
      );
      const scope = { type: "diagnostics" };
      policies.push({
        op: "compute",
        power: host,
        leases: rows,
        now: at,
        settings,
        output,
        scope,
        run:
          !helpers.host(hostPower, settings) &&
          [...map.values()].some((value) => helpers.run(value, scope, now, settings)),
      });
    }
for (const count of [0, 1, 15, 16, 17])
  for (const extra of [
    {},
    { clientId: "client-0" },
    { sessionId: "other" },
    { rpcClientId: 1 },
    { updatedAt: "2026-10-07T23:59:59.000Z" },
  ]) {
    const rows = Array.from({ length: count }, (_, index) => ({
      ...lease,
      clientId: `client-${index}`,
      updatedAt: `2026-10-07T23:59:${String(index).padStart(2, "0")}.000Z`,
    }));
    const incoming = { ...lease, clientId: "incoming", ...extra },
      map = new Map(
        rows.map((value) => [
          JSON.stringify([value.sessionId, value.rpcClientId, value.clientId]),
          decode("ClientActivityLease", value),
        ]),
      );
    const output = upsert(map, decode("ClientActivityLease", incoming), now);
    policies.push({
      op: "upsert",
      leases: rows,
      incoming,
      now: at,
      output: [...output.values()].map((value) => wire("ClientActivityLease", value)),
    });
  }
writeFileSync(
  new URL("../crates/contracts/tests/fixtures/background.jsonl", import.meta.url),
  codecs.map(JSON.stringify).join("\n") + "\n",
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/background-policy.jsonl", import.meta.url),
  policies.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ codecs: codecs.length, policies: policies.length }));
