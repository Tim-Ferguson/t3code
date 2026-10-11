// Development-only witnesses of the CLI the remote SSH launch and the auth commands depend on, computed by the unchanged
// original: what `t3 auth ...` prints (`cliAuthFormat.ts`), the pairing URL and the terminal QR code (`startupAccess.ts`), and the
// durations `--ttl` accepts (`cli/config.ts` `DurationFromString`). Written to a `.jsonl` file so the formatter leaves it alone.
import { writeFileSync } from "node:fs";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Duration from "../../apps/server/node_modules/effect/dist/Duration.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import {
  formatIssuedPairingCredential,
  formatIssuedSession,
  formatPairingCredentialList,
  formatSessionList,
} from "../../apps/server/src/cliAuthFormat.ts";
import { buildPairingUrl, renderTerminalQrCode } from "../../apps/server/src/startupAccess.ts";
import { DurationFromString } from "../../apps/server/src/cli/config.ts";

const at = (iso) => DateTime.makeUnsafe(iso);
const rows = [];

// Issued pairing credentials, as text and JSON, with and without a label and a base URL.
const issuedCases = [
  {
    id: "link-1",
    credential: "ABCD2345EFGH",
    scopes: ["orchestration:read", "orchestration:operate"],
    subject: "one-time-token",
    createdAt: at("2026-10-11T12:00:00.000Z"),
    expiresAt: at("2026-10-11T12:05:00.000Z"),
  },
  {
    id: "link-2",
    credential: "WXYZ6789JKLM",
    label: "t3 pair",
    scopes: ["orchestration:read"],
    subject: "one-time-token",
    createdAt: at("2026-10-11T12:00:00.000Z"),
    expiresAt: at("2026-10-11T12:30:00.123Z"),
  },
];
for (const issued of issuedCases)
  for (const baseUrl of [
    undefined,
    "https://box.example.ts.net",
    "http://127.0.0.1:3773/ignored?token=old&x=1 y",
  ])
    for (const json of [false, true]) {
      rows.push({
        kind: "issuedPairing",
        input: {
          id: issued.id,
          credential: issued.credential,
          label: issued.label ?? null,
          scopes: issued.scopes,
          expiresAt: DateTime.formatIso(issued.expiresAt),
          baseUrl: baseUrl ?? null,
          json,
        },
        output: formatIssuedPairingCredential(issued, { json, ...(baseUrl ? { baseUrl } : {}) }),
      });
    }

// Lists of pairing links.
const links = [
  {
    id: "link-1",
    label: "t3 pair",
    scopes: ["orchestration:read"],
    permissions: ["orchestration:read"],
    subject: "one-time-token",
    createdAt: at("2026-10-11T12:00:00.000Z"),
    expiresAt: at("2026-10-11T12:05:00.000Z"),
  },
  {
    id: "link-2",
    scopes: ["orchestration:read", "orchestration:operate"],
    subject: "one-time-token",
    createdAt: at("2026-10-11T11:00:00.000Z"),
    expiresAt: at("2026-10-11T11:05:00.000Z"),
  },
];
const wireLinks = links.map((link) => ({
  ...link,
  createdAt: DateTime.formatIso(link.createdAt),
  expiresAt: DateTime.formatIso(link.expiresAt),
}));
for (const json of [false, true])
  for (const subset of [[], wireLinks.slice(0, 1), wireLinks])
    rows.push({
      kind: "pairingList",
      rows: subset,
      json,
      output: formatPairingCredentialList(
        subset.map((link) => ({
          ...link,
          createdAt: at(link.createdAt),
          expiresAt: at(link.expiresAt),
        })),
        { json },
      ),
    });

// Issued sessions.
const sessions = [
  {
    sessionId: "s-1",
    token: "tok.en",
    method: "bearer-access-token",
    scopes: ["orchestration:read", "orchestration:operate"],
    subject: "cli-issued-session",
    client: { label: "ci", deviceType: "bot" },
    expiresAt: at("2026-11-10T12:00:00.000Z"),
  },
  {
    sessionId: "s-2",
    token: "t2",
    method: "bearer-access-token",
    scopes: ["orchestration:read"],
    subject: "x",
    client: { deviceType: "bot" },
    expiresAt: at("2026-11-10T12:00:00.000Z"),
  },
  {
    sessionId: "s-3",
    token: "t3",
    method: "bearer-access-token",
    scopes: ["orchestration:read"],
    subject: "x",
    client: { deviceType: "unknown" },
    expiresAt: at("2026-11-10T12:00:00.000Z"),
  },
  {
    sessionId: "s-4",
    token: "t4",
    method: "bearer-access-token",
    scopes: ["orchestration:read"],
    subject: "x",
    client: {
      label: "L",
      deviceType: "mobile",
      os: "iOS",
      browser: "Safari",
      ipAddress: "10.0.0.2",
    },
    expiresAt: at("2026-11-10T12:00:00.000Z"),
  },
];
for (const session of sessions)
  for (const [json, tokenOnly] of [
    [false, false],
    [true, false],
    [false, true],
  ])
    rows.push({
      kind: "issuedSession",
      session: { ...session, expiresAt: DateTime.formatIso(session.expiresAt) },
      json,
      tokenOnly,
      output: formatIssuedSession(session, { json, tokenOnly }),
    });

// Lists of sessions.
const listed = [
  {
    sessionId: "s-1",
    method: "bearer-access-token",
    scopes: ["orchestration:read"],
    subject: "cli-issued-session",
    client: { label: "ci", deviceType: "bot" },
    connected: true,
    current: false,
    issuedAt: "2026-10-11T12:00:00.000Z",
    expiresAt: "2026-11-10T12:00:00.000Z",
    lastConnectedAt: "2026-10-11T12:01:00.000Z",
  },
  {
    sessionId: "s-2",
    method: "browser-session-cookie",
    scopes: ["orchestration:read", "access:write"],
    subject: "one-time-token",
    client: { deviceType: "unknown" },
    connected: false,
    current: false,
    issuedAt: "2026-10-10T12:00:00.000Z",
    expiresAt: "2026-11-09T12:00:00.000Z",
    lastConnectedAt: null,
  },
];
const asDates = (session) => ({
  ...session,
  issuedAt: at(session.issuedAt),
  expiresAt: at(session.expiresAt),
  lastConnectedAt: session.lastConnectedAt ? at(session.lastConnectedAt) : null,
});
for (const json of [false, true])
  for (const subset of [[], listed.slice(0, 1), listed])
    rows.push({
      kind: "sessionList",
      rows: subset,
      json,
      output: formatSessionList(subset.map(asDates), { json }),
    });

// The pairing URL and the QR code of a URL.
for (const [base, token] of [
  ["http://localhost:3773", "ABCD2345EFGH"],
  ["https://box.example.ts.net:8443/x?token=old&keep=1#frag", "T0KEN+/="],
  ["http://[::1]:3000", "a b"],
]) {
  const url = buildPairingUrl(base, token);
  rows.push({ kind: "pairingUrl", base, token, url });
}
for (const url of [
  "http://localhost:3773/pair#token=ABCD2345EFGH",
  "https://box.example.ts.net/pair#token=" + "X".repeat(60),
]) {
  rows.push({ kind: "qr", url, output: renderTerminalQrCode(url) });
}

// `--ttl`.
const decode = Schema.decodeUnknownExit(DurationFromString);
for (const text of [
  "5m",
  "1h",
  "30d",
  "2w",
  "500ms",
  "10s",
  "15 minutes",
  "1 hour",
  "2 days",
  "  7d ",
  "5M",
  "0m",
  "",
  "abc",
  "5",
  "1.5h",
  "-5m",
  "3 fortnights",
  "12 SECONDS",
  "100 millis",
  "1 milli",
  "2 weeks",
  "1 week",
  "1 minute",
  "1 second",
  "3 Days",
  "3  days",
  "1 millisecond",
  "5 nanos",
  "1 hours",
]) {
  const exit = decode(text);
  rows.push({
    kind: "duration",
    text,
    millis: exit._tag === "Success" ? Duration.toMillis(exit.value) : null,
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/cli-output.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} rows`);
