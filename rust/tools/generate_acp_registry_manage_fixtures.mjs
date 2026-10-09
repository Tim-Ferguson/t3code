// From repo root, Node >=23.6: node rust/tools/generate_acp_registry_manage_fixtures.mjs
// Evaluates the unchanged source text of AcpRegistryProbe.ts that normalizes `session/list` and
// `providers/list` answers (and its exported URL rule), plus the source thread-id derivation.
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { normalizeAcpRegistryWebUrl } from "../../apps/server/src/provider/acp/AcpRegistryProbe.ts";
import { deriveThreadFromProviderThread } from "../../apps/server/src/orchestration-v2/IdAllocator.ts";

const source = fs.readFileSync("apps/server/src/provider/acp/AcpRegistryProbe.ts", "utf8");
const between = (start, end) => {
  const begin = source.indexOf(start);
  const finish = source.indexOf(end, begin + start.length);
  if (begin < 0 || finish < 0) throw Error("Missing original markers: " + start);
  return source.slice(begin, finish);
};
const strip = (text) => stripTypeScriptTypes(text, { mode: "transform" });
const helpers = strip(
  between("const boundedText = ", "\n\nexport function normalizeAcpRegistryWebUrl"),
);
const sessionsBody = between(
  "    const sessions = listed.sessions.slice(0, 256)",
  "    return AcpRegistryListSessionsResult.make({",
);
const sessionsTail = between(
  "    return AcpRegistryListSessionsResult.make({",
  "\n    });\n  },",
).replace("    return AcpRegistryListSessionsResult.make({", "");
const providersBody = between(
  "    return AcpRegistryListProvidersResult.make({",
  "\n    });\n  },",
).replace("    return AcpRegistryListProvidersResult.make({", "");
const normalizeSessions = new Function(
  "listed",
  "initialized",
  helpers + "\n" + sessionsBody + "\nreturn {" + sessionsTail + "\n};",
);
const normalizeProviders = new Function(
  "listed",
  "normalizeAcpRegistryWebUrl",
  helpers + "\nreturn {" + providersBody + "\n};",
);

const fixtures = [];
const long = (n, c = "a") => c.repeat(n);
const sessionCases = [
  { listed: { sessions: [{ sessionId: "s", cwd: "/p" }] }, caps: {} },
  {
    listed: {
      sessions: [
        {
          sessionId: "s",
          cwd: "/p",
          title: "  padded  ",
          updatedAt: "  2026  ",
          additionalDirectories: ["/a", " b", "", "/c"],
        },
      ],
    },
    caps: { loadSession: true, sessionCapabilities: { resume: {}, delete: {} } },
  },
  {
    listed: {
      sessions: [
        { sessionId: " s", cwd: "/p" },
        { sessionId: "ok", cwd: "/p " },
        { sessionId: "", cwd: "/p" },
        { sessionId: long(1025), cwd: "/p" },
        { sessionId: long(1024), cwd: long(4096) },
        { sessionId: long(1024), cwd: long(4097) },
      ],
    },
    caps: {},
  },
  {
    listed: { sessions: [{ sessionId: "s", cwd: "/p", title: long(1100), updatedAt: long(200) }] },
    caps: {},
  },
  { listed: { sessions: [{ sessionId: "s", cwd: "/p", title: "😀".repeat(600) }] }, caps: {} },
  { listed: { sessions: [{ sessionId: "s", cwd: "/p", title: "   " }] }, caps: {} },
  {
    listed: {
      sessions: [{ sessionId: "s", cwd: "/p", title: null, updatedAt: null }],
      nextCursor: null,
    },
    caps: { sessionCapabilities: { list: {} } },
  },
  { listed: { sessions: [], nextCursor: "  next  " }, caps: { loadSession: false } },
  { listed: { sessions: [], nextCursor: "" }, caps: {} },
  { listed: { sessions: [], nextCursor: long(3000) }, caps: {} },
  {
    listed: {
      sessions: Array.from({ length: 300 }, (_, i) => ({ sessionId: "s" + i, cwd: "/p" })),
    },
    caps: {},
  },
  {
    listed: {
      sessions: [
        {
          sessionId: "s",
          cwd: "/p",
          additionalDirectories: Array.from({ length: 40 }, (_, i) => "/d" + i),
        },
      ],
    },
    caps: {},
  },
  {
    listed: { sessions: [{ sessionId: "s", cwd: "/p" }] },
    caps: { loadSession: "yes", sessionCapabilities: { resume: null, delete: false } },
  },
];
for (const { listed, caps } of sessionCases) {
  const initialized = { agentCapabilities: caps };
  fixtures.push({
    kind: "sessions",
    listed,
    initialized,
    expected: JSON.parse(JSON.stringify(normalizeSessions(listed, initialized))),
  });
}
const providerCases = [
  {
    providers: [
      {
        providerId: "p",
        supported: ["a", "b"],
        required: false,
        current: { apiType: "a", baseUrl: "https://x.test/v1" },
      },
    ],
  },
  { providers: [{ providerId: "p", supported: [], required: true, current: null }] },
  {
    providers: [
      {
        providerId: "p",
        supported: ["a"],
        required: false,
        current: { apiType: "a", baseUrl: "ftp://x.test" },
      },
    ],
  },
  {
    providers: [
      {
        providerId: "p",
        supported: ["a"],
        required: false,
        current: { apiType: " a", baseUrl: "https://x.test" },
      },
    ],
  },
  {
    providers: [
      {
        providerId: "p",
        supported: ["a"],
        required: false,
        current: { apiType: "a", baseUrl: "HTTPS://X.TEST:443/a/../b" },
      },
    ],
  },
  {
    providers: [
      { providerId: " p", supported: ["a"], required: false },
      { providerId: "", supported: [], required: false },
      { providerId: long(257), supported: [], required: false },
    ],
  },
  {
    providers: [
      {
        providerId: "p",
        supported: [" a", "", long(65), long(64), ...Array.from({ length: 20 }, (_, i) => "v" + i)],
        required: false,
      },
    ],
  },
  {
    providers: Array.from({ length: 70 }, (_, i) => ({
      providerId: "p" + i,
      supported: ["a"],
      required: i % 2 === 0,
    })),
  },
  {
    providers: [
      {
        providerId: "p",
        supported: ["a"],
        required: false,
        current: { apiType: "a", baseUrl: "https://x.test/" + long(2100) },
      },
    ],
  },
];
for (const listed of providerCases)
  fixtures.push({
    kind: "providers",
    listed,
    expected: JSON.parse(JSON.stringify(normalizeProviders(listed, normalizeAcpRegistryWebUrl))),
  });
for (const url of [
  "https://a.test",
  "http://a.test:80/x?y=1#z",
  "HTTP://A.TEST",
  "ftp://a.test",
  "javascript:alert(1)",
  "not a url",
  "",
  "https://" + long(2100),
  "https://user:pass@a.test/",
  "https://a.test/ü",
  "//a.test",
  "https://a.test/a b",
])
  fixtures.push({ kind: "url", url, expected: normalizeAcpRegistryWebUrl(url) ?? null });
for (const [instance, session] of [
  ["agent", "s-1"],
  ["work space", "a/b:c"],
  ["i", "ü😀"],
  ["i", "100%"],
  ["i", "a b"],
  ["x:y", "z"],
])
  fixtures.push({
    kind: "thread",
    instance,
    session,
    expected: deriveThreadFromProviderThread({
      driver: "acpRegistry",
      providerInstanceId: instance,
      nativeThreadId: session,
    }),
  });
fs.writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-manage.jsonl",
  fixtures.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
console.log(fixtures.length + " unchanged-source ACP registry management witnesses");
