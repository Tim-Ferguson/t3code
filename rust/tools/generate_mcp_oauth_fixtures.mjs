// Development-only witnesses of the MCP OAuth server (`apps/server/src/auth/McpOAuth.ts`): the signed client id, the CSRF token
// and the S256 challenge it makes, computed with the original's own `signPayload` / `base64UrlEncode` (`auth/utils.ts`) under a
// fixed key, and `redirectMatches` itself. The client id, CSRF token and PKCE check are closures of the original's `make`, so
// they are rebuilt here exactly as written there. Written to a `.jsonl` file so the formatter leaves it alone.
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { base64UrlEncode, signPayload } from "../../apps/server/src/auth/utils.ts";
import { redirectMatches } from "../../apps/server/src/auth/McpOAuth.ts";

const key = Uint8Array.from({ length: 32 }, (_, index) => index);
const sign = (domain, payload) => signPayload(`${domain}.${payload}`, key);
const signClientId = (name, redirectUris) => {
  const body = base64UrlEncode(JSON.stringify({ v: 1, n: name, r: redirectUris }));
  return `${body}.${sign("mcp-client-id", body)}`;
};

const rows = [];
const clients = [
  ["Claude Code", ["http://localhost/callback"]],
  ["Hosted bot", ["https://bot.example/oauth/callback", "http://127.0.0.1:8080/cb?x=1&y=2"]],
  ["MCP client", ["http://[::1]/cb"]],
  ['Quote " backslash \\ tab \t newline \n control \u0001 del \u007f', ["https://bot.example/a b"]],
  ["Line   para   sep, snowman ☃, emoji \u{1F600}, accent é", ["https://bot.example/ü?q=ö"]],
  [
    "x".repeat(100),
    [
      "http://localhost/a",
      "http://localhost/b",
      "http://localhost/c",
      "http://localhost/d",
      "https://e.example/",
    ],
  ],
];
for (const [name, redirectUris] of clients) {
  rows.push({ kind: "clientId", name, redirectUris, clientId: signClientId(name, redirectUris) });
}
const challenge = (verifier) => createHash("sha256").update(verifier).digest("base64url");
for (const verifier of [
  "a".repeat(43) + "-verifier-for-tests",
  "A".repeat(43),
  "z-._~".repeat(20),
]) {
  rows.push({ kind: "pkce", verifier, challenge: challenge(verifier) });
}
for (const [sessionId, clientId, redirectUri, codeChallenge] of [
  ["session-1", rows[0].clientId, "http://localhost:51234/callback", rows[6].challenge],
  ["session-2", rows[1].clientId, "https://bot.example/oauth/callback", rows[7].challenge],
]) {
  rows.push({
    kind: "csrf",
    sessionId,
    clientId,
    redirectUri,
    codeChallenge,
    token: sign("mcp-csrf", JSON.stringify([sessionId, clientId, redirectUri, codeChallenge])),
  });
}
const redirects = [
  ["http://localhost/callback", "http://localhost:9/callback"],
  ["http://127.0.0.1:1/callback", "http://127.0.0.1:2/callback"],
  ["http://localhost/callback", "http://127.0.0.1/callback"],
  ["http://localhost/callback", "https://localhost/callback"],
  ["https://bot.example/cb", "https://bot.example/cb"],
  ["https://bot.example/cb", "https://bot.example:8443/cb"],
  ["https://bot.example/cb", "https://bot.example:443/cb"],
  ["https://bot.example/cb", "https://bot.example/cb?x=1"],
  ["https://bot.example/cb", "https://evil.example/cb"],
  ["http://[::1]:5/cb", "http://[::1]:6/cb"],
  ["http://localhost/cb?a=1", "http://localhost:7/cb?a=1"],
  ["http://localhost/cb?a=1", "http://localhost:7/cb?a=2"],
  ["http://LOCALHOST/cb", "http://localhost:7/cb"],
  ["http://localhost/cb", "http://localhost:7/CB"],
  ["http://bot.example/cb", "http://bot.example/cb"],
  ["https://u:p@bot.example/cb", "https://u:p@bot.example/cb"],
  ["https://bot.example/cb#", "https://bot.example/cb"],
  ["javascript:alert(1)", "javascript:alert(1)"],
  ["http://localhost/" + "a".repeat(520), "http://localhost/" + "a".repeat(520)],
  ["https://bot.example/%7Euser", "https://bot.example/~user"],
  ["https://bot.example", "https://bot.example/"],
  ["http://127.1/cb", "http://127.0.0.1:3/cb"],
  ["not a url", "not a url"],
];
for (const [registered, presented] of redirects) {
  rows.push({
    kind: "redirectMatches",
    registered,
    presented,
    result: redirectMatches(registered, presented),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-oauth.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} rows`);
