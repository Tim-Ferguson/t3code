// Runs the unchanged ChatGPT sign-in URL checks (packages/shared/src/codexAuthHandoff.ts, providerAuthReturnUrl.ts) over fixed inputs
// and records the answers for the Rust port (`codex_auth_handoff.rs`).
//   node rust/tools/generate_codex_auth_handoff_fixtures.mjs          writes crates/server/tests/fixtures/codex-auth-handoff.jsonl
//   node rust/tools/generate_codex_auth_handoff_fixtures.mjs --check  compares with the file
import * as NodeFS from "node:fs";

import {
  codexAuthDeliveryUrl,
  codexAuthHandoffUrl,
  codexAuthorizationRequest,
  codexCallbackUrl,
  readCodexAuthDelivery,
  readCodexAuthHandoff,
} from "../../packages/shared/src/codexAuthHandoff.ts";
import { providerAuthReturnUrl } from "../../packages/shared/src/providerAuthReturnUrl.ts";

const target = new URL("../crates/server/tests/fixtures/codex-auth-handoff.jsonl", import.meta.url);
const rows = [];
const attempt = (run) => {
  try {
    return { ok: run() };
  } catch (error) {
    return { error: error instanceof Error ? error.message : String(error) };
  }
};

const state = "abcdefghijklmnopqrstuvwxyz012345";
const challenge = "A".repeat(43);
const authorize = (overrides = {}, drop = []) => {
  const url = new URL("https://auth.openai.com/api/accounts/authorize");
  const parameters = {
    client_id: "dynamic_agent_client",
    redirect_uri: "http://127.0.0.1:41234/auth/callback",
    response_type: "code",
    code_challenge_method: "S256",
    code_challenge: challenge,
    state,
    scope: "openid",
    ...overrides,
  };
  for (const [key, value] of Object.entries(parameters))
    if (!drop.includes(key)) url.searchParams.set(key, value);
  return url.toString();
};
const valid = authorize();
const environmentId = "environment-1";
const handoff = {
  authorizationUrl: valid,
  returnUrl: "http://localhost:5173/settings/providers?instanceId=codex",
  environmentId,
  instanceId: "codex",
  flowId: "flow-1",
};

for (const [name, value] of [
  ["valid", valid],
  ["an app client id", authorize({ client_id: "oaiapp_abc-123" })],
  ["another client id", authorize({ client_id: "other" })],
  ["another origin", valid.replace("https://auth.openai.com", "https://evil.example")],
  ["another path", valid.replace("/api/accounts/authorize", "/oauth/authorize")],
  ["credentials in the url", valid.replace("https://", "https://user:pass@")],
  ["a fragment", `${valid}#x`],
  ["a redirect off loopback", authorize({ redirect_uri: "http://localhost:41234/auth/callback" })],
  ["a redirect on https", authorize({ redirect_uri: "https://127.0.0.1:41234/auth/callback" })],
  ["a redirect with another path", authorize({ redirect_uri: "http://127.0.0.1:41234/cb" })],
  ["a port out of range", authorize({ redirect_uri: "http://127.0.0.1:70000/auth/callback" })],
  ["port zero", authorize({ redirect_uri: "http://127.0.0.1:0/auth/callback" })],
  ["a short state", authorize({ state: "short" })],
  ["a state with symbols", authorize({ state: "abcdefghijklmnopqrstuvwxyz01234!" })],
  ["another response type", authorize({ response_type: "token" })],
  ["another challenge method", authorize({ code_challenge_method: "plain" })],
  ["a short challenge", authorize({ code_challenge: "short" })],
  ["no state", authorize({}, ["state"])],
  ["a repeated state", `${valid}&state=${state}`],
  ["an empty state", authorize({ state: "" })],
  ["too long", authorize({ scope: "x".repeat(17000) })],
  ["not a url", "nope"],
]) {
  rows.push({
    kind: "authorization",
    name,
    value,
    ...attempt(() => codexAuthorizationRequest(value)),
  });
}

const redirect = "http://127.0.0.1:41234/auth/callback";
const callback = (query) => `${redirect}?${query}`;
for (const [name, value, redirectUri, expected] of [
  ["a code", callback(`code=abc&state=${state}`), redirect, state],
  ["an error", callback(`error=access_denied&state=${state}`), redirect, state],
  [
    "a code with an app client",
    callback(`code=abc&state=${state}&client_id=oaiapp_x1`),
    redirect,
    state,
  ],
  [
    "a code with another client",
    callback(`code=abc&state=${state}&client_id=nope`),
    redirect,
    state,
  ],
  [
    "two client ids",
    callback(`code=abc&state=${state}&client_id=oaiapp_a&client_id=oaiapp_b`),
    redirect,
    state,
  ],
  ["another state", callback(`code=abc&state=other`), redirect, state],
  ["no state", callback(`code=abc`), redirect, state],
  ["a code and an error", callback(`code=abc&error=x&state=${state}`), redirect, state],
  ["neither", callback(`state=${state}`), redirect, state],
  ["an empty code", callback(`code=&state=${state}`), redirect, state],
  ["two codes", callback(`code=a&code=b&state=${state}`), redirect, state],
  [
    "another origin",
    `http://127.0.0.1:41235/auth/callback?code=abc&state=${state}`,
    redirect,
    state,
  ],
  ["another path", `http://127.0.0.1:41234/other?code=abc&state=${state}`, redirect, state],
  ["a fragment", `${callback(`code=abc&state=${state}`)}#x`, redirect, state],
  [
    "credentials",
    callback(`code=abc&state=${state}`).replace("http://", "http://a:b@"),
    redirect,
    state,
  ],
  ["too long", callback(`code=${"a".repeat(17000)}&state=${state}`), redirect, state],
]) {
  rows.push({
    kind: "callback",
    name,
    value,
    redirectUri,
    state: expected,
    ...attempt(() => codexCallbackUrl(value, redirectUri, expected).toString()),
  });
}

const returns = [
  "http://localhost:5173/welcome",
  "http://localhost:5173/welcome?machine=a&project=b",
  "http://localhost:5173/welcome#agents:abc-1",
  "http://localhost:5173/welcome#other",
  "http://127.0.0.1:3000/settings",
  "http://127.0.0.1:3000/settings/providers?machine=a&other=b&instanceId=codex",
  "http://[::1]:3000/settings/x?environmentId=e&junk=1#frag",
  "http://0.0.0.0:3000/welcome",
  "https://app.t3.codes/welcome",
  "https://app.t3.codes/settings/providers?checkout=c&q=1",
  "https://app.t3.codes/other",
  "https://evil.example/welcome",
  "https://app.t3.codes.evil.example/welcome",
  "t3code://app/welcome",
  "t3code-dev://app/settings/x?project=p",
  "t3code://other/welcome",
  "t3code://app/other",
  "ftp://localhost/welcome",
  "http://user:pass@localhost/welcome",
  "http://localhost/settingsx",
  "http://localhost/settings/",
  "http://localhost/welcome/",
  "http://localhost:5173/settings/a b?project=a%20b&x=1",
  "not a url",
  "",
];
for (const value of returns)
  rows.push({ kind: "return", value, answer: providerAuthReturnUrl(value) ?? null });
rows.push({ kind: "return", value: null, answer: providerAuthReturnUrl(undefined) ?? null });

for (const development of [false, true]) {
  const url = codexAuthHandoffUrl(handoff, development);
  rows.push({
    kind: "handoff",
    development,
    input: handoff,
    url,
    read: readCodexAuthHandoff(url, development) ?? null,
    readOther: readCodexAuthHandoff(url, !development) ?? null,
  });
}
{
  const url = codexAuthHandoffUrl(handoff, false);
  const encoded = (input) => attempt(() => codexAuthHandoffUrl(input, false));
  const bad = [
    ["another scheme", { ok: url.replace("t3code:", "t3codex:") }],
    ["another host", { ok: url.replace("//auth/", "//other/") }],
    ["another path", { ok: url.replace("/codex", "/claude") }],
    ["a fragment", { ok: `${url}#x` }],
    ["two requests", { ok: `${url}&request=x` }],
    ["no request", { ok: "t3code://auth/codex" }],
    ["a request that is not json", { ok: "t3code://auth/codex?request=nope" }],
    [
      "a bad authorization url",
      encoded({ ...handoff, authorizationUrl: authorize({ client_id: "x" }) }),
    ],
    ["a bad return address", encoded({ ...handoff, returnUrl: "https://evil.example/welcome" })],
    ["an empty flow id", encoded({ ...handoff, flowId: "" })],
    ["a long flow id", encoded({ ...handoff, flowId: "f".repeat(129) })],
  ];
  for (const [name, built] of bad)
    rows.push({
      kind: "handoff-read",
      name,
      value: built.ok ?? null,
      encodeError: built.error ?? null,
      read: built.ok ? (readCodexAuthHandoff(built.ok, false) ?? null) : null,
    });
}

const delivered = attempt(() => codexAuthDeliveryUrl(handoff, callback(`code=abc&state=${state}`)));
rows.push({
  kind: "delivery",
  name: "a code",
  input: handoff,
  callbackUrl: callback(`code=abc&state=${state}`),
  ...delivered,
  read: delivered.ok ? (readCodexAuthDelivery(delivered.ok) ?? null) : null,
});
for (const [name, input, callbackUrl] of [
  ["an error", handoff, callback(`error=access_denied&state=${state}`)],
  [
    "a return with a hash",
    { ...handoff, returnUrl: "http://localhost:5173/welcome#agents:x-1" },
    callback(`code=c&state=${state}`),
  ],
  ["a callback for another state", handoff, callback(`code=abc&state=other`)],
  [
    "a bad return address",
    { ...handoff, returnUrl: "https://evil.example/welcome" },
    callback(`code=abc&state=${state}`),
  ],
]) {
  const result = attempt(() => codexAuthDeliveryUrl(input, callbackUrl));
  rows.push({
    kind: "delivery",
    name,
    input,
    callbackUrl,
    ...result,
    read: result.ok ? (readCodexAuthDelivery(result.ok) ?? null) : null,
  });
}
for (const [name, value] of [
  ["no hash", "http://localhost:5173/welcome"],
  ["another hash", "http://localhost:5173/welcome#other=1"],
  ["not json", "http://localhost:5173/welcome#codex-auth=nope"],
  [
    "a bad return",
    `https://evil.example/welcome#codex-auth=${encodeURIComponent(JSON.stringify({ environmentId, instanceId: "codex", flowId: "f", callbackUrl: "x", returnHash: "" }))}`,
  ],
])
  rows.push({ kind: "delivery-read", name, value, read: readCodexAuthDelivery(value) ?? null });

const output = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
if (process.argv.includes("--check")) {
  if (NodeFS.readFileSync(target, "utf8") !== output) {
    console.error(
      "codex-auth-handoff.jsonl is out of date; run rust/tools/generate_codex_auth_handoff_fixtures.mjs",
    );
    process.exit(1);
  }
} else {
  NodeFS.writeFileSync(target, output);
  console.log(`wrote ${rows.length} rows (${output.length} bytes)`);
}
